use agentscm::Error;
use agentscm::evidence::{
    ArtifactInput, ArtifactStatus, CurrentIssuerTrust, EvaluationClock, EvidenceEnvelope,
    ExecutionClaimStatus, Freshness, IntegrityStatus, MAX_EVIDENCE_BYTES,
    MAX_SUPPLIED_ARTIFACT_BYTES, RegistryBindingStatus, SuppliedArtifact, VerificationReport,
    verify_evidence,
};
use agentscm::policy::issuers::{IssuerRegistrySnapshot, MAX_ISSUER_REGISTRY_BYTES};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

const PROFILE: &[u8] = include_bytes!("../../protocol/evidence-v1/profile.md");
const VECTORS_BYTES: &[u8] = include_bytes!("../../protocol/evidence-v1/vectors.json");
const PORTABLE_PROFILE: &[u8] =
    include_bytes!("../../tools/portable-verifier/protocol/evidence-v1/profile.md");
const PORTABLE_VECTORS: &[u8] =
    include_bytes!("../../tools/portable-verifier/protocol/evidence-v1/vectors.json");

fn vectors() -> Value {
    serde_json::from_slice(VECTORS_BYTES).expect("normative vectors must be JSON")
}

fn named<'a>(vectors: &'a Value, section: &str, name: &str) -> &'a Value {
    vectors[section]
        .as_array()
        .expect("vector section must be an array")
        .iter()
        .find(|entry| entry["name"] == name)
        .unwrap_or_else(|| panic!("missing {section} vector {name}"))
}

fn registry(vectors: &Value, name: &str) -> IssuerRegistrySnapshot {
    let input = named(vectors, "registries", name)["input_text"]
        .as_str()
        .expect("registry input_text");
    IssuerRegistrySnapshot::from_slice(input.as_bytes())
        .unwrap_or_else(|error| panic!("registry {name}: {error}"))
}

fn fixture_bytes(vectors: &Value, name: &str) -> Vec<u8> {
    named(vectors, "signed_fixtures", name)["envelope_input_text"]
        .as_str()
        .expect("fixture envelope_input_text")
        .as_bytes()
        .to_vec()
}

fn clock(value: Option<&Value>) -> EvaluationClock {
    let Some(value) = value.filter(|value| !value.is_null()) else {
        return EvaluationClock::Known { unix_ms: 2_000 };
    };
    match value["kind"].as_str().expect("clock kind") {
        "known" => EvaluationClock::Known {
            unix_ms: value["unix_ms"].as_u64().expect("known clock unix_ms"),
        },
        "uncertain" => EvaluationClock::Uncertain,
        other => panic!("unsupported vector clock {other}"),
    }
}

struct OwnedArtifact {
    artifact_id: String,
    kind: String,
    bytes: Vec<u8>,
}

fn supplied(value: Option<&Value>) -> Vec<OwnedArtifact> {
    let Some(entries) = value.and_then(Value::as_array) else {
        return Vec::new();
    };
    entries
        .iter()
        .map(|entry| OwnedArtifact {
            artifact_id: entry["artifact_id"]
                .as_str()
                .expect("artifact ID")
                .to_owned(),
            kind: entry["kind"].as_str().expect("artifact kind").to_owned(),
            bytes: entry
                .get("bytes_hex")
                .and_then(Value::as_str)
                .map(hex_bytes)
                .unwrap_or_default(),
        })
        .collect()
}

fn artifact_inputs(owned: &[OwnedArtifact]) -> Vec<ArtifactInput<'_>> {
    owned
        .iter()
        .map(|artifact| ArtifactInput {
            artifact_id: &artifact.artifact_id,
            supplied: match artifact.kind.as_str() {
                "full" => SuppliedArtifact::Full(&artifact.bytes),
                "partial" => SuppliedArtifact::Partial(&artifact.bytes),
                "redacted" => SuppliedArtifact::Redacted,
                "unavailable" => SuppliedArtifact::Unavailable,
                other => panic!("unsupported supplied artifact kind {other}"),
            },
        })
        .collect()
}

