//! Synthetic keys, hashes and Git object ids only.

use agentscm::evidence::{EvaluationClock, EvidencePayload, sign_evidence};
use agentscm::local_validation::comment;
use agentscm::local_validation::*;
use agentscm::policy::issuers::{
    CommonControl, IssuerKeyRecord, IssuerRecord, IssuerRegistrySnapshot, issuer_key_id,
};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use ed25519_dalek::SigningKey;
use sha2::Digest as _;

const COMMIT: &str = "1111111111111111111111111111111111111111";
const TREE: &str = "2222222222222222222222222222222222222222";
const BASE: &str = "3333333333333333333333333333333333333333";
const ISSUED: u64 = 1_000_000;
const NOW: u64 = ISSUED + 60_000;

fn hex(byte: u8) -> String {
    format!("{byte:02x}").repeat(32)
}

fn deps() -> Vec<(String, String)> {
    vec![
        ("Cargo.lock".into(), hex(8)),
        ("rust-toolchain.toml".into(), hex(9)),
    ]
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

fn recipe() -> ValidationRecipe {
    ValidationRecipe {
        recipe_id: "gate".into(),
        commands: vec![RecipeCommand {
            id: "test".into(),
            argv: vec!["cargo".into(), "test".into()],
            timeout_secs: 60,
            test_selection: None,
            paths: None,
        }],
        dependency_files: None,
        toolchain_probes: None,
        constituents: None,
    }
}

fn recipe_hash() -> String {
    recipe().sha256().unwrap()
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
        recipe_sha256: recipe_hash(),
        results: vec![CommandResult {
            id: "test".into(),
            argv: vec!["cargo".into(), "test".into()],
            exit_code: Some(0),
            stop: StopReason::Exited,
            duration_ms: 5,
            stdout_sha256: hex(4),
            stderr_sha256: hex(5),
            truncated: false,
            incomplete: false,
        }],
        not_required: vec![],
        selections: vec![],
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
        recipe_sha256: recipe_hash(),
        required_services: Services {
            postgres: true,
            s3: true,
        },
        max_age_seconds: 3600,
        epoch: 3,
        exempt_paths: None,
        check_id: None,
    }
}

type Tamper = Box<dyn FnOnce(&mut EvidencePayload)>;

struct Case {
    record: ValidationRecord,
    recipe: ValidationRecipe,
    issued_at_ms: u64,
    registry: IssuerRegistrySnapshot,
    policy: ValidationPolicy,
    /// Policy used at verification time when it differs from the signed one.
    assess_policy: Option<ValidationPolicy>,
    observed: [String; 5],
    /// Ids the changed paths require; every recipe command when unset.
    required: Option<Vec<String>>,
    selections: Vec<CommandSelection>,
    clock: EvaluationClock,
    tamper_payload: Option<Tamper>,
    tamper_record: bool,
    tamper_envelope: bool,
}

impl Case {
    fn new() -> Self {
        Self {
            record: record(),
            recipe: recipe(),
            issued_at_ms: ISSUED,
            registry: registry(),
            policy: policy(),
            assess_policy: None,
            observed: ["repo-1", COMMIT, TREE, "refs/heads/main", BASE].map(String::from),
            required: None,
            selections: vec![],
            clock: EvaluationClock::Known { unix_ms: NOW },
            tamper_payload: None,
            tamper_record: false,
            tamper_envelope: false,
        }
    }

    fn run(self) -> Verdict {
        self.execute().0
    }

    /// The comment body this case would be posted as (empty when it cannot render).
    fn body(self) -> String {
        self.execute().1
    }

    fn execute(self) -> (Verdict, String) {
        let reg = registry();
        let key_id = reg.issuers[0].keys[0].key_id.clone();
        let issuance = Issuance {
            issuer_id: "maintainer",
            key_id: &key_id,
            attempt_id: "attempt-1",
            run_generation: 1,
            issued_at_ms: self.issued_at_ms,
            expires_at_ms: ISSUED + 86_400_000,
            issuer_registry_sha256: &self.registry.digest().unwrap(),
            dependencies: &deps(),
        };
        let (mut payload, mut record_bytes) =
            evidence_payload(&self.record, &self.recipe, &self.policy, &issuance).unwrap();
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
        let body = comment::render(&envelope_bytes, &record_bytes).unwrap_or_default();
        let [repository, commit, tree, base_ref, merge_base] = &self.observed;
        let required = self
            .required
            .unwrap_or_else(|| self.recipe.commands.iter().map(|c| c.id.clone()).collect());
        let verdict = assess(
            &envelope_bytes,
            &record_bytes,
            &self.recipe,
            &self.registry,
            self.assess_policy.as_ref().unwrap_or(&self.policy),
            Observed {
                repository,
                commit,
                tree,
                base_ref,
                merge_base,
                required: &required,
                selections: &self.selections,
            },
            self.clock,
        );
        (verdict, body)
    }
}

fn expect(verdict: Verdict, state: VerdictState, reason: &str) {
    assert_eq!(
        (verdict.state, verdict.reason),
        (state, reason),
        "{verdict:?}"
    );
}

fn comment_bytes() -> (Vec<u8>, Vec<u8>) {
    let registry = registry();
    let key_id = registry.issuers[0].keys[0].key_id.clone();
    let issuance = Issuance {
        issuer_id: "maintainer",
        key_id: &key_id,
        attempt_id: "attempt-1",
        run_generation: 1,
        issued_at_ms: ISSUED,
        expires_at_ms: ISSUED + 86_400_000,
        issuer_registry_sha256: &registry.digest().unwrap(),
        dependencies: &deps(),
    };
    let (payload, record) = evidence_payload(&record(), &recipe(), &policy(), &issuance).unwrap();
    let envelope = serde_json::to_vec(&sign_evidence(payload, &signing_key()).unwrap()).unwrap();
    (envelope, record)
}

fn flat_comment(envelope: &[u8], record: &[u8]) -> String {
    format!(
        "<!-- agentscm-local-validation:v1 {} -->\n\nlegacy summary\n\n```agentscm-envelope-v1\n{}\n```\n\n```agentscm-record-v1\n{}\n```\n",
        comment::envelope_sha256(envelope),
        STANDARD.encode(envelope),
        STANDARD.encode(record),
    )
}

#[test]
fn rendered_comment_is_stable_collapsed_and_round_trips_exact_bytes() {
    let (envelope, record) = comment_bytes();
    let body = comment::render(&envelope, &record).unwrap();
    assert_eq!(body, comment::render(&envelope, &record).unwrap());
    assert!(body.starts_with("<!-- agentscm-local-validation:v1 "));
    assert!(body.contains("alt=\"AgentSCM icon\" width=\"28\""));
    assert!(body.contains("**AgentSCM · Local validation**"));
    assert!(body.contains("Source: <code>repo&#x2d;1</code> at <code>111111111111</code>"));
    assert!(body.contains("the verifier checks the signature and source bindings"));
    assert!(body.contains("Coverage: 1/1 checks reported; 5 omissions."));
    assert!(body.contains("<summary>Coverage and environment</summary>"));
    for omission in [
        "<code>ignored tests</code>",
        "<code>windows</code>",
        "<code>container smoke</code>",
        "<code>hub end&#x2d;to&#x2d;end</code>",
        "<code>live execution</code>",
    ] {
        assert!(body.contains(&format!("- {omission}\n")));
    }
    let visible = body.split("<details>").next().unwrap();
    assert_eq!(visible.matches("pass").count(), 1);
    assert!(!visible.contains(COMMIT));
    assert_eq!(body.matches("<details>").count(), 4);
    assert_eq!(body.matches("</details>").count(), 4);
    assert!(!body.contains("<details open"));

    let sections: Vec<_> = body
        .lines()
        .filter(|line| line.starts_with("<summary>"))
        .collect();
    assert_eq!(
        sections,
        [
            "<summary>Command results</summary>",
            "<summary>Coverage and environment</summary>",
            "<summary>Source details</summary>",
            "<summary>Signed machine-readable evidence</summary>"
        ]
    );

    let signed_details = body
        .find("<summary>Signed machine-readable evidence</summary>")
        .unwrap();
    let envelope_fence = body.find("```agentscm-envelope-v1\n").unwrap();
    let record_fence = body.find("```agentscm-record-v1\n").unwrap();
    let signed_close = body[record_fence..].find("\n</details>").unwrap() + record_fence;
    assert!(signed_details < envelope_fence && envelope_fence < record_fence);
    assert!(record_fence < signed_close);
    assert_eq!(comment::parse(&body).unwrap(), (envelope, record));
}

#[test]
fn rendered_comment_reports_absent_services_as_environment_facts() {
    let (envelope, _) = comment_bytes();
    let mut missing_service_record = record();
    missing_service_record.services.s3 = false;
    let record_bytes = serde_json::to_vec(&missing_service_record).unwrap();
    let body = comment::render(&envelope, &record_bytes).unwrap();

    assert!(body.contains("Services available: PostgreSQL=true, S3=false"));
    assert!(!body.contains("Coverage is incomplete"));
    assert!(!body.contains("Missing services"));
}

#[test]
fn rendered_comment_preserves_reported_failure_and_historical_service_omissions() {
    let (envelope, record) = comment_bytes();
    let mut envelope_json: serde_json::Value = serde_json::from_slice(&envelope).unwrap();
    envelope_json["payload"]["outcome"] = "fail".into();
    envelope_json["payload"]["coverage"]["omissions"] =
        serde_json::json!(["postgres tests", "s3 tests"]);
    let envelope = serde_json::to_vec(&envelope_json).unwrap();
    let body = comment::render(&envelope, &record).unwrap();
    let visible = body.split("<details>").next().unwrap();
    assert!(visible.contains("**Reported outcome: fail**"));
    assert!(visible.contains("2 omissions"));
    assert!(body.contains("- <code>postgres tests</code>\n"));
    assert!(body.contains("- <code>s3 tests</code>\n"));
    assert_eq!(comment::parse(&body).unwrap(), (envelope, record));
}

#[test]
fn parser_accepts_legacy_flat_carrier() {
    let (envelope, record) = comment_bytes();
    assert_eq!(
        comment::parse(&flat_comment(&envelope, &record)).unwrap(),
        (envelope, record)
    );
}

