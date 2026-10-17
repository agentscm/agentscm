use std::collections::HashSet;

use serde::{Deserialize, Serialize};

use crate::checks::CheckSubject;
use crate::context::{DependencyClaim, DependencyState};
use crate::{Error, Result};

pub const EVIDENCE_VERSION: u32 = 1;
pub const MAX_EVIDENCE_BYTES: usize = 256 * 1024;
pub const MAX_ARTIFACTS: usize = 64;
pub const MAX_DEPENDENCIES: usize = 128;
pub const MAX_COVERAGE_OMISSIONS: usize = 128;
pub const MAX_COVERAGE_SCOPE: usize = 128;
pub const MAX_SUPPLIED_ARTIFACT_BYTES: usize = 64 * 1024 * 1024;

pub(crate) const MAX_IDENTIFIER_BYTES: usize = 128;
pub(crate) const MAX_TEXT_BYTES: usize = 2 * 1024;
pub(crate) const SIGNATURE_DOMAIN: &[u8] = b"agentscm.portable-evidence.v1";

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct EvidenceEnvelope {
    pub version: u32,
    pub algorithm: SignatureAlgorithm,
    pub payload: EvidencePayload,
    pub signature_hex: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct EvidenceEnvelopeWire {
    version: u32,
    algorithm: SignatureAlgorithm,
    payload: EvidencePayload,
    signature_hex: String,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SignatureAlgorithm {
    Ed25519,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct EvidencePayload {
    pub version: u32,
    pub requirement_id: String,
    pub check_kind: String,
    pub subject: CheckSubject,
    pub installed_policy_sha256: String,
    pub installed_policy_epoch: u64,
    pub issuer_registry_sha256: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dependency_snapshot_sha256: Option<String>,
    pub attempt_id: String,
    pub run_generation: u64,
    pub request_sha256: String,
    pub limits_sha256: String,
    pub terminal_record_sha256: String,
    pub issuer_id: String,
    pub key_id: String,
    pub issued_at_ms: u64,
    pub expires_at_ms: u64,
    pub terminal_state: TerminalState,
    pub recovery_state: RecoveryState,
    pub outcome: EvidenceOutcome,
    pub execution_basis: ExecutionBasis,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub qualification_claim: Option<QualificationClaim>,
    pub coverage: CoverageClaim,
    pub dependencies: Vec<DependencyClaim>,
    pub artifacts: Vec<ArtifactRecord>,
    pub output: OutputCommitment,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TerminalState {
    Success,
    Failed,
    Incomplete,
    Cancelled,
    Indeterminate,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RecoveryState {
    NotRequired,
    Recovered,
    Cancelled,
    Uncertain,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum EvidenceOutcome {
    Pass,
    Fail,
    Incomplete,
    Unavailable,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ExecutionBasis {
    ControllerObserved,
    WorkerAsserted,
    Unqualified,
    Fixture,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct QualificationClaim {
    pub kind: String,
    pub profile: String,
    pub pin_sha256: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct CoverageClaim {
    pub scope: Vec<String>,
    pub expected_count: u32,
    pub observed_count: u32,
    pub inventory_complete: bool,
    pub omissions: Vec<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ArtifactRecord {
    pub artifact_id: String,
    pub media_type: String,
    pub sha256: String,
    pub bytes: u64,
    pub claimed_availability: ArtifactAvailability,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expires_at_ms: Option<u64>,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ArtifactAvailability {
    Full,
    Partial,
    Redacted,
    Unavailable,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct OutputCommitment {
    pub stdout_sha256: String,
    pub stdout_bytes: u64,
    pub stderr_sha256: String,
    pub stderr_bytes: u64,
    pub truncated: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EvaluationClock {
    Known { unix_ms: u64 },
    Uncertain,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IntegrityStatus {
    Verified,
    UnknownKey,
    InvalidPublicKey,
    NonCanonicalPublicKey,
    WeakPublicKey,
    InvalidSignature,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CurrentIssuerTrust {
    Trusted,
    UnknownIssuer,
    UnknownKey,
    FutureIssued,
    NotYetValid,
    Expired,
    Revoked,
    ClockUncertain,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RegistryBindingStatus {
    Current,
    Stale,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Freshness {
    Fresh,
    FutureIssued,
    Expired,
    ClockUncertain,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ExecutionClaimStatus {
    ControllerObservedAssertion,
    WorkerAssertion,
    Unqualified,
    Fixture,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ArtifactStatus {
    Full,
    Partial,
    Redacted,
    Unavailable,
    Expired,
    HashMismatch,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ArtifactAssessment {
    pub artifact_id: String,
    pub status: ArtifactStatus,
    pub commitment_matches: bool,
}

/// This type can only be obtained by successful strict signature verification.
/// It is deliberately neither serializable nor constructible by callers.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VerifiedReceipt {
    pub(crate) payload: EvidencePayload,
    pub(crate) envelope_sha256: String,
}

impl VerifiedReceipt {
    pub fn payload(&self) -> &EvidencePayload {
        &self.payload
    }

    pub fn envelope_sha256(&self) -> &str {
        &self.envelope_sha256
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VerificationReport {
    pub(crate) integrity: IntegrityStatus,
    pub(crate) registry_binding: RegistryBindingStatus,
    pub(crate) current_issuer_trust: CurrentIssuerTrust,
    pub(crate) freshness: Freshness,
    pub(crate) execution_claim: ExecutionClaimStatus,
    pub(crate) artifacts: Vec<ArtifactAssessment>,
    pub(crate) evaluated_clock: EvaluationClock,
    pub(crate) evaluated_registry_sha256: String,
    pub(crate) envelope_sha256: String,
    pub(crate) verified_receipt: Option<VerifiedReceipt>,
}

impl VerificationReport {
    pub fn integrity(&self) -> IntegrityStatus {
        self.integrity
    }

    pub fn registry_binding(&self) -> RegistryBindingStatus {
        self.registry_binding
    }

    pub fn current_issuer_trust(&self) -> CurrentIssuerTrust {
        self.current_issuer_trust
    }

    pub fn freshness(&self) -> Freshness {
        self.freshness
    }

    pub fn execution_claim(&self) -> ExecutionClaimStatus {
        self.execution_claim
    }

    pub fn artifacts(&self) -> &[ArtifactAssessment] {
        &self.artifacts
    }

    pub fn evaluated_clock(&self) -> EvaluationClock {
        self.evaluated_clock
    }

    pub fn evaluated_registry_sha256(&self) -> &str {
        &self.evaluated_registry_sha256
    }

    pub fn envelope_sha256(&self) -> &str {
        &self.envelope_sha256
    }

    pub fn verified_receipt(&self) -> Option<&VerifiedReceipt> {
        self.verified_receipt.as_ref()
    }
}

impl EvidenceEnvelope {
    pub fn from_slice(input: &[u8]) -> Result<Self> {
        if input.len() > MAX_EVIDENCE_BYTES {
            return Err(Error::Limit(format!(
                "evidence input exceeds {MAX_EVIDENCE_BYTES} bytes"
            )));
        }
        let wire: EvidenceEnvelopeWire = serde_json::from_slice(input)?;
        let envelope = Self {
            version: wire.version,
            algorithm: wire.algorithm,
            payload: wire.payload,
            signature_hex: wire.signature_hex,
        };
        envelope.validate()?;
        Ok(envelope)
    }

    pub fn validate(&self) -> Result<()> {
        if self.version != EVIDENCE_VERSION {
            return Err(Error::Invalid(format!(
                "unsupported evidence envelope version {}",
                self.version
            )));
        }
        self.payload.validate()?;
        validate_lower_hex("Ed25519 signature", &self.signature_hex, 64)
    }

    pub fn canonical_bytes(&self) -> Result<Vec<u8>> {
        self.validate()?;
        bounded_json(&self.canonicalized(), "canonical evidence envelope")
    }

    fn canonicalized(&self) -> Self {
        let mut value = self.clone();
        value.payload = value.payload.canonicalized();
        value
    }
}

impl EvidencePayload {
    pub fn validate(&self) -> Result<()> {
        if self.version != EVIDENCE_VERSION {
            return Err(Error::Invalid(format!(
                "unsupported evidence payload version {}",
                self.version
            )));
        }
        validate_identifier("requirement ID", &self.requirement_id)?;
        validate_check_kind(&self.check_kind)?;
        validate_subject(&self.subject)?;
        validate_lower_hex("installed policy digest", &self.installed_policy_sha256, 32)?;
        validate_integer("installed policy epoch", self.installed_policy_epoch, false)?;
        validate_lower_hex("issuer registry digest", &self.issuer_registry_sha256, 32)?;
        if let Some(digest) = &self.dependency_snapshot_sha256 {
            validate_lower_hex("dependency snapshot digest", digest, 32)?;
        }
        validate_identifier("attempt ID", &self.attempt_id)?;
        validate_integer("run generation", self.run_generation, true)?;
        validate_lower_hex("request digest", &self.request_sha256, 32)?;
        validate_lower_hex("limits digest", &self.limits_sha256, 32)?;
        validate_lower_hex("terminal record digest", &self.terminal_record_sha256, 32)?;
        validate_identifier("issuer ID", &self.issuer_id)?;
        validate_lower_hex("issuer key ID", &self.key_id, 32)?;
        validate_integer("issuance timestamp", self.issued_at_ms, false)?;
        validate_integer("expiry timestamp", self.expires_at_ms, true)?;
        if self.expires_at_ms <= self.issued_at_ms {
            return Err(Error::Invalid(
                "evidence expiry must be later than issuance".into(),
            ));
        }
        validate_terminal(self)?;
        if let Some(claim) = &self.qualification_claim {
            claim.validate()?;
        }
        self.coverage.validate()?;
        validate_count("dependencies", self.dependencies.len(), MAX_DEPENDENCIES)?;
        let mut dependency_ids = HashSet::with_capacity(self.dependencies.len());
        for dependency in &self.dependencies {
            validate_dependency(dependency)?;
            if !dependency_ids.insert(dependency.identity.as_str()) {
                return Err(Error::Invalid(format!(
                    "duplicate evidence dependency: {}",
                    dependency.identity
                )));
            }
        }
        validate_count("artifacts", self.artifacts.len(), MAX_ARTIFACTS)?;
        let mut artifact_ids = HashSet::with_capacity(self.artifacts.len());
        for artifact in &self.artifacts {
            artifact.validate()?;
            if artifact
                .expires_at_ms
                .is_some_and(|expires_at_ms| expires_at_ms <= self.issued_at_ms)
            {
                return Err(Error::Invalid(
                    "artifact expiry must be later than evidence issuance".into(),
                ));
            }
            if !artifact_ids.insert(artifact.artifact_id.as_str()) {
                return Err(Error::Invalid(format!(
                    "duplicate evidence artifact: {}",
                    artifact.artifact_id
                )));
            }
        }
        let Some(record_artifact) = self
            .artifacts
            .iter()
            .find(|artifact| artifact.artifact_id == "record")
        else {
            return Err(Error::Invalid(
                "portable evidence requires a terminal record artifact".into(),
            ));
        };
        if record_artifact.sha256 != self.terminal_record_sha256 {
            return Err(Error::Invalid(
                "terminal record artifact does not match its record digest".into(),
            ));
        }
        validate_output_artifact(
            &self.artifacts,
            "stdout",
            &self.output.stdout_sha256,
            self.output.stdout_bytes,
        )?;
        validate_output_artifact(
            &self.artifacts,
            "stderr",
            &self.output.stderr_sha256,
            self.output.stderr_bytes,
        )?;
        self.output.validate()?;
        bounded_json(&self.canonicalized(), "canonical evidence payload")?;
        Ok(())
    }

    pub fn canonical_bytes(&self) -> Result<Vec<u8>> {
        self.validate()?;
        bounded_json(&self.canonicalized(), "canonical evidence payload")
    }

    pub(crate) fn canonicalized(&self) -> Self {
        let mut value = self.clone();
        value.coverage.scope.sort();
        value.coverage.omissions.sort();
        value
            .dependencies
            .sort_by(|left, right| left.identity.cmp(&right.identity));
        value
            .artifacts
            .sort_by(|left, right| left.artifact_id.cmp(&right.artifact_id));
        value
    }
}

fn validate_output_artifact(
    artifacts: &[ArtifactRecord],
    artifact_id: &str,
    expected_sha256: &str,
    expected_bytes: u64,
) -> Result<()> {
    if let Some(artifact) = artifacts
        .iter()
        .find(|artifact| artifact.artifact_id == artifact_id)
        && (artifact.sha256 != expected_sha256 || artifact.bytes != expected_bytes)
    {
        return Err(Error::Invalid(format!(
            "{artifact_id} artifact does not match its output commitment"
        )));
    }
    Ok(())
}

fn validate_check_kind(value: &str) -> Result<()> {
    match value {
        "source_paths/v1" | "command/v1" | "model_review/v1" => Ok(()),
        _ => Err(Error::Invalid(format!(
            "unsupported portable evidence check kind: {value}"
        ))),
    }
}

impl QualificationClaim {
    fn validate(&self) -> Result<()> {
        validate_identifier("qualification kind", &self.kind)?;
        validate_identifier("qualification profile", &self.profile)?;
        validate_lower_hex("qualification pin", &self.pin_sha256, 32)
    }
}

impl CoverageClaim {
    fn validate(&self) -> Result<()> {
        validate_count("coverage scope", self.scope.len(), MAX_COVERAGE_SCOPE)?;
        validate_count(
            "coverage omissions",
            self.omissions.len(),
            MAX_COVERAGE_OMISSIONS,
        )?;
        validate_unique_text("coverage scope", &self.scope)?;
        validate_unique_text("coverage omissions", &self.omissions)?;
        for item in &self.scope {
            validate_text("coverage scope item", item)?;
        }
        for omission in &self.omissions {
            validate_text("coverage omission", omission)?;
        }
        if self.observed_count > self.expected_count {
            return Err(Error::Invalid(
                "coverage observed count exceeds expected count".into(),
            ));
        }
        if self.inventory_complete
            && (self.observed_count != self.expected_count || !self.omissions.is_empty())
        {
            return Err(Error::Invalid(
                "complete coverage requires equal counts and no omissions".into(),
            ));
        }
        Ok(())
    }
}

impl ArtifactRecord {
    fn validate(&self) -> Result<()> {
        validate_identifier("artifact ID", &self.artifact_id)?;
        validate_text("artifact media type", &self.media_type)?;
        validate_lower_hex("artifact digest", &self.sha256, 32)?;
        validate_integer("artifact byte count", self.bytes, false)?;
        if let Some(expires_at_ms) = self.expires_at_ms {
            validate_integer("artifact expiry timestamp", expires_at_ms, true)?;
        }
        Ok(())
    }
}

impl OutputCommitment {
    fn validate(&self) -> Result<()> {
        validate_lower_hex("stdout digest", &self.stdout_sha256, 32)?;
        validate_lower_hex("stderr digest", &self.stderr_sha256, 32)?;
        validate_integer("stdout byte count", self.stdout_bytes, false)?;
        validate_integer("stderr byte count", self.stderr_bytes, false)
    }
}

pub(crate) fn signature_message(payload: &EvidencePayload) -> Result<Vec<u8>> {
    let canonical = payload.canonical_bytes()?;
    let domain_len = u64::try_from(SIGNATURE_DOMAIN.len())
        .map_err(|_| Error::Limit("signature domain length overflow".into()))?;
    let payload_len = u64::try_from(canonical.len())
        .map_err(|_| Error::Limit("evidence payload length overflow".into()))?;
    let mut message = Vec::with_capacity(16 + SIGNATURE_DOMAIN.len() + canonical.len());
    message.extend_from_slice(&domain_len.to_be_bytes());
    message.extend_from_slice(SIGNATURE_DOMAIN);
    message.extend_from_slice(&payload_len.to_be_bytes());
    message.extend_from_slice(&canonical);
    Ok(message)
}

fn validate_subject(subject: &CheckSubject) -> Result<()> {
    validate_identifier("subject repository", &subject.repository)?;
    subject.change.validate()?;
    if subject.repository != subject.change.repository {
        return Err(Error::Invalid(
            "evidence subject repository identities do not match".into(),
        ));
    }
    validate_integer("subject change version", subject.change_version, true)?;
    validate_lower_hex("subject context digest", &subject.context_digest, 32)?;
    validate_lower_hex("subject revision", &subject.revision, 32)?;
    validate_lower_hex("subject recipe digest", &subject.recipe_sha256, 32)?;
    validate_lower_hex("subject toolchain digest", &subject.toolchain_sha256, 32)?;
    validate_lower_hex(
        "subject environment digest",
        &subject.environment_sha256,
        32,
    )
}

fn validate_terminal(payload: &EvidencePayload) -> Result<()> {
    if payload.recovery_state == RecoveryState::Uncertain
        && !matches!(
            payload.terminal_state,
            TerminalState::Incomplete | TerminalState::Indeterminate
        )
    {
        return Err(Error::Invalid(
            "uncertain recovery requires incomplete or indeterminate terminal state".into(),
        ));
    }
    if payload.outcome == EvidenceOutcome::Pass && payload.terminal_state != TerminalState::Success
    {
        return Err(Error::Invalid(
            "passing evidence requires successful terminal state".into(),
        ));
    }
    if payload.terminal_state == TerminalState::Success && payload.outcome != EvidenceOutcome::Pass
    {
        return Err(Error::Invalid(
            "successful terminal state requires passing outcome".into(),
        ));
    }
    Ok(())
}

fn validate_dependency(claim: &DependencyClaim) -> Result<()> {
    validate_identifier("dependency identity", &claim.identity)?;
    match &claim.state {
        DependencyState::Declared {} | DependencyState::Unknown {} => Ok(()),
        DependencyState::Resolved { revision } => {
            validate_lower_hex("dependency revision", revision, 32)
        }
        DependencyState::Observed {
            revision,
            evidence_digest,
        } => {
            if let Some(revision) = revision {
                validate_lower_hex("observed dependency revision", revision, 32)?;
            }
            validate_lower_hex("dependency evidence digest", evidence_digest, 32)
        }
        DependencyState::Unavailable { reason } => {
            validate_text("dependency unavailable reason", reason)
        }
    }
}

fn validate_unique_text(kind: &str, values: &[String]) -> Result<()> {
    let mut seen = HashSet::with_capacity(values.len());
    for value in values {
        if !seen.insert(value.as_str()) {
            return Err(Error::Invalid(format!("duplicate {kind}: {value}")));
        }
    }
    Ok(())
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

fn validate_text(kind: &str, value: &str) -> Result<()> {
    if value.is_empty() || value.len() > MAX_TEXT_BYTES || value.contains('\0') {
        return Err(Error::Limit(format!(
            "{kind} must be NUL-free and contain 1 to {MAX_TEXT_BYTES} bytes"
        )));
    }
    Ok(())
}

pub(crate) fn validate_lower_hex(kind: &str, value: &str, byte_len: usize) -> Result<()> {
    if value.len() != byte_len * 2
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err(Error::Invalid(format!(
            "{kind} must be exactly {byte_len} bytes of lowercase hexadecimal"
        )));
    }
    Ok(())
}

fn validate_integer(kind: &str, value: u64, positive: bool) -> Result<()> {
    if value > i64::MAX as u64 || (positive && value == 0) {
        return Err(Error::Limit(format!(
            "{kind} must fit a {}signed 64-bit integer",
            if positive {
                "positive "
            } else {
                "non-negative "
            }
        )));
    }
    Ok(())
}

fn validate_count(kind: &str, count: usize, maximum: usize) -> Result<()> {
    if count > maximum {
        return Err(Error::Limit(format!("{kind} exceeds {maximum} entries")));
    }
    Ok(())
}

fn bounded_json<T: Serialize>(value: &T, kind: &str) -> Result<Vec<u8>> {
    let bytes = serde_json::to_vec(value)?;
    if bytes.len() > MAX_EVIDENCE_BYTES {
        return Err(Error::Limit(format!(
            "{kind} exceeds {MAX_EVIDENCE_BYTES} bytes"
        )));
    }
    Ok(bytes)
}
