use std::collections::HashSet;
use std::io::Write;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::checks::CheckSubject;
use crate::context::{InputIdentity, ReviewContext};
use crate::{Error, Result};

pub const INSTALLED_POLICY_VERSION: u32 = 1;
pub const MAX_POLICY_BYTES: usize = 256 * 1024;
pub const MAX_POLICY_CHECKS: usize = 64;
pub const MAX_POLICY_DEPENDENCIES: usize = 128;
pub const MAX_POLICY_ISSUERS: usize = 128;
pub const MAX_POLICY_SCOPE: usize = 128;
pub const MAX_POLICY_ARTIFACTS: usize = 64;

/// An operator-installed exact-pin policy. Installation and authenticity belong
/// to the durable authority, not this ordinary serializable document.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct InstalledPolicy {
    pub version: u32,
    pub name: String,
    pub repository: String,
    pub target: String,
    pub epoch: u64,
    pub required_checks: Vec<CheckRequirement>,
    pub required_dependencies: Vec<DependencyRequirement>,
    pub require_complete_dependency_inventory: bool,
    pub minimum_retention_ms: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct CheckRequirement {
    pub requirement_id: String,
    pub check_kind: String,
    pub recipe_sha256: String,
    pub toolchain_sha256: String,
    pub environment_sha256: String,
    pub execution: RequiredExecution,
    pub trusted_issuer_ids: Vec<String>,
    pub minimum_independent_issuers: u32,
    pub required_scope: Vec<String>,
    pub require_complete_coverage: bool,
    pub required_artifacts: Vec<String>,
    pub maximum_age_ms: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum RequiredExecution {
    SourceIntegrity,
    QualifiedCommand {
        profile: String,
        qualification_sha256: String,
    },
    QualifiedModel {
        profile: String,
        qualification_sha256: String,
    },
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum DependencyBasis {
    Declared,
    Resolved,
    Observed,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct DependencyRequirement {
    pub identity: String,
    pub revision_sha256: String,
    pub minimum_basis: DependencyBasis,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BoundRequirement {
    pub requirement: CheckRequirement,
    pub subject: CheckSubject,
}

impl InstalledPolicy {
    pub fn from_slice(bytes: &[u8]) -> Result<Self> {
        if bytes.len() > MAX_POLICY_BYTES {
            return Err(Error::Limit(
                "installed policy input exceeds its byte limit".into(),
            ));
        }
        let policy: Self = serde_json::from_slice(bytes)?;
        policy.validate()?;
        Ok(policy)
    }

    pub fn validate(&self) -> Result<()> {
        if self.version != INSTALLED_POLICY_VERSION {
            return Err(Error::Invalid(
                "unsupported installed policy version".into(),
            ));
        }
        identifier(&self.name)?;
        identifier(&self.repository)?;
        identifier(&self.target)?;
        integer(self.epoch)?;
        duration(self.minimum_retention_ms)?;
        if self.required_checks.is_empty()
            || self.required_checks.len() > MAX_POLICY_CHECKS
            || self.required_dependencies.len() > MAX_POLICY_DEPENDENCIES
        {
            return Err(Error::Limit(
                "installed policy requirement count exceeds its profile".into(),
            ));
        }
        let mut checks = HashSet::new();
        for requirement in &self.required_checks {
            requirement.validate()?;
            if !checks.insert(&requirement.requirement_id) {
                return Err(Error::Invalid(
                    "duplicate installed check requirement".into(),
                ));
            }
        }
        let mut dependencies = HashSet::new();
        for dependency in &self.required_dependencies {
            identifier(&dependency.identity)?;
            digest(&dependency.revision_sha256)?;
            if !dependencies.insert(&dependency.identity) {
                return Err(Error::Invalid(
                    "duplicate installed dependency requirement".into(),
                ));
            }
        }
        Ok(())
    }

    pub fn canonical_bytes(&self) -> Result<Vec<u8>> {
        self.validate()?;
        let mut canonical = self.clone();
        canonical
            .required_checks
            .sort_by(|a, b| a.requirement_id.cmp(&b.requirement_id));
        canonical
            .required_dependencies
            .sort_by(|a, b| a.identity.cmp(&b.identity));
        for check in &mut canonical.required_checks {
            check.trusted_issuer_ids.sort();
            check.required_scope.sort();
            check.required_artifacts.sort();
        }
        bounded_json(&canonical, MAX_POLICY_BYTES)
    }

    pub fn digest(&self) -> Result<String> {
        domain_hash(b"agentscm.installed-policy.v1\0", &self.canonical_bytes()?)
    }

    /// Derives all installed minima even if the candidate declares no checks.
    /// Exact pins are deliberately explicit in this first policy profile; a
    /// future trusted recipe-template factory must not accept candidate waivers.
    pub fn derive_requirements(&self, context: &ReviewContext) -> Result<Vec<BoundRequirement>> {
        self.validate()?;
        context.validate()?;
        if context.change.repository != self.repository
            || context.target != self.target
            || context.policy_epoch != self.epoch
        {
            return Err(Error::Conflict(
                "context does not bind the installed policy scope/epoch".into(),
            ));
        }
        let context_digest = context.digest()?;
        let mut result = Vec::with_capacity(self.required_checks.len());
        for requirement in &self.required_checks {
            result.push(BoundRequirement {
                requirement: requirement.clone(),
                subject: CheckSubject {
                    repository: self.repository.clone(),
                    change: context.change.clone(),
                    change_version: context.change_version,
                    context_digest: context_digest.clone(),
                    revision: context.revision.clone(),
                    recipe_sha256: requirement.recipe_sha256.clone(),
                    toolchain_sha256: requirement.toolchain_sha256.clone(),
                    environment_sha256: requirement.environment_sha256.clone(),
                },
            });
        }
        result.sort_by(|a, b| {
            a.requirement
                .requirement_id
                .cmp(&b.requirement.requirement_id)
        });
        Ok(result)
    }
}

impl CheckRequirement {
    pub fn validate(&self) -> Result<()> {
        identifier(&self.requirement_id)?;
        identifier(&self.check_kind)?;
        digest(&self.recipe_sha256)?;
        digest(&self.toolchain_sha256)?;
        digest(&self.environment_sha256)?;
        duration(self.maximum_age_ms)?;
        let expected_kind = match &self.execution {
            RequiredExecution::SourceIntegrity => "source_paths/v1",
            RequiredExecution::QualifiedCommand {
                profile,
                qualification_sha256,
            } => {
                identifier(profile)?;
                digest(qualification_sha256)?;
                "command/v1"
            }
            RequiredExecution::QualifiedModel {
                profile,
                qualification_sha256,
            } => {
                identifier(profile)?;
                digest(qualification_sha256)?;
                "model_review/v1"
            }
        };
        if self.check_kind != expected_kind {
            return Err(Error::Invalid(
                "check kind does not match required execution semantics".into(),
            ));
        }
        unique_identifiers(&self.trusted_issuer_ids, 1, MAX_POLICY_ISSUERS)?;
        unique_identifiers(&self.required_artifacts, 1, MAX_POLICY_ARTIFACTS)?;
        if !self.required_artifacts.iter().any(|id| id == "record") {
            return Err(Error::Invalid(
                "policy must require the complete record artifact".into(),
            ));
        }
        if self.minimum_independent_issuers == 0
            || self.minimum_independent_issuers as usize > self.trusted_issuer_ids.len()
            || self.minimum_independent_issuers > 128
            || self.required_scope.is_empty()
            || self.required_scope.len() > MAX_POLICY_SCOPE
        {
            return Err(Error::Limit(
                "installed check independence/scope exceeds its profile".into(),
            ));
        }
        let mut scope = HashSet::new();
        for entry in &self.required_scope {
            if entry.is_empty()
                || entry.len() > 1024
                || entry.contains('\0')
                || !scope.insert(entry)
            {
                return Err(Error::Invalid(
                    "installed check scope is empty, duplicate or oversized".into(),
                ));
            }
        }
        Ok(())
    }
}

pub(crate) fn identifier(value: &str) -> Result<()> {
    if value.is_empty()
        || value.len() > 128
        || !value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'/' | b':'))
    {
        return Err(Error::Invalid("invalid policy identifier".into()));
    }
    Ok(())
}

pub(crate) fn digest(value: &str) -> Result<()> {
    if value.len() != 64
        || !value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    {
        return Err(Error::Invalid("invalid policy SHA-256 digest".into()));
    }
    Ok(())
}

pub(crate) fn integer(value: u64) -> Result<()> {
    if value > i64::MAX as u64 {
        return Err(Error::Limit("policy integer exceeds signed storage".into()));
    }
    Ok(())
}

pub(crate) fn duration(value: u64) -> Result<()> {
    if value == 0 || value > 366 * 24 * 60 * 60 * 1000 {
        return Err(Error::Limit(
            "policy duration must be positive and at most 366 days".into(),
        ));
    }
    Ok(())
}

pub(crate) fn unique_identifiers(values: &[String], minimum: usize, maximum: usize) -> Result<()> {
    if values.len() < minimum || values.len() > maximum {
        return Err(Error::Limit("policy list count exceeds its profile".into()));
    }
    let mut seen = HashSet::new();
    for value in values {
        identifier(value)?;
        if !seen.insert(value) {
            return Err(Error::Invalid("duplicate policy identifier".into()));
        }
    }
    Ok(())
}

pub(crate) fn bounded_json<T: Serialize>(value: &T, maximum: usize) -> Result<Vec<u8>> {
    struct Writer {
        bytes: Vec<u8>,
        maximum: usize,
    }
    impl Write for Writer {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            if bytes.len() > self.maximum.saturating_sub(self.bytes.len()) {
                return Err(std::io::Error::other("policy canonical byte limit"));
            }
            self.bytes.extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let mut writer = Writer {
        bytes: Vec::new(),
        maximum,
    };
    serde_json::to_writer(&mut writer, value)
        .map_err(|_| Error::Limit("policy canonical byte limit".into()))?;
    Ok(writer.bytes)
}

pub(crate) fn domain_hash(domain: &[u8], bytes: &[u8]) -> Result<String> {
    let mut hasher = Sha256::new();
    hasher.update(domain);
    hasher.update(bytes);
    Ok(format!("{:x}", hasher.finalize()))
}

pub(crate) fn known_identity(input: &InputIdentity) -> Option<&str> {
    match input {
        InputIdentity::Digest { sha256 } => Some(sha256),
        _ => None,
    }
}
