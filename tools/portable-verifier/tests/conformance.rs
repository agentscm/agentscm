use agentscm_portable_verifier::format::{
    Clock, ErrorCode, IntegrityStatus, MAX_JSON_BYTES, MAX_SUPPLIED_ARTIFACT_BYTES,
    SuppliedArtifact, SuppliedArtifactContent, decode_exact_lower_hex, lower_hex, parse_envelope,
    parse_registry, sha256_hex, signature_message,
};
use agentscm_portable_verifier::verify;
use agentscm_portable_verifier::verify::{assess_independence, verify_detached_strict};
use serde_json::{Value, json};

const VECTORS: &str = include_str!("../protocol/evidence-v1/vectors.json");

fn vectors() -> Value {
    serde_json::from_str(VECTORS).expect("normative vectors must be JSON")
}

fn named<'a>(vectors: &'a Value, section: &str, name: &str) -> &'a Value {
    vectors[section]
        .as_array()
        .expect("vector section must be an array")
        .iter()
        .find(|entry| entry["name"] == name)
        .unwrap_or_else(|| panic!("missing {section} vector {name}"))
}

fn registry_bytes(vectors: &Value, name: &str) -> Vec<u8> {
    named(vectors, "registries", name)["input_text"]
        .as_str()
        .expect("registry input_text")
        .as_bytes()
        .to_vec()
}

fn fixture_bytes(vectors: &Value, name: &str) -> Vec<u8> {
    named(vectors, "signed_fixtures", name)["envelope_input_text"]
        .as_str()
        .expect("fixture envelope_input_text")
        .as_bytes()
        .to_vec()
}

fn clock(value: Option<&Value>) -> Clock {
    match value {
        Some(value) if !value.is_null() => {
            serde_json::from_value(value.clone()).expect("vector clock must match the profile")
        }
        _ => Clock::Known { unix_ms: 2_000 },
    }
}

fn supplied(value: Option<&Value>) -> Vec<SuppliedArtifact> {
    let Some(entries) = value.and_then(Value::as_array) else {
        return Vec::new();
    };
    entries
        .iter()
        .map(|entry| {
            let artifact_id = entry["artifact_id"]
                .as_str()
                .expect("artifact ID")
                .to_owned();
            let content = match entry["kind"].as_str().expect("artifact kind") {
                "full" => SuppliedArtifactContent::Full(hex_bytes(&entry["bytes_hex"])),
                "partial" => SuppliedArtifactContent::Partial(hex_bytes(&entry["bytes_hex"])),
                "redacted" => SuppliedArtifactContent::Redacted,
                "unavailable" => SuppliedArtifactContent::Unavailable,
                other => panic!("unexpected artifact kind {other}"),
            };
            SuppliedArtifact {
                artifact_id,
                content,
            }
        })
        .collect()
}

fn hex_bytes(value: &Value) -> Vec<u8> {
    let text = value.as_str().expect("hex string");
    assert_eq!(text.len() % 2, 0);
    text.as_bytes()
        .chunks_exact(2)
        .map(|pair| {
            let digit = |byte: u8| match byte {
                b'0'..=b'9' => byte - b'0',
                b'a'..=b'f' => byte - b'a' + 10,
                _ => panic!("non-lowercase hex"),
            };
            (digit(pair[0]) << 4) | digit(pair[1])
        })
        .collect()
}

fn error_name(code: ErrorCode) -> String {
    serde_json::to_value(code)
        .expect("error code serialization")
        .as_str()
        .expect("error code string")
        .to_owned()
}

fn report_projection(report: &agentscm_portable_verifier::VerificationReport) -> Value {
    let mut value = serde_json::to_value(report).expect("report serialization");
    let object = value.as_object_mut().expect("report object");
    object.insert(
        "verified_receipt_present".to_owned(),
        Value::Bool(report.verified_receipt().is_some()),
    );
    if let Some(receipt) = report.verified_receipt() {
        let canonical_payload =
            serde_json::to_vec(receipt.canonical_payload()).expect("receipt payload serialization");
        assert_eq!(
            sha256_hex(&canonical_payload),
            receipt.canonical_payload_sha256(),
            "receipt digest must describe its bounded canonical payload"
        );
        object.insert(
            "verified_receipt".to_owned(),
            json!({
                "canonical_payload_sha256": receipt.canonical_payload_sha256(),
                "envelope_sha256": receipt.envelope_sha256(),
            }),
        );
    }
    value
}

fn assert_subset(expected: &Value, actual: &Value, path: &str) {
    match expected {
        Value::Object(expected_object) => {
            let actual_object = actual
                .as_object()
                .unwrap_or_else(|| panic!("{path}: expected object, got {actual}"));
            for (key, expected_value) in expected_object {
                let actual_value = actual_object
                    .get(key)
                    .unwrap_or_else(|| panic!("{path}: missing key {key}"));
                assert_subset(expected_value, actual_value, &format!("{path}.{key}"));
            }
        }
        _ => assert_eq!(expected, actual, "{path}"),
    }
}

