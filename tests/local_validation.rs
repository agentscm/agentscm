//! Synthetic keys, hashes and Git object ids only.

use agentscm::evidence::{EvaluationClock, EvidencePayload, sign_evidence};
use agentscm::local_validation::*;
use agentscm::policy::issuers::{
    CommonControl, IssuerKeyRecord, IssuerRecord, IssuerRegistrySnapshot, issuer_key_id,
};
use ed25519_dalek::SigningKey;

const COMMIT: &str = "1111111111111111111111111111111111111111";
const TREE: &str = "2222222222222222222222222222222222222222";
const BASE: &str = "3333333333333333333333333333333333333333";
const ISSUED: u64 = 1_000_000;
const NOW: u64 = ISSUED + 60_000;

fn hex(byte: u8) -> String {
    format!("{byte:02x}").repeat(32)
}

fn signing_key() -> SigningKey {
    SigningKey::from_bytes(&[7; 32])
}

fn registry() -> IssuerRegistrySnapshot {
    let public_key_hex: String = signing_key()
        .verifying_key()
        .as_bytes()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    IssuerRegistrySnapshot {
        version: 1,
        repository: "repo-1".into(),
        epoch: 1,
        issuers: vec![IssuerRecord {
            issuer_id: "maintainer".into(),
            trust_domain: "local".into(),
            common_control: CommonControl::Unknown {},
            allowed_check_kinds: vec![CHECK_KIND.into()],
            keys: vec![IssuerKeyRecord {
                key_id: issuer_key_id(&public_key_hex).unwrap(),
                public_key_hex,
                not_before_ms: 0,
                not_after_ms: 100_000_000,
                revoked_at_ms: None,
            }],
        }],
    }
}

fn record() -> ValidationRecord {
    ValidationRecord {
        subject: GitValidationSubject {
            repository: "repo-1".into(),
            commit: COMMIT.into(),
            tree: TREE.into(),
        },
        context: GitValidationContext {
            base_ref: "refs/heads/main".into(),
            merge_base: BASE.into(),
        },
        base_tip: BASE.into(),
        recipe_sha256: hex(1),
        results: vec![CommandResult {
            id: "test".into(),
            argv: vec!["cargo".into(), "test".into()],
            exit_code: Some(0),
            stop: StopReason::Exited,
            duration_ms: 5,
            stdout_sha256: hex(4),
            stderr_sha256: hex(5),
            truncated: false,
        }],
        services: Services {
            postgres: true,
            s3: true,
        },
        platform: "linux-x86_64".into(),
        toolchain: vec!["rustc 1.0.0".into()],
        tree_unchanged_after: true,
    }
}

fn policy() -> ValidationPolicy {
    ValidationPolicy {
        repository: "repo-1".into(),
        recipe_sha256: hex(1),
        required_services: Services {
            postgres: true,
            s3: true,
        },
        max_age_seconds: 3600,
        epoch: 3,
    }
}

type Tamper = Box<dyn FnOnce(&mut EvidencePayload)>;

struct Case {
    record: ValidationRecord,
    registry: IssuerRegistrySnapshot,
    policy: ValidationPolicy,
    /// Policy used at verification time when it differs from the signed one.
    assess_policy: Option<ValidationPolicy>,
    observed: [String; 5],
    clock: EvaluationClock,
    tamper_payload: Option<Tamper>,
    tamper_record: bool,
    tamper_envelope: bool,
}

impl Case {
    fn new() -> Self {
        Self {
            record: record(),
            registry: registry(),
            policy: policy(),
            assess_policy: None,
            observed: ["repo-1", COMMIT, TREE, "refs/heads/main", BASE].map(String::from),
            clock: EvaluationClock::Known { unix_ms: NOW },
            tamper_payload: None,
            tamper_record: false,
            tamper_envelope: false,
        }
    }

