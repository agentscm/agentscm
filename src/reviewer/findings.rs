use std::collections::{HashMap, HashSet};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::checks::CheckSubject;
use crate::{Error, Result};

const MAX_TITLE_BYTES: usize = 512;
const MAX_DETAIL_BYTES: usize = 16 * 1024;
const MAX_CITATIONS: usize = 16;
const MAX_DELIVERED_RANGES: usize = 4_096;
const MAX_IDENTIFIER_BYTES: usize = 128;

const SUBJECT_DOMAIN: &[u8] = b"agentscm.finding-subject.v1\0";
const CLAIM_DOMAIN: &[u8] = b"agentscm.finding-claim.v1\0";
const FAMILY_DOMAIN: &[u8] = b"agentscm.finding-family.v1\0";
const OCCURRENCE_DOMAIN: &[u8] = b"agentscm.finding-occurrence.v1\0";

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum FindingSeverity {
    Note,
    Warning,
    Error,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(deny_unknown_fields)]
pub struct SourceCitation {
    pub row_id: String,
    pub start_line: u32,
    pub end_line: u32,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct FindingClaim {
    pub title: String,
    pub detail: String,
    pub severity: FindingSeverity,
    pub citations: Vec<SourceCitation>,
}

impl FindingClaim {
    /// Validates this claim against the exact source ranges confirmed as delivered.
    ///
    /// A successful result establishes only structural citation containment. It does
    /// not establish model attention, semantic support, or evidence authenticity.
    pub fn validate(&self, delivered_ranges: &[(String, u32, u32)]) -> Result<()> {
        self.validate_structure()?;
        if delivered_ranges.len() > MAX_DELIVERED_RANGES {
            return Err(Error::Limit(format!(
                "delivered row catalog exceeds {MAX_DELIVERED_RANGES} ranges"
            )));
        }

        let mut delivered = HashMap::with_capacity(delivered_ranges.len());
        for (row_id, start_line, end_line) in delivered_ranges {
            validate_digest("delivered row ID", row_id)?;
            if *start_line == 0 || start_line > end_line {
                return Err(Error::Invalid(format!(
                    "delivered range for row {row_id} starts after it ends"
                )));
            }
            if delivered
                .insert(row_id.as_str(), (*start_line, *end_line))
                .is_some()
            {
                return Err(Error::Invalid(format!(
                    "duplicate delivered row ID: {row_id}"
                )));
            }
        }

        for citation in &self.citations {
            let Some((delivered_start, delivered_end)) = delivered.get(citation.row_id.as_str())
            else {
                return Err(Error::Invalid(format!(
                    "citation row {} was not delivered",
                    citation.row_id
                )));
            };
            if citation.start_line < *delivered_start || citation.end_line > *delivered_end {
                return Err(Error::Invalid(format!(
                    "citation range for row {} is outside the delivered range",
                    citation.row_id
                )));
            }
        }
        Ok(())
    }

    fn validate_structure(&self) -> Result<()> {
        validate_bounded_text("finding title", &self.title, MAX_TITLE_BYTES)?;
        validate_bounded_text("finding detail", &self.detail, MAX_DETAIL_BYTES)?;
        if self.citations.is_empty() || self.citations.len() > MAX_CITATIONS {
            return Err(Error::Limit(format!(
                "finding claim exceeds {MAX_CITATIONS} citations"
            )));
        }

        let mut citations = HashSet::with_capacity(self.citations.len());
        for citation in &self.citations {
            validate_digest("citation row ID", &citation.row_id)?;
            if citation.start_line == 0 || citation.start_line > citation.end_line {
                return Err(Error::Invalid(format!(
                    "citation for row {} starts after it ends",
                    citation.row_id
                )));
            }
            if !citations.insert((
                citation.row_id.as_str(),
                citation.start_line,
                citation.end_line,
            )) {
                return Err(Error::Invalid(format!(
                    "duplicate citation for row {} lines {}-{}",
                    citation.row_id, citation.start_line, citation.end_line
                )));
            }
        }
        Ok(())
    }
}

/// Deterministic finding identifiers. These ordinary values are unauthenticated.
///
/// Private fields prevent callers from casually replacing lineage identifiers;
/// deserialized values are checked before they can be used for continuation.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct FindingIdentity {
    family_id: String,
    occurrence_id: String,
    subject_sha256: String,
    claim_sha256: String,
}

impl FindingIdentity {
    pub fn family_id(&self) -> &str {
        &self.family_id
    }

    pub fn occurrence_id(&self) -> &str {
        &self.occurrence_id
    }