#[test]
fn parser_strictly_rejects_malformed_carriers() {
    let (envelope, record) = comment_bytes();
    let valid = flat_comment(&envelope, &record);

    let duplicate = format!(
        "{valid}\n```agentscm-envelope-v1\n{}\n```\n",
        STANDARD.encode(&envelope)
    );
    assert!(comment::parse(&duplicate).is_err());

    let missing = valid
        .split("```agentscm-record-v1")
        .next()
        .unwrap()
        .to_owned();
    assert!(comment::parse(&missing).is_err());

    let bad_base64 = valid.replacen(&STANDARD.encode(&envelope), "not-base64!", 1);
    assert!(comment::parse(&bad_base64).is_err());

    let mut bad_digest = valid.clone();
    let digest_start = "<!-- agentscm-local-validation:v1 ".len();
    bad_digest.replace_range(digest_start..=digest_start, "0");
    if bad_digest == valid {
        bad_digest.replace_range(digest_start..=digest_start, "1");
    }
    assert!(comment::parse(&bad_digest).is_err());
    assert!(comment::parse(&"x".repeat(comment::MAX_COMMENT_BYTES + 1)).is_err());
}

#[test]
fn renderer_preserves_the_comment_size_bound() {
    let (envelope, _) = comment_bytes();
    let mut large_record = record();
    large_record.toolchain = vec!["x".repeat(2048); 16];
    large_record.results[0].argv = vec!["x".repeat(256); 64];
    let record_bytes = serde_json::to_vec(&large_record).unwrap();
    assert!(record_bytes.len() <= MAX_RECORD_BYTES);
    assert!(matches!(
        comment::render(&envelope, &record_bytes),
        Err(agentscm::Error::Limit(_))
    ));
}

