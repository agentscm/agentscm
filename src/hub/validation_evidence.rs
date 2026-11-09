//! Durable, received-unverified validation history. No conversation expiry applies.
//! Objects persist until explicit repository administration/removal. Missing or corrupt
//! objects are unavailable evidence and must never be treated as successful validation.
use super::{HubError, Result};
use crate::backend::{ImmutableBackend, ObjectKey, SOURCE_PROFILE_V1};
use crate::local_validation::bundle::ValidationBundleV1;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ValidationSummaryConfig {
    pub repository: String,
    pub connection: String,
    pub enabled: bool,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ValidationEvidenceReceipt {
    pub repository: String,
    pub proposal: i64,
    pub generation: i64,
    pub bundle_sha256: String,
    pub received_at: i64,
    pub received_by: String,
    /// Always received_unverified; storing bytes confers no issuer authority.
    pub state: String,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ValidationEvidencePage {
    pub entries: Vec<ValidationEvidenceReceipt>,
    pub next_after: Option<i64>,
}
/// Only constructed after the immutable backend confirms the exact durable object.
#[derive(Clone, Debug)]
pub struct DurableValidationBundle {
    pub(crate) repository: String,
    pub(crate) digest: String,
    pub(crate) canonical_repository: String,
}
fn key(repository: &str, digest: &str) -> crate::Result<ObjectKey> {
    ObjectKey::new("hub-validation", repository, SOURCE_PROFILE_V1, digest)
}
pub fn persist_bundle(
    backend: &dyn ImmutableBackend,
    repository: &str,
    canonical_repository: &str,
    bundle: &ValidationBundleV1,
) -> Result<DurableValidationBundle> {
    bundle.validate_repository(canonical_repository)?;
    let bytes = bundle.to_bytes()?;
    let digest = crate::store::digest(&bytes);
    let key = key(repository, &digest)?;
    backend.prepare_publication(&key, "v1")?;
    backend.put_if_absent(&key, "v1", &bytes)?;
    if backend.get_verified(&key, "v1")? != bytes {
        return Err(HubError::Unavailable("validation object differs".into()));
    }
    Ok(DurableValidationBundle {
        repository: repository.into(),
        digest,
        canonical_repository: canonical_repository.into(),
    })
}
pub fn read_bundle(
    backend: &dyn ImmutableBackend,
    receipt: &ValidationEvidenceReceipt,
) -> Result<ValidationBundleV1> {
    let bytes = backend
        .get_verified(&key(&receipt.repository, &receipt.bundle_sha256)?, "v1")
        .map_err(|_| HubError::Unavailable("validation evidence object unavailable".into()))?;
    if crate::store::digest(&bytes) != receipt.bundle_sha256 {
        return Err(HubError::Unavailable(
            "validation evidence object corrupt".into(),
        ));
    }
    ValidationBundleV1::from_slice(&bytes)
        .map_err(|_| HubError::Unavailable("validation evidence object malformed".into()))
}
pub(crate) fn page(
    mut entries: Vec<ValidationEvidenceReceipt>,
    limit: usize,
) -> ValidationEvidencePage {
    let more = entries.len() > limit;
    entries.truncate(limit);
    ValidationEvidencePage {
        next_after: more.then(|| entries.last().unwrap().generation),
        entries,
    }
}
pub(crate) fn validate_page(proposal: i64, after: i64, limit: usize) -> Result<()> {
    if proposal <= 0 || after < 0 || !(1..=100).contains(&limit) {
        return Err(HubError::Invalid(
            "invalid validation history bounds".into(),
        ));
    }
    Ok(())
}

/// Bounded controller-only snapshot. Receipts remain unverified; no partial inventory
/// can qualify a requirement. This API is deliberately absent from HTTP/MCP routes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ValidationSnapshot {
    pub config: Option<ValidationSummaryConfig>,
    pub generation: i64,
    pub entries: Vec<ValidationEvidenceReceipt>,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PublicationPhase {
    Idle,
    Preparing,
    Attempting,
}

/// A prior context is retained even if subsequent trusted configuration cannot load.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ValidationStatusSlot {
    pub head: String,
    pub context: String,
}

/// Controller publication journal, never signed evidence. An Attempting operation has
/// exactly one remote send. An uncertain reply leaves it here until exact operation-marker
/// readback proves application. Absence, timeouts and lease expiry cannot authorize a resend.
/// Preparing forbids a summary write until a successful CAS to Attempting. Every worker must
/// use that CAS immediately before its one send; this also fences an abandoned preparation.
/// This journal records comment publication only, never proof of status-write completion.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ValidationPublication {
    pub revision: i64,
    pub evidence_generation: i64,
    pub connection: String,
    pub phase: PublicationPhase,
    pub operation_id: String,
    pub body_sha256: String,
    pub source_sha256: String,
    pub comment_id: Option<i64>,
    pub status_slots: Vec<ValidationStatusSlot>,
}

