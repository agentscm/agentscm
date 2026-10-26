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

fn recipe() -> ValidationRecipe {
    ValidationRecipe {
        recipe_id: "gate".into(),
        commands: vec![RecipeCommand {
            id: "test".into(),
            argv: vec!["cargo".into(), "test".into()],
            timeout_secs: 60,
        }],
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
        recipe_sha256: recipe_hash(),
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
    recipe: ValidationRecipe,
    issued_at_ms: u64,
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
            recipe: recipe(),
            issued_at_ms: ISSUED,
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
            cargo_lock_sha256: &hex(8),
            toolchain_file_sha256: &hex(9),
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
        cargo_lock_sha256: &hex(8),
        toolchain_file_sha256: &hex(9),
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
    assert!(body.contains("**Local validation: reported pass**"));
    assert!(body.contains("Source: <code>repo&#x2d;1</code> at <code>111111111111</code>"));
    assert!(body.contains("the verifier checks the signature and source bindings"));
    assert!(body.contains("Coverage: 1/1 checks reported; 5 omissions."));
    assert!(body.contains("<summary>Coverage omissions (5)</summary>"));
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
fn rendered_comment_warns_when_services_are_missing() {
    let (envelope, _) = comment_bytes();
    let mut missing_service_record = record();
    missing_service_record.services.s3 = false;
    let record_bytes = serde_json::to_vec(&missing_service_record).unwrap();
    let body = comment::render(&envelope, &record_bytes).unwrap();

    assert!(body.contains("Missing services: S3. Coverage is incomplete."));
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
    assert!(body.contains("**Local validation: reported unknown**"));
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
    assert!(body.contains("- <code>s3 tests</code>\n"));
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
        "no attestation for this head",
    );
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