#[test]
fn canonical_registry_vectors() {
    let vectors = vectors();
    for case in vectors["registries"].as_array().expect("registries") {
        let parsed = parse_registry(case["input_text"].as_str().unwrap().as_bytes())
            .unwrap_or_else(|error| panic!("{}: {error:?}", case["name"]));
        assert_eq!(
            String::from_utf8(parsed.canonical_registry().to_vec()).unwrap(),
            case["canonical_text"].as_str().unwrap(),
            "{} canonical registry",
            case["name"]
        );
        assert_eq!(
            parsed.canonical_sha256(),
            case["canonical_sha256"].as_str().unwrap(),
            "{} registry digest",
            case["name"]
        );
    }
}

#[test]
fn signed_fixture_canonical_bytes_messages_and_reports() {
    let vectors = vectors();
    for case in vectors["signed_fixtures"]
        .as_array()
        .expect("signed fixtures")
    {
        let name = case["name"].as_str().unwrap();
        let parsed = parse_envelope(case["envelope_input_text"].as_str().unwrap().as_bytes())
            .unwrap_or_else(|error| panic!("{name}: {error:?}"));
        assert_eq!(
            String::from_utf8(parsed.canonical_payload().to_vec()).unwrap(),
            case["canonical_payload_text"].as_str().unwrap(),
            "{name} canonical payload"
        );
        assert_eq!(
            sha256_hex(parsed.canonical_payload()),
            case["canonical_payload_sha256"].as_str().unwrap(),
            "{name} payload digest"
        );
        assert_eq!(
            lower_hex(&signature_message(parsed.canonical_payload())),
            case["signature_message_hex"].as_str().unwrap(),
            "{name} signature message"
        );
        assert_eq!(
            String::from_utf8(parsed.canonical_envelope().to_vec()).unwrap(),
            case["canonical_envelope_text"].as_str().unwrap(),
            "{name} canonical envelope"
        );
        assert_eq!(
            sha256_hex(parsed.canonical_envelope()),
            case["canonical_envelope_sha256"].as_str().unwrap(),
            "{name} envelope digest"
        );

        let inputs = case.get("verification_inputs");
        let report = verify(
            case["envelope_input_text"].as_str().unwrap().as_bytes(),
            &registry_bytes(&vectors, case["registry"].as_str().unwrap()),
            clock(inputs.and_then(|value| value.get("clock"))),
            &supplied(inputs.and_then(|value| value.get("supplied_artifacts"))),
        )
        .unwrap_or_else(|error| panic!("{name}: {error:?}"));
        assert_subset(&case["expected_report"], &report_projection(&report), name);
    }
}

#[test]
fn report_cases_match_independent_dimensions() {
    let vectors = vectors();
    for case in vectors["report_cases"].as_array().expect("report cases") {
        let name = case["name"].as_str().unwrap();
        let envelope = case
            .get("input_raw_text")
            .and_then(Value::as_str)
            .map(|value| value.as_bytes().to_vec())
            .unwrap_or_else(|| fixture_bytes(&vectors, case["signed_fixture"].as_str().unwrap()));
        let report = verify(
            &envelope,
            &registry_bytes(&vectors, case["registry"].as_str().unwrap()),
            clock(case.get("clock")),
            &supplied(case.get("supplied_artifacts")),
        )
        .unwrap_or_else(|error| panic!("{name}: {error:?}"));
        if let Some(expected) = case.get("expected").filter(|value| !value.is_null()) {
            assert_subset(expected, &report_projection(&report), name);
        }
        if let Some(expected) = case
            .get("expected_artifact")
            .filter(|value| !value.is_null())
        {
            let artifact_id = expected["artifact_id"].as_str().unwrap();
            let actual = report
                .artifacts()
                .iter()
                .find(|artifact| artifact.artifact_id == artifact_id)
                .expect("expected artifact assessment");
            assert_subset(expected, &serde_json::to_value(actual).unwrap(), name);
        }
    }
}

#[test]
fn envelope_structural_negatives_are_rejected_before_reports() {
    let vectors = vectors();
    for case in vectors["structural_negative_cases"]
        .as_array()
        .expect("structural negative cases")
    {
        let name = case["name"].as_str().unwrap();
        let expected = case["expected_error"].as_str().unwrap();
        let error = if let Some(text) = case.get("input_raw_text").and_then(Value::as_str) {
            parse_envelope(text.as_bytes()).expect_err(name)
        } else if name == "raw_envelope_cap" {
            parse_envelope(&vec![b' '; MAX_JSON_BYTES + 1]).expect_err(name)
        } else if name == "supplied_artifact_total_cap" {
            verify(
                &fixture_bytes(&vectors, case["signed_fixture"].as_str().unwrap()),
                &registry_bytes(&vectors, "current"),
                Clock::Known { unix_ms: 2_000 },
                &[SuppliedArtifact {
                    artifact_id: "record".to_owned(),
                    content: SuppliedArtifactContent::Full(vec![
                        0;
                        MAX_SUPPLIED_ARTIFACT_BYTES + 1
                    ]),
                }],
            )
            .expect_err(name)
        } else if name == "known_clock_signed64_cap" {
            verify(
                &fixture_bytes(&vectors, case["signed_fixture"].as_str().unwrap()),
                &registry_bytes(&vectors, "current"),
                clock(case.get("clock")),
                &[],
            )
            .expect_err(name)
        } else {
            panic!("unhandled structural negative {name}");
        };
        assert_eq!(error_name(error.code), expected, "{name}");
    }
}

