use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use agentscm_portable_verifier::format::MAX_JSON_BYTES;
use serde_json::Value;

const VECTORS: &str = include_str!("../protocol/evidence-v1/vectors.json");
const MAX_TEST_SUMMARY_BYTES: usize = 16_384;
const MAX_TEST_ERROR_BYTES: usize = 1_024;

static NEXT_TEMP_ID: AtomicU64 = AtomicU64::new(0);

struct TestDir {
    path: PathBuf,
}

impl TestDir {
    fn new() -> Self {
        let process = std::process::id();
        let epoch_nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("test clock must follow the Unix epoch")
            .as_nanos();
        for _ in 0..100 {
            let sequence = NEXT_TEMP_ID.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "agentscm-portable-verifier-cli-test-{process}-{epoch_nanos}-{sequence}"
            ));
            match fs::create_dir(&path) {
                Ok(()) => return Self { path },
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(error) => panic!("cannot create owned test directory: {error}"),
            }
        }
        panic!("cannot allocate a unique owned test directory")
    }

    fn write(&self, name: &str, bytes: &[u8]) -> PathBuf {
        let path = self.path.join(name);
        fs::write(&path, bytes).expect("test input write must succeed");
        path
    }
}

impl Drop for TestDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

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

fn registry_text<'a>(vectors: &'a Value, name: &str) -> &'a str {
    named(vectors, "registries", name)["input_text"]
        .as_str()
        .expect("registry input_text")
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

fn verify_command(envelope: &Path, registry: &Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_agentscm-portable-verifier"));
    command
        .arg("verify")
        .arg("--envelope")
        .arg(envelope)
        .arg("--registry")
        .arg(registry)
        .arg("--clock-known")
        .arg("2000");
    command
}

fn output_json(output: &Output) -> Value {
    assert!(output.status.success(), "{output:?}");
    assert!(output.stderr.is_empty());
    assert!(output.stdout.len() <= MAX_TEST_SUMMARY_BYTES);
    assert_eq!(
        output.stdout.iter().filter(|byte| **byte == b'\n').count(),
        1
    );
    serde_json::from_slice(&output.stdout).expect("CLI summary must be one JSON value")
}

fn error_json(output: &Output) -> Value {
    assert!(!output.status.success(), "{output:?}");
    assert!(output.stdout.is_empty());
    assert!(output.stderr.len() <= MAX_TEST_ERROR_BYTES);
    assert_eq!(
        output.stderr.iter().filter(|byte| **byte == b'\n').count(),
        1
    );
    serde_json::from_slice(&output.stderr).expect("CLI error must be one JSON value")
}

#[test]
fn positive_fixture_reports_bounded_dimensions_and_artifact_without_source_echo() {
    let vectors = vectors();
    let fixture = named(&vectors, "signed_fixtures", "positive_basic_fixture");
    let expected = &fixture["expected_report"];
    let inputs = &fixture["verification_inputs"]["supplied_artifacts"];
    let record = inputs
        .as_array()
        .unwrap()
        .iter()
        .find(|artifact| artifact["artifact_id"] == "record")
        .expect("record fixture artifact");

    let directory = TestDir::new();
    let envelope = directory.write(
        "envelope.json",
        fixture["envelope_input_text"].as_str().unwrap().as_bytes(),
    );
    let registry = directory.write(
        "registry.json",
        registry_text(&vectors, fixture["registry"].as_str().unwrap()).as_bytes(),
    );
    let artifact = directory.write("terminal-record.json", &hex_bytes(&record["bytes_hex"]));

    let output = verify_command(&envelope, &registry)
        .arg("--artifact-full")
        .arg("record")
        .arg(&artifact)
        .output()
        .expect("portable verifier binary should start");
    let stdout_text = std::str::from_utf8(&output.stdout).expect("summary must be UTF-8");
    let report = output_json(&output);

    assert_eq!(report["integrity"], "verified");
    assert_eq!(report["registry_binding"], "current");
    assert_eq!(report["current_issuer_trust"], "trusted");
    assert_eq!(report["freshness"], "fresh");
    assert_eq!(report["execution_claim"], "fixture");
    assert_eq!(report["verified_receipt_present"], true);
    assert_eq!(
        report["canonical_payload_sha256"],
        expected["verified_receipt"]["canonical_payload_sha256"]
    );
    assert_eq!(report["envelope_sha256"], expected["envelope_sha256"]);
    assert_eq!(
        report["evaluated_registry_sha256"],
        expected["evaluated_registry_sha256"]
    );
    let record_report = report["artifacts"]
        .as_array()
        .unwrap()
        .iter()
        .find(|entry| entry["artifact_id"] == "record")
        .expect("record assessment");
    assert_eq!(record_report["status"], "full");
    assert_eq!(record_report["commitment_matches"], true);

    assert!(!stdout_text.contains("requirement_id"));
    assert!(!stdout_text.contains("signature_hex"));
    assert!(!stdout_text.contains("public_key_hex"));
    assert!(!stdout_text.contains("terminal record"));
    assert!(!stdout_text.contains(fixture["signature_hex"].as_str().unwrap()));
}

