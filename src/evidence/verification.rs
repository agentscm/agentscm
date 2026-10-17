use std::collections::{HashMap, HashSet};

use ed25519_dalek::{Signature, VerifyingKey};
use sha2::{Digest, Sha256};

use super::contracts::*;
use super::signature::decode_hex;
use crate::policy::issuers::{IssuerRegistrySnapshot, KeyTrustStatus};
use crate::{Error, Result};

#[derive(Clone, Copy, Debug)]
pub struct ArtifactInput<'a> {
    pub artifact_id: &'a str,
    pub supplied: SuppliedArtifact<'a>,
}

#[derive(Clone, Copy, Debug)]
pub enum SuppliedArtifact<'a> {
    Full(&'a [u8]),
    Partial(&'a [u8]),
    Redacted,
    Unavailable,
}

/// Strictly verifies portable evidence against one caller-authenticated,
/// installed registry snapshot. The report remains descriptive ordinary data.
pub fn verify_evidence(
    envelope_bytes: &[u8],
    registry: &IssuerRegistrySnapshot,
    clock: EvaluationClock,
    artifacts: &[ArtifactInput<'_>],
) -> Result<VerificationReport> {
    if let EvaluationClock::Known { unix_ms } = clock
        && unix_ms > i64::MAX as u64
    {
        return Err(Error::Limit(
            "evaluation timestamp must fit a non-negative signed 64-bit integer".into(),
        ));
    }
    let envelope = EvidenceEnvelope::from_slice(envelope_bytes)?;
    let envelope_sha256 = format!("{:x}", Sha256::digest(envelope.canonical_bytes()?));
    registry.validate()?;
    if envelope.payload.subject.repository != registry.repository {
        return Err(Error::Invalid(
            "evidence subject repository does not match issuer registry".into(),
        ));
    }
    let evaluated_registry_sha256 = registry.digest()?;
    let registry_binding = if envelope.payload.issuer_registry_sha256 == evaluated_registry_sha256 {
        RegistryBindingStatus::Current
    } else {
        RegistryBindingStatus::Stale
    };
    validate_artifact_inputs(&envelope.payload, artifacts)?;

    let (integrity, verified_receipt) = verify_integrity(&envelope, registry, &envelope_sha256)?;
    let (current_issuer_trust, freshness) = temporal_status(&envelope, registry, clock)?;
    let execution_claim = match envelope.payload.execution_basis {
        ExecutionBasis::ControllerObserved => ExecutionClaimStatus::ControllerObservedAssertion,
        ExecutionBasis::WorkerAsserted => ExecutionClaimStatus::WorkerAssertion,
        ExecutionBasis::Unqualified => ExecutionClaimStatus::Unqualified,
        ExecutionBasis::Fixture => ExecutionClaimStatus::Fixture,
    };
    let artifacts = assess_artifacts(&envelope.payload, clock, artifacts);

    Ok(VerificationReport {
        integrity,
        registry_binding,
        current_issuer_trust,
        freshness,
        execution_claim,
        artifacts,
        evaluated_clock: clock,
        evaluated_registry_sha256,
        envelope_sha256,
        verified_receipt,
    })
}

fn verify_integrity(
    envelope: &EvidenceEnvelope,
    registry: &IssuerRegistrySnapshot,
    envelope_sha256: &str,
) -> Result<(IntegrityStatus, Option<VerifiedReceipt>)> {
    let Some(key) = registry.key_record(&envelope.payload.issuer_id, &envelope.payload.key_id)?
    else {
        return Ok((IntegrityStatus::UnknownKey, None));
    };
    let public_key_bytes = key.public_key_bytes()?;
    if !is_canonical_ed25519_encoding(&public_key_bytes) {
        return Ok((IntegrityStatus::NonCanonicalPublicKey, None));
    }
    let verifying_key = match VerifyingKey::from_bytes(&public_key_bytes) {
        Ok(key) => key,
        Err(_) => return Ok((IntegrityStatus::InvalidPublicKey, None)),
    };
    if verifying_key.is_weak() {
        return Ok((IntegrityStatus::WeakPublicKey, None));
    }
    let signature_bytes = decode_hex::<64>("Ed25519 signature", &envelope.signature_hex)?;
    let signature = Signature::from_bytes(&signature_bytes);
    if verifying_key
        .verify_strict(&signature_message(&envelope.payload)?, &signature)
        .is_err()
    {
        return Ok((IntegrityStatus::InvalidSignature, None));
    }
    Ok((
        IntegrityStatus::Verified,
        Some(VerifiedReceipt {
            payload: envelope.payload.canonicalized(),
            envelope_sha256: envelope_sha256.to_owned(),
        }),
    ))
}

fn temporal_status(
    envelope: &EvidenceEnvelope,
    registry: &IssuerRegistrySnapshot,
    clock: EvaluationClock,
) -> Result<(CurrentIssuerTrust, Freshness)> {
    let Some(issuer) = registry
        .issuers
        .iter()
        .find(|issuer| issuer.issuer_id == envelope.payload.issuer_id)
    else {
        return Ok((
            CurrentIssuerTrust::UnknownIssuer,
            freshness(&envelope.payload, clock),
        ));
    };
    if !issuer
        .keys
        .iter()
        .any(|key| key.key_id == envelope.payload.key_id)
    {
        return Ok((
            CurrentIssuerTrust::UnknownKey,
            freshness(&envelope.payload, clock),
        ));
    }
    let EvaluationClock::Known { unix_ms } = clock else {
        return Ok((
            CurrentIssuerTrust::ClockUncertain,
            Freshness::ClockUncertain,
        ));
    };
    let trust = registry.key_trust(
        &envelope.payload.issuer_id,
        &envelope.payload.key_id,
        envelope.payload.issued_at_ms,
        unix_ms,
    )?;
    let trust = match trust.status {
        KeyTrustStatus::UnknownIssuer => CurrentIssuerTrust::UnknownIssuer,
        KeyTrustStatus::UnknownKey => CurrentIssuerTrust::UnknownKey,
        KeyTrustStatus::FutureIssued => CurrentIssuerTrust::FutureIssued,
        KeyTrustStatus::NotYetValid => CurrentIssuerTrust::NotYetValid,
        KeyTrustStatus::Expired => CurrentIssuerTrust::Expired,
        KeyTrustStatus::Revoked => CurrentIssuerTrust::Revoked,
        KeyTrustStatus::Trusted => CurrentIssuerTrust::Trusted,
    };
    Ok((trust, freshness(&envelope.payload, clock)))
}

fn freshness(payload: &EvidencePayload, clock: EvaluationClock) -> Freshness {
    let EvaluationClock::Known { unix_ms } = clock else {
        return Freshness::ClockUncertain;
    };
    if payload.issued_at_ms > unix_ms {
        Freshness::FutureIssued
    } else if payload.expires_at_ms <= unix_ms {
        Freshness::Expired
    } else {
        Freshness::Fresh
    }
}

fn assess_artifacts(
    payload: &EvidencePayload,
    clock: EvaluationClock,
    inputs: &[ArtifactInput<'_>],
) -> Vec<ArtifactAssessment> {
    let inputs: HashMap<&str, SuppliedArtifact<'_>> = inputs
        .iter()
        .map(|input| (input.artifact_id, input.supplied))
        .collect();
    let mut assessments: Vec<ArtifactAssessment> = payload
        .artifacts
        .iter()
        .map(|record| {
            let expired = match (record.expires_at_ms, clock) {
                (Some(expiry), EvaluationClock::Known { unix_ms }) => expiry <= unix_ms,
                _ => false,
            };
            if expired {
                return ArtifactAssessment {
                    artifact_id: record.artifact_id.clone(),
                    status: ArtifactStatus::Expired,
                    commitment_matches: false,
                };
            }
            if record.expires_at_ms.is_some() && clock == EvaluationClock::Uncertain {
                return ArtifactAssessment {
                    artifact_id: record.artifact_id.clone(),
                    status: ArtifactStatus::Unavailable,
                    commitment_matches: false,
                };
            }
            let (observed_status, matches) = match inputs.get(record.artifact_id.as_str()) {
                Some(SuppliedArtifact::Full(bytes)) => {
                    let matches = bytes_match(record, bytes);
                    (
                        if matches {
                            ArtifactStatus::Full
                        } else {
                            ArtifactStatus::HashMismatch
                        },
                        matches,
                    )
                }
                Some(SuppliedArtifact::Partial(bytes)) => {
                    let matches = bytes_match(record, bytes);
                    (
                        if matches {
                            ArtifactStatus::Partial
                        } else {
                            ArtifactStatus::HashMismatch
                        },
                        matches,
                    )
                }
                Some(SuppliedArtifact::Redacted) => (ArtifactStatus::Redacted, false),
                Some(SuppliedArtifact::Unavailable) | None => (ArtifactStatus::Unavailable, false),
            };
            let status = constrain_availability(record.claimed_availability, observed_status);
            ArtifactAssessment {
                artifact_id: record.artifact_id.clone(),
                status,
                commitment_matches: matches,
            }
        })
        .collect();
    assessments.sort_by(|left, right| left.artifact_id.cmp(&right.artifact_id));
    assessments
}

fn constrain_availability(
    claimed: ArtifactAvailability,
    observed: ArtifactStatus,
) -> ArtifactStatus {
    if observed == ArtifactStatus::HashMismatch {
        return observed;
    }
    match claimed {
        ArtifactAvailability::Full => observed,
        ArtifactAvailability::Partial => match observed {
            ArtifactStatus::Full | ArtifactStatus::Partial => ArtifactStatus::Partial,
            other => other,
        },
        ArtifactAvailability::Redacted => ArtifactStatus::Redacted,
        ArtifactAvailability::Unavailable => ArtifactStatus::Unavailable,
    }
}

fn bytes_match(record: &ArtifactRecord, bytes: &[u8]) -> bool {
    u64::try_from(bytes.len()).ok() == Some(record.bytes)
        && format!("{:x}", Sha256::digest(bytes)) == record.sha256
}

fn validate_artifact_inputs(payload: &EvidencePayload, inputs: &[ArtifactInput<'_>]) -> Result<()> {
    if inputs.len() > MAX_ARTIFACTS {
        return Err(Error::Limit(format!(
            "supplied artifacts exceed {MAX_ARTIFACTS} entries"
        )));
    }
    let mut identifiers = HashSet::with_capacity(inputs.len());
    let mut total = 0usize;
    for input in inputs {
        validate_identifier("supplied artifact ID", input.artifact_id)?;
        if !payload
            .artifacts
            .iter()
            .any(|record| record.artifact_id == input.artifact_id)
        {
            return Err(Error::Invalid(format!(
                "supplied artifact is not committed by the evidence: {}",
                input.artifact_id
            )));
        }
        if !identifiers.insert(input.artifact_id) {
            return Err(Error::Invalid(format!(
                "duplicate supplied artifact: {}",
                input.artifact_id
            )));
        }
        let bytes = match input.supplied {
            SuppliedArtifact::Full(bytes) | SuppliedArtifact::Partial(bytes) => bytes.len(),
            SuppliedArtifact::Redacted | SuppliedArtifact::Unavailable => 0,
        };
        total = total
            .checked_add(bytes)
            .ok_or_else(|| Error::Limit("supplied artifact byte count overflow".into()))?;
        if total > MAX_SUPPLIED_ARTIFACT_BYTES {
            return Err(Error::Limit(format!(
                "supplied artifact bytes exceed {MAX_SUPPLIED_ARTIFACT_BYTES}"
            )));
        }
    }
    Ok(())
}

/// Rejects encodings where the Edwards y-coordinate is not below 2^255 - 19.
/// The high bit encodes the x-coordinate sign and is excluded from this test.
fn is_canonical_ed25519_encoding(bytes: &[u8; 32]) -> bool {
    const FIELD_MODULUS: [u8; 32] = [
        0xed, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff,
        0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff,
        0xff, 0x7f,
    ];
    let mut y = *bytes;
    y[31] &= 0x7f;
    for index in (0..32).rev() {
        if y[index] < FIELD_MODULUS[index] {
            return true;
        }
        if y[index] > FIELD_MODULUS[index] {
            return false;
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::is_canonical_ed25519_encoding;

    #[test]
    fn rejects_field_modulus_and_larger_encodings() {
        let mut modulus = [0xff; 32];
        modulus[0] = 0xed;
        modulus[31] = 0x7f;
        assert!(!is_canonical_ed25519_encoding(&modulus));
        modulus[0] = 0xee;
        assert!(!is_canonical_ed25519_encoding(&modulus));
        modulus[0] = 0xec;
        assert!(is_canonical_ed25519_encoding(&modulus));
    }
}
