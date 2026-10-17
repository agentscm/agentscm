use std::collections::{HashMap, HashSet};

use ed25519_dalek::{Signature, VerifyingKey};

use crate::format::{
    ArtifactAssessment, ArtifactAvailability, ArtifactStatus, Clock, CommonControl, ErrorCode,
    EvidencePayload, ExecutionBasis, ExecutionClaim, FreshnessStatus, IndependenceReport,
    IntegrityStatus, IssuerKeyRecord, IssuerRegistrySnapshot, IssuerTrustStatus,
    MAX_SUPPLIED_ARTIFACT_BYTES, ParsedEnvelope, ParsedRegistry, RegistryBindingStatus,
    SuppliedArtifact, SuppliedArtifactContent, VerificationReport, VerifiedReceipt, VerifyError,
    decode_exact_lower_hex, parse_envelope, parse_registry, sha256_hex, signature_message,
    validate_clock, validate_identifier,
};

pub fn verify(
    envelope_bytes: &[u8],
    registry_bytes: &[u8],
    clock: Clock,
    supplied_artifacts: &[SuppliedArtifact],
) -> Result<VerificationReport, VerifyError> {
    let envelope = parse_envelope(envelope_bytes)?;
    let registry = parse_registry(registry_bytes)?;
    verify_parsed(envelope, registry, clock, supplied_artifacts)
}

pub fn verify_parsed(
    envelope: ParsedEnvelope,
    registry: ParsedRegistry,
    clock: Clock,
    supplied_artifacts: &[SuppliedArtifact],
) -> Result<VerificationReport, VerifyError> {
    validate_clock(&clock)?;
    if envelope.envelope.payload.subject.repository != registry.registry.repository {
        return Err(VerifyError::new(
            ErrorCode::RepositoryMismatch,
            "evidence and registry repositories differ",
        ));
    }
    validate_supplied_artifacts(&envelope.envelope.payload, supplied_artifacts)?;

    let envelope_sha256 = sha256_hex(&envelope.canonical_envelope);
    let registry_binding =
        if envelope.envelope.payload.issuer_registry_sha256 == registry.canonical_sha256 {
            RegistryBindingStatus::Current
        } else {
            RegistryBindingStatus::Stale
        };
    let integrity = integrity_status(&envelope, &registry.registry);
    let current_issuer_trust =
        current_issuer_trust(&envelope.envelope.payload, &registry.registry, &clock);
    let freshness = freshness(&envelope.envelope.payload, &clock);
    let execution_claim = execution_claim(envelope.envelope.payload.execution_basis);
    let artifacts = assess_artifacts(&envelope.envelope.payload, &clock, supplied_artifacts);
    let verified_receipt = if integrity == IntegrityStatus::Verified {
        Some(VerifiedReceipt {
            canonical_payload: envelope.envelope.payload.clone(),
            canonical_payload_sha256: sha256_hex(&envelope.canonical_payload),
            envelope_sha256: envelope_sha256.clone(),
        })
    } else {
        None
    };

    Ok(VerificationReport {
        integrity,
        registry_binding,
        current_issuer_trust,
        freshness,
        execution_claim,
        artifacts,
        evaluated_clock: clock,
        evaluated_registry_sha256: registry.canonical_sha256,
        envelope_sha256,
        verified_receipt,
    })
}

fn integrity_status(
    envelope: &ParsedEnvelope,
    registry: &IssuerRegistrySnapshot,
) -> IntegrityStatus {
    let payload = &envelope.envelope.payload;
    let Some(issuer) = registry
        .issuers
        .iter()
        .find(|issuer| issuer.issuer_id == payload.issuer_id)
    else {
        return IntegrityStatus::UnknownKey;
    };
    let Some(key) = issuer.keys.iter().find(|key| key.key_id == payload.key_id) else {
        return IntegrityStatus::UnknownKey;
    };
    verify_detached_strict(
        &key.public_key_hex,
        &envelope.envelope.signature_hex,
        &signature_message(&envelope.canonical_payload),
    )
}