fn hex_bytes(text: &str) -> Vec<u8> {
    assert_eq!(text.len() % 2, 0, "hex string must have even length");
    text.as_bytes()
        .chunks_exact(2)
        .map(|pair| (hex_nibble(pair[0]) << 4) | hex_nibble(pair[1]))
        .collect()
}

fn hex_nibble(byte: u8) -> u8 {
    match byte {
        b'0'..=b'9' => byte - b'0',
        b'a'..=b'f' => byte - b'a' + 10,
        _ => panic!("vector hex must be lowercase"),
    }
}

fn sha256_hex(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn integrity(value: IntegrityStatus) -> &'static str {
    match value {
        IntegrityStatus::Verified => "verified",
        IntegrityStatus::UnknownKey => "unknown_key",
        IntegrityStatus::InvalidPublicKey => "invalid_public_key",
        IntegrityStatus::NonCanonicalPublicKey => "non_canonical_public_key",
        IntegrityStatus::WeakPublicKey => "weak_public_key",
        IntegrityStatus::InvalidSignature => "invalid_signature",
    }
}

fn registry_binding(value: RegistryBindingStatus) -> &'static str {
    match value {
        RegistryBindingStatus::Current => "current",
        RegistryBindingStatus::Stale => "stale",
    }
}

fn issuer_trust(value: CurrentIssuerTrust) -> &'static str {
    match value {
        CurrentIssuerTrust::Trusted => "trusted",
        CurrentIssuerTrust::UnknownIssuer => "unknown_issuer",
        CurrentIssuerTrust::UnknownKey => "unknown_key",
        CurrentIssuerTrust::FutureIssued => "future_issued",
        CurrentIssuerTrust::NotYetValid => "not_yet_valid",
        CurrentIssuerTrust::Expired => "expired",
        CurrentIssuerTrust::Revoked => "revoked",
        CurrentIssuerTrust::ClockUncertain => "clock_uncertain",
    }
}

fn freshness(value: Freshness) -> &'static str {
    match value {
        Freshness::Fresh => "fresh",
        Freshness::FutureIssued => "future_issued",
        Freshness::Expired => "expired",
        Freshness::ClockUncertain => "clock_uncertain",
    }
}

fn execution_claim(value: ExecutionClaimStatus) -> &'static str {
    match value {
        ExecutionClaimStatus::ControllerObservedAssertion => "controller_observed_assertion",
        ExecutionClaimStatus::WorkerAssertion => "worker_assertion",
        ExecutionClaimStatus::Unqualified => "unqualified",
        ExecutionClaimStatus::Fixture => "fixture",
    }
}

fn artifact_status(value: ArtifactStatus) -> &'static str {
    match value {
        ArtifactStatus::Full => "full",
        ArtifactStatus::Partial => "partial",
        ArtifactStatus::Redacted => "redacted",
        ArtifactStatus::Unavailable => "unavailable",
        ArtifactStatus::Expired => "expired",
        ArtifactStatus::HashMismatch => "hash_mismatch",
    }
}

fn report_projection(report: &VerificationReport) -> Value {
    let receipt = report.verified_receipt();
    let mut value = json!({
        "integrity": integrity(report.integrity()),
        "registry_binding": registry_binding(report.registry_binding()),
        "current_issuer_trust": issuer_trust(report.current_issuer_trust()),
        "freshness": freshness(report.freshness()),
        "execution_claim": execution_claim(report.execution_claim()),
        "artifacts": report.artifacts().iter().map(|artifact| json!({
            "artifact_id": artifact.artifact_id.as_str(),
            "status": artifact_status(artifact.status),
            "commitment_matches": artifact.commitment_matches,
        })).collect::<Vec<_>>(),
        "verified_receipt_present": receipt.is_some(),
        "evaluated_registry_sha256": report.evaluated_registry_sha256(),
        "envelope_sha256": report.envelope_sha256(),
    });
    if let Some(receipt) = receipt {
        value
            .as_object_mut()
            .expect("report projection object")
            .insert(
                "verified_receipt".to_owned(),
                json!({
                    "canonical_payload_sha256": sha256_hex(
                        &receipt.payload().canonical_bytes().expect("receipt canonical payload")
                    ),
                    "envelope_sha256": receipt.envelope_sha256(),
                }),
            );
    }
    value
}