#[test]
fn hostile_display_strings_cannot_escape_collapsed_sections() {
    let (envelope, mut record_bytes) = comment_bytes();
    let mut envelope_json: serde_json::Value = serde_json::from_slice(&envelope).unwrap();
    envelope_json["payload"]["outcome"] = "</details>\n```spoof".into();
    envelope_json["payload"]["coverage"]["omissions"] =
        serde_json::json!(["</details>\n<details open>spoof"]);
    let hostile_envelope = serde_json::to_vec(&envelope_json).unwrap();

    let mut hostile_record: serde_json::Value = serde_json::from_slice(&record_bytes).unwrap();
    hostile_record["context"]["base_ref"] = "refs/heads/_spoof_".into();
    hostile_record["results"][0]["id"] = "test:_spoof_".into();
    hostile_record["platform"] = "</details>\n<details open>spoof".into();
    hostile_record["toolchain"][0] = "```agentscm-record-v1\nspoof".into();
    record_bytes = serde_json::to_vec(&hostile_record).unwrap();

    let body = comment::render(&hostile_envelope, &record_bytes).unwrap();
    assert!(body.contains("**Reported outcome: unknown**"));
    assert_eq!(body.matches("<details>").count(), 4);
    assert_eq!(body.matches("</details>").count(), 4);
    assert!(!body.contains("<details open>"));
    assert!(!body.contains("```spoof"));
    assert!(!body.contains("refs/heads/_spoof_"));
    assert!(!body.contains("test:_spoof_"));
    assert_eq!(
        comment::parse(&body).unwrap(),
        (hostile_envelope, record_bytes)
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
fn service_omissions_follow_policy_and_availability() {
    for required_postgres in [false, true] {
        for required_s3 in [false, true] {
            for available_postgres in [false, true] {
                for available_s3 in [false, true] {
                    let mut c = Case::new();
                    c.policy.required_services = Services {
                        postgres: required_postgres,
                        s3: required_s3,
                    };
                    c.record.services = Services {
                        postgres: available_postgres,
                        s3: available_s3,
                    };
                    let missing_postgres = required_postgres && !available_postgres;
                    let missing_s3 = required_s3 && !available_s3;
                    c.tamper_payload = Some(Box::new(move |p| {
                        let mut expected: Vec<String> = [
                            "ignored tests",
                            "windows",
                            "container smoke",
                            "hub end-to-end",
                            "live execution",
                        ]
                        .map(String::from)
                        .to_vec();
                        if missing_postgres {
                            expected.push("postgres tests".into());
                        }
                        if missing_s3 {
                            expected.push("s3 tests".into());
                        }
                        assert_eq!(p.coverage.omissions, expected);
                        assert_eq!(
                            p.outcome,
                            if missing_postgres || missing_s3 {
                                agentscm::evidence::EvidenceOutcome::Fail
                            } else {
                                agentscm::evidence::EvidenceOutcome::Pass
                            }
                        );
                    }));
                    if missing_postgres || missing_s3 {
                        fails(c, "required service missing");
                    } else {
                        expect(c.run(), VerdictState::Success, "validated");
                    }
                }
            }
        }
    }
}

#[test]
fn historical_optional_service_omissions_still_verify() {
    let mut c = Case::new();
    c.policy.required_services = Services {
        postgres: false,
        s3: false,
    };
    c.record.services = c.policy.required_services;
    c.tamper_payload = Some(Box::new(|p| {
        p.coverage.omissions.push("postgres tests".into());
        p.coverage.omissions.push("s3 tests".into());
    }));
    expect(c.run(), VerdictState::Success, "validated");
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
fn producer_passes_when_policy_does_not_require_the_missing_service() {
    let mut c = Case::new();
    c.record.services.s3 = false;
    c.policy.required_services.s3 = false;
    let (verdict, body) = c.execute();
    assert_eq!(verdict.state, VerdictState::Success);
    assert!(!body.contains("- <code>s3 tests</code>\n"));
}

#[test]
fn missing_required_service_fails_verification() {
    let mut c = Case::new();
    c.record.services.s3 = false;
    c.policy.required_services.s3 = true;
    fails(c, "required service missing");
}

const OLD: &str = "5555555555555555555555555555555555555555";

fn pass() -> Case {
    Case::new()
}

fn fail() -> Case {
    let mut c = Case::new();
    c.record.results[0].exit_code = Some(1);
    c
}

fn old_head(mut c: Case) -> Case {
    c.record.subject.commit = OLD.into();
    c
}

fn select_all(bodies: &[&str], clock: EvaluationClock) -> Verdict {
    select(
        bodies,
        &recipe(),
        &registry(),
        &policy(),
        Observed {
            repository: "repo-1",
            commit: COMMIT,
            tree: TREE,
            base_ref: "refs/heads/main",
            merge_base: BASE,
            required: &["test".to_owned()],
            selections: &[],
        },
        clock,
    )
}

fn known(bodies: &[&str]) -> Verdict {
    select_all(bodies, EvaluationClock::Known { unix_ms: NOW })
}

#[test]
fn applies_to_head_only_for_the_observed_commit() {
    assert!(pass().run().applies_to_head);
    assert!(fail().run().applies_to_head);
    assert!(!old_head(pass()).run().applies_to_head);
    let mut c = Case::new();
    c.observed[0] = "repo-2".into();
    assert!(!c.run().applies_to_head);
    let mut c = Case::new();
    c.tamper_envelope = true;
    assert!(!c.run().applies_to_head);
}

fn at(mut c: Case, offset: u64) -> Case {
    c.issued_at_ms = ISSUED + offset;
    c
}

#[test]
fn latest_signed_issue_time_decides_regardless_of_order() {
    let (old_pass, current_fail) = (old_head(pass()).body(), fail().body());
    expect(
        known(&[&old_pass, &current_fail]),
        VerdictState::Failure,
        "command failed",
    );
    let (fail2, pass1, pass2) = (
        at(fail(), 2).body(),
        at(pass(), 1).body(),
        at(pass(), 2).body(),
    );
    for bodies in [
        [fail2.as_str(), pass1.as_str()],
        [pass1.as_str(), fail2.as_str()],
    ] {
        expect(known(&bodies), VerdictState::Failure, "command failed");
    }
    expect(
        known(&[&pass1, &fail2, &pass1]),
        VerdictState::Failure,
        "command failed",
    );
    let fail1 = at(fail(), 1).body();
    for bodies in [
        [fail1.as_str(), pass2.as_str()],
        [pass2.as_str(), fail1.as_str()],
    ] {
        expect(known(&bodies), VerdictState::Success, "validated");
    }
}

#[test]
fn skipped_commands_are_never_a_pass() {
    let mut c = Case::new();
    c.recipe.commands.push(RecipeCommand {
        id: "lint".into(),
        argv: vec!["cargo".into(), "clippy".into()],
        timeout_secs: 60,
        test_selection: None,
        paths: None,
    });
    c.policy.recipe_sha256 = c.recipe.sha256().unwrap();
    c.record.recipe_sha256 = c.policy.recipe_sha256.clone();
    c.tamper_payload = Some(Box::new(|p| {
        assert!(p.coverage.omissions.contains(&"skipped: lint".to_owned()));
        assert_eq!(p.coverage.expected_count, 2);
        p.outcome = agentscm::evidence::EvidenceOutcome::Pass;
        p.terminal_state = agentscm::evidence::TerminalState::Success;
    }));
    let verdict = c.run();
    expect(verdict.clone(), VerdictState::Failure, "incomplete run");
    assert!(verdict.detail.contains("lint"));
}

#[test]
fn recipe_must_match_policy_and_argv_must_match_recipe() {
    let mut c = Case::new();
    c.recipe.commands[0].timeout_secs = 61;
    expect(c.run(), VerdictState::Error, "recipe does not match policy");
    let mut c = Case::new();
    c.record.results[0].argv = vec!["true".into()];
    fails(c, "incomplete run");
}

#[test]
fn junk_stale_and_forged_bodies_never_override_an_applicable_one() {
    let current_pass = pass().body();
    let mut tampered = Case::new();
    tampered.tamper_record = true;
    let mut forged = fail();
    forged.registry.issuers[0].keys.clear();
    let mut wrong_key = fail();
    wrong_key.registry.issuers[0].issuer_id = "someone-else".into();
    let later = [
        "not an attestation".to_owned(),
        old_head(fail()).body(),
        tampered.body(),
        forged.body(),
        wrong_key.body(),
        current_pass.replace("Local validation", "Local validation: edited"),
    ];
    let mut bodies = vec![current_pass.as_str()];
    bodies.extend(later.iter().map(String::as_str));
    expect(known(&bodies), VerdictState::Success, "validated");
}

#[test]
fn nothing_applicable_is_pending_never_success() {
    for bodies in [vec![], vec!["junk"]] {
        expect(
            known(&bodies),
            VerdictState::Pending,
            "no attestation for this head",
        );
    }
    let (old, junk) = (old_head(pass()).body(), "junk");
    let verdict = known(&[&old, junk]);
    expect(
        verdict.clone(),
        VerdictState::Pending,
        "attestation is for a different head",
    );
    assert_eq!(verdict.outcome, Outcome::StaleHead);
    assert!(!verdict.applies_to_head && verdict.detail.contains("stale attestation"));
}

#[test]
fn uncertain_clock_surfaces_as_error() {
    let body = pass().body();
    expect(
        select_all(&[&body, "junk"], EvaluationClock::Uncertain),
        VerdictState::Error,
        "clock uncertain",
    );
}

/// The repository's own configuration must decode and agree, so drift fails CI.
#[test]
fn repository_configuration_is_consistent() {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join(".github/local-validation");
    let read = |name: &str| std::fs::read(dir.join(name)).unwrap();
    let recipe = ValidationRecipe::from_slice(&read("recipe.json")).unwrap();
    let policy = ValidationPolicy::from_slice(&read("policy.json")).unwrap();
    let registry = IssuerRegistrySnapshot::from_slice(&read("registry.json")).unwrap();
    assert_eq!(policy.recipe_sha256, recipe.sha256().unwrap());
    assert_eq!(policy.repository, registry.repository);
    assert_eq!(policy.epoch, registry.epoch);
}

#[test]
fn recipe_without_new_fields_keeps_its_canonical_bytes() {
    let legacy = r#"{"recipe_id":"gate","commands":[{"id":"test","argv":["cargo","test"],"timeout_secs":60}]}"#;
    assert_eq!(serde_json::to_string(&recipe()).unwrap(), legacy);
    assert_eq!(
        ValidationRecipe::from_slice(legacy.as_bytes()).unwrap(),
        recipe()
    );
    let mut declared = recipe();
    declared.dependency_files = Some(vec!["bun.lock".into()]);
    assert_ne!(declared.sha256().unwrap(), recipe_hash());
}

#[test]
fn recipe_defaults_and_declared_lists() {
    let r = recipe();
    assert_eq!(
        r.dependency_file_list(),
        ["Cargo.lock", "rust-toolchain.toml"]
    );
    let probes = r.toolchain_probe_list();
    assert_eq!(probes.len(), 4);
    assert_eq!(probes[3], ["git", "--version"]);
    let mut r = recipe();
    r.dependency_files = Some(vec!["bun.lock".into(), "package.json".into()]);
    r.toolchain_probes = Some(vec![vec!["bun".into(), "--version".into()]]);
    r.validate().unwrap();
    assert_eq!(r.dependency_file_list(), ["bun.lock", "package.json"]);
    assert_eq!(
        r.toolchain_probe_list(),
        [vec!["bun", "--version"], vec!["git", "--version"]]
    );
}

#[test]
fn invalid_recipe_dependency_and_probe_declarations_are_rejected() {
    let bad_files: Vec<Vec<&str>> = vec![
        vec![],
        vec!["../x"],
        vec!["/abs"],
        vec!["-x"],
        vec!["a//b"],
        vec!["a/./b"],
        vec!["a/"],
        vec!["a\\b"],
        vec!["a b"],
        vec!["x", "x"],
    ];
    for files in bad_files {
        let mut r = recipe();
        r.dependency_files = Some(files.iter().map(|f| (*f).to_owned()).collect());
        assert!(r.validate().is_err(), "{files:?}");
    }
    let mut r = recipe();
    r.dependency_files = Some((0..17).map(|i| format!("f{i}")).collect());
    assert!(r.validate().is_err());
    for probes in [
        vec![],
        vec![vec![]],
        vec![vec!["a".to_owned(), String::new()]],
    ] {
        let mut r = recipe();
        r.toolchain_probes = Some(probes);
        assert!(r.validate().is_err());
    }
    let mut r = recipe();
    r.toolchain_probes = Some(vec![vec!["a".into()]; 16]);
    assert!(r.validate().is_err());
    r.toolchain_probes = Some(vec![vec!["a".into()]; 15]);
    r.validate().unwrap();
    let unknown =
        r#"{"recipe_id":"gate","commands":[{"id":"t","argv":["a"],"timeout_secs":1}],"extra":1}"#;
    assert!(ValidationRecipe::from_slice(unknown.as_bytes()).is_err());
}

#[test]
fn payload_dependencies_follow_the_issuance_order() {
    let registry = registry();
    let key_id = registry.issuers[0].keys[0].key_id.clone();
    let dependencies = vec![
        ("bun.lock".to_owned(), hex(1)),
        ("package.json".to_owned(), hex(2)),
    ];
    let (payload, _) = evidence_payload(
        &record(),
        &recipe(),
        &policy(),
        &Issuance {
            issuer_id: "maintainer",
            key_id: &key_id,
            attempt_id: "attempt-1",
            run_generation: 1,
            issued_at_ms: ISSUED,
            expires_at_ms: ISSUED + 86_400_000,
            issuer_registry_sha256: &registry.digest().unwrap(),
            dependencies: &dependencies,
        },
    )
    .unwrap();
    let identities: Vec<_> = payload.dependencies.iter().map(|d| &*d.identity).collect();
    assert_eq!(identities, ["bun.lock", "package.json"]);
}

#[test]
fn repository_policy_and_recipe_keep_their_canonical_bytes() {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join(".github/local-validation");
    let recipe_bytes = std::fs::read(dir.join("recipe.json")).unwrap();
    let policy_bytes = std::fs::read(dir.join("policy.json")).unwrap();
    let recipe = ValidationRecipe::from_slice(&recipe_bytes).unwrap();
    let policy = ValidationPolicy::from_slice(&policy_bytes).unwrap();
    assert_eq!(
        recipe.sha256().unwrap(),
        "e0d6b6b2dfd0350c7a0b1a745d4901248a9a8441a09f3cc9335f81c4369657cc"
    );
    assert_eq!(policy.recipe_sha256, recipe.sha256().unwrap());
    assert_eq!(
        serde_json::to_vec(&policy).unwrap(),
        policy_bytes.trim_ascii_end()
    );
    let mut declared = policy.clone();
    declared.exempt_paths = Some(vec!["docs/**".into()]);
    assert_ne!(declared.digest().unwrap(), policy.digest().unwrap());
}

#[test]
fn paths_and_exempt_paths_round_trip() {
    let mut r = recipe();
    r.commands[0].paths = Some(vec![
        "src".into(),
        ":(glob)docs/**".into(),
        "a..b".into(),
        ":(glob)a..b/**".into(),
    ]);
    let bytes = serde_json::to_vec(&r).unwrap();
    assert_eq!(ValidationRecipe::from_slice(&bytes).unwrap(), r);
    assert_ne!(r.sha256().unwrap(), recipe_hash());
    let mut p = policy();
    p.exempt_paths = Some(vec![":(exclude)x".into(), ":(top,literal)a b".into()]);
    p.validate().unwrap();
    let bytes = serde_json::to_vec(&p).unwrap();
    assert_eq!(ValidationPolicy::from_slice(&bytes).unwrap(), p);
}

#[test]
fn invalid_pathspecs_are_rejected() {
    let bad = [
        vec![],
        vec!["".into()],
        vec!["-x".into()],
        vec![":!x".into()],
        vec![":^x".into()],
        vec![":/x".into()],
        vec![":(attr:foo)x".into()],
        vec![":(icase)x".into()],
        vec![":(glob,icase)x".into()],
        vec![":(glob".into()],
        vec![":(glob,literal)x".into()],
        vec![":(top)".into()],
        vec![":(literal)".into()],
        vec!["/x".into()],
        vec![":(top,literal)/x".into()],
        vec!["a\0b".into()],
        vec!["../x".into()],
        vec!["a/../x".into()],
        vec![":(top)../x".into()],
        vec!["x".to_owned(); 65],
    ];
    for specs in bad {
        let mut r = recipe();
        r.commands[0].paths = Some(specs.clone());
        assert!(r.validate().is_err(), "{specs:?}");
        let mut p = policy();
        p.exempt_paths = Some(specs.clone());
        assert!(p.validate().is_err(), "{specs:?}");
    }
    let mut p = policy();
    p.exempt_paths = Some(vec![":(glob)docs/**".into(), ":(exclude)x".into()]);
    p.validate().unwrap();
}

#[cfg(unix)]
mod scoped {
    use super::*;
    use agentscm::local_validation::run::{only_exempt, required_commands, trust_paths};
    use std::path::Path;
    use std::process::Command;

    fn git(dir: &Path, args: &[&str]) -> String {
        let out = Command::new("git")
            .current_dir(dir)
            .args(["-c", "user.name=t", "-c", "user.email=t@example.invalid"])
            .args([
                "-c",
                "commit.gpgsign=false",
                "-c",
                "core.hooksPath=/dev/null",
            ])
            .args(args)
            .output()
            .unwrap();
        assert!(out.status.success(), "git {args:?}");
        String::from_utf8(out.stdout).unwrap().trim().to_owned()
    }

    /// A repository with a base commit; `change` edits the tree for the head commit.
    fn repo(change: impl FnOnce(&Path)) -> (tempfile::TempDir, String, String) {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path();
        git(p, &["init", "-q"]);
        for file in [
            "src/a.rs",
            ".github/local-validation/recipe.json",
            "README.md",
        ] {
            std::fs::create_dir_all(p.join(file).parent().unwrap()).unwrap();
            std::fs::write(p.join(file), "one\n").unwrap();
        }
        git(p, &["add", "."]);
        git(p, &["commit", "-qm", "base"]);
        let base = git(p, &["rev-parse", "HEAD"]);
        change(p);
        git(p, &["add", "-A"]);
        git(p, &["commit", "-q", "--allow-empty", "-m", "head"]);
        let head = git(p, &["rev-parse", "HEAD"]);
        (dir, base, head)
    }

    fn write(p: &Path, file: &str) {
        std::fs::create_dir_all(p.join(file).parent().unwrap()).unwrap();
        std::fs::write(p.join(file), "two\n").unwrap();
    }

    fn trust() -> Vec<String> {
        trust_paths(".github/local-validation", DEFAULT_VERIFIER_WORKFLOW)
    }

    fn exempt(dir: &Path, base: &str, head: &str, specs: &[&str]) -> bool {
        let specs: Vec<String> = specs.iter().map(|s| (*s).to_owned()).collect();
        only_exempt(dir, base, head, &specs, &trust()).unwrap()
    }

    #[test]
    fn docs_only_change_is_exempt() {
        let (d, b, h) = repo(|p| write(p, "docs/x.md"));
        assert!(exempt(d.path(), &b, &h, &["docs/**"]));
    }

    #[test]
    fn rename_out_of_an_exempt_tree_is_not_exempt() {
        let (d, b, h) = repo(|p| {
            std::fs::create_dir_all(p.join("docs")).unwrap();
            std::fs::rename(p.join("src/a.rs"), p.join("docs/a.rs")).unwrap();
        });
        assert!(!exempt(d.path(), &b, &h, &["docs/**"]));
    }

    #[test]
    fn empty_diff_is_not_exempt() {
        let (d, b, h) = repo(|_| {});
        assert!(!exempt(d.path(), &b, &h, &["docs/**"]));
    }

    #[test]
    fn trust_input_changes_are_never_exempt() {
        for file in [
            ".github/local-validation/recipe.json",
            DEFAULT_VERIFIER_WORKFLOW,
        ] {
            let (d, b, h) = repo(|p| write(p, file));
            assert!(!exempt(d.path(), &b, &h, &[".github/**"]), "{file}");
        }
    }

    #[test]
    fn mixed_change_is_not_exempt() {
        let (d, b, h) = repo(|p| {
            write(p, "docs/x.md");
            write(p, "src/a.rs");
        });
        assert!(!exempt(d.path(), &b, &h, &["docs/**"]));
    }

    #[test]
    fn combined_git_batch_detects_a_semantic_failure_between_green_heads() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path();
        git(p, &["init", "-q", "-b", "main"]);
        std::fs::write(p.join("left.rs"), "pub const VALUE: u32 = 1;\n").unwrap();
        std::fs::write(p.join("right.rs"), "pub const VALUE: u32 = 1;\n").unwrap();
        std::fs::write(
            p.join("check.rs"),
            "mod left; mod right; #[test] fn capacity() { assert!(left::VALUE + right::VALUE <= 3); }\n",
        ).unwrap();
        git(p, &["add", "."]);
        git(p, &["commit", "-qm", "base"]);
        let base = git(p, &["rev-parse", "HEAD"]);
        let output = tempfile::tempdir().unwrap();
        let check = || {
            let binary = output.path().join("capacity");
            let compiled = Command::new("rustc")
                .current_dir(p)
                .args(["--test", "check.rs", "-o"])
                .arg(&binary)
                .output()
                .unwrap();
            assert!(
                compiled.status.success(),
                "{}",
                String::from_utf8_lossy(&compiled.stderr)
            );
            Command::new(binary).output().unwrap()
        };
        git(p, &["checkout", "-qb", "left"]);
        std::fs::write(p.join("left.rs"), "pub const VALUE: u32 = 2;\n").unwrap();
        git(p, &["commit", "-qam", "left"]);
        assert!(check().status.success());
        git(p, &["checkout", "-qb", "right", &base]);
        std::fs::write(p.join("right.rs"), "pub const VALUE: u32 = 2;\n").unwrap();
        git(p, &["commit", "-qam", "right"]);
        assert!(check().status.success());
        git(p, &["merge", "--no-ff", "-qm", "batch", "left"]);
        let combined = check();
        assert!(!combined.status.success());
        assert!(String::from_utf8_lossy(&combined.stdout).contains("capacity ... FAILED"));
        assert_eq!(
            git(p, &["rev-list", "--parents", "-n", "1", "HEAD"])
                .split_whitespace()
                .count(),
            3
        );
    }

    fn selection_recipe() -> ValidationRecipe {
        let mut r = recipe();
        r.commands[0].argv = ["cargo", "nextest", "run", "--locked"]
            .map(String::from)
            .to_vec();
        r.commands[0].test_selection = Some(TestSelection {
            rules: vec![
                TestSelectionRule {
                    path: "src/a.rs".into(),
                    filter: "test(/^a::/)".into(),
                },
                TestSelectionRule {
                    path: "README.md".into(),
                    filter: "test(/^b::/)".into(),
                },
            ],
        });
        r
    }

    #[test]
    fn selected_tests_are_deterministic_and_fail_closed() {
        use agentscm::local_validation::run::command_selections;
        let r = selection_recipe();
        let (d, b, h) = repo(|p| {
            write(p, "src/a.rs");
            write(p, "README.md");
        });
        let selected = command_selections(d.path(), &b, &h, &r, &trust()).unwrap();
        assert_eq!(selected[0].reason, SelectionReason::Selected);
        assert_eq!(selected[0].paths, ["README.md", "src/a.rs"]);
        assert_eq!(
            selected[0].argv.last().unwrap(),
            "(test(/^a::/)) | (test(/^b::/))"
        );
        for (path, reason) in [
            ("unknown", SelectionReason::NonModifiedPath),
            (
                ".github/local-validation/recipe.json",
                SelectionReason::TrustInput,
            ),
        ] {
            let (d, b, h) = repo(|p| write(p, path));
            let selected = command_selections(d.path(), &b, &h, &r, &trust()).unwrap();
            assert_eq!(selected[0].reason, reason);
            assert_eq!(selected[0].argv, r.commands[0].argv);
        }
        let (d, b, h) = repo(|p| {
            std::fs::remove_file(p.join("src/a.rs")).unwrap();
        });
        assert_eq!(
            command_selections(d.path(), &b, &h, &r, &trust()).unwrap()[0].reason,
            SelectionReason::NonModifiedPath
        );
        let (d, b, h) = repo(|p| {
            std::fs::rename(p.join("src/a.rs"), p.join("new.rs")).unwrap();
        });
        assert_eq!(
            command_selections(d.path(), &b, &h, &r, &trust()).unwrap()[0].reason,
            SelectionReason::NonModifiedPath
        );
    }

    #[test]
    fn selected_tests_handle_unmapped_shared_empty_and_duplicate_filters() {
        use agentscm::local_validation::run::command_selections;
        let mut r = selection_recipe();
        r.commands[0]
            .test_selection
            .as_mut()
            .unwrap()
            .rules
            .remove(1);
        let (d, b, h) = repo(|p| write(p, "README.md"));
        assert_eq!(
            command_selections(d.path(), &b, &h, &r, &trust()).unwrap()[0].reason,
            SelectionReason::UnmappedPath
        );
        let (d, b, h) = repo(|_| {});
        assert_eq!(
            command_selections(d.path(), &b, &h, &r, &trust()).unwrap()[0].reason,
            SelectionReason::EmptyChange
        );
        r.commands[0]
            .test_selection
            .as_mut()
            .unwrap()
            .rules
            .push(TestSelectionRule {
                path: "Cargo.toml".into(),
                filter: "all()".into(),
            });
        let (d, b, h) = repo(|p| write(p, "Cargo.toml"));
        assert_eq!(
            command_selections(d.path(), &b, &h, &r, &trust()).unwrap()[0].reason,
            SelectionReason::SharedInput
        );
        r.commands[0]
            .test_selection
            .as_mut()
            .unwrap()
            .rules
            .push(TestSelectionRule {
                path: "README.md".into(),
                filter: "test(/^a::/)".into(),
            });
        let (d, b, h) = repo(|p| {
            write(p, "README.md");
            write(p, "src/a.rs");
        });
        assert_eq!(
            command_selections(d.path(), &b, &h, &r, &trust()).unwrap()[0].filters,
            ["test(/^a::/)"]
        );
    }

    fn scoped_recipe() -> ValidationRecipe {
        let mut r = recipe();
        let command = |id: &str, paths: Option<&[&str]>| RecipeCommand {
            id: id.into(),
            argv: vec!["true".into()],
            timeout_secs: 1,
            test_selection: None,
            paths: paths.map(|p| p.iter().map(|s| (*s).to_owned()).collect()),
        };
        r.commands = vec![
            command("always", None),
            command("rust", Some(&["src"])),
            command("docs", Some(&["docs"])),
        ];
        r
    }

    fn required(dir: &Path, base: &str, head: &str) -> Vec<String> {
        required_commands(dir, base, head, &scoped_recipe(), &trust()).unwrap()
    }

    #[test]
    fn commands_follow_their_paths() {
        let (d, b, h) = repo(|p| write(p, "docs/x.md"));
        assert_eq!(required(d.path(), &b, &h), ["always", "docs"]);
        let (d, b, h) = repo(|p| write(p, "src/a.rs"));
        assert_eq!(required(d.path(), &b, &h), ["always", "rust"]);
    }

    #[test]
    fn repository_recipe_selects_local_checks_from_real_git_changes() {
        let recipe = ValidationRecipe::from_slice(include_bytes!(
            "../../.github/local-validation/recipe.json"
        ))
        .unwrap();
        const RUST: &[&str] = &["fmt", "clippy", "nextest", "doctest"];
        const UI: &[&str] = &["ui-install", "ui-lint", "ui-build", "ui-notices"];
        const DOCS: &[&str] = &["docs-install", "docs-audit", "docs-build", "docs-check"];
        const RELEASE: &[&str] = &[
            "release-fmt",
            "release-dir",
            "release-test-build",
            "release-test",
            "release-build",
            "release-check",
        ];
        let cases: &[(&[&str], &[&str])] = &[
            (&["src/a.rs"], RUST),
            (&["ui/src/example.tsx"], UI),
            (&["ui/tests/unit/x.test.tsx"], UI),
            (&["ui/tests/integration/x.test.tsx"], UI),
            (&["ui/tests/e2e/x.spec.ts"], UI),
            (&["ui/public/x.png"], UI),
            (&["DESIGN.md"], &[]),
            (
                &["ui/tests/fixtures/workflow-library-router.json"],
                &[RUST, UI].concat(),
            ),
            (&["THIRD_PARTY_NOTICES.md"], &[RUST, UI].concat()),
            (&["docs/openapi-work-insights.json"], &[RUST, DOCS].concat()),
            (
                &["ui/dev-seed/mint/Cargo.lock"],
                &[RUST, UI, RELEASE].concat(),
            ),
            (&["ui/newfile"], &[RUST, UI].concat()),
            (&["ui/package.json"], &[RUST, UI].concat()),
            (&["ui/src/x.tsx", "src/a.rs"], &[RUST, UI].concat()),
            (&["DESIGN.md", "ui/src/x.tsx"], UI),
            (
                &["install.sh"],
                &[RUST, &["shellcheck", "npm-installer"]].concat(),
            ),
            (&["docs/example.md"], DOCS),
            (&["docs/advanced/nested.md"], DOCS),
            (&["docs-site/astro.config.mjs"], DOCS),
            (&["docs/assets/agentscm-icon.png"], &[RUST, DOCS].concat()),
            (&["docs/example.md", "src/a.rs"], &[RUST, DOCS].concat()),
            (&["README.md", "AGENTS.md", ".agents/commands/pr.md"], &[]),
            (&[".agents/bound-target.sh"], RUST),
            (&[".github/pull_request_template.md"], &["workflow-lint"]),
            (
                &[".github/workflows/zizmor.yml"],
                &[RUST, &["workflow-lint"]].concat(),
            ),
            (
                &["tools/portable-verifier/Cargo.lock"],
                &[RUST, &["deny-portable"]].concat(),
            ),
            (&["deny.toml"], &[RUST, &["deny", "deny-portable"]].concat()),
            (&["Cargo.toml"], &[RUST, &["deny"], RELEASE].concat()),
        ];
        for (paths, expected) in cases {
            let (dir, base, head) = repo(|p| paths.iter().for_each(|path| write(p, path)));
            assert_eq!(
                required_commands(dir.path(), &base, &head, &recipe, &trust()).unwrap(),
                *expected,
                "{paths:?}"
            );
        }
        for path in [
            ".github/local-validation/recipe.json",
            ".github/local-validation/policy.json",
            DEFAULT_VERIFIER_WORKFLOW,
        ] {
            let (dir, base, head) = repo(|p| write(p, path));
            assert_eq!(
                required_commands(dir.path(), &base, &head, &recipe, &trust()).unwrap(),
                recipe
                    .commands
                    .iter()
                    .map(|c| c.id.as_str())
                    .collect::<Vec<_>>(),
                "{path}"
            );
        }
    }

    #[test]
    fn a_trust_input_change_requires_every_command() {
        let (d, b, h) = repo(|p| write(p, ".github/local-validation/recipe.json"));
        assert_eq!(required(d.path(), &b, &h), ["always", "rust", "docs"]);
    }

    #[test]
    fn the_verifier_workflow_is_a_trust_input_from_a_subdirectory_repo() {
        let (d, b, h) = repo(|p| write(p, DEFAULT_VERIFIER_WORKFLOW));
        let sub = d.path().join("src");
        // A root-level workflow change is outside the subdirectory but still a trust change.
        assert_eq!(
            required_commands(
                &sub,
                &b,
                &h,
                &scoped_recipe(),
                &trust_paths("x", DEFAULT_VERIFIER_WORKFLOW)
            )
            .unwrap(),
            ["always", "rust", "docs"]
        );
        let specs = vec!["docs/**".to_owned()];
        let trust = trust_paths("x", DEFAULT_VERIFIER_WORKFLOW);
        assert!(!only_exempt(&sub, &b, &h, &specs, &trust).unwrap());
    }

    /// Without magic, `*` matches across `/`, so `*.md` also matches `docs/x.md`.
    #[test]
    fn default_wildcard_pathspec_matches_at_depth() {
        let (d, b, h) = repo(|p| write(p, "docs/x.md"));
        assert!(exempt(d.path(), &b, &h, &["*.md"]));
        assert!(!exempt(d.path(), &b, &h, &[":(glob)*.md"]));
    }
}

/// A two-command recipe whose `lint` is path-scoped; `required` says what the diff needs.
fn scoped(required: &[&str], not_required: &[&str]) -> Case {
    let mut c = Case::new();
    c.recipe.commands.push(RecipeCommand {
        id: "lint".into(),
        argv: vec!["cargo".into(), "clippy".into()],
        timeout_secs: 60,
        test_selection: None,
        paths: Some(vec!["src".into()]),
    });
    c.policy.recipe_sha256 = c.recipe.sha256().unwrap();
    c.record.recipe_sha256 = c.policy.recipe_sha256.clone();
    c.required = Some(required.iter().map(|s| (*s).to_owned()).collect());
    c.record.not_required = not_required.iter().map(|s| (*s).to_owned()).collect();
    c
}

#[test]
fn not_required_command_is_reported_apart_from_skipped_and_passes() {
    let mut c = scoped(&["test"], &["lint"]);
    c.tamper_payload = Some(Box::new(|p| {
        assert!(
            p.coverage
                .omissions
                .contains(&"not required: lint".to_owned())
        );
        assert!(!p.coverage.omissions.contains(&"skipped: lint".to_owned()));
        assert_eq!(p.coverage.expected_count, 1);
        assert_eq!(p.coverage.observed_count, 1);
    }));
    let body = Case {
        tamper_payload: None,
        ..scoped(&["test"], &["lint"])
    }
    .body();
    assert!(body.contains("Not required by the changed paths: <code>lint</code>"));
    expect(c.run(), VerdictState::Success, "validated");
}

#[test]
fn a_run_where_no_command_is_required_verifies() {
    let mut c = scoped(&[], &["test", "lint"]);
    c.record.results.clear();
    c.tamper_payload = Some(Box::new(|p| {
        for id in ["test", "lint"] {
            assert!(
                p.coverage
                    .omissions
                    .contains(&format!("not required: {id}"))
            );
        }
        assert_eq!(p.coverage.expected_count, 0);
        assert_eq!(p.coverage.observed_count, 0);
    }));
    expect(c.run(), VerdictState::Success, "validated");
}

#[test]
fn a_command_cannot_be_both_a_result_and_not_required() {
    let mut r = record();
    r.not_required = vec![r.results[0].id.clone()];
    assert!(r.validate().is_err());
}

#[test]
fn records_without_not_required_keep_their_bytes() {
    let bytes = record().to_bytes().unwrap();
    assert!(!String::from_utf8(bytes).unwrap().contains("not_required"));
}

#[test]
fn forged_not_required_claim_for_a_required_command_fails() {
    let verdict = scoped(&["test", "lint"], &["lint"]).run();
    assert_eq!(verdict.reason, "incomplete run", "{verdict:?}");
    assert_eq!(verdict.state, VerdictState::Failure);
    assert!(
        verdict
            .detail
            .contains("lint is required by the changed paths")
    );
}

#[test]
fn missing_required_result_fails() {
    let verdict = scoped(&["test", "lint"], &[]).run();
    expect(verdict.clone(), VerdictState::Failure, "incomplete run");
    assert!(verdict.detail.contains("lint did not run"));
}

#[test]
fn unclaimed_or_wrong_not_required_list_fails() {
    // The diff does not require lint, but the record neither ran it nor listed it.
    expect(
        scoped(&["test"], &[]).run(),
        VerdictState::Failure,
        "incomplete run",
    );
    // The record ran lint although the diff does not require it.
    let mut c = scoped(&["test"], &[]);
    let mut lint = c.record.results[0].clone();
    lint.id = "lint".into();
    lint.argv = vec!["cargo".into(), "clippy".into()];
    c.record.results.push(lint);
    expect(c.run(), VerdictState::Failure, "incomplete run");
}

#[test]
fn conditional_command_that_ran_passes_when_required() {
    let mut c = scoped(&["test", "lint"], &[]);
    let mut lint = c.record.results[0].clone();
    lint.id = "lint".into();
    lint.argv = vec!["cargo".into(), "clippy".into()];
    c.record.results.push(lint);
    expect(c.run(), VerdictState::Success, "validated");
}

#[test]
fn command_result_preserves_incomplete_capture_and_legacy_encoding() {
    let legacy = serde_json::json!({
        "id": "test", "argv": ["test"], "exit_code": null,
        "stop": "output_limit", "duration_ms": 1,
        "stdout_sha256": "a", "stderr_sha256": "b", "truncated": true
    });
    let mut result: CommandResult = serde_json::from_value(legacy.clone()).unwrap();
    assert!(!result.incomplete);
    assert_eq!(serde_json::to_value(&result).unwrap(), legacy);
    result.incomplete = true;
    let encoded = serde_json::to_value(&result).unwrap();
    assert_eq!(encoded["incomplete"], true);
    assert_eq!(
        serde_json::from_value::<CommandResult>(encoded).unwrap(),
        result
    );
}

#[test]
fn incomplete_capture_cannot_validate_a_successful_exit() {
    let mut c = Case::new();
    c.record.results[0].incomplete = true;
    fails(c, "capture incomplete");
}

// ---- independent checks (check_id) and constituents ----

fn check(id: &str) -> Case {
    let mut c = Case::new();
    c.policy.check_id = Some(id.into());
    c
}

fn select_with(
    bodies: &[&str],
    recipe: &ValidationRecipe,
    registry: &IssuerRegistrySnapshot,
    policy: &ValidationPolicy,
    clock: EvaluationClock,
) -> Verdict {
    select(
        bodies,
        recipe,
        registry,
        policy,
        Observed {
            repository: "repo-1",
            commit: COMMIT,
            tree: TREE,
            base_ref: "refs/heads/main",
            merge_base: BASE,
            required: &["test".to_owned()],
            selections: &[],
        },
        clock,
    )
}

fn known_for(bodies: &[&str], policy: &ValidationPolicy) -> Verdict {
    select_with(
        bodies,
        &recipe(),
        &registry(),
        policy,
        EvaluationClock::Known { unix_ms: NOW },
    )
}

fn policy_for(id: Option<&str>) -> ValidationPolicy {
    ValidationPolicy {
        check_id: id.map(String::from),
        ..policy()
    }
}

fn envelope_json(body: &str) -> serde_json::Value {
    serde_json::from_slice(&comment::parse(body).unwrap().0).unwrap()
}

#[test]
fn policy_and_recipe_without_new_fields_keep_canonical_bytes_and_digests() {
    let legacy_policy = format!(
        r#"{{"repository":"repo-1","recipe_sha256":"{}","required_services":{{"postgres":true,"s3":true}},"max_age_seconds":3600,"epoch":3}}"#,
        recipe_hash()
    );
    assert_eq!(serde_json::to_string(&policy()).unwrap(), legacy_policy);
    assert_eq!(
        ValidationPolicy::from_slice(legacy_policy.as_bytes()).unwrap(),
        policy()
    );
    assert_eq!(
        policy().digest().unwrap(),
        format!("{:x}", sha2::Sha256::digest(legacy_policy.as_bytes()))
    );
    assert_eq!(policy().requirement_id(), REQUIREMENT_ID);
    assert_eq!(policy().status_context(), "agentscm/local-validation");

    let named = policy_for(Some("static"));
    assert_ne!(named.digest().unwrap(), policy().digest().unwrap());
    assert_eq!(named.requirement_id(), "local-validation/static");
    assert_eq!(named.status_context(), "agentscm/local-validation/static");
    assert_eq!(
        named.constituent_status_context("lint"),
        "agentscm/local-validation/static/lint"
    );
    let mut with_groups = recipe();
    with_groups.constituents = Some(vec![Constituent {
        id: "unit".into(),
        name: "Unit".into(),
        commands: vec!["test".into()],
    }]);
    assert_ne!(with_groups.sha256().unwrap(), recipe_hash());
}

#[test]
fn legacy_shaped_evidence_still_verifies() {
    let body = pass().body();
    let envelope = envelope_json(&body);
    assert_eq!(envelope["payload"]["requirement_id"], "local-validation");
    expect(known(&[&body]), VerdictState::Success, "validated");
    assert_eq!(known(&[&body]).outcome, Outcome::Passed);
}

#[test]
fn check_ids_are_validated() {
    for id in ["a", "unit-tests", "0x", &"a".repeat(40)] {
        policy_for(Some(id)).validate().unwrap();
    }
    for id in ["", "Upper", "-x", "a_b", "a/b", "a b", &"a".repeat(41)] {
        assert!(policy_for(Some(id)).validate().is_err(), "{id:?}");
    }
}

#[test]
fn two_independent_checks_never_satisfy_or_influence_each_other() {
    let (a, b) = (policy_for(Some("alpha")), policy_for(Some("beta")));
    let a_pass = check("alpha").body();
    let b_pass = check("beta").body();
    let mut a_fail = check("alpha");
    a_fail.record.results[0].exit_code = Some(1);
    let a_fail = a_fail.body();
    let a_old = old_head(check("alpha")).body();
    assert_eq!(
        envelope_json(&a_pass)["payload"]["requirement_id"],
        "local-validation/alpha"
    );

    // Direct assessment: another check's envelope is Unverified and never applies.
    let mut c = check("alpha");
    c.assess_policy = Some(b.clone());
    let verdict = c.run();
    assert_eq!(
        (verdict.state, verdict.outcome, verdict.applies_to_head),
        (VerdictState::Failure, Outcome::Unverified, false)
    );

    let nothing = known_for(&[], &b);
    assert_eq!(nothing.outcome, Outcome::Missing);
    for foreign in [&a_pass, &a_fail, &a_old] {
        // B sees A's evidence exactly as if it were absent, including reason and detail.
        assert_eq!(known_for(&[foreign], &b), nothing);
        // A neighbouring failure or stale head never overrides B's own pass.
        expect(
            known_for(&[&b_pass, foreign], &b),
            VerdictState::Success,
            "validated",
        );
        expect(
            known_for(&[foreign, &b_pass], &b),
            VerdictState::Success,
            "validated",
        );
    }
    // And the other direction.
    let nothing_a = known_for(&[], &a);
    for foreign in [&b_pass, &check("beta").body()] {
        assert_eq!(known_for(&[foreign], &a), nothing_a);
    }
    expect(
        known_for(&[&a_pass, &b_pass], &a),
        VerdictState::Success,
        "validated",
    );
    expect(
        known_for(&[&b_pass, &a_fail], &a),
        VerdictState::Failure,
        "command failed",
    );
    // Legacy and named checks are also independent.
    let legacy = pass().body();
    assert_eq!(known_for(&[&legacy], &a), nothing_a);
    assert_eq!(
        known_for(&[&a_pass], &policy_for(None)),
        known_for(&[], &policy_for(None))
    );
}

#[test]
fn the_same_recipe_under_two_check_ids_yields_distinct_requirements() {
    let (a, b) = (check("alpha"), check("beta"));
    assert_eq!(a.recipe.sha256().unwrap(), b.recipe.sha256().unwrap());
    assert_ne!(
        a.policy.digest().unwrap(),
        b.policy.digest().unwrap(),
        "policy digest differs by check"
    );
    let (a_body, b_body) = (a.body(), b.body());
    assert_ne!(
        envelope_json(&a_body)["payload"]["requirement_id"],
        envelope_json(&b_body)["payload"]["requirement_id"]
    );
    let (pa, pb) = (policy_for(Some("alpha")), policy_for(Some("beta")));
    expect(
        known_for(&[&a_body], &pa),
        VerdictState::Success,
        "validated",
    );
    expect(
        known_for(&[&b_body], &pb),
        VerdictState::Success,
        "validated",
    );
    assert_eq!(known_for(&[&a_body], &pb).state, VerdictState::Pending);
    assert_eq!(known_for(&[&b_body], &pa).state, VerdictState::Pending);
}

#[test]
fn enrolled_key_forgery_fails_closed() {
    // A signed requirement for another or unknown id never satisfies this check.
    for requirement in ["local-validation/other", "local-validation", "unknown"] {
        let mut c = check("alpha");
        let requirement = requirement.to_owned();
        c.tamper_payload = Some(Box::new(move |p| p.requirement_id = requirement));
        let verdict = c.run();
        assert_eq!(
            (verdict.state, verdict.outcome, verdict.applies_to_head),
            (VerdictState::Failure, Outcome::Unverified, false),
            "{verdict:?}"
        );
    }
    let coverage_fails = |tamper: Tamper, c: Case| {
        let mut c = c;
        c.tamper_payload = Some(tamper);
        let verdict = c.run();
        assert_eq!(
            (verdict.state, verdict.reason, verdict.applies_to_head),
            (VerdictState::Failure, "coverage mismatch", true),
            "{verdict:?}"
        );
        assert_eq!(verdict.outcome, Outcome::Unverified);
    };
    coverage_fails(
        Box::new(|p| p.coverage.scope = vec!["other".into()]),
        check("alpha"),
    );
    coverage_fails(Box::new(|p| p.coverage.scope.clear()), check("alpha"));
    coverage_fails(Box::new(|p| p.coverage.observed_count = 0), check("alpha"));
    coverage_fails(Box::new(|p| p.coverage.expected_count = 2), check("alpha"));
    coverage_fails(
        Box::new(|p| p.coverage.omissions.retain(|o| o != "not required: lint")),
        scoped(&["test"], &["lint"]),
    );
    // Scope is compared as a set, so the signed sort order cannot matter.
    let mut c = scoped(&["test", "lint"], &[]);
    let mut lint = c.record.results[0].clone();
    lint.id = "lint".into();
    lint.argv = vec!["cargo".into(), "clippy".into()];
    c.record.results.push(lint);
    c.tamper_payload = Some(Box::new(|p| p.coverage.scope.reverse()));
    expect(c.run(), VerdictState::Success, "validated");
}

#[test]
fn lifecycle_causes_are_distinct_non_success_and_chosen_only_by_authenticated_evidence() {
    let p = policy_for(Some("alpha"));
    let one = |c: Case, registry: &IssuerRegistrySnapshot, clock: EvaluationClock| {
        let body = c.body();
        select_with(&[&body], &recipe(), registry, &p, clock)
    };
    let known_now = EvaluationClock::Known { unix_ms: NOW };
    let mut seen = std::collections::BTreeMap::new();
    let mut expect_cause = |verdict: Verdict, outcome: Outcome| {
        assert_eq!(
            (verdict.state, verdict.outcome, verdict.applies_to_head),
            (VerdictState::Pending, outcome, false),
            "{verdict:?}"
        );
        assert_ne!(verdict.reason, "no attestation for this head");
        seen.insert(outcome.as_str(), verdict.reason);
    };

    expect_cause(
        one(old_head(check("alpha")), &registry(), known_now),
        Outcome::StaleHead,
    );

    let mut policy_changed = check("alpha");
    policy_changed.policy.epoch = 2;
    expect_cause(
        one(policy_changed, &registry(), known_now),
        Outcome::StalePolicy,
    );

    // Signed longer than the clock allows.
    expect_cause(
        one(
            check("alpha"),
            &registry(),
            EvaluationClock::Known {
                unix_ms: ISSUED + 90_000_000,
            },
        ),
        Outcome::Expired,
    );
    expect_cause(
        one(
            check("alpha"),
            &registry(),
            EvaluationClock::Known {
                unix_ms: ISSUED + 3_601_000,
            },
        ),
        Outcome::Expired,
    );

    let mut revoked = registry();
    revoked.issuers[0].keys[0].revoked_at_ms = Some(ISSUED + 1);
    let mut c = check("alpha");
    c.registry = revoked.clone();
    expect_cause(one(c, &revoked, known_now), Outcome::Revoked);

    // The signing key's own validity ended: re-attesting with it cannot help.
    let mut key_expired = registry();
    key_expired.issuers[0].keys[0].not_after_ms = ISSUED + 1;
    let key_expired_verdict = one(check("alpha"), &key_expired, known_now);
    assert_eq!(
        key_expired_verdict.reason,
        "attestation signing key expired"
    );
    expect_cause(key_expired_verdict, Outcome::KeyExpired);
    assert_eq!(Outcome::KeyExpired.as_str(), "key_expired");

    // A real revocation edits the registry after signing, so the registry digest changed too.
    // The revocation is still the cause reported; a change that revokes nothing stays stale.
    let signed_before_revocation = one(check("alpha"), &revoked, known_now);
    assert_eq!(signed_before_revocation.outcome, Outcome::Revoked);
    assert_eq!(signed_before_revocation.state, VerdictState::Pending);
    assert_eq!(
        signed_before_revocation.reason,
        "attestation signing key revoked"
    );

    let mut changed = registry();
    changed.epoch = 2;
    expect_cause(
        one(check("alpha"), &changed, known_now),
        Outcome::RegistryStale,
    );

    let unique: std::collections::HashSet<_> = seen.iter().collect();
    assert_eq!(
        unique.len(),
        seen.len(),
        "reasons must be distinct: {seen:?}"
    );

    // An unauthenticated comment cannot choose the reason, even when newer.
    let stale = old_head(check("alpha")).body();
    let mut forged = old_head(check("alpha"));
    forged.issued_at_ms = ISSUED + 50;
    forged.tamper_envelope = true;
    let forged = forged.body();
    let mut other_key = old_head(check("alpha"));
    other_key.registry.issuers[0].keys.clear();
    let mut revoked_key_body = check("alpha");
    revoked_key_body.registry = revoked;
    revoked_key_body.issued_at_ms = ISSUED + 60;
    let foreign_expired = at(old_head(check("beta")), 70).body();
    let verdict = known_for(
        &[&stale, &forged, &other_key.body(), &foreign_expired, "junk"],
        &p,
    );
    assert_eq!(verdict.outcome, Outcome::StaleHead, "{verdict:?}");
    // Nothing authenticated: Missing, never a lifecycle reason.
    let verdict = known_for(&[&forged, &foreign_expired, "junk"], &p);
    assert_eq!(verdict.outcome, Outcome::Missing);
    assert_eq!(verdict.reason, "no attestation for this head");
    // A later applicable verdict still wins over any lifecycle cause.
    let pass = check("alpha").body();
    expect(
        known_for(&[&stale, &pass], &p),
        VerdictState::Success,
        "validated",
    );
}

fn cmd(id: &str, paths: Option<&[&str]>) -> RecipeCommand {
    RecipeCommand {
        id: id.into(),
        argv: vec!["run".into(), id.into()],
        timeout_secs: 60,
        test_selection: None,
        paths: paths.map(|p| p.iter().map(|s| (*s).to_owned()).collect()),
    }
}

fn ok_result(id: &str) -> CommandResult {
    CommandResult {
        id: id.into(),
        argv: vec!["run".into(), id.into()],
        ..record().results[0].clone()
    }
}

fn group(id: &str, name: &str, commands: &[&str]) -> Constituent {
    Constituent {
        id: id.into(),
        name: name.into(),
        commands: commands.iter().map(|c| (*c).to_owned()).collect(),
    }
}

/// setup (in no constituent), unit, lint and a path-scoped docs command.
fn grouped() -> Case {
    let mut c = check("grouped");
    c.recipe.commands = vec![
        cmd("setup", None),
        cmd("unit", None),
        cmd("lint", None),
        cmd("docs", Some(&["docs"])),
    ];
    c.recipe.constituents = Some(vec![
        group("unit-tests", "Unit tests", &["unit"]),
        group("static", "Static", &["lint"]),
        group("documentation", "Documentation", &["docs"]),
    ]);
    c.policy.recipe_sha256 = c.recipe.sha256().unwrap();
    c.record.recipe_sha256 = c.policy.recipe_sha256.clone();
    c.record.results = ["setup", "unit", "lint", "docs"].map(ok_result).to_vec();
    c
}

fn outcomes(verdict: &Verdict) -> Vec<(&str, Outcome, VerdictState)> {
    verdict
        .constituents
        .iter()
        .map(|c| (c.id.as_str(), c.outcome, c.state))
        .collect()
}

#[test]
fn grouped_recipe_all_pass() {
    let verdict = grouped().run();
    expect(verdict.clone(), VerdictState::Success, "validated");
    assert_eq!(verdict.constituents[0].name, "Unit tests");
    assert_eq!(
        outcomes(&verdict),
        [
            ("unit-tests", Outcome::Passed, VerdictState::Success),
            ("static", Outcome::Passed, VerdictState::Success),
            ("documentation", Outcome::Passed, VerdictState::Success),
        ]
    );
    // Without constituents the verdict carries none.
    assert!(check("alpha").run().constituents.is_empty());
}

#[test]
fn grouped_failure_at_each_position_marks_earlier_passed_and_later_did_not_run() {
    let expected = |k: usize| -> Vec<(&'static str, Outcome, VerdictState)> {
        // Groups: unit-tests at result 1, static at 2, documentation at 3.
        ["unit-tests", "static", "documentation"]
            .iter()
            .enumerate()
            .map(|(i, id)| {
                let position = i + 1;
                match position.cmp(&k) {
                    std::cmp::Ordering::Less => (*id, Outcome::Passed, VerdictState::Success),
                    std::cmp::Ordering::Equal => (*id, Outcome::Failed, VerdictState::Failure),
                    std::cmp::Ordering::Greater => (*id, Outcome::DidNotRun, VerdictState::Pending),
                }
            })
            .collect()
    };
    for k in 1..=3 {
        let mut c = grouped();
        c.record.results.truncate(k + 1);
        c.record.results[k].exit_code = Some(1);
        let verdict = c.run();
        expect(verdict.clone(), VerdictState::Failure, "command failed");
        assert_eq!(outcomes(&verdict), expected(k), "k={k}");
        assert!(
            verdict
                .constituents
                .iter()
                .all(|c| c.state != VerdictState::Success || c.outcome == Outcome::Passed)
        );
    }
}

#[test]
fn grouped_failing_setup_command_leaves_every_constituent_did_not_run() {
    let mut c = grouped();
    c.record.results.truncate(1);
    c.record.results[0].exit_code = Some(2);
    let verdict = c.run();
    expect(verdict.clone(), VerdictState::Failure, "command failed");
    assert!(
        outcomes(&verdict)
            .iter()
            .all(|(_, o, s)| (*o, *s) == (Outcome::DidNotRun, VerdictState::Pending))
    );
}

#[test]
fn grouped_structural_problems_leave_constituents_empty() {
    let mut c = grouped();
    c.record.tree_unchanged_after = false;
    let verdict = c.run();
    expect(verdict.clone(), VerdictState::Failure, "tree changed");
    assert!(verdict.constituents.is_empty());

    let mut c = grouped();
    c.record.services.s3 = false;
    let verdict = c.run();
    expect(
        verdict.clone(),
        VerdictState::Failure,
        "required service missing",
    );
    assert!(verdict.constituents.is_empty());

    // A signed pass over a failed record.
    let mut c = grouped();
    c.record.results[1].exit_code = Some(1);
    c.tamper_payload = Some(Box::new(|p| {
        p.outcome = agentscm::evidence::EvidenceOutcome::Pass;
        p.terminal_state = agentscm::evidence::TerminalState::Success;
    }));
    let verdict = c.run();
    expect(verdict.clone(), VerdictState::Failure, "command failed");
    assert!(verdict.constituents.is_empty());

    // A signed failure over a clean record.
    let mut c = grouped();
    c.tamper_payload = Some(Box::new(|p| {
        p.outcome = agentscm::evidence::EvidenceOutcome::Fail;
        p.terminal_state = agentscm::evidence::TerminalState::Failed;
    }));
    let verdict = c.run();
    expect(verdict.clone(), VerdictState::Failure, "outcome not pass");
    assert!(verdict.constituents.is_empty());

    // Order departure and coverage mismatch.
    let mut c = grouped();
    c.record.results.swap(1, 2);
    let verdict = c.run();
    expect(verdict.clone(), VerdictState::Failure, "incomplete run");
    assert!(verdict.constituents.is_empty());
    let mut c = grouped();
    c.tamper_payload = Some(Box::new(|p| p.coverage.observed_count = 1));
    let verdict = c.run();
    assert_eq!(verdict.reason, "coverage mismatch");
    assert!(verdict.constituents.is_empty());
}

#[test]
fn grouped_truncated_output_on_exit_zero_is_a_failed_constituent() {
    let mut c = grouped();
    c.record.results[2].truncated = true;
    let verdict = c.run();
    expect(verdict.clone(), VerdictState::Failure, "output truncated");
    assert_eq!(
        outcomes(&verdict),
        [
            ("unit-tests", Outcome::Passed, VerdictState::Success),
            ("static", Outcome::Failed, VerdictState::Failure),
            ("documentation", Outcome::Passed, VerdictState::Success),
        ]
    );
}

#[test]
fn grouped_path_filtered_constituent_is_not_required() {
    let mut c = grouped();
    c.required = Some(["setup", "unit", "lint"].map(String::from).to_vec());
    c.record.not_required = vec!["docs".into()];
    c.record.results.truncate(3);
    let verdict = c.run();
    expect(verdict.clone(), VerdictState::Success, "validated");
    assert_eq!(
        outcomes(&verdict)[2],
        ("documentation", Outcome::NotRequired, VerdictState::Success)
    );
    assert_eq!(verdict.constituents[2].reason, "not required");
}

#[test]
fn constituent_declarations_are_validated() {
    let with = |groups: Vec<Constituent>| {
        let mut r = recipe();
        r.commands.push(cmd("lint", None));
        r.constituents = Some(groups);
        r.validate()
    };
    with(vec![
        group("unit", "Unit", &["test"]),
        group("static", "Static", &["lint"]),
    ])
    .unwrap();
    with(vec![group("unit", "Unit", &["test"])]).unwrap(); // lint is setup
    let long = "n".repeat(65);
    let bad: Vec<Vec<Constituent>> = vec![
        vec![],
        vec![group("unit", "Unit", &[])],
        vec![group("unit", "Unit", &["missing"])],
        vec![group("a", "A", &["test"]), group("b", "B", &["test"])],
        vec![group("a", "A", &["test"]), group("a", "B", &["lint"])],
        vec![group("Unit", "Unit", &["test"])],
        vec![group("-a", "A", &["test"])],
        vec![group("", "A", &["test"])],
        vec![group("a", "", &["test"])],
        vec![group("a", &long, &["test"])],
        vec![group("a", "bad\nname", &["test"])],
        vec![group("a", "bad\u{7}name", &["test"])],
        vec![group("a", "A", &["test", "test"])],
    ];
    for groups in bad {
        assert!(with(groups.clone()).is_err(), "{groups:?}");
    }
    with(vec![group("a", &"n".repeat(64), &["test"])]).unwrap();

    let unknown = r#"{"id":"a","name":"A","commands":["test"],"extra":1}"#;
    assert!(serde_json::from_str::<Constituent>(unknown).is_err());

    // A recipe with constituents under a policy without a check id is not assessable.
    let mut c = grouped();
    c.policy.check_id = None;
    c.assess_policy = Some(c.policy.clone());
    let verdict = c.run();
    assert_eq!(
        (verdict.state, verdict.outcome, verdict.reason),
        (VerdictState::Error, Outcome::Error, "policy invalid")
    );
}

#[test]
fn selected_evidence_requires_independent_selection_and_exact_argv() {
    let selected_case = || {
        let mut c = Case::new();
        c.recipe.commands[0].argv = ["cargo", "nextest", "run", "--locked"]
            .map(String::from)
            .to_vec();
        c.recipe.commands[0].test_selection = Some(TestSelection {
            rules: vec![TestSelectionRule {
                path: "tests/a.rs".into(),
                filter: "test(/^a::/)".into(),
            }],
        });
        c.policy.recipe_sha256 = c.recipe.sha256().unwrap();
        c.record.recipe_sha256 = c.policy.recipe_sha256.clone();
        let mut argv = c.recipe.commands[0].argv.clone();
        argv.extend(["-E".into(), "(test(/^a::/))".into()]);
        c.record.results[0].argv = argv.clone();
        c.record.selections = vec![CommandSelection {
            command_id: "test".into(),
            reason: SelectionReason::Selected,
            paths: vec!["tests/a.rs".into()],
            filters: vec!["test(/^a::/)".into()],
            argv,
        }];
        c.selections = c.record.selections.clone();
        c
    };
    assert_eq!(selected_case().run().state, VerdictState::Success);
    let mut changed = selected_case();
    changed.record.selections[0].reason = SelectionReason::UnmappedPath;
    assert_eq!(changed.run().reason, "selection mismatch");
    let mut changed = selected_case();
    changed.record.results[0].argv = vec!["true".into()];
    assert_ne!(changed.run().state, VerdictState::Success);
    let mut changed = selected_case();
    changed.record.selections[0].paths.push("forged.rs".into());
    assert_eq!(changed.run().reason, "selection mismatch");

    let mut c = Case::new();
    c.record.selections = vec![CommandSelection {
        command_id: "test".into(),
        reason: SelectionReason::Selected,
        paths: vec![],
        filters: vec![],
        argv: vec!["true".into()],
    }];
    assert_eq!(c.run().reason, "selection mismatch");
}

#[test]
fn absent_selection_preserves_legacy_serialization() {
    assert!(
        !String::from_utf8(serde_json::to_vec(&recipe()).unwrap())
            .unwrap()
            .contains("test_selection")
    );
    assert!(
        !String::from_utf8(record().to_bytes().unwrap())
            .unwrap()
            .contains("selections")
    );
}

#[test]
fn validation_bundle_exact_bytes_bounds_and_identity() {
    use agentscm::local_validation::bundle::{EvidencePairV1, ValidationBundleV1};
    let (envelope, record) = comment::parse(&Case::new().body()).unwrap();
    let mut spaced = envelope.clone();
    spaced.push(b' ');
    let first = EvidencePairV1::new(&envelope, &record);
    let second = EvidencePairV1::new(&spaced, &record);
    let bundle = ValidationBundleV1 {
        version: 1,
        entries: vec![first.clone(), second.clone()],
    };
    let bytes = bundle.to_bytes().unwrap();
    assert_eq!(
        bytes,
        ValidationBundleV1 {
            version: 1,
            entries: vec![second, first.clone()]
        }
        .to_bytes()
        .unwrap()
    );
    let decoded = ValidationBundleV1::from_slice(&bytes).unwrap();
    assert!(
        decoded
            .entries
            .iter()
            .any(|pair| pair.decode().unwrap().0 == spaced)
    );
    assert!(
        decoded
            .validate_repository("other.example/acme/widget")
            .is_err()
    );
    assert!(
        ValidationBundleV1 {
            version: 1,
            entries: vec![first.clone(), first]
        }
        .to_bytes()
        .is_err()
    );
    assert!(ValidationBundleV1::from_slice(br#"{"version":2,"entries":[]}"#).is_err());
    assert!(
        ValidationBundleV1::from_slice(br#"{"version":1,"entries":[],"unknown":true}"#).is_err()
    );
    let mut corrupt = record.clone();
    corrupt.push(b' ');
    assert!(EvidencePairV1::new(&envelope, &corrupt).decode().is_err());
    assert!(
        EvidencePairV1::new(&envelope, &record[..record.len() - 1])
            .decode()
            .is_err()
    );
}

#[test]
fn validation_evidence_durable_receipts_pagination_and_revocation() {
    use agentscm::hub::connection::NewConnection;
    use agentscm::hub::store::{self, ForgeIdentity, NewAgentToken};
    use agentscm::hub::validation_evidence::{
        ValidationSummaryConfig, persist_bundle, read_bundle,
    };
    use agentscm::local_validation::bundle::{EvidencePairV1, ValidationBundleV1};
    let temp = tempfile::tempdir().unwrap();
    let store = store::open(&format!(
        "sqlite://{}",
        temp.path().join("hub.db").display()
    ))
    .unwrap();
    let identity = ForgeIdentity {
        provider: "github".into(),
        host: "github.example".into(),
        user_id: 123,
    };
    let user = store.upsert_user(&identity, "synthetic").unwrap();
    let connection = NewConnection {
        id: format!("con_{}", "1".repeat(32)),
        provider: identity.provider.clone(),
        host: identity.host.clone(),
        kind: "app".into(),
        app_id: Some(1),
        app_slug: Some("synthetic".into()),
        acts_as: None,
    };
    store.insert_connection(&connection, "synthetic").unwrap();
    let repo = store
        .upsert_repository(
            &connection.id,
            &identity.host,
            77,
            "acme/widget",
            None,
            Some(1),
        )
        .unwrap();
    let now = agentscm::hub::store::unix_now();
    let token = store
        .insert_agent_token(&NewAgentToken {
            owner: user.id.clone(),
            name: "synthetic".into(),
            repository: repo.full_name.clone(),
            digest: store::agent_token_digest(&"a".repeat(64)).unwrap(),
            expires_at: now + 3600,
            repository_id: Some(77),
            read_only: false,
        })
        .unwrap();
    let backend = agentscm::backend::LocalBackend::open(temp.path().join("objects")).unwrap();
    let mut case = Case::new();
    case.record.subject.repository = "github.example/acme/widget".into();
    case.policy.repository = case.record.subject.repository.clone();
    case.registry.repository = case.record.subject.repository.clone();
    let (envelope, record) = comment::parse(&case.body()).unwrap();
    let bundle = ValidationBundleV1 {
        version: 1,
        entries: vec![EvidencePairV1::new(&envelope, &record)],
    };
    let durable =
        persist_bundle(&backend, &repo.id, "github.example/acme/widget", &bundle).unwrap();
    assert!(
        store
            .receive_validation_evidence(&token, 1, &durable, now)
            .is_err()
    );
    let mut config = ValidationSummaryConfig {
        repository: repo.id.clone(),
        connection: connection.id.clone(),
        enabled: true,
    };
    store.configure_validation_summary(&config).unwrap();
    assert_eq!(
        store.validation_summary_config(&repo.id).unwrap(),
        Some(config.clone())
    );
    assert!(
        store
            .receive_validation_evidence(&token, 0, &durable, now)
            .is_err()
    );
    let first = store
        .receive_validation_evidence(&token, 1, &durable, now)
        .unwrap();
    assert_eq!(first.state, "received_unverified");
    assert_eq!(first.generation, 1);
    let concurrent: Vec<_> = (0..4)
        .map(|_| {
            let store = store.clone();
            let token = token.clone();
            let durable = durable.clone();
            std::thread::spawn(move || {
                store
                    .receive_validation_evidence(&token, 1, &durable, now)
                    .unwrap()
            })
        })
        .collect();
    for worker in concurrent {
        assert_eq!(worker.join().unwrap(), first);
    }
    let reader = store
        .insert_agent_token(&NewAgentToken {
            owner: user.id.clone(),
            name: "reader".into(),
            repository: repo.full_name.clone(),
            digest: store::agent_token_digest(&"b".repeat(64)).unwrap(),
            expires_at: now + 3600,
            repository_id: Some(77),
            read_only: true,
        })
        .unwrap();
    assert!(
        store
            .receive_validation_evidence(&reader, 1, &durable, now)
            .is_err()
    );
    assert_eq!(
        store
            .validation_evidence_history(&reader, &repo.id, 1, 0, 10, now)
            .unwrap()
            .entries
            .len(),
        1
    );

    assert_eq!(
        first,
        store
            .receive_validation_evidence(&token, 1, &durable, now + 1)
            .unwrap()
    );
    assert_eq!(
        read_bundle(&backend, &first).unwrap().to_bytes().unwrap(),
        bundle.to_bytes().unwrap()
    );
    let mut second_envelope = envelope.clone();
    second_envelope.push(b' ');
    let second_bundle = ValidationBundleV1 {
        version: 1,
        entries: vec![EvidencePairV1::new(&second_envelope, &record)],
    };
    let second = persist_bundle(
        &backend,
        &repo.id,
        "github.example/acme/widget",
        &second_bundle,
    )
    .unwrap();
    assert_eq!(
        store
            .receive_validation_evidence(&token, 1, &second, now)
            .unwrap()
            .generation,
        2
    );
    let page = store
        .validation_evidence_history(&token, &repo.id, 1, 0, 1, now)
        .unwrap();
    assert_eq!(page.entries, vec![first.clone()]);
    assert_eq!(page.next_after, Some(1));
    let next = store
        .validation_evidence_history(&token, &repo.id, 1, 1, 1, now)
        .unwrap();
    assert_eq!(next.entries[0].generation, 2);
    assert_eq!(next.next_after, None);
    assert!(
        store
            .validation_evidence_history(&token, &repo.id, 1, 0, 101, now)
            .is_err()
    );
    let wrong = persist_bundle(&backend, &repo.id, "other.example/acme/widget", &bundle);
    assert!(wrong.is_err());
    config.enabled = false;
    store.configure_validation_summary(&config).unwrap();
    assert!(
        store
            .receive_validation_evidence(&token, 1, &durable, now)
            .is_err()
    );
    assert_eq!(
        store
            .validation_evidence_history(&token, &repo.id, 1, 0, 10, now)
            .unwrap()
            .entries
            .len(),
        2
    );
    store.revoke_agent_token(&token).unwrap();
    assert!(
        store
            .validation_evidence_history(&token, &repo.id, 1, 0, 10, now)
            .is_err()
    );
    assert!(
        store
            .receive_validation_evidence(&token, 1, &durable, now)
            .is_err()
    );
    std::fs::remove_dir_all(temp.path().join("objects")).unwrap();
    assert!(read_bundle(&backend, &first).is_err());
}