pub fn verify_detached_strict(
    public_key_hex: &str,
    signature_hex: &str,
    message: &[u8],
) -> IntegrityStatus {
    let Ok(public_key_bytes) = decode_exact_lower_hex::<32>(public_key_hex) else {
        return IntegrityStatus::InvalidPublicKey;
    };
    if has_noncanonical_y(public_key_bytes) {
        return IntegrityStatus::NonCanonicalPublicKey;
    }
    let Ok(verifying_key) = VerifyingKey::from_bytes(&public_key_bytes) else {
        return IntegrityStatus::InvalidPublicKey;
    };
    if verifying_key.is_weak() {
        return IntegrityStatus::WeakPublicKey;
    }
    let Ok(signature_bytes) = decode_exact_lower_hex::<64>(signature_hex) else {
        return IntegrityStatus::InvalidSignature;
    };
    let signature = Signature::from_bytes(&signature_bytes);
    if verifying_key.verify_strict(message, &signature).is_ok() {
        IntegrityStatus::Verified
    } else {
        IntegrityStatus::InvalidSignature
    }
}

fn has_noncanonical_y(mut encoded: [u8; 32]) -> bool {
    encoded[31] &= 0x7f;
    let mut modulus = [0xff_u8; 32];
    modulus[0] = 0xed;
    modulus[31] = 0x7f;
    for index in (0..32).rev() {
        if encoded[index] > modulus[index] {
            return true;
        }
        if encoded[index] < modulus[index] {
            return false;
        }
    }
    true
}

fn current_issuer_trust(
    payload: &EvidencePayload,
    registry: &IssuerRegistrySnapshot,
    clock: &Clock,
) -> IssuerTrustStatus {
    let Some(issuer) = registry
        .issuers
        .iter()
        .find(|issuer| issuer.issuer_id == payload.issuer_id)
    else {
        return IssuerTrustStatus::UnknownIssuer;
    };
    let Some(key) = issuer.keys.iter().find(|key| key.key_id == payload.key_id) else {
        return IssuerTrustStatus::UnknownKey;
    };
    let Clock::Known { unix_ms } = clock else {
        return IssuerTrustStatus::ClockUncertain;
    };
    evaluate_known_clock_trust(payload, key, *unix_ms)
}

fn evaluate_known_clock_trust(
    payload: &EvidencePayload,
    key: &IssuerKeyRecord,
    now_ms: u64,
) -> IssuerTrustStatus {
    if key
        .revoked_at_ms
        .is_some_and(|revoked_at_ms| revoked_at_ms <= now_ms)
    {
        IssuerTrustStatus::Revoked
    } else if payload.issued_at_ms > now_ms {
        IssuerTrustStatus::FutureIssued
    } else if payload.issued_at_ms < key.not_before_ms || now_ms < key.not_before_ms {
        IssuerTrustStatus::NotYetValid
    } else if payload.issued_at_ms >= key.not_after_ms || now_ms >= key.not_after_ms {
        IssuerTrustStatus::Expired
    } else {
        IssuerTrustStatus::Trusted
    }
}

fn freshness(payload: &EvidencePayload, clock: &Clock) -> FreshnessStatus {
    match clock {
        Clock::Uncertain {} => FreshnessStatus::ClockUncertain,
        Clock::Known { unix_ms } if payload.issued_at_ms > *unix_ms => {
            FreshnessStatus::FutureIssued
        }
        Clock::Known { unix_ms } if payload.expires_at_ms <= *unix_ms => FreshnessStatus::Expired,
        Clock::Known { .. } => FreshnessStatus::Fresh,
    }
}

fn execution_claim(basis: ExecutionBasis) -> ExecutionClaim {
    match basis {
        ExecutionBasis::ControllerObserved => ExecutionClaim::ControllerObservedAssertion,
        ExecutionBasis::WorkerAsserted => ExecutionClaim::WorkerAssertion,
        ExecutionBasis::Unqualified => ExecutionClaim::Unqualified,
        ExecutionBasis::Fixture => ExecutionClaim::Fixture,
    }
}

