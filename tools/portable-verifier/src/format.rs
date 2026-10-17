use std::collections::HashSet;

use serde::{Deserialize, Deserializer, Serialize};
use sha2::{Digest, Sha256};

pub const MAX_JSON_BYTES: usize = 262_144;
pub const MAX_SUPPLIED_ARTIFACT_BYTES: usize = 67_108_864;
pub const SIGNED_U64_MAX: u64 = i64::MAX as u64;
pub const SIGNATURE_DOMAIN: &[u8] = b"agentscm.portable-evidence.v1";
pub const KEY_ID_DOMAIN: &[u8] = b"agentscm.ed25519-key/v1\0";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    RawSizeLimit,
    JsonInvalid,
    JsonUnknownField,
    JsonDuplicateField,
    JsonIntegerType,
    UnsupportedVersion,
    UnsupportedAlgorithm,
    UnsupportedCheckKind,
    InvalidIdentifier,
    InvalidText,
    InvalidDigest,
    InvalidSignatureEncoding,
    IntegerLimit,
    InvalidTimeWindow,
    InvalidTerminalCombination,
    InvalidSubject,
    CountLimit,
    DuplicateSemanticEntry,
    MissingRecordArtifact,
    ArtifactCommitmentMismatch,
    KeyCountLimit,
    DuplicateKeyAlias,
    KeyIdMismatch,
    EmptyKnownControl,
    RepositoryMismatch,
    ClockLimit,
    SuppliedArtifactSizeLimit,
    UnknownSuppliedArtifact,
    InvalidSuppliedArtifact,
    Io,
    InvalidCli,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct VerifyError {
    pub code: ErrorCode,
    pub message: &'static str,
}

impl VerifyError {
    pub const fn new(code: ErrorCode, message: &'static str) -> Self {
        Self { code, message }
    }
}

impl std::fmt::Display for VerifyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.message)
    }
}