pub(crate) fn validate_snapshot_limit(proposal: i64, limit: usize) -> Result<()> {
    if proposal <= 0 || !(1..=1000).contains(&limit) {
        return Err(HubError::Invalid(
            "invalid validation snapshot bounds".into(),
        ));
    }
    Ok(())
}

/// Validates the journal state machine independently of backend. Reserving and beginning
/// require the current generation/config; recovery and abort may complete an older operation.
/// Returns true when the transaction must fence against current config/evidence.
pub(crate) fn publication_transition(
    previous: Option<&ValidationPublication>,
    expected_revision: i64,
    next: &ValidationPublication,
) -> Result<bool> {
    use PublicationPhase::*;
    let invalid = || HubError::Invalid("invalid validation publication transition".into());
    let hex = |s: &str| {
        s.len() == 64
            && s.bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    };
    if expected_revision < 0
        || expected_revision.checked_add(1) != Some(next.revision)
        || next.evidence_generation < 0
        || next.connection.is_empty()
        || next.connection.len() > 128
        || !hex(&next.operation_id)
        || !hex(&next.body_sha256)
        || !hex(&next.source_sha256)
        || next.comment_id.is_some_and(|id| id <= 0)
        || next.status_slots.len() > 512
        || next.status_slots.iter().any(|s| {
            !matches!(s.head.len(), 40 | 64)
                || !s
                    .head
                    .bytes()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
                || s.context.is_empty()
                || s.context.len() > 100
                || s.context.chars().any(char::is_control)
        })
    {
        return Err(invalid());
    }
    let Some(old) = previous else {
        if expected_revision != 0 || next.phase != Preparing {
            return Err(invalid());
        }
        return Ok(true);
    };
    if old.revision != expected_revision {
        return Err(HubError::Invalid(
            "validation publication revision changed".into(),
        ));
    }
    // Previously written contexts must remain discoverable for invalidation, even after disable.
    if old
        .status_slots
        .iter()
        .any(|slot| !next.status_slots.contains(slot))
    {
        return Err(invalid());
    }
    if old.phase == Idle && next.phase == Preparing {
        if next.operation_id == old.operation_id || next.comment_id != old.comment_id {
            return Err(invalid());
        }
        return Ok(true);
    }
    if !matches!(
        (old.phase, next.phase),
        (Preparing, Attempting | Idle) | (Attempting, Idle)
    ) || next.operation_id != old.operation_id
        || next.connection != old.connection
        || next.evidence_generation != old.evidence_generation
        || next.body_sha256 != old.body_sha256
        || next.source_sha256 != old.source_sha256
        || (old.phase == Preparing && next.comment_id != old.comment_id)
    {
        return Err(invalid());
    }
    Ok(next.phase == Attempting)
}

/// Receipts are append-only and generations are contiguous; a missing row is an
/// unavailable inventory, not evidence that a formerly submitted result never existed.
pub(crate) fn validate_snapshot_inventory(
    generation: i64,
    entries: &[ValidationEvidenceReceipt],
) -> Result<()> {
    if generation < 0
        || usize::try_from(generation).ok() != Some(entries.len())
        || entries
            .iter()
            .enumerate()
            .any(|(index, receipt)| receipt.generation != index as i64 + 1)
    {
        return Err(HubError::Unavailable(
            "validation receipt inventory incomplete".into(),
        ));
    }
    Ok(())
}