fn validate_supplied_artifacts(
    payload: &EvidencePayload,
    supplied: &[SuppliedArtifact],
) -> Result<(), VerifyError> {
    if supplied.len() > 64 {
        return Err(VerifyError::new(
            ErrorCode::CountLimit,
            "supplied artifact count exceeds its limit",
        ));
    }
    let committed: HashSet<&str> = payload
        .artifacts
        .iter()
        .map(|artifact| artifact.artifact_id.as_str())
        .collect();
    let mut ids = HashSet::new();
    let mut total_bytes = 0_usize;
    for artifact in supplied {
        validate_identifier(&artifact.artifact_id)?;
        if !ids.insert(artifact.artifact_id.as_str()) {
            return Err(VerifyError::new(
                ErrorCode::DuplicateSemanticEntry,
                "duplicate supplied artifact identity",
            ));
        }
        if !committed.contains(artifact.artifact_id.as_str()) {
            return Err(VerifyError::new(
                ErrorCode::UnknownSuppliedArtifact,
                "supplied artifact is not committed by the evidence",
            ));
        }
        let length = match &artifact.content {
            SuppliedArtifactContent::Full(bytes) | SuppliedArtifactContent::Partial(bytes) => {
                bytes.len()
            }
            SuppliedArtifactContent::Redacted | SuppliedArtifactContent::Unavailable => 0,
        };
        total_bytes = total_bytes.checked_add(length).ok_or_else(|| {
            VerifyError::new(
                ErrorCode::SuppliedArtifactSizeLimit,
                "supplied artifact byte count overflows",
            )
        })?;
        if total_bytes > MAX_SUPPLIED_ARTIFACT_BYTES {
            return Err(VerifyError::new(
                ErrorCode::SuppliedArtifactSizeLimit,
                "supplied artifact bytes exceed the total limit",
            ));
        }
    }
    Ok(())
}

fn assess_artifacts(
    payload: &EvidencePayload,
    clock: &Clock,
    supplied: &[SuppliedArtifact],
) -> Vec<ArtifactAssessment> {
    let supplied_by_id: HashMap<&str, &SuppliedArtifactContent> = supplied
        .iter()
        .map(|artifact| (artifact.artifact_id.as_str(), &artifact.content))
        .collect();
    payload
        .artifacts
        .iter()
        .map(|record| {
            if let Some(expires_at_ms) = record.expires_at_ms {
                match clock {
                    Clock::Known { unix_ms } if *unix_ms >= expires_at_ms => {
                        return ArtifactAssessment {
                            artifact_id: record.artifact_id.clone(),
                            status: ArtifactStatus::Expired,
                            commitment_matches: false,
                        };
                    }
                    Clock::Uncertain {} => {
                        return ArtifactAssessment {
                            artifact_id: record.artifact_id.clone(),
                            status: ArtifactStatus::Unavailable,
                            commitment_matches: false,
                        };
                    }
                    Clock::Known { .. } => {}
                }
            }

            let (observed, commitment_matches) =
                match supplied_by_id.get(record.artifact_id.as_str()) {
                    Some(SuppliedArtifactContent::Full(bytes)) => compare_artifact_bytes(
                        bytes,
                        record.bytes,
                        &record.sha256,
                        ArtifactStatus::Full,
                    ),
                    Some(SuppliedArtifactContent::Partial(bytes)) => compare_artifact_bytes(
                        bytes,
                        record.bytes,
                        &record.sha256,
                        ArtifactStatus::Partial,
                    ),
                    Some(SuppliedArtifactContent::Redacted) => (ArtifactStatus::Redacted, false),
                    Some(SuppliedArtifactContent::Unavailable) | None => {
                        (ArtifactStatus::Unavailable, false)
                    }
                };
            let status = constrain_artifact_status(observed, record.claimed_availability);
            ArtifactAssessment {
                artifact_id: record.artifact_id.clone(),
                status,
                commitment_matches,
            }
        })
        .collect()
}