impl std::error::Error for VerifyError {}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EvidenceEnvelope {
    pub version: u32,
    pub algorithm: String,
    pub payload: EvidencePayload,
    pub signature_hex: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EvidencePayload {
    pub version: u32,
    pub requirement_id: String,
    pub check_kind: CheckKind,
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum CheckKind {
    #[serde(rename = "source_paths/v1")]
    SourcePathsV1,
    #[serde(rename = "command/v1")]
    CommandV1,
    #[serde(rename = "model_review/v1")]
    ModelReviewV1,
}

impl<'de> Deserialize<'de> for CheckKind {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = String::deserialize(deserializer)?;
        match value.as_str() {
            "source_paths/v1" => Ok(Self::SourcePathsV1),
            "command/v1" => Ok(Self::CommandV1),
            "model_review/v1" => Ok(Self::ModelReviewV1),
            _ => Err(serde::de::Error::custom("unsupported check kind")),
        }
    }
}

impl CheckKind {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::SourcePathsV1 => "source_paths/v1",
            Self::CommandV1 => "command/v1",
            Self::ModelReviewV1 => "model_review/v1",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
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

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ChangeId {
    pub repository: String,
    pub id: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TerminalState {
    Success,
    Failed,
    Incomplete,
    Cancelled,
    Indeterminate,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RecoveryState {
    NotRequired,
    Recovered,
    Cancelled,
    Uncertain,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EvidenceOutcome {
    Pass,
    Fail,
    Incomplete,
    Unavailable,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExecutionBasis {
    ControllerObserved,
    WorkerAsserted,
    Unqualified,
    Fixture,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct QualificationClaim {
    pub kind: String,
    pub profile: String,
    pub pin_sha256: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CoverageClaim {
    pub scope: Vec<String>,
    pub expected_count: u32,
    pub observed_count: u32,
    pub inventory_complete: bool,
    pub omissions: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DependencyClaim {
    pub identity: String,
    pub state: DependencyState,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case", deny_unknown_fields)]
pub enum DependencyState {
    Declared {},
    Resolved {
        revision: String,
    },
    Observed {
        revision: Option<String>,
        evidence_digest: String,
    },
    Unknown {},
    Unavailable {
        reason: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ArtifactAvailability {
    Full,
    Partial,
    Redacted,
    Unavailable,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OutputCommitment {
    pub stdout_sha256: String,
    pub stdout_bytes: u64,
    pub stderr_sha256: String,
    pub stderr_bytes: u64,
    pub truncated: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IssuerRegistrySnapshot {
    pub version: u32,
    pub repository: String,
    pub epoch: u64,
    pub issuers: Vec<IssuerRecord>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IssuerRecord {
    pub issuer_id: String,
    pub trust_domain: String,
    pub common_control: CommonControl,
    pub allowed_check_kinds: Vec<String>,
    pub keys: Vec<IssuerKeyRecord>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case", deny_unknown_fields)]
pub enum CommonControl {
    Known { roots: Vec<String> },
    Unknown {},
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IssuerKeyRecord {
    pub key_id: String,
    pub public_key_hex: String,
    pub not_before_ms: u64,
    pub not_after_ms: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub revoked_at_ms: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedEnvelope {
    pub(crate) envelope: EvidenceEnvelope,
    pub(crate) canonical_payload: Vec<u8>,
    pub(crate) canonical_envelope: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedRegistry {
    pub(crate) registry: IssuerRegistrySnapshot,
    pub(crate) canonical_registry: Vec<u8>,
    pub(crate) canonical_sha256: String,
}

impl ParsedEnvelope {
    pub fn envelope(&self) -> &EvidenceEnvelope {
        &self.envelope
    }

    pub fn canonical_payload(&self) -> &[u8] {
        &self.canonical_payload
    }

    pub fn canonical_envelope(&self) -> &[u8] {
        &self.canonical_envelope
    }
}

impl ParsedRegistry {
    pub fn registry(&self) -> &IssuerRegistrySnapshot {
        &self.registry
    }

    pub fn canonical_registry(&self) -> &[u8] {
        &self.canonical_registry
    }

    pub fn canonical_sha256(&self) -> &str {
        &self.canonical_sha256
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Clock {
    Known { unix_ms: u64 },
    Uncertain {},
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SuppliedArtifact {
    pub artifact_id: String,
    pub content: SuppliedArtifactContent,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SuppliedArtifactContent {
    Full(Vec<u8>),
    Partial(Vec<u8>),
    Redacted,
    Unavailable,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum IntegrityStatus {
    Verified,
    UnknownKey,
    InvalidPublicKey,
    NonCanonicalPublicKey,
    WeakPublicKey,
    InvalidSignature,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RegistryBindingStatus {
    Current,
    Stale,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum IssuerTrustStatus {
    Trusted,
    UnknownIssuer,
    UnknownKey,
    FutureIssued,
    NotYetValid,
    Expired,
    Revoked,
    ClockUncertain,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum FreshnessStatus {
    Fresh,
    FutureIssued,
    Expired,
    ClockUncertain,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ExecutionClaim {
    ControllerObservedAssertion,
    WorkerAssertion,
    Unqualified,
    Fixture,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ArtifactStatus {
    Full,
    Partial,
    Redacted,
    Unavailable,
    Expired,
    HashMismatch,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ArtifactAssessment {
    pub artifact_id: String,
    pub status: ArtifactStatus,
    pub commitment_matches: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct VerifiedReceipt {
    pub(crate) canonical_payload: EvidencePayload,
    #[serde(skip)]
    pub(crate) canonical_payload_sha256: String,
    pub(crate) envelope_sha256: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct VerificationReport {
    pub(crate) integrity: IntegrityStatus,
    pub(crate) registry_binding: RegistryBindingStatus,
    pub(crate) current_issuer_trust: IssuerTrustStatus,
    pub(crate) freshness: FreshnessStatus,
    pub(crate) execution_claim: ExecutionClaim,
    pub(crate) artifacts: Vec<ArtifactAssessment>,
    pub(crate) evaluated_clock: Clock,
    pub(crate) evaluated_registry_sha256: String,
    pub(crate) envelope_sha256: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) verified_receipt: Option<VerifiedReceipt>,
}

impl VerifiedReceipt {
    pub fn canonical_payload(&self) -> &EvidencePayload {
        &self.canonical_payload
    }

    pub fn canonical_payload_sha256(&self) -> &str {
        &self.canonical_payload_sha256
    }

    pub fn envelope_sha256(&self) -> &str {
        &self.envelope_sha256
    }
}

impl VerificationReport {
    pub const fn integrity(&self) -> IntegrityStatus {
        self.integrity
    }

    pub const fn registry_binding(&self) -> RegistryBindingStatus {
        self.registry_binding
    }

    pub const fn current_issuer_trust(&self) -> IssuerTrustStatus {
        self.current_issuer_trust
    }

    pub const fn freshness(&self) -> FreshnessStatus {
        self.freshness
    }

    pub const fn execution_claim(&self) -> ExecutionClaim {
        self.execution_claim
    }

    pub fn artifacts(&self) -> &[ArtifactAssessment] {
        &self.artifacts
    }

    pub const fn evaluated_clock(&self) -> &Clock {
        &self.evaluated_clock
    }

    pub fn evaluated_registry_sha256(&self) -> &str {
        &self.evaluated_registry_sha256
    }

    pub fn envelope_sha256(&self) -> &str {
        &self.envelope_sha256
    }

    pub const fn verified_receipt(&self) -> Option<&VerifiedReceipt> {
        self.verified_receipt.as_ref()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct IndependenceReport {
    pub independent_count: usize,
    pub groups: Vec<Vec<String>>,
    pub unknown_issuer_ids: Vec<String>,
    pub unknown_control_issuer_ids: Vec<String>,
    pub disallowed_check_kind_issuer_ids: Vec<String>,
    pub duplicate_issuer_ids: Vec<String>,
}

pub fn parse_envelope(raw: &[u8]) -> Result<ParsedEnvelope, VerifyError> {
    check_raw_size(raw)?;
    let mut envelope: EvidenceEnvelope = decode_json(raw)?;
    validate_envelope(&envelope)?;
    normalize_payload(&mut envelope.payload);
    let canonical_payload = canonical_json(&envelope.payload)?;
    let canonical_envelope = canonical_json(&envelope)?;
    Ok(ParsedEnvelope {
        envelope,
        canonical_payload,
        canonical_envelope,
    })
}

pub fn parse_registry(raw: &[u8]) -> Result<ParsedRegistry, VerifyError> {
    check_raw_size(raw)?;
    let mut registry: IssuerRegistrySnapshot = decode_json(raw)?;
    validate_registry(&registry)?;
    normalize_registry(&mut registry);
    let canonical_registry = canonical_json(&registry)?;
    let canonical_sha256 = sha256_hex(&canonical_registry);
    Ok(ParsedRegistry {
        registry,
        canonical_registry,
        canonical_sha256,
    })
}

fn check_raw_size(raw: &[u8]) -> Result<(), VerifyError> {
    if raw.len() > MAX_JSON_BYTES {
        return Err(VerifyError::new(
            ErrorCode::RawSizeLimit,
            "JSON input exceeds the byte limit",
        ));
    }
    Ok(())
}

fn decode_json<T: for<'de> Deserialize<'de>>(raw: &[u8]) -> Result<T, VerifyError> {
    serde_json::from_slice(raw).map_err(|error| {
        let text = error.to_string();
        if text.contains("duplicate field") {
            VerifyError::new(
                ErrorCode::JsonDuplicateField,
                "JSON contains a duplicate object member",
            )
        } else if text.contains("unknown field") {
            VerifyError::new(
                ErrorCode::JsonUnknownField,
                "JSON contains an unknown object member",
            )
        } else if text.contains("unsupported check kind") {
            VerifyError::new(
                ErrorCode::UnsupportedCheckKind,
                "unsupported evidence check kind",
            )
        } else if text.contains("floating point")
            || text.contains("invalid type: floating")
            || text.contains("invalid type: integer") && text.contains("expected u32")
        {
            VerifyError::new(
                ErrorCode::JsonIntegerType,
                "an integer field does not use the required integer token",
            )
        } else {
            VerifyError::new(ErrorCode::JsonInvalid, "input is not valid profile JSON")
        }
    })
}

fn canonical_json<T: Serialize>(value: &T) -> Result<Vec<u8>, VerifyError> {
    let bytes = serde_json::to_vec(value).map_err(|_| {
        VerifyError::new(
            ErrorCode::JsonInvalid,
            "canonical JSON serialization failed",
        )
    })?;
    if bytes.len() > MAX_JSON_BYTES {
        return Err(VerifyError::new(
            ErrorCode::RawSizeLimit,
            "canonical JSON exceeds the byte limit",
        ));
    }
    Ok(bytes)
}

fn validate_envelope(envelope: &EvidenceEnvelope) -> Result<(), VerifyError> {
    if envelope.version != 1 || envelope.payload.version != 1 {
        return Err(VerifyError::new(
            ErrorCode::UnsupportedVersion,
            "unsupported evidence version",
        ));
    }
    if envelope.algorithm != "ed25519" {
        return Err(VerifyError::new(
            ErrorCode::UnsupportedAlgorithm,
            "unsupported signature algorithm",
        ));
    }
    validate_signature(&envelope.signature_hex)?;
    validate_payload(&envelope.payload)
}

fn validate_payload(payload: &EvidencePayload) -> Result<(), VerifyError> {
    validate_identifier(&payload.requirement_id)?;
    validate_identifier(&payload.subject.repository)?;
    validate_identifier(&payload.subject.change.repository)?;
    validate_identifier(&payload.subject.change.id)?;
    validate_identifier(&payload.attempt_id)?;
    validate_identifier(&payload.issuer_id)?;
    for digest in [
        &payload.subject.context_digest,
        &payload.subject.revision,
        &payload.subject.recipe_sha256,
        &payload.subject.toolchain_sha256,
        &payload.subject.environment_sha256,
        &payload.installed_policy_sha256,
        &payload.issuer_registry_sha256,
        &payload.request_sha256,
        &payload.limits_sha256,
        &payload.terminal_record_sha256,
        &payload.key_id,
        &payload.output.stdout_sha256,
        &payload.output.stderr_sha256,
    ] {
        validate_digest(digest)?;
    }
    if let Some(digest) = &payload.dependency_snapshot_sha256 {
        validate_digest(digest)?;
    }
    for value in [
        payload.installed_policy_epoch,
        payload.subject.change_version,
        payload.run_generation,
        payload.issued_at_ms,
        payload.expires_at_ms,
        payload.output.stdout_bytes,
        payload.output.stderr_bytes,
    ] {
        validate_u64(value)?;
    }
    if payload.subject.change_version == 0
        || payload.run_generation == 0
        || payload.expires_at_ms == 0
    {
        return Err(VerifyError::new(
            ErrorCode::IntegerLimit,
            "a positive integer field is zero",
        ));
    }
    if payload.expires_at_ms <= payload.issued_at_ms {
        return Err(VerifyError::new(
            ErrorCode::InvalidTimeWindow,
            "evidence expiry is not after issuance",
        ));
    }
    if payload.subject.repository != payload.subject.change.repository {
        return Err(VerifyError::new(
            ErrorCode::InvalidSubject,
            "subject repository values differ",
        ));
    }
    if matches!(payload.recovery_state, RecoveryState::Uncertain)
        && !matches!(
            payload.terminal_state,
            TerminalState::Incomplete | TerminalState::Indeterminate
        )
    {
        return Err(VerifyError::new(
            ErrorCode::InvalidTerminalCombination,
            "uncertain recovery has an invalid terminal state",
        ));
    }
    if matches!(payload.outcome, EvidenceOutcome::Pass)
        != matches!(payload.terminal_state, TerminalState::Success)
    {
        return Err(VerifyError::new(
            ErrorCode::InvalidTerminalCombination,
            "pass and success must occur together",
        ));
    }
    if let Some(claim) = &payload.qualification_claim {
        validate_identifier(&claim.kind)?;
        validate_identifier(&claim.profile)?;
        validate_digest(&claim.pin_sha256)?;
    }
    validate_coverage(&payload.coverage)?;
    validate_dependencies(&payload.dependencies)?;
    validate_artifacts(payload)?;
    Ok(())
}

fn validate_coverage(coverage: &CoverageClaim) -> Result<(), VerifyError> {
    if coverage.scope.len() > 128 || coverage.omissions.len() > 128 {
        return Err(VerifyError::new(
            ErrorCode::CountLimit,
            "coverage array exceeds its count limit",
        ));
    }
    validate_unique_texts(&coverage.scope)?;
    validate_unique_texts(&coverage.omissions)?;
    if coverage.observed_count > coverage.expected_count {
        return Err(VerifyError::new(
            ErrorCode::CountLimit,
            "observed coverage exceeds expected coverage",
        ));
    }
    if coverage.inventory_complete
        && (coverage.observed_count != coverage.expected_count || !coverage.omissions.is_empty())
    {
        return Err(VerifyError::new(
            ErrorCode::InvalidSubject,
            "complete inventory has incomplete coverage",
        ));
    }
    Ok(())
}

fn validate_dependencies(dependencies: &[DependencyClaim]) -> Result<(), VerifyError> {
    if dependencies.len() > 128 {
        return Err(VerifyError::new(
            ErrorCode::CountLimit,
            "dependency count exceeds its limit",
        ));
    }
    let mut identities = HashSet::new();
    for dependency in dependencies {
        validate_identifier(&dependency.identity)?;
        if !identities.insert(dependency.identity.as_str()) {
            return Err(VerifyError::new(
                ErrorCode::DuplicateSemanticEntry,
                "duplicate dependency identity",
            ));
        }
        match &dependency.state {
            DependencyState::Resolved { revision } => validate_digest(revision)?,
            DependencyState::Observed {
                revision,
                evidence_digest,
            } => {
                if let Some(revision) = revision {
                    validate_digest(revision)?;
                }
                validate_digest(evidence_digest)?;
            }
            DependencyState::Unavailable { reason } => validate_text(reason)?,
            DependencyState::Declared {} | DependencyState::Unknown {} => {}
        }
    }
    Ok(())
}

fn validate_artifacts(payload: &EvidencePayload) -> Result<(), VerifyError> {
    if payload.artifacts.len() > 64 {
        return Err(VerifyError::new(
            ErrorCode::CountLimit,
            "artifact count exceeds its limit",
        ));
    }
    let mut ids = HashSet::new();
    let mut record_found = false;
    for artifact in &payload.artifacts {
        validate_identifier(&artifact.artifact_id)?;
        validate_text(&artifact.media_type)?;
        validate_digest(&artifact.sha256)?;
        validate_u64(artifact.bytes)?;
        if !ids.insert(artifact.artifact_id.as_str()) {
            return Err(VerifyError::new(
                ErrorCode::DuplicateSemanticEntry,
                "duplicate artifact identity",
            ));
        }
        if let Some(expires_at_ms) = artifact.expires_at_ms {
            validate_u64(expires_at_ms)?;
            if expires_at_ms == 0 || expires_at_ms <= payload.issued_at_ms {
                return Err(VerifyError::new(
                    ErrorCode::InvalidTimeWindow,
                    "artifact expiry is not after evidence issuance",
                ));
            }
        }
        match artifact.artifact_id.as_str() {
            "record" => {
                record_found = true;
                if artifact.sha256 != payload.terminal_record_sha256 {
                    return Err(VerifyError::new(
                        ErrorCode::ArtifactCommitmentMismatch,
                        "record artifact does not match terminal record",
                    ));
                }
            }
            "stdout" => {
                if artifact.sha256 != payload.output.stdout_sha256
                    || artifact.bytes != payload.output.stdout_bytes
                {
                    return Err(VerifyError::new(
                        ErrorCode::ArtifactCommitmentMismatch,
                        "stdout artifact does not match output commitment",
                    ));
                }
            }
            "stderr"
                if artifact.sha256 != payload.output.stderr_sha256
                    || artifact.bytes != payload.output.stderr_bytes =>
            {
                return Err(VerifyError::new(
                    ErrorCode::ArtifactCommitmentMismatch,
                    "stderr artifact does not match output commitment",
                ));
            }
            _ => {}
        }
    }
    if !record_found {
        return Err(VerifyError::new(
            ErrorCode::MissingRecordArtifact,
            "record artifact is required",
        ));
    }
    Ok(())
}

fn validate_registry(registry: &IssuerRegistrySnapshot) -> Result<(), VerifyError> {
    if registry.version != 1 {
        return Err(VerifyError::new(
            ErrorCode::UnsupportedVersion,
            "unsupported registry version",
        ));
    }
    validate_identifier(&registry.repository)?;
    validate_u64(registry.epoch)?;
    if registry.issuers.len() > 128 {
        return Err(VerifyError::new(
            ErrorCode::CountLimit,
            "issuer count exceeds its limit",
        ));
    }
    let mut issuer_ids = HashSet::new();
    let mut key_ids = HashSet::new();
    let mut public_keys = HashSet::new();
    for issuer in &registry.issuers {
        validate_identifier(&issuer.issuer_id)?;
        validate_identifier(&issuer.trust_domain)?;
        if !issuer_ids.insert(issuer.issuer_id.as_str()) {
            return Err(VerifyError::new(
                ErrorCode::DuplicateSemanticEntry,
                "duplicate issuer identity",
            ));
        }
        match &issuer.common_control {
            CommonControl::Known { roots } => {
                if roots.is_empty() {
                    return Err(VerifyError::new(
                        ErrorCode::EmptyKnownControl,
                        "known control requires at least one root",
                    ));
                }
                if roots.len() > 16 {
                    return Err(VerifyError::new(
                        ErrorCode::CountLimit,
                        "common-control root count exceeds its limit",
                    ));
                }
                let mut seen = HashSet::new();
                for root in roots {
                    validate_identifier(root)?;
                    if !seen.insert(root.as_str()) {
                        return Err(VerifyError::new(
                            ErrorCode::DuplicateSemanticEntry,
                            "duplicate common-control root",
                        ));
                    }
                }
            }
            CommonControl::Unknown {} => {}
        }
        if issuer.allowed_check_kinds.len() > 64 {
            return Err(VerifyError::new(
                ErrorCode::CountLimit,
                "allowed check-kind count exceeds its limit",
            ));
        }
        let mut allowed = HashSet::new();
        for kind in &issuer.allowed_check_kinds {
            validate_identifier(kind)?;
            if !allowed.insert(kind.as_str()) {
                return Err(VerifyError::new(
                    ErrorCode::DuplicateSemanticEntry,
                    "duplicate allowed check kind",
                ));
            }
        }
        if issuer.keys.len() > 8 {
            return Err(VerifyError::new(
                ErrorCode::KeyCountLimit,
                "issuer key count exceeds its limit",
            ));
        }
        for key in &issuer.keys {
            validate_digest(&key.key_id)?;
            let public_key = decode_exact_lower_hex::<32>(&key.public_key_hex).map_err(|_| {
                VerifyError::new(ErrorCode::InvalidDigest, "invalid public-key encoding")
            })?;
            validate_u64(key.not_before_ms)?;
            validate_u64(key.not_after_ms)?;
            if key.not_after_ms == 0 || key.not_after_ms <= key.not_before_ms {
                return Err(VerifyError::new(
                    ErrorCode::InvalidTimeWindow,
                    "invalid key validity window",
                ));
            }
            if let Some(revoked_at_ms) = key.revoked_at_ms {
                validate_u64(revoked_at_ms)?;
            }
            if !key_ids.insert(key.key_id.as_str()) || !public_keys.insert(public_key) {
                return Err(VerifyError::new(
                    ErrorCode::DuplicateKeyAlias,
                    "registry reuses a key identity or public key",
                ));
            }
            let mut hash = Sha256::new();
            hash.update(KEY_ID_DOMAIN);
            hash.update(public_key);
            if lower_hex(&hash.finalize()) != key.key_id {
                return Err(VerifyError::new(
                    ErrorCode::KeyIdMismatch,
                    "key ID does not bind the public key",
                ));
            }
        }
    }
    Ok(())
}

fn normalize_payload(payload: &mut EvidencePayload) {
    payload.coverage.scope.sort();
    payload.coverage.omissions.sort();
    payload
        .dependencies
        .sort_by(|left, right| left.identity.cmp(&right.identity));
    payload
        .artifacts
        .sort_by(|left, right| left.artifact_id.cmp(&right.artifact_id));
}

fn normalize_registry(registry: &mut IssuerRegistrySnapshot) {
    for issuer in &mut registry.issuers {
        if let CommonControl::Known { roots } = &mut issuer.common_control {
            roots.sort();
        }
        issuer.allowed_check_kinds.sort();
        issuer
            .keys
            .sort_by(|left, right| left.key_id.cmp(&right.key_id));
    }
    registry
        .issuers
        .sort_by(|left, right| left.issuer_id.cmp(&right.issuer_id));
}

pub fn validate_clock(clock: &Clock) -> Result<(), VerifyError> {
    if let Clock::Known { unix_ms } = clock
        && *unix_ms > SIGNED_U64_MAX
    {
        return Err(VerifyError::new(
            ErrorCode::ClockLimit,
            "known clock exceeds signed-64 range",
        ));
    }
    Ok(())
}

pub fn validate_identifier(value: &str) -> Result<(), VerifyError> {
    if value.is_empty()
        || value.len() > 128
        || !value.bytes().all(|byte| {
            byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b':' | b'/')
        })
    {
        return Err(VerifyError::new(
            ErrorCode::InvalidIdentifier,
            "invalid identifier",
        ));
    }
    Ok(())
}

fn validate_text(value: &str) -> Result<(), VerifyError> {
    if value.is_empty() || value.len() > 2048 || value.contains('\0') {
        return Err(VerifyError::new(
            ErrorCode::InvalidText,
            "invalid text field",
        ));
    }
    Ok(())
}

fn validate_unique_texts(values: &[String]) -> Result<(), VerifyError> {
    let mut seen = HashSet::new();
    for value in values {
        validate_text(value)?;
        if !seen.insert(value.as_str()) {
            return Err(VerifyError::new(
                ErrorCode::DuplicateSemanticEntry,
                "duplicate set-like text entry",
            ));
        }
    }
    Ok(())
}

fn validate_digest(value: &str) -> Result<(), VerifyError> {
    decode_exact_lower_hex::<32>(value)
        .map(|_| ())
        .map_err(|_| VerifyError::new(ErrorCode::InvalidDigest, "invalid lowercase SHA-256 digest"))
}

fn validate_signature(value: &str) -> Result<(), VerifyError> {
    decode_exact_lower_hex::<64>(value)
        .map(|_| ())
        .map_err(|_| {
            VerifyError::new(
                ErrorCode::InvalidSignatureEncoding,
                "invalid lowercase Ed25519 signature",
            )
        })
}

fn validate_u64(value: u64) -> Result<(), VerifyError> {
    if value > SIGNED_U64_MAX {
        return Err(VerifyError::new(
            ErrorCode::IntegerLimit,
            "integer exceeds signed-64 range",
        ));
    }
    Ok(())
}

pub fn decode_exact_lower_hex<const N: usize>(value: &str) -> Result<[u8; N], VerifyError> {
    if value.len() != N * 2
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err(VerifyError::new(
            ErrorCode::JsonInvalid,
            "invalid lowercase hexadecimal encoding",
        ));
    }
    let mut output = [0_u8; N];
    let bytes = value.as_bytes();
    for index in 0..N {
        output[index] = (hex_nibble(bytes[index * 2]) << 4) | hex_nibble(bytes[index * 2 + 1]);
    }
    Ok(output)
}

fn hex_nibble(byte: u8) -> u8 {
    match byte {
        b'0'..=b'9' => byte - b'0',
        b'a'..=b'f' => byte - b'a' + 10,
        _ => unreachable!("hex was validated before decoding"),
    }
}

pub fn lower_hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut result = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        result.push(HEX[(byte >> 4) as usize] as char);
        result.push(HEX[(byte & 0x0f) as usize] as char);
    }
    result
}

pub fn sha256_hex(bytes: &[u8]) -> String {
    lower_hex(&Sha256::digest(bytes))
}

pub fn signature_message(canonical_payload: &[u8]) -> Vec<u8> {
    let mut message = Vec::with_capacity(16 + SIGNATURE_DOMAIN.len() + canonical_payload.len());
    message.extend_from_slice(&(SIGNATURE_DOMAIN.len() as u64).to_be_bytes());
    message.extend_from_slice(SIGNATURE_DOMAIN);
    message.extend_from_slice(&(canonical_payload.len() as u64).to_be_bytes());
    message.extend_from_slice(canonical_payload);
    message
}
