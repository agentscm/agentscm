use agentscm::Error;
use agentscm::checks::CheckSubject;
use agentscm::context::{ChangeId, DependencyClaim, DependencyState};
use agentscm::evidence::*;
use agentscm::policy::issuers::{
    CommonControl, IssuerKeyRecord, IssuerRecord, IssuerRegistrySnapshot, issuer_key_id,
};
use ed25519_dalek::SigningKey;
use serde_json::Value;
use sha2::{Digest, Sha256};

const NOW: u64 = 2_000;

fn digest(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn key_hex(key: &SigningKey) -> String {
    key.verifying_key()
        .as_bytes()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn registry_for(key: &SigningKey) -> IssuerRegistrySnapshot {
    let public_key_hex = key_hex(key);
    IssuerRegistrySnapshot {
        version: 1,
        repository: "repo-1".into(),
        epoch: 3,
        issuers: vec![IssuerRecord {
            issuer_id: "issuer-1".into(),
            trust_domain: "operator-1".into(),
            common_control: CommonControl::Known {
                roots: vec!["root-1".into()],
            },
            allowed_check_kinds: vec!["command/v1".into()],
            keys: vec![IssuerKeyRecord {
                key_id: issuer_key_id(&public_key_hex).unwrap(),
                public_key_hex,
                not_before_ms: 100,
                not_after_ms: 10_000,
                revoked_at_ms: None,
            }],
        }],
    }
}

fn payload(registry: &IssuerRegistrySnapshot) -> EvidencePayload {
    let key_id = registry.issuers[0].keys[0].key_id.clone();
    EvidencePayload {
        version: 1,
        requirement_id: "required-command".into(),
        check_kind: "command/v1".into(),
        subject: CheckSubject {
            repository: "repo-1".into(),
            change: ChangeId {
                repository: "repo-1".into(),
                id: "change-1".into(),
            },
            change_version: 4,
            context_digest: "1".repeat(64),
            revision: "2".repeat(64),
            recipe_sha256: "3".repeat(64),
            toolchain_sha256: "4".repeat(64),
            environment_sha256: "5".repeat(64),
        },
        installed_policy_sha256: "6".repeat(64),
        installed_policy_epoch: 8,
        issuer_registry_sha256: registry.digest().unwrap(),
        dependency_snapshot_sha256: Some("7".repeat(64)),
        attempt_id: "attempt-1".into(),
        run_generation: 2,
        request_sha256: "8".repeat(64),
        limits_sha256: "9".repeat(64),
        terminal_record_sha256: digest(b"hello"),
        issuer_id: "issuer-1".into(),
        key_id,
        issued_at_ms: 1_000,
        expires_at_ms: 5_000,
        terminal_state: TerminalState::Success,
        recovery_state: RecoveryState::NotRequired,
        outcome: EvidenceOutcome::Pass,
        execution_basis: ExecutionBasis::ControllerObserved,
        qualification_claim: Some(QualificationClaim {
            kind: "oci".into(),
            profile: "local-isolation/v1".into(),
            pin_sha256: "a".repeat(64),
        }),
        coverage: CoverageClaim {
            scope: vec!["z".into(), "a".into()],
            expected_count: 2,
            observed_count: 1,
            inventory_complete: false,
            omissions: vec!["missing-z".into()],
        },
        dependencies: vec![
            DependencyClaim {
                identity: "z-dependency".into(),
                state: DependencyState::Unknown {},
            },
            DependencyClaim {
                identity: "a-dependency".into(),
                state: DependencyState::Observed {
                    revision: Some("b".repeat(64)),
                    evidence_digest: "c".repeat(64),
                },
            },
        ],
        artifacts: vec![ArtifactRecord {
            artifact_id: "record".into(),
            media_type: "application/octet-stream".into(),
            sha256: digest(b"hello"),
            bytes: 5,
            claimed_availability: ArtifactAvailability::Full,
            expires_at_ms: Some(4_000),
        }],
        output: OutputCommitment {
            stdout_sha256: digest(b"hello"),
            stdout_bytes: 5,
            stderr_sha256: digest(b""),
            stderr_bytes: 0,
            truncated: false,
        },
    }
}

fn signed_fixture() -> (SigningKey, IssuerRegistrySnapshot, EvidenceEnvelope) {
    let key = SigningKey::from_bytes(&[7; 32]);
    let registry = registry_for(&key);
    let envelope = sign_evidence(payload(&registry), &key).unwrap();
    (key, registry, envelope)
}

fn verify(
    envelope: &EvidenceEnvelope,
    registry: &IssuerRegistrySnapshot,
    clock: EvaluationClock,
    artifacts: &[ArtifactInput<'_>],
) -> VerificationReport {
    verify_evidence(
        &serde_json::to_vec(envelope).unwrap(),
        registry,
        clock,
        artifacts,
    )
    .unwrap()
}

#[test]
fn strict_signature_yields_private_verified_receipt_but_not_execution_proof() {
    let (_, registry, envelope) = signed_fixture();
    let report = verify(
        &envelope,
        &registry,
        EvaluationClock::Known { unix_ms: NOW },
        &[ArtifactInput {
            artifact_id: "record",
            supplied: SuppliedArtifact::Full(b"hello"),
        }],
    );
    assert_eq!(report.integrity(), IntegrityStatus::Verified);
    assert_eq!(report.registry_binding(), RegistryBindingStatus::Current);
    assert_eq!(report.current_issuer_trust(), CurrentIssuerTrust::Trusted);
    assert_eq!(report.freshness(), Freshness::Fresh);
    assert_eq!(
        report.execution_claim(),
        ExecutionClaimStatus::ControllerObservedAssertion
    );
    assert_eq!(report.artifacts()[0].status, ArtifactStatus::Full);
    assert!(report.artifacts()[0].commitment_matches);
    assert_eq!(
        report.evaluated_clock(),
        EvaluationClock::Known { unix_ms: NOW }
    );
    assert_eq!(
        report.evaluated_registry_sha256(),
        registry.digest().unwrap()
    );
    assert_eq!(
        report.envelope_sha256(),
        digest(&envelope.canonical_bytes().unwrap())
    );
    assert_eq!(
        report
            .verified_receipt()
            .unwrap()
            .payload()
            .canonical_bytes()
            .unwrap(),
        envelope.payload.canonical_bytes().unwrap()
    );
    assert_eq!(
        report.verified_receipt().unwrap().envelope_sha256(),
        report.envelope_sha256()
    );
}

#[test]
fn changing_every_binding_group_invalidates_the_signature() {
    let (_, registry, envelope) = signed_fixture();
    let original = serde_json::to_value(&envelope).unwrap();
    let replacements: &[(&[&str], Value)] = &[
        (
            &["payload", "requirement_id"],
            Value::String("other".into()),
        ),
        (
            &["payload", "check_kind"],
            Value::String("source_paths/v1".into()),
        ),
        (
            &["payload", "subject", "revision"],
            Value::String("d".repeat(64)),
        ),
        (
            &["payload", "subject", "context_digest"],
            Value::String("d".repeat(64)),
        ),
        (
            &["payload", "subject", "recipe_sha256"],
            Value::String("d".repeat(64)),
        ),
        (
            &["payload", "subject", "toolchain_sha256"],
            Value::String("d".repeat(64)),
        ),
        (
            &["payload", "subject", "environment_sha256"],
            Value::String("d".repeat(64)),
        ),
        (&["payload", "subject", "change_version"], Value::from(5)),
        (
            &["payload", "installed_policy_sha256"],
            Value::String("e".repeat(64)),
        ),
        (&["payload", "installed_policy_epoch"], Value::from(9)),
        (
            &["payload", "issuer_registry_sha256"],
            Value::String("f".repeat(64)),
        ),
        (
            &["payload", "dependency_snapshot_sha256"],
            Value::String("0".repeat(64)),
        ),
        (
            &["payload", "attempt_id"],
            Value::String("attempt-2".into()),
        ),
        (&["payload", "run_generation"], Value::from(3)),
        (
            &["payload", "request_sha256"],
            Value::String("a".repeat(64)),
        ),
        (&["payload", "limits_sha256"], Value::String("b".repeat(64))),
        (
            &["payload", "terminal_record_sha256"],
            Value::String("b".repeat(64)),
        ),
        (&["payload", "issuer_id"], Value::String("issuer-2".into())),
        (&["payload", "key_id"], Value::String("d".repeat(64))),
        (&["payload", "issued_at_ms"], Value::from(1_001)),
        (&["payload", "expires_at_ms"], Value::from(5_001)),
        (
            &["payload", "execution_basis"],
            Value::String("worker_asserted".into()),
        ),
        (
            &["payload", "qualification_claim", "pin_sha256"],
            Value::String("d".repeat(64)),
        ),
        (&["payload", "coverage", "observed_count"], Value::from(2)),
        (
            &["payload", "output", "stdout_sha256"],
            Value::String("c".repeat(64)),
        ),
    ];
    for (path, replacement) in replacements {
        let mut changed = original.clone();
        let mut cursor = &mut changed;
        for component in &path[..path.len() - 1] {
            cursor = &mut cursor[*component];
        }
        cursor[path[path.len() - 1]] = replacement.clone();
        let bytes = serde_json::to_vec(&changed).unwrap();
        match verify_evidence(
            &bytes,
            &registry,
            EvaluationClock::Known { unix_ms: NOW },
            &[],
        ) {
            Ok(report) => assert_ne!(
                report.integrity(),
                IntegrityStatus::Verified,
                "path {path:?}"
            ),
            Err(Error::Invalid(_)) => {}
            other => panic!("unexpected tamper result for {path:?}: {other:?}"),
        }
    }

    let mut semantic_changes = Vec::new();
    let mut changed = envelope.clone();
    changed.payload.subject.change.id = "change-2".into();
    semantic_changes.push(changed);
    let mut changed = envelope.clone();
    changed.payload.terminal_state = TerminalState::Failed;
    changed.payload.outcome = EvidenceOutcome::Fail;
    semantic_changes.push(changed);
    let mut changed = envelope.clone();
    changed.payload.recovery_state = RecoveryState::Recovered;
    semantic_changes.push(changed);
    let mut changed = envelope.clone();
    changed.payload.coverage.scope[0] = "other-scope".into();
    semantic_changes.push(changed);
    let mut changed = envelope.clone();
    changed.payload.dependencies[0].identity = "other-dependency".into();
    semantic_changes.push(changed);
    let mut changed = envelope.clone();
    changed.payload.artifacts[0].media_type = "application/json".into();
    semantic_changes.push(changed);
    let mut changed = envelope.clone();
    changed.payload.output.truncated = true;
    semantic_changes.push(changed);
    for changed in semantic_changes {
        let report = verify(
            &changed,
            &registry,
            EvaluationClock::Known { unix_ms: NOW },
            &[],
        );
        assert_ne!(report.integrity(), IntegrityStatus::Verified);
    }
}

#[test]
fn canonical_order_is_stable_and_duplicates_are_rejected() {
    let (key, registry, envelope) = signed_fixture();
    let mut reordered = envelope.clone();
    reordered.payload.coverage.scope.reverse();
    reordered.payload.dependencies.reverse();
    let report = verify(
        &reordered,
        &registry,
        EvaluationClock::Known { unix_ms: NOW },
        &[],
    );
    assert_eq!(report.integrity(), IntegrityStatus::Verified);

    let mut duplicate = payload(&registry);
    duplicate.coverage.scope.push("a".into());
    assert!(matches!(
        sign_evidence(duplicate, &key),
        Err(Error::Invalid(_))
    ));
}

#[test]
fn unknown_fields_and_raw_size_cap_are_rejected_before_verification() {
    let (_, registry, envelope) = signed_fixture();
    let mut value = serde_json::to_value(envelope).unwrap();
    value["payload"]["qualification_claim"]["trusted"] = Value::Bool(true);
    assert!(matches!(
        verify_evidence(
            &serde_json::to_vec(&value).unwrap(),
            &registry,
            EvaluationClock::Known { unix_ms: NOW },
            &[]
        ),
        Err(Error::Json(_))
    ));
    let oversized = vec![b' '; MAX_EVIDENCE_BYTES + 1];
    assert!(matches!(
        EvidenceEnvelope::from_slice(&oversized),
        Err(Error::Limit(_))
    ));
}

#[test]
fn unknown_bad_revoked_and_expired_keys_keep_integrity_and_trust_separate() {
    let (_, registry, envelope) = signed_fixture();
    let mut bad = envelope.clone();
    let replacement = if bad.signature_hex.starts_with("00") {
        "01"
    } else {
        "00"
    };
    bad.signature_hex.replace_range(0..2, replacement);
    assert_eq!(
        verify(
            &bad,
            &registry,
            EvaluationClock::Known { unix_ms: NOW },
            &[]
        )
        .integrity(),
        IntegrityStatus::InvalidSignature
    );
    assert_eq!(
        verify(
            &bad,
            &registry,
            EvaluationClock::Known { unix_ms: NOW },
            &[]
        )
        .envelope_sha256(),
        digest(&bad.canonical_bytes().unwrap())
    );

    let mut unknown = envelope.clone();
    unknown.payload.key_id = "0".repeat(64);
    let unknown_report = verify(
        &unknown,
        &registry,
        EvaluationClock::Known { unix_ms: NOW },
        &[],
    );
    assert_eq!(unknown_report.integrity(), IntegrityStatus::UnknownKey);
    assert_eq!(
        unknown_report.current_issuer_trust(),
        CurrentIssuerTrust::UnknownKey
    );

    let (key, mut revoked_registry, _) = signed_fixture();
    revoked_registry.issuers[0].keys[0].revoked_at_ms = Some(1_500);
    let revoked = sign_evidence(payload(&revoked_registry), &key).unwrap();
    let revoked_report = verify(
        &revoked,
        &revoked_registry,
        EvaluationClock::Known { unix_ms: NOW },
        &[],
    );
    assert_eq!(revoked_report.integrity(), IntegrityStatus::Verified);
    assert_eq!(
        revoked_report.current_issuer_trust(),
        CurrentIssuerTrust::Revoked
    );

    let (key, mut expired_registry, _) = signed_fixture();
    expired_registry.issuers[0].keys[0].not_after_ms = 1_500;
    let expired = sign_evidence(payload(&expired_registry), &key).unwrap();
    let expired_report = verify(
        &expired,
        &expired_registry,
        EvaluationClock::Known { unix_ms: NOW },
        &[],
    );
    assert_eq!(expired_report.integrity(), IntegrityStatus::Verified);
    assert_eq!(
        expired_report.current_issuer_trust(),
        CurrentIssuerTrust::Expired
    );
}

#[test]
fn invalid_weak_and_noncanonical_public_keys_are_rejected_before_signature_work() {
    for (public_key, expected) in [
        (
            {
                let mut bytes = [0u8; 32];
                bytes[0] = 2; // Canonical y-coordinate without a curve point.
                bytes
            },
            IntegrityStatus::InvalidPublicKey,
        ),
        (
            {
                let mut bytes = [0u8; 32];
                bytes[0] = 1;
                bytes
            },
            IntegrityStatus::WeakPublicKey,
        ),
        (
            {
                let mut bytes = [0xff; 32];
                bytes[0] = 0xed;
                bytes[31] = 0x7f;
                bytes
            },
            IntegrityStatus::NonCanonicalPublicKey,
        ),
    ] {
        let public_key_hex: String = public_key
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect();
        let mut registry = registry_for(&SigningKey::from_bytes(&[7; 32]));
        let key_id = issuer_key_id(&public_key_hex).unwrap();
        registry.issuers[0].keys[0].key_id = key_id.clone();
        registry.issuers[0].keys[0].public_key_hex = public_key_hex;
        let mut envelope = signed_fixture().2;
        envelope.payload.key_id = key_id;
        envelope.payload.issuer_registry_sha256 = registry.digest().unwrap();
        assert_eq!(
            verify(
                &envelope,
                &registry,
                EvaluationClock::Known { unix_ms: NOW },
                &[]
            )
            .integrity(),
            expected
        );
    }
}

#[test]
fn oversized_clock_is_rejected_even_for_unknown_issuer_or_key() {
    let (_, registry, envelope) = signed_fixture();
    let mut unknown_issuer = registry.clone();
    unknown_issuer.issuers.clear();
    let mut unknown_key = envelope.clone();
    unknown_key.payload.key_id = "a".repeat(64);
    for (input, installed) in [
        (&envelope, &registry),
        (&envelope, &unknown_issuer),
        (&unknown_key, &registry),
    ] {
        assert!(
            verify_evidence(
                &serde_json::to_vec(input).unwrap(),
                installed,
                EvaluationClock::Known {
                    unix_ms: i64::MAX as u64 + 1
                },
                &[],
            )
            .is_err()
        );
    }
}

#[test]
fn uncertain_clock_is_never_fresh_or_currently_trusted() {
    let (_, registry, envelope) = signed_fixture();
    let report = verify(&envelope, &registry, EvaluationClock::Uncertain, &[]);
    assert_eq!(report.integrity(), IntegrityStatus::Verified);
    assert_eq!(
        report.current_issuer_trust(),
        CurrentIssuerTrust::ClockUncertain
    );
    assert_eq!(report.freshness(), Freshness::ClockUncertain);
}

#[test]
fn receipt_expiry_does_not_erase_signature_integrity() {
    let (_, registry, envelope) = signed_fixture();
    let report = verify(
        &envelope,
        &registry,
        EvaluationClock::Known { unix_ms: 5_000 },
        &[],
    );
    assert_eq!(report.integrity(), IntegrityStatus::Verified);
    assert_eq!(report.current_issuer_trust(), CurrentIssuerTrust::Trusted);
    assert_eq!(report.freshness(), Freshness::Expired);
}

#[test]
fn current_registry_and_clock_are_bound_into_the_read_only_report() {
    let (_, mut registry, envelope) = signed_fixture();
    registry.issuers[0].keys[0].revoked_at_ms = Some(1_500);
    let report = verify(
        &envelope,
        &registry,
        EvaluationClock::Known { unix_ms: NOW },
        &[],
    );
    assert_eq!(report.integrity(), IntegrityStatus::Verified);
    assert_eq!(report.registry_binding(), RegistryBindingStatus::Stale);
    assert_eq!(report.current_issuer_trust(), CurrentIssuerTrust::Revoked);
    assert_eq!(
        report.evaluated_clock(),
        EvaluationClock::Known { unix_ms: NOW }
    );
    assert_eq!(
        report.evaluated_registry_sha256(),
        registry.digest().unwrap()
    );
}

#[test]
fn availability_comes_from_supplied_bytes_and_clock() {
    let (key, registry, envelope) = signed_fixture();
    for (input, status, matches) in [
        (
            SuppliedArtifact::Full(b"hell0"),
            ArtifactStatus::HashMismatch,
            false,
        ),
        (
            SuppliedArtifact::Partial(b"hello"),
            ArtifactStatus::Partial,
            true,
        ),
        (SuppliedArtifact::Redacted, ArtifactStatus::Redacted, false),
        (
            SuppliedArtifact::Unavailable,
            ArtifactStatus::Unavailable,
            false,
        ),
    ] {
        let report = verify(
            &envelope,
            &registry,
            EvaluationClock::Known { unix_ms: NOW },
            &[ArtifactInput {
                artifact_id: "record",
                supplied: input,
            }],
        );
        assert_eq!(report.artifacts()[0].status, status);
        assert_eq!(report.artifacts()[0].commitment_matches, matches);
    }
    let expired = verify(
        &envelope,
        &registry,
        EvaluationClock::Known { unix_ms: 4_000 },
        &[ArtifactInput {
            artifact_id: "record",
            supplied: SuppliedArtifact::Full(b"hello"),
        }],
    );
    assert_eq!(expired.artifacts()[0].status, ArtifactStatus::Expired);

    let mut partial_claim = payload(&registry);
    partial_claim.artifacts[0].claimed_availability = ArtifactAvailability::Partial;
    let partial_claim = sign_evidence(partial_claim, &key).unwrap();
    let constrained = verify(
        &partial_claim,
        &registry,
        EvaluationClock::Known { unix_ms: NOW },
        &[ArtifactInput {
            artifact_id: "record",
            supplied: SuppliedArtifact::Full(b"hello"),
        }],
    );
    assert_eq!(constrained.artifacts()[0].status, ArtifactStatus::Partial);
}

#[test]
fn unqualified_and_fixture_bases_remain_only_signed_assertions() {
    let (key, registry, _) = signed_fixture();
    for (basis, expected) in [
        (
            ExecutionBasis::Unqualified,
            ExecutionClaimStatus::Unqualified,
        ),
        (ExecutionBasis::Fixture, ExecutionClaimStatus::Fixture),
    ] {
        let mut payload = payload(&registry);
        payload.execution_basis = basis;
        let envelope = sign_evidence(payload, &key).unwrap();
        let report = verify(
            &envelope,
            &registry,
            EvaluationClock::Known { unix_ms: NOW },
            &[],
        );
        assert_eq!(report.integrity(), IntegrityStatus::Verified);
        assert_eq!(report.execution_claim(), expected);
    }
}

#[test]
fn collection_caps_and_uncommitted_artifact_inputs_fail_closed() {
    let (key, registry, envelope) = signed_fixture();
    let mut too_many = payload(&registry);
    too_many.artifacts = (0..=MAX_ARTIFACTS)
        .map(|index| ArtifactRecord {
            artifact_id: format!("artifact-{index}"),
            media_type: "application/octet-stream".into(),
            sha256: digest(b""),
            bytes: 0,
            claimed_availability: ArtifactAvailability::Unavailable,
            expires_at_ms: None,
        })
        .collect();
    assert!(matches!(
        sign_evidence(too_many, &key),
        Err(Error::Limit(_))
    ));

    let mut missing_record = payload(&registry);
    missing_record.artifacts.clear();
    assert!(matches!(
        sign_evidence(missing_record, &key),
        Err(Error::Invalid(_))
    ));

    let mut mismatched_stdout = payload(&registry);
    mismatched_stdout.artifacts.push(ArtifactRecord {
        artifact_id: "stdout".into(),
        media_type: "application/octet-stream".into(),
        sha256: digest(b"different"),
        bytes: 9,
        claimed_availability: ArtifactAvailability::Full,
        expires_at_ms: None,
    });
    assert!(matches!(
        sign_evidence(mismatched_stdout, &key),
        Err(Error::Invalid(_))
    ));

    assert!(matches!(
        verify_evidence(
            &serde_json::to_vec(&envelope).unwrap(),
            &registry,
            EvaluationClock::Known { unix_ms: NOW },
            &[ArtifactInput {
                artifact_id: "not-signed",
                supplied: SuppliedArtifact::Full(b"bytes"),
            }],
        ),
        Err(Error::Invalid(_))
    ));
}