    pub fn subject_sha256(&self) -> &str {
        &self.subject_sha256
    }

    pub fn claim_sha256(&self) -> &str {
        &self.claim_sha256
    }

    fn validate(&self) -> Result<()> {
        validate_digest("finding family ID", &self.family_id)?;
        validate_digest("finding occurrence ID", &self.occurrence_id)?;
        validate_digest("finding subject digest", &self.subject_sha256)?;
        validate_digest("finding claim digest", &self.claim_sha256)?;
        let expected = occurrence_digest(&self.family_id, &self.subject_sha256, &self.claim_sha256);
        if self.occurrence_id != expected {
            return Err(Error::Invalid(
                "finding occurrence ID does not match its family, subject, and claim".into(),
            ));
        }
        Ok(())
    }
}

/// Assigns a new family anchored to this first observed occurrence.
///
/// Callers must validate citations against their delivered-row catalog before
/// recording the returned identity. The attempt coordinates are trusted
/// controller inputs, never lineage identifiers accepted from model output.
pub fn assign_new_finding(
    subject: &CheckSubject,
    claim: &FindingClaim,
    attempt_id: &str,
    run_generation: u64,
    call_index: u64,
    ordinal: u64,
) -> Result<FindingIdentity> {
    validate_identifier("attempt ID", attempt_id)?;
    if run_generation == 0 || run_generation > i64::MAX as u64 {
        return Err(Error::Invalid(
            "finding run generation must be a positive signed 64-bit integer".into(),
        ));
    }
    let subject_sha256 = subject_digest(subject)?;
    let claim_sha256 = claim_digest(claim)?;

    let mut family = Sha256::new();
    family.update(FAMILY_DOMAIN);
    hash_field(&mut family, subject_sha256.as_bytes());
    hash_field(&mut family, claim_sha256.as_bytes());
    hash_field(&mut family, attempt_id.as_bytes());
    family.update(run_generation.to_be_bytes());
    family.update(call_index.to_be_bytes());
    family.update(ordinal.to_be_bytes());
    let family_id = format!("{:x}", family.finalize());
    let occurrence_id = occurrence_digest(&family_id, &subject_sha256, &claim_sha256);

    Ok(FindingIdentity {
        family_id,
        occurrence_id,
        subject_sha256,
        claim_sha256,
    })
}

/// Derives an occurrence only when a trusted caller explicitly links it to a family.
///
/// Retaining the family is an administrative lineage assertion, not proof of
/// continuity. The returned occurrence binds the newly supplied subject and claim;
/// model output cannot supply or replace the retained family identifier.
pub fn continue_finding(
    previous: &FindingIdentity,
    subject: &CheckSubject,
    claim: &FindingClaim,
) -> Result<FindingIdentity> {
    previous.validate()?;
    let subject_sha256 = subject_digest(subject)?;
    let claim_sha256 = claim_digest(claim)?;
    let occurrence_id = occurrence_digest(&previous.family_id, &subject_sha256, &claim_sha256);
    Ok(FindingIdentity {
        family_id: previous.family_id.clone(),
        occurrence_id,
        subject_sha256,
        claim_sha256,
    })
}

fn subject_digest(subject: &CheckSubject) -> Result<String> {
    validate_subject(subject)?;
    let bytes = serde_json::to_vec(subject)?;
    Ok(domain_digest(SUBJECT_DOMAIN, &bytes))
}

fn claim_digest(claim: &FindingClaim) -> Result<String> {
    claim.validate_structure()?;
    let bytes = serde_json::to_vec(claim)?;
    Ok(domain_digest(CLAIM_DOMAIN, &bytes))
}

fn occurrence_digest(family_id: &str, subject_sha256: &str, claim_sha256: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(OCCURRENCE_DOMAIN);
    hash_field(&mut hasher, family_id.as_bytes());
    hash_field(&mut hasher, subject_sha256.as_bytes());
    hash_field(&mut hasher, claim_sha256.as_bytes());
    format!("{:x}", hasher.finalize())
}

fn domain_digest(domain: &[u8], bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(domain);
    hash_field(&mut hasher, bytes);
    format!("{:x}", hasher.finalize())
}

fn hash_field(hasher: &mut Sha256, field: &[u8]) {
    hasher.update((field.len() as u64).to_be_bytes());
    hasher.update(field);
}