#[test]
fn registry_validation_negatives_are_rejected() {
    let vectors = vectors();
    for case in vectors["registry_validation_cases"]
        .as_array()
        .expect("registry validation cases")
    {
        let name = case["name"].as_str().unwrap();
        let error = if let Some(text) = case.get("input_raw_text").and_then(Value::as_str) {
            parse_registry(text.as_bytes()).expect_err(name)
        } else {
            parse_registry(&vec![b' '; MAX_JSON_BYTES + 1]).expect_err(name)
        };
        assert_eq!(
            error_name(error.code),
            case["expected_error"].as_str().unwrap(),
            "{name}"
        );
    }
}

#[test]
fn empty_internally_tagged_states_reject_unknown_and_duplicate_members() {
    let vectors = vectors();
    let envelope = String::from_utf8(fixture_bytes(&vectors, "positive_basic_fixture")).unwrap();
    for state in ["unknown", "declared"] {
        let base = envelope.replacen(
            r#"{"state":"unknown"}"#,
            &format!(r#"{{"state":"{state}"}}"#),
            1,
        );
        for (replacement, expected) in [
            (
                format!(r#"{{"state":"{state}","unexpected":true}}"#),
                ErrorCode::JsonUnknownField,
            ),
            (
                format!(r#"{{"state":"{state}","state":"{state}"}}"#),
                ErrorCode::JsonDuplicateField,
            ),
        ] {
            let input = base.replacen(&format!(r#"{{"state":"{state}"}}"#), &replacement, 1);
            assert_eq!(parse_envelope(input.as_bytes()).unwrap_err().code, expected);
        }
    }

    let registry = String::from_utf8(registry_bytes(&vectors, "independence")).unwrap();
    for (replacement, expected) in [
        (
            r#"{"state":"unknown","unexpected":true}"#,
            ErrorCode::JsonUnknownField,
        ),
        (
            r#"{"state":"unknown","state":"unknown"}"#,
            ErrorCode::JsonDuplicateField,
        ),
    ] {
        let input = registry.replacen(r#"{"state":"unknown"}"#, replacement, 1);
        assert_eq!(parse_registry(input.as_bytes()).unwrap_err().code, expected);
    }

    for input in [
        r#"{"kind":"uncertain","unexpected":true}"#,
        r#"{"kind":"uncertain","kind":"uncertain"}"#,
    ] {
        assert!(serde_json::from_str::<Clock>(input).is_err());
    }
}

#[test]
fn common_control_independence_uses_full_transitive_graph() {
    let vectors = vectors();
    for case in vectors["independence_cases"]
        .as_array()
        .expect("independence cases")
    {
        let registry = parse_registry(&registry_bytes(
            &vectors,
            case["registry"].as_str().unwrap(),
        ))
        .unwrap();
        let selected: Vec<String> = case["selected_issuer_ids"]
            .as_array()
            .unwrap()
            .iter()
            .map(|value| value.as_str().unwrap().to_owned())
            .collect();
        let actual =
            assess_independence(&registry, &selected, case["check_kind"].as_str().unwrap())
                .unwrap();
        assert_eq!(
            serde_json::to_value(actual).unwrap(),
            case["expected"],
            "{}",
            case["name"]
        );
    }
}

#[test]
fn primitive_self_check_and_public_key_classifications() {
    let vectors = vectors();
    let primitive = &vectors["primitive_self_check"];
    assert_eq!(
        verify_detached_strict(
            primitive["public_key_hex"].as_str().unwrap(),
            primitive["signature_hex"].as_str().unwrap(),
            &hex_bytes(&primitive["message_hex"]),
        ),
        IntegrityStatus::Verified
    );
    let mut changed = hex_bytes(&primitive["message_hex"]);
    changed[0] ^= 1;
    assert_eq!(
        verify_detached_strict(
            primitive["public_key_hex"].as_str().unwrap(),
            primitive["signature_hex"].as_str().unwrap(),
            &changed,
        ),
        IntegrityStatus::InvalidSignature
    );

    let invalid = &vectors["invalid_public_key_math"];
    assert!(
        invalid["rust_primitive_classification_pending"]
            .as_bool()
            .unwrap()
    );
    assert_eq!(
        verify_detached_strict(
            invalid["encoded_public_key_hex"].as_str().unwrap(),
            &"00".repeat(64),
            b"classification only",
        ),
        IntegrityStatus::InvalidPublicKey
    );

    assert!(decode_exact_lower_hex::<32>("AA").is_err());
}