    fn run(self) -> Verdict {
        let reg = registry();
        let key_id = reg.issuers[0].keys[0].key_id.clone();
        let issuance = Issuance {
            issuer_id: "maintainer",
            key_id: &key_id,
            attempt_id: "attempt-1",
            run_generation: 1,
            issued_at_ms: ISSUED,
            expires_at_ms: ISSUED + 86_400_000,
            issuer_registry_sha256: &self.registry.digest().unwrap(),
            cargo_lock_sha256: &hex(8),
            toolchain_file_sha256: &hex(9),
        };
        let (mut payload, mut record_bytes) =
            evidence_payload(&self.record, &self.policy, &issuance).unwrap();
        if let Some(tamper) = self.tamper_payload {
            tamper(&mut payload);
        }
        let envelope = sign_evidence(payload, &signing_key()).unwrap();
        let mut envelope_bytes = serde_json::to_vec(&envelope).unwrap();
        if self.tamper_envelope {
            let at = String::from_utf8_lossy(&envelope_bytes)
                .find("\"signature_hex\":\"")
                .unwrap()
                + 17;
            envelope_bytes[at] = if envelope_bytes[at] == b'0' {
                b'1'
            } else {
                b'0'
            };
        }
        if self.tamper_record {
            record_bytes.push(b' ');
        }
        let [repository, commit, tree, base_ref, merge_base] = &self.observed;
        assess(
            &envelope_bytes,
            &record_bytes,
            &self.registry,
            self.assess_policy.as_ref().unwrap_or(&self.policy),
            Observed {
                repository,
                commit,
                tree,
                base_ref,
                merge_base,
            },
            self.clock,
        )
    }
}

fn expect(verdict: Verdict, state: VerdictState, reason: &str) {
    assert_eq!(
        (verdict.state, verdict.reason),
        (state, reason),
        "{verdict:?}"
    );
}

fn fails(case: Case, reason: &str) {
    expect(case.run(), VerdictState::Failure, reason);
}

#[test]
fn issue_time_in_the_future_fails_without_underflow() {
    let mut case = Case::new();
    case.clock = EvaluationClock::Known {
        unix_ms: ISSUED - 1,
    };
    assert_eq!(case.run().state, VerdictState::Failure);
}