fn validate_subject(subject: &CheckSubject) -> Result<()> {
    validate_identifier("repository ID", &subject.repository)?;
    subject.change.validate()?;
    if subject.repository != subject.change.repository {
        return Err(Error::Invalid(
            "finding subject repository identities do not match".into(),
        ));
    }
    if subject.change_version == 0 || subject.change_version > i64::MAX as u64 {
        return Err(Error::Invalid(
            "finding subject change version must be a positive signed 64-bit integer".into(),
        ));
    }
    validate_digest("context digest", &subject.context_digest)?;
    validate_digest("revision", &subject.revision)?;
    validate_digest("recipe digest", &subject.recipe_sha256)?;
    validate_digest("toolchain digest", &subject.toolchain_sha256)?;
    validate_digest("environment digest", &subject.environment_sha256)
}

fn validate_bounded_text(kind: &str, value: &str, maximum: usize) -> Result<()> {
    if value.trim().is_empty() {
        return Err(Error::Invalid(format!("{kind} must not be empty")));
    }
    if value.len() > maximum {
        return Err(Error::Limit(format!(
            "{kind} exceeds its {maximum}-byte limit"
        )));
    }
    Ok(())
}

fn validate_identifier(kind: &str, value: &str) -> Result<()> {
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

fn validate_digest(kind: &str, value: &str) -> Result<()> {
    if value.len() != 64
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err(Error::Invalid(format!(
            "{kind} must be a lowercase SHA-256 digest"
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context::ChangeId;

    fn subject(revision_digit: char) -> CheckSubject {
        CheckSubject {
            repository: "repository-1".into(),
            change: ChangeId {
                repository: "repository-1".into(),
                id: "change-1".into(),
            },
            change_version: 1,
            context_digest: "1".repeat(64),
            revision: revision_digit.to_string().repeat(64),
            recipe_sha256: "3".repeat(64),
            toolchain_sha256: "4".repeat(64),
            environment_sha256: "5".repeat(64),
        }
    }

    fn claim() -> FindingClaim {
        FindingClaim {
            title: "Unchecked authorization".into(),
            detail: "The protected operation does not verify its caller.".into(),
            severity: FindingSeverity::Error,
            citations: vec![SourceCitation {
                row_id: "a".repeat(64),
                start_line: 12,
                end_line: 14,
            }],
        }
    }

    #[test]
    fn citations_must_be_within_delivered_ranges() {
        let claim = claim();
        assert!(claim.validate(&[("a".repeat(64), 10, 20)]).is_ok());
        assert!(claim.validate(&[("a".repeat(64), 13, 20)]).is_err());
        assert!(claim.validate(&[("b".repeat(64), 10, 20)]).is_err());

        let mut duplicate = claim;
        duplicate.citations.push(duplicate.citations[0].clone());
        assert!(duplicate.validate(&[("a".repeat(64), 10, 20)]).is_err());
    }

    #[test]
    fn new_identity_is_deterministic() {
        let first = assign_new_finding(&subject('2'), &claim(), "attempt-1", 7, 2, 0).unwrap();
        let second = assign_new_finding(&subject('2'), &claim(), "attempt-1", 7, 2, 0).unwrap();
        assert_eq!(first, second);
    }

    #[test]
    fn finding_subject_generation_must_fit_authority_storage() {
        let mut invalid = subject('2');
        invalid.change_version = i64::MAX as u64 + 1;
        assert!(assign_new_finding(&invalid, &claim(), "attempt-1", 7, 2, 0).is_err());
        let original = assign_new_finding(&subject('2'), &claim(), "attempt-1", 7, 2, 0).unwrap();
        assert!(continue_finding(&original, &invalid, &claim()).is_err());
    }

    #[test]
    fn explicit_continuation_retains_family_and_changes_subject_occurrence() {
        let first = assign_new_finding(&subject('2'), &claim(), "attempt-1", 7, 2, 0).unwrap();
        let continued = continue_finding(&first, &subject('6'), &claim()).unwrap();

        assert_eq!(continued.family_id(), first.family_id());
        assert_ne!(continued.subject_sha256(), first.subject_sha256());
        assert_ne!(continued.occurrence_id(), first.occurrence_id());
        assert_eq!(continued.claim_sha256(), first.claim_sha256());
    }

    #[test]
    fn continuation_binds_a_changed_claim_to_the_retained_family() {
        let first = assign_new_finding(&subject('2'), &claim(), "attempt-1", 7, 2, 0).unwrap();
        let mut changed = claim();
        changed.detail.push_str(" Additional detail.");
        let continued = continue_finding(&first, &subject('6'), &changed).unwrap();

        assert_eq!(continued.family_id(), first.family_id());
        assert_ne!(continued.claim_sha256(), first.claim_sha256());
        assert_ne!(continued.occurrence_id(), first.occurrence_id());
    }
}