fn assert_subset(expected: &Value, actual: &Value, path: &str) {
    match expected {
        Value::Object(expected) => {
            let actual = actual
                .as_object()
                .unwrap_or_else(|| panic!("{path}: expected object, got {actual}"));
            for (key, expected_value) in expected {
                let actual_value = actual
                    .get(key)
                    .unwrap_or_else(|| panic!("{path}: missing key {key}"));
                assert_subset(expected_value, actual_value, &format!("{path}.{key}"));
            }
        }
        _ => assert_eq!(expected, actual, "{path}"),
    }
}

fn assert_error_code(name: &str, expected: &str, error: Error) {
    let message = error.to_string();
    let matches = match expected {
        "json_unknown_field" => {
            matches!(error, Error::Json(_)) && message.contains("unknown field")
        }
        "json_duplicate_field" => {
            matches!(error, Error::Json(_)) && message.contains("duplicate field")
        }
        "json_integer_type" => matches!(error, Error::Json(_)),
        "raw_size_limit" => matches!(error, Error::Limit(_)) && message.contains("input exceeds"),
        "invalid_identifier" => {
            matches!(error, Error::Invalid(_)) && message.contains("unsupported characters")
        }
        "unsupported_version" => {
            matches!(error, Error::Invalid(_)) && message.contains("unsupported evidence")
        }
        "unsupported_check_kind" => {
            matches!(error, Error::Invalid(_))
                && message.contains("unsupported portable evidence check kind")
        }
        "invalid_terminal_combination" => {
            matches!(error, Error::Invalid(_)) && message.contains("terminal state")
        }
        "duplicate_semantic_entry" => {
            matches!(error, Error::Invalid(_)) && message.contains("duplicate")
        }
        "supplied_artifact_size_limit" => {
            matches!(error, Error::Limit(_)) && message.contains("supplied artifact bytes exceed")
        }
        "clock_limit" => {
            matches!(error, Error::Limit(_)) && message.contains("evaluation timestamp")
        }
        "duplicate_key_alias" => {
            matches!(error, Error::Invalid(_))
                && (message.contains("duplicate issuer key ID")
                    || message.contains("duplicate issuer public key material"))
        }
        "key_id_mismatch" => {
            matches!(error, Error::Invalid(_)) && message.contains("key ID does not match")
        }
        "empty_known_control" => {
            matches!(error, Error::Invalid(_)) && message.contains("known common control")
        }
        "invalid_time_window" => {
            matches!(error, Error::Invalid(_)) && message.contains("not-after timestamp")
        }
        "key_count_limit" => {
            matches!(error, Error::Limit(_)) && message.contains("issuer keys count exceeds 8")
        }
        other => panic!("{name}: unhandled normative error code {other}"),
    };
    assert!(matches, "{name}: expected {expected}, got {message}");
}

#[test]
fn root_and_portable_protocol_copies_are_byte_exact() {
    assert_eq!(PROFILE, PORTABLE_PROFILE, "profile.md differs");
    assert_eq!(VECTORS_BYTES, PORTABLE_VECTORS, "vectors.json differs");
}

#[test]
fn canonical_registry_vectors_match_the_production_registry_api() {
    let vectors = vectors();
    let cases = vectors["registries"].as_array().expect("registries");
    assert_eq!(cases.len(), 10, "normative registry vector count changed");
    for case in cases {
        let name = case["name"].as_str().expect("registry name");
        let parsed = IssuerRegistrySnapshot::from_slice(
            case["input_text"].as_str().expect("input_text").as_bytes(),
        )
        .unwrap_or_else(|error| panic!("{name}: {error}"));
        assert_eq!(
            parsed.canonical_bytes().expect("canonical registry"),
            case["canonical_text"]
                .as_str()
                .expect("canonical_text")
                .as_bytes(),
            "{name}: canonical registry"
        );
        assert_eq!(
            parsed.digest().expect("registry digest"),
            case["canonical_sha256"].as_str().expect("canonical_sha256"),
            "{name}: registry digest"
        );
    }
}