#[test]
fn recipe_and_policy_decode_strictly() {
    let recipe = br#"{"recipe_id":"r","commands":[{"id":"a","argv":["x"],"timeout_secs":1}]}"#;
    assert!(ValidationRecipe::from_slice(recipe).is_ok());
    assert!(ValidationRecipe::from_slice(br#"{"recipe_id":"r","commands":[],"x":1}"#).is_err());
    assert!(ValidationRecipe::from_slice(br#"{"recipe_id":"r","commands":[]}"#).is_err());
    let mut value = serde_json::to_value(policy()).unwrap();
    assert!(ValidationPolicy::from_slice(&serde_json::to_vec(&value).unwrap()).is_ok());
    value["extra"] = true.into();
    assert!(ValidationPolicy::from_slice(&serde_json::to_vec(&value).unwrap()).is_err());
    assert!(ValidationPolicy::from_slice(&vec![b' '; MAX_RECORD_BYTES + 1]).is_err());
}

#[test]
fn valid_attestation_succeeds() {
    expect(Case::new().run(), VerdictState::Success, "validated");
}

#[test]
fn strict_decode_rejects_unknown_fields_and_oversize() {
    let mut value = serde_json::to_value(record()).unwrap();
    value["extra"] = true.into();
    assert!(ValidationRecord::from_slice(&serde_json::to_vec(&value).unwrap()).is_err());
    assert!(ValidationRecord::from_slice(&vec![b' '; MAX_RECORD_BYTES + 1]).is_err());
    let mut long = record();
    long.platform = "x".repeat(5000);
    assert!(long.validate().is_err());
    let mut uppercase = record();
    uppercase.subject.commit = COMMIT.to_uppercase().replace('1', "A");
    assert!(uppercase.validate().is_err());
}

#[test]
fn tampering_fails() {
    let mut c = Case::new();
    c.tamper_envelope = true;
    fails(c, "signature not verified");
    let mut c = Case::new();
    c.tamper_record = true;
    fails(c, "record not committed");
}

#[test]
fn record_must_match_signed_payload() {
    let mut c = Case::new();
    c.tamper_payload = Some(Box::new(|p| p.subject.revision = hex(0xaa)));
    fails(c, "record does not match payload");
    let mut c = Case::new();
    c.tamper_payload = Some(Box::new(|p| p.subject.context_digest = hex(0xbb)));
    fails(c, "record does not match payload");
}

#[test]
fn issuer_and_time_failures() {
    let mut c = Case::new();
    c.registry.issuers[0].keys[0].revoked_at_ms = Some(NOW - 1);
    fails(c, "issuer not trusted");
    let mut c = Case::new();
    c.registry.issuers[0].keys.clear();
    fails(c, "signature not verified");
    let mut c = Case::new();
    c.tamper_payload = Some(Box::new(|p| p.issuer_registry_sha256 = hex(0xcc)));
    fails(c, "issuer registry stale");
    let mut c = Case::new();
    c.clock = EvaluationClock::Known {
        unix_ms: ISSUED + 86_400_000,
    };
    fails(c, "attestation not fresh");
    let mut c = Case::new();
    c.clock = EvaluationClock::Known {
        unix_ms: ISSUED + 7_200_000,
    };
    fails(c, "attestation too old");
    let mut c = Case::new();
    c.clock = EvaluationClock::Uncertain;
    expect(c.run(), VerdictState::Error, "clock uncertain");
}

#[test]
fn unknown_signer_is_not_trusted() {
    let mut c = Case::new();
    c.registry.issuers[0].issuer_id = "someone-else".into();
    fails(c, "signature not verified");
}

#[test]
fn wrong_repository_and_stale_head() {
    let mut c = Case::new();
    c.observed[0] = "repo-2".into();
    fails(c, "wrong repository");
    let mut c = Case::new();
    c.policy.repository = "repo-2".into();
    fails(c, "wrong repository");
    for (index, value) in [
        (1, "4444444444444444444444444444444444444444"),
        (2, "4444444444444444444444444444444444444444"),
        (3, "refs/heads/other"),
        (4, "4444444444444444444444444444444444444444"),
    ] {
        let mut c = Case::new();
        c.observed[index] = value.into();
        fails(c, "stale attestation");
    }
}

#[test]
fn policy_and_recipe_mismatch() {
    let mut c = Case::new();
    c.assess_policy = Some(ValidationPolicy {
        epoch: 4,
        ..policy()
    });
    fails(c, "policy mismatch");
    let mut c = Case::new();
    c.record.recipe_sha256 = hex(2);
    fails(c, "recipe mismatch");
}

#[test]
fn incomplete_or_failed_runs_fail() {
    let mut c = Case::new();
    c.record.results[0].exit_code = Some(1);
    let verdict = c.run();
    expect(verdict.clone(), VerdictState::Failure, "command failed");
    assert!(verdict.detail.contains("test"));
    let mut c = Case::new();
    c.record.results[0].truncated = true;
    fails(c, "output truncated");
    let mut c = Case::new();
    c.record.results[0].exit_code = None;
    c.record.results[0].stop = StopReason::TimedOut;
    fails(c, "command did not exit");
    let mut c = Case::new();
    c.record.services.s3 = false;
    fails(c, "required service missing");
    let mut c = Case::new();
    c.record.tree_unchanged_after = false;
    fails(c, "tree changed");
}

#[test]
fn signed_pass_for_failed_record_is_rejected() {
    let mut c = Case::new();
    c.record.results[0].exit_code = Some(1);
    c.tamper_payload = Some(Box::new(|p| {
        p.outcome = agentscm::evidence::EvidenceOutcome::Pass;
        p.terminal_state = agentscm::evidence::TerminalState::Success;
    }));
    fails(c, "command failed");
}

#[test]
fn producer_marks_missing_service_run_as_not_pass() {
    let mut c = Case::new();
    c.record.services.s3 = false;
    c.policy.required_services.s3 = false;
    // The producer marks the run failed without all services, so the signed
    // outcome is not pass even though the policy tolerates the absence.
    fails(c, "outcome not pass");
}
