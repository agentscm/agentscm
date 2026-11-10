//! Bounded exact-byte audit carrier. Storage is not signature or issuer verification.
use super::{MAX_RECORD_BYTES, ValidationRecord};
use crate::evidence::EvidenceEnvelope;
use crate::{Error, Result};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use serde::{Deserialize, Serialize};

pub const MAX_BUNDLE_BYTES: usize = 2 * 1024 * 1024;
pub const MAX_BUNDLE_ENTRIES: usize = 32;
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct EvidencePairV1 {
    pub envelope_base64: String,
    pub record_base64: String,
}
impl EvidencePairV1 {
    pub fn new(envelope: &[u8], record: &[u8]) -> Self {
        Self {
            envelope_base64: STANDARD.encode(envelope),
            record_base64: STANDARD.encode(record),
        }
    }
    pub fn decode(&self) -> Result<(Vec<u8>, Vec<u8>)> {
        let invalid =
            || Error::Invalid("validation bundle pair encoding or commitment mismatch".into());
        if self.envelope_base64.len() > 4 * crate::evidence::MAX_EVIDENCE_BYTES.div_ceil(3)
            || self.record_base64.len() > 4 * MAX_RECORD_BYTES.div_ceil(3)
        {
            return Err(invalid());
        }
        let envelope = STANDARD
            .decode(&self.envelope_base64)
            .map_err(|_| invalid())?;
        let record = STANDARD
            .decode(&self.record_base64)
            .map_err(|_| invalid())?;
        let parsed = EvidenceEnvelope::from_slice(&envelope)?;
        let value = ValidationRecord::from_slice(&record)?;
        let digest = crate::store::digest(&record);
        if parsed.payload.subject.repository != value.subject.repository
            || parsed.payload.terminal_record_sha256 != digest
            || !parsed.payload.artifacts.iter().any(|a| {
                a.artifact_id == "record" && a.sha256 == digest && a.bytes == record.len() as u64
            })
        {
            return Err(invalid());
        }
        Ok((envelope, record))
    }
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ValidationBundleV1 {
    pub version: u32,
    pub entries: Vec<EvidencePairV1>,
}
impl ValidationBundleV1 {
    pub fn from_slice(bytes: &[u8]) -> Result<Self> {
        if bytes.len() > MAX_BUNDLE_BYTES {
            return Err(Error::Limit("validation bundle too large".into()));
        }
        let bundle: Self = serde_json::from_slice(bytes)?;
        bundle.to_bytes()?;
        Ok(bundle)
    }
    /// Canonical carrier order gives identical pairs the same identity without rewriting signed bytes.
    pub fn to_bytes(&self) -> Result<Vec<u8>> {
        if self.version != 1 || self.entries.is_empty() || self.entries.len() > MAX_BUNDLE_ENTRIES {
            return Err(Error::Invalid(
                "invalid validation bundle version or entry count".into(),
            ));
        }
        let mut entries = Vec::new();
        for pair in &self.entries {
            let (envelope, _) = pair.decode()?;
            entries.push((crate::store::digest(&envelope), pair.clone()));
        }
        entries.sort_by(|a, b| a.0.cmp(&b.0));
        if entries.windows(2).any(|w| w[0].0 == w[1].0) {
            return Err(Error::Invalid("duplicate bundle envelope".into()));
        }
        let bytes = serde_json::to_vec(&Self {
            version: 1,
            entries: entries.into_iter().map(|(_, p)| p).collect(),
        })?;
        if bytes.len() > MAX_BUNDLE_BYTES {
            return Err(Error::Limit("validation bundle too large".into()));
        }
        Ok(bytes)
    }
    pub fn validate_repository(&self, repository: &str) -> Result<()> {
        for pair in &self.entries {
            let (_, record) = pair.decode()?;
            if ValidationRecord::from_slice(&record)?.subject.repository != repository {
                return Err(Error::Invalid(
                    "validation bundle repository mismatch".into(),
                ));
            }
        }
        Ok(())
    }
}
