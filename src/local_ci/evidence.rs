//! Identity mapping into portable evidence v1. Signing and trust verification remain separate.
use crate::checks::CheckSubject;
use crate::context::ChangeId;
use crate::evidence::{ArtifactAvailability, ArtifactRecord};
use crate::{Error, Result};

use super::{ImmutablePlan, SourceIdentity, domain_digest, raw_sha256};

pub const LOCAL_CI_CHECK_KIND: &str = "command/v1";
pub const LOCAL_CI_REQUIREMENT_ID: &str = "local-ci-plan/v1";

/// Exact canonical bytes and their ordinary SHA-256 artifact commitment. The plan's domain
/// digest is a different identity and must not be used as the artifact byte hash.
pub struct PlanArtifact {
    pub bytes: Vec<u8>,
    pub record: ArtifactRecord,
}

impl ImmutablePlan {
    /// Derive an existing portable evidence subject. The controller supplies the current change
    /// identity/version independently; this method does not create authority for that change.
    pub fn evidence_subject(&self, change: ChangeId, change_version: u64) -> Result<CheckSubject> {
        self.validate()?;
        change.validate()?;
        if change.repository != self.subject.head().repository()
            || change_version == 0
            || change_version > i64::MAX as u64
        {
            return Err(Error::Invalid(
                "evidence change does not match plan repository/version".into(),
            ));
        }
        let canonical = Self::from_slice(&self.canonical_bytes()?)?;
        let revision = match canonical.subject.execution_source() {
            SourceIdentity::Native { revision, .. } => revision.clone(),
            source @ SourceIdentity::Git { .. } => domain_digest(
                b"agentscm.local-ci.git-source.v1\0",
                &serde_json::to_vec(source)?,
            )?,
        };
        let runners: Vec<_> = canonical
            .cells
            .iter()
            .map(|c| (&c.job, &c.id, &c.runner))
            .collect();
        let environments: Vec<_> = canonical
            .cells
            .iter()
            .map(|c| (&c.job, &c.id, &c.command.environment, &c.services))
            .collect();
        Ok(CheckSubject {
            repository: change.repository.clone(),
            change,
            change_version,
            context_digest: canonical.digest()?,
            revision,
            recipe_sha256: canonical.requirements_sha256.clone(),
            toolchain_sha256: domain_digest(
                b"agentscm.local-ci.runners.v1\0",
                &serde_json::to_vec(&runners)?,
            )?,
            environment_sha256: domain_digest(
                b"agentscm.local-ci.environment.v1\0",
                &serde_json::to_vec(&(&canonical.event_inputs, environments))?,
            )?,
        })
    }

    pub fn evidence_artifact(&self) -> Result<PlanArtifact> {
        let bytes = self.canonical_bytes()?;
        let record = ArtifactRecord {
            artifact_id: "local-ci-plan".into(),
            media_type: "application/vnd.agentscm.local-ci-plan+json".into(),
            sha256: raw_sha256(&bytes),
            bytes: bytes.len() as u64,
            claimed_availability: ArtifactAvailability::Full,
            expires_at_ms: None,
        };
        Ok(PlanArtifact { bytes, record })
    }
}