#[test]
fn signed_fixtures_match_production_canonicalization_hashes_and_strict_verification() {
    let vectors = vectors();
    let cases = vectors["signed_fixtures"]
        .as_array()
        .expect("signed fixtures");
    assert_eq!(cases.len(), 8, "normative signed fixture count changed");
    for case in cases {
        let name = case["name"].as_str().expect("fixture name");
        let input = case["envelope_input_text"]
            .as_str()
            .expect("envelope_input_text");
        let envelope = EvidenceEnvelope::from_slice(input.as_bytes())
            .unwrap_or_else(|error| panic!("{name}: {error}"));
        let canonical_payload = envelope
            .payload
            .canonical_bytes()
            .expect("canonical payload");
        let canonical_envelope = envelope.canonical_bytes().expect("canonical envelope");
        assert_eq!(
            canonical_payload,
            case["canonical_payload_text"]
                .as_str()
                .expect("canonical_payload_text")
                .as_bytes(),
            "{name}: canonical payload"
        );
        assert_eq!(
            sha256_hex(&canonical_payload),
            case["canonical_payload_sha256"]
                .as_str()
                .expect("canonical_payload_sha256"),
            "{name}: canonical payload digest"
        );
        assert_eq!(
            canonical_envelope,
            case["canonical_envelope_text"]
                .as_str()
                .expect("canonical_envelope_text")
                .as_bytes(),
            "{name}: canonical envelope"
        );
        assert_eq!(
            sha256_hex(&canonical_envelope),
            case["canonical_envelope_sha256"]
                .as_str()
                .expect("canonical_envelope_sha256"),
            "{name}: canonical envelope digest"
        );
        assert_eq!(
            envelope.signature_hex,
            case["signature_hex"].as_str().expect("signature_hex"),
            "{name}: signature bytes"
        );

        let inputs = case.get("verification_inputs");
        let owned = supplied(inputs.and_then(|value| value.get("supplied_artifacts")));
        let artifacts = artifact_inputs(&owned);
        let report = verify_evidence(
            input.as_bytes(),
            &registry(&vectors, case["registry"].as_str().expect("registry name")),
            clock(inputs.and_then(|value| value.get("clock"))),
            &artifacts,
        )
        .unwrap_or_else(|error| panic!("{name}: {error}"));
        assert_subset(&case["expected_report"], &report_projection(&report), name);
    }
}

#[test]
fn report_cases_match_production_status_dimensions() {
    let vectors = vectors();
    let cases = vectors["report_cases"].as_array().expect("report cases");
    assert_eq!(cases.len(), 20, "normative report case count changed");
    for case in cases {
        let name = case["name"].as_str().expect("report case name");
        let envelope = case
            .get("input_raw_text")
            .and_then(Value::as_str)
            .map(|text| text.as_bytes().to_vec())
            .unwrap_or_else(|| {
                fixture_bytes(
                    &vectors,
                    case["signed_fixture"]
                        .as_str()
                        .expect("signed fixture name"),
                )
            });
        let owned = supplied(case.get("supplied_artifacts"));
        let artifacts = artifact_inputs(&owned);
        let report = verify_evidence(
            &envelope,
            &registry(&vectors, case["registry"].as_str().expect("registry name")),
            clock(case.get("clock")),
            &artifacts,
        )
        .unwrap_or_else(|error| panic!("{name}: {error}"));
        let actual = report_projection(&report);
        if let Some(expected) = case.get("expected").filter(|value| !value.is_null()) {
            assert_subset(expected, &actual, name);
        }
        if let Some(expected) = case
            .get("expected_artifact")
            .filter(|value| !value.is_null())
        {
            let artifact_id = expected["artifact_id"].as_str().expect("artifact ID");
            let actual = actual["artifacts"]
                .as_array()
                .expect("artifact projections")
                .iter()
                .find(|artifact| artifact["artifact_id"].as_str() == Some(artifact_id))
                .unwrap_or_else(|| panic!("{name}: missing artifact {artifact_id}"));
            assert_subset(expected, actual, name);
        }
    }
}

