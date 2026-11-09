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