#[test]
fn invalid_signature_is_a_successful_descriptive_report() {
    let vectors = vectors();
    let case = named(&vectors, "report_cases", "invalid_signature");
    let directory = TestDir::new();
    let envelope = directory.write(
        "invalid-signature.json",
        case["input_raw_text"].as_str().unwrap().as_bytes(),
    );
    let registry = directory.write(
        "registry.json",
        registry_text(&vectors, case["registry"].as_str().unwrap()).as_bytes(),
    );

    let output = verify_command(&envelope, &registry)
        .output()
        .expect("portable verifier binary should start");
    let report = output_json(&output);

    assert_eq!(report["integrity"], "invalid_signature");
    assert_eq!(report["registry_binding"], "current");
    assert_eq!(report["current_issuer_trust"], "trusted");
    assert_eq!(report["freshness"], "fresh");
    assert_eq!(report["execution_claim"], "fixture");
    assert_eq!(report["verified_receipt_present"], false);
    assert!(
        !report
            .as_object()
            .unwrap()
            .contains_key("canonical_payload_sha256")
    );
}

#[test]
fn malformed_and_unknown_nested_inputs_return_bounded_json_errors() {
    let vectors = vectors();
    let unknown = named(
        &vectors,
        "structural_negative_cases",
        "unknown_nested_field",
    );
    let directory = TestDir::new();
    let registry = directory.write(
        "registry.json",
        registry_text(&vectors, "current").as_bytes(),
    );

    for (name, input, expected_code) in [
        (
            "unknown-nested.json",
            unknown["input_raw_text"].as_str().unwrap().as_bytes(),
            "json_unknown_field",
        ),
        ("malformed.json", &b"{"[..], "json_invalid"),
    ] {
        let envelope = directory.write(name, input);
        let output = verify_command(&envelope, &registry)
            .output()
            .expect("portable verifier binary should start");
        let error = error_json(&output);
        assert_eq!(output.status.code(), Some(1));
        assert_eq!(error["code"], expected_code);
        assert!(
            error["message"]
                .as_str()
                .is_some_and(|message| !message.is_empty())
        );
    }
}

#[test]
fn oversized_json_input_returns_raw_size_error() {
    let vectors = vectors();
    let directory = TestDir::new();
    let envelope = directory.write("oversized.json", &vec![b' '; MAX_JSON_BYTES + 1]);
    let registry = directory.write(
        "registry.json",
        registry_text(&vectors, "current").as_bytes(),
    );

    let output = verify_command(&envelope, &registry)
        .output()
        .expect("portable verifier binary should start");
    let error = error_json(&output);

    assert_eq!(output.status.code(), Some(1));
    assert_eq!(error["code"], "raw_size_limit");
}