#[test]
fn structural_negative_vectors_are_rejected_by_production_entry_points() {
    let vectors = vectors();
    let cases = vectors["structural_negative_cases"]
        .as_array()
        .expect("structural negative cases");
    assert_eq!(
        cases.len(),
        13,
        "normative structural negative count changed"
    );
    for case in cases {
        let name = case["name"].as_str().expect("negative case name");
        let error = if let Some(text) = case.get("input_raw_text").and_then(Value::as_str) {
            EvidenceEnvelope::from_slice(text.as_bytes()).expect_err(name)
        } else if name == "raw_envelope_cap" {
            EvidenceEnvelope::from_slice(&vec![b' '; MAX_EVIDENCE_BYTES + 1]).expect_err(name)
        } else if name == "supplied_artifact_total_cap" {
            let bytes = vec![0; MAX_SUPPLIED_ARTIFACT_BYTES / 2 + 1];
            verify_evidence(
                &fixture_bytes(
                    &vectors,
                    case["signed_fixture"]
                        .as_str()
                        .expect("signed fixture name"),
                ),
                &registry(&vectors, "current"),
                EvaluationClock::Known { unix_ms: 2_000 },
                &[
                    ArtifactInput {
                        artifact_id: "record",
                        supplied: SuppliedArtifact::Full(&bytes),
                    },
                    ArtifactInput {
                        artifact_id: "stdout",
                        supplied: SuppliedArtifact::Full(&bytes),
                    },
                ],
            )
            .expect_err(name)
        } else if name == "known_clock_signed64_cap" {
            verify_evidence(
                &fixture_bytes(
                    &vectors,
                    case["signed_fixture"]
                        .as_str()
                        .expect("signed fixture name"),
                ),
                &registry(&vectors, "current"),
                clock(case.get("clock")),
                &[],
            )
            .expect_err(name)
        } else {
            panic!("unhandled structural negative vector {name}");
        };
        assert_error_code(
            name,
            case["expected_error"].as_str().expect("expected_error"),
            error,
        );
    }
}

#[test]
fn registry_negative_vectors_are_rejected_by_production_parser() {
    let vectors = vectors();
    let cases = vectors["registry_validation_cases"]
        .as_array()
        .expect("registry validation cases");
    assert_eq!(cases.len(), 9, "normative registry negative count changed");
    for case in cases {
        let name = case["name"].as_str().expect("registry negative name");
        let error = case
            .get("input_raw_text")
            .and_then(Value::as_str)
            .map(|text| IssuerRegistrySnapshot::from_slice(text.as_bytes()).expect_err(name))
            .unwrap_or_else(|| {
                IssuerRegistrySnapshot::from_slice(&vec![b' '; MAX_ISSUER_REGISTRY_BYTES + 1])
                    .expect_err(name)
            });
        assert_error_code(
            name,
            case["expected_error"].as_str().expect("expected_error"),
            error,
        );
    }
}

#[test]
fn common_control_vector_uses_the_production_registry_utility() {
    let vectors = vectors();
    let cases = vectors["independence_cases"]
        .as_array()
        .expect("independence cases");
    assert_eq!(
        cases.len(),
        1,
        "normative independence vector count changed"
    );
    for case in cases {
        let name = case["name"].as_str().expect("independence case name");
        let registry = registry(&vectors, case["registry"].as_str().expect("registry name"));
        let selected = case["selected_issuer_ids"]
            .as_array()
            .expect("selected issuer IDs")
            .iter()
            .map(|value| value.as_str().expect("issuer ID").to_owned())
            .collect::<Vec<_>>();
        let actual = registry
            .assess_independence(&selected, case["check_kind"].as_str().expect("check kind"))
            .unwrap_or_else(|error| panic!("{name}: {error}"));
        assert_eq!(
            serde_json::to_value(actual).expect("independence projection"),
            case["expected"],
            "{name}"
        );
    }
}