fn compare_artifact_bytes(
    bytes: &[u8],
    expected_bytes: u64,
    expected_sha256: &str,
    matching_status: ArtifactStatus,
) -> (ArtifactStatus, bool) {
    if bytes.len() as u64 == expected_bytes && sha256_hex(bytes) == expected_sha256 {
        (matching_status, true)
    } else {
        (ArtifactStatus::HashMismatch, false)
    }
}

fn constrain_artifact_status(
    observed: ArtifactStatus,
    claimed: ArtifactAvailability,
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

pub fn assess_independence(
    parsed_registry: &ParsedRegistry,
    selected_issuer_ids: &[String],
    check_kind: &str,
) -> Result<IndependenceReport, VerifyError> {
    validate_identifier(check_kind)?;
    if selected_issuer_ids.len() > 128 {
        return Err(VerifyError::new(
            ErrorCode::CountLimit,
            "selected issuer count exceeds its limit",
        ));
    }
    for issuer_id in selected_issuer_ids {
        validate_identifier(issuer_id)?;
    }

    let registry = &parsed_registry.registry;
    let mut parent: Vec<usize> = (0..registry.issuers.len()).collect();
    for left in 0..registry.issuers.len() {
        for right in (left + 1)..registry.issuers.len() {
            if shares_control(&registry.issuers[left], &registry.issuers[right]) {
                union(&mut parent, left, right);
            }
        }
    }

    let by_id: HashMap<&str, usize> = registry
        .issuers
        .iter()
        .enumerate()
        .map(|(index, issuer)| (issuer.issuer_id.as_str(), index))
        .collect();
    let mut seen = HashSet::new();
    let mut duplicates = HashSet::new();
    let mut unknown = Vec::new();
    let mut unknown_control = Vec::new();
    let mut disallowed = Vec::new();
    let mut eligible_by_root: HashMap<usize, Vec<String>> = HashMap::new();

    for selected in selected_issuer_ids {
        if !seen.insert(selected.as_str()) {
            duplicates.insert(selected.clone());
            continue;
        }
        let Some(&index) = by_id.get(selected.as_str()) else {
            unknown.push(selected.clone());
            continue;
        };
        let issuer = &registry.issuers[index];
        let allowed = issuer
            .allowed_check_kinds
            .iter()
            .any(|allowed| allowed == check_kind);
        if !allowed {
            disallowed.push(selected.clone());
        }
        let known_control = matches!(&issuer.common_control, CommonControl::Known { .. });
        if !known_control {
            unknown_control.push(selected.clone());
        }
        if allowed && known_control {
            let root = find(&mut parent, index);
            eligible_by_root
                .entry(root)
                .or_default()
                .push(selected.clone());
        }
    }

    let mut groups: Vec<Vec<String>> = eligible_by_root.into_values().collect();
    for group in &mut groups {
        group.sort();
    }
    groups.sort();
    unknown.sort();
    unknown_control.sort();
    disallowed.sort();
    let mut duplicate_issuer_ids: Vec<String> = duplicates.into_iter().collect();
    duplicate_issuer_ids.sort();

    Ok(IndependenceReport {
        independent_count: groups.len(),
        groups,
        unknown_issuer_ids: unknown,
        unknown_control_issuer_ids: unknown_control,
        disallowed_check_kind_issuer_ids: disallowed,
        duplicate_issuer_ids,
    })
}

fn shares_control(left: &crate::format::IssuerRecord, right: &crate::format::IssuerRecord) -> bool {
    if left.trust_domain == right.trust_domain {
        return true;
    }
    match (&left.common_control, &right.common_control) {
        (CommonControl::Known { roots: left }, CommonControl::Known { roots: right }) => {
            left.iter().any(|root| right.contains(root))
        }
        _ => false,
    }
}

fn find(parent: &mut [usize], node: usize) -> usize {
    if parent[node] != node {
        parent[node] = find(parent, parent[node]);
    }
    parent[node]
}

fn union(parent: &mut [usize], left: usize, right: usize) {
    let left_root = find(parent, left);
    let right_root = find(parent, right);
    if left_root != right_root {
        parent[right_root] = left_root;
    }
}
