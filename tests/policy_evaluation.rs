//! Synthetic conditional-record fixtures only. These tests do not claim that
//! an execution occurred or that any supplied snapshot is authenticated/current.

use agentscm::Error;
use agentscm::checks::CheckState;
use agentscm::context::{
    ChangeId, DependencyClaim, DependencyState, InputIdentity, RequiredEvidence, ReviewContext,
};
use agentscm::evidence::{
    ArtifactAvailability, ArtifactInput, ArtifactRecord, CoverageClaim, EvaluationClock,
    EvidenceEnvelope, EvidenceOutcome, EvidencePayload, ExecutionBasis, OutputCommitment,
    QualificationClaim, RecoveryState, SuppliedArtifact, TerminalState, VerificationReport,
    sign_evidence, verify_evidence,
};
use agentscm::policy::*;
use ed25519_dalek::SigningKey;
use sha2::{Digest, Sha256};

const NOW: u64 = 5_000;
const RECORD: &[u8] = b"synthetic terminal record";
const DEPENDENCY_PROOF: &[u8] = b"synthetic dependency observation";

fn hash(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn fixed(byte: u8) -> String {
    format!("{byte:02x}").repeat(32)
}

fn key_hex(key: &SigningKey) -> String {
    key.verifying_key()
        .as_bytes()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

#[derive(Clone)]
struct SyntheticIssuer {
    id: String,
    key: SigningKey,
    trust_domain: String,
    common_control: CommonControl,
    selected: bool,
}

impl SyntheticIssuer {
    fn known(id: &str, byte: u8, trust_domain: &str, roots: &[&str], selected: bool) -> Self {
        Self {
            id: id.into(),
            key: SigningKey::from_bytes(&[byte; 32]),
            trust_domain: trust_domain.into(),
            common_control: CommonControl::Known {
                roots: roots.iter().map(|root| (*root).to_owned()).collect(),
            },
            selected,
        }
    }
}

#[derive(Clone, Copy)]
enum RecordSupply {
    Full,
    Partial,
    Redacted,
    Unavailable,
    HashMismatch,
}

struct Fixture {
    policy: InstalledPolicy,
    context: ReviewContext,
    registry: IssuerRegistrySnapshot,
    attempts: CurrentAttemptSnapshot,
    dependencies: DependencySnapshot,
    qualifications: QualificationSnapshot,
    reports: Vec<VerificationReport>,
    issuers: Vec<SyntheticIssuer>,
}

impl Fixture {
    fn source(issuers: Vec<SyntheticIssuer>, minimum_independent_issuers: u32) -> Self {
        let trusted_issuer_ids = issuers
            .iter()
            .filter(|issuer| issuer.selected)
            .map(|issuer| issuer.id.clone())
            .collect();
        let policy = InstalledPolicy {
            version: 1,
            name: "protected-main".into(),
            repository: "repo-1".into(),
            target: "main".into(),
            epoch: 7,
            required_checks: vec![CheckRequirement {
                requirement_id: "source-integrity".into(),
                check_kind: "source_paths/v1".into(),
                recipe_sha256: fixed(1),
                toolchain_sha256: fixed(2),
                environment_sha256: fixed(3),
                execution: RequiredExecution::SourceIntegrity,
                trusted_issuer_ids,
                minimum_independent_issuers,
                required_scope: vec!["src/lib.rs".into()],
                require_complete_coverage: true,
                required_artifacts: vec!["record".into()],
                maximum_age_ms: 2_000,
            }],
            required_dependencies: Vec::new(),
            require_complete_dependency_inventory: false,
            minimum_retention_ms: 1_000,
        };
        let context = context(Vec::new());
        let context_sha256 = context.digest().unwrap();
        let registry = registry(&issuers, "source_paths/v1");
        let dependencies = DependencySnapshot {
            version: 1,
            repository: "repo-1".into(),
            generation: 1,
            observed_at_ms: 3_000,
            expires_at_ms: 10_000,
            inventory_complete: true,
            bindings: Vec::new(),
        };
        let qualifications = QualificationSnapshot {
            version: 1,
            repository: "repo-1".into(),
            epoch: 1,
            qualifications: Vec::new(),
        };
        let mut fixture = Self {
            policy,
            context,
            registry,
            attempts: CurrentAttemptSnapshot {
                version: 1,
                repository: "repo-1".into(),
                context_sha256,
                inventory_complete: true,
                attempts: Vec::new(),
            },
            dependencies,
            qualifications,
            reports: Vec::new(),
            issuers,
        };
        fixture.rebuild_all(EvaluationClock::Known { unix_ms: NOW });
        fixture
    }

    fn evaluation(&self, clock: EvaluationClock) -> PolicyEvaluation {
        evaluate_policy(
            &self.policy,
            PolicyInputs {
                context: &self.context,
                registry: &self.registry,
                current_attempts: &self.attempts,
                dependencies: &self.dependencies,
                qualifications: &self.qualifications,
                clock,
                evidence: &self.reports,
            },
        )
        .unwrap()
    }

    fn rebuild_all(&mut self, clock: EvaluationClock) {
        let selected: Vec<SyntheticIssuer> = self
            .issuers
            .iter()
            .filter(|issuer| issuer.selected)
            .cloned()
            .collect();
        self.reports.clear();
        self.attempts.attempts.clear();
        self.attempts.context_sha256 = self.context.digest().unwrap();
        for (index, issuer) in selected.iter().enumerate() {
            let (report, attempt) = make_evidence(
                self,
                issuer,
                &format!("attempt-{index}"),
                clock,
                RecordSupply::Full,
                |_| {},
            );
            self.reports.push(report);
            self.attempts.attempts.push(attempt);
        }
    }

    fn rebuild_one<F>(&mut self, clock: EvaluationClock, supply: RecordSupply, mutate: F)
    where
        F: FnOnce(&mut EvidencePayload),
    {
        let issuer = self
            .issuers
            .iter()
            .find(|issuer| issuer.selected)
            .unwrap()
            .clone();
        self.reports.clear();
        self.attempts.attempts.clear();
        self.attempts.context_sha256 = self.context.digest().unwrap();
        let (report, attempt) = make_evidence(self, &issuer, "attempt-0", clock, supply, mutate);
        self.reports.push(report);
        self.attempts.attempts.push(attempt);
    }
}

fn context(dependencies: Vec<DependencyClaim>) -> ReviewContext {
    ReviewContext {
        version: 1,
        change: ChangeId {
            repository: "repo-1".into(),
            id: "change-1".into(),
        },
        change_version: 4,
        revision: fixed(4),
        base: None,
        target: "main".into(),
        policy_epoch: 7,
        intent: "synthetic conditional policy evaluation".into(),
        acceptance_criteria: vec!["evaluate exact installed requirements".into()],
        scope: vec!["src/lib.rs".into()],
        exclusions: Vec::new(),
        omissions: Vec::new(),
        dependencies,
        recipe: InputIdentity::Digest { sha256: fixed(5) },
        toolchain: InputIdentity::Digest { sha256: fixed(6) },
        environment: InputIdentity::Digest { sha256: fixed(7) },
        required_evidence: Vec::new(),
        required_capabilities: vec!["agentscm.review-context/v1".into()],
    }
}

fn registry(issuers: &[SyntheticIssuer], check_kind: &str) -> IssuerRegistrySnapshot {
    IssuerRegistrySnapshot {
        version: 1,
        repository: "repo-1".into(),
        epoch: 4,
        issuers: issuers
            .iter()
            .map(|issuer| {
                let public_key_hex = key_hex(&issuer.key);
                IssuerRecord {
                    issuer_id: issuer.id.clone(),
                    trust_domain: issuer.trust_domain.clone(),
                    common_control: issuer.common_control.clone(),
                    allowed_check_kinds: vec![check_kind.into()],
                    keys: vec![IssuerKeyRecord {
                        key_id: issuer_key_id(&public_key_hex).unwrap(),
                        public_key_hex,
                        not_before_ms: 100,
                        not_after_ms: 20_000,
                        revoked_at_ms: None,
                    }],
                }
            })
            .collect(),
    }
}

fn make_evidence<F>(
    fixture: &Fixture,
    issuer: &SyntheticIssuer,
    attempt_id: &str,
    clock: EvaluationClock,
    supply: RecordSupply,
    mutate: F,
) -> (VerificationReport, CurrentAttemptBinding)
where
    F: FnOnce(&mut EvidencePayload),
{
    let bound = fixture
        .policy
        .derive_requirements(&fixture.context)
        .unwrap();
    assert_eq!(bound.len(), 1);
    let requirement = &bound[0];
    let dependency_snapshot_sha256 = if fixture.policy.required_dependencies.is_empty()
        && !fixture.policy.require_complete_dependency_inventory
    {
        None
    } else {
        Some(fixture.dependencies.digest().unwrap())
    };
    let qualification_claim = match &requirement.requirement.execution {
        RequiredExecution::SourceIntegrity => None,
        RequiredExecution::QualifiedCommand {
            profile,
            qualification_sha256,
        }
        | RequiredExecution::QualifiedModel {
            profile,
            qualification_sha256,
        } => Some(QualificationClaim {
            kind: requirement.requirement.check_kind.clone(),
            profile: profile.clone(),
            pin_sha256: qualification_sha256.clone(),
        }),
    };
    let mut artifacts = vec![ArtifactRecord {
        artifact_id: "record".into(),
        media_type: "application/json".into(),
        sha256: hash(RECORD),
        bytes: RECORD.len() as u64,
        claimed_availability: ArtifactAvailability::Full,
        expires_at_ms: Some(12_000),
    }];
    if fixture
        .context
        .dependencies
        .iter()
        .any(|dependency| matches!(&dependency.state, DependencyState::Observed { .. }))
    {
        artifacts.push(ArtifactRecord {
            artifact_id: "dependency-proof".into(),
            media_type: "application/octet-stream".into(),
            sha256: hash(DEPENDENCY_PROOF),
            bytes: DEPENDENCY_PROOF.len() as u64,
            claimed_availability: ArtifactAvailability::Full,
            expires_at_ms: Some(12_000),
        });
    }
    let issuer_record = fixture
        .registry
        .issuers
        .iter()
        .find(|record| record.issuer_id == issuer.id)
        .unwrap();
    let mut payload = EvidencePayload {
        version: 1,
        requirement_id: requirement.requirement.requirement_id.clone(),
        check_kind: requirement.requirement.check_kind.clone(),
        subject: requirement.subject.clone(),
        installed_policy_sha256: fixture.policy.digest().unwrap(),
        installed_policy_epoch: fixture.policy.epoch,
        issuer_registry_sha256: fixture.registry.digest().unwrap(),
        dependency_snapshot_sha256,
        attempt_id: attempt_id.into(),
        run_generation: 1,
        request_sha256: fixed(8),
        limits_sha256: fixed(9),
        terminal_record_sha256: hash(RECORD),
        issuer_id: issuer.id.clone(),
        key_id: issuer_record.keys[0].key_id.clone(),
        issued_at_ms: 4_000,
        expires_at_ms: 12_000,
        terminal_state: TerminalState::Success,
        recovery_state: RecoveryState::NotRequired,
        outcome: EvidenceOutcome::Pass,
        execution_basis: ExecutionBasis::ControllerObserved,
        qualification_claim,
        coverage: CoverageClaim {
            scope: requirement.requirement.required_scope.clone(),
            expected_count: requirement.requirement.required_scope.len() as u32,
            observed_count: requirement.requirement.required_scope.len() as u32,
            inventory_complete: true,
            omissions: Vec::new(),
        },
        dependencies: fixture.context.dependencies.clone(),
        artifacts,
        output: OutputCommitment {
            stdout_sha256: hash(b""),
            stdout_bytes: 0,
            stderr_sha256: hash(b""),
            stderr_bytes: 0,
            truncated: false,
        },
    };
    mutate(&mut payload);
    let envelope: EvidenceEnvelope = sign_evidence(payload, &issuer.key).unwrap();
    let mut supplied = Vec::new();
    supplied.push(ArtifactInput {
        artifact_id: "record",
        supplied: match supply {
            RecordSupply::Full => SuppliedArtifact::Full(RECORD),
            RecordSupply::Partial => SuppliedArtifact::Partial(RECORD),
            RecordSupply::Redacted => SuppliedArtifact::Redacted,
            RecordSupply::Unavailable => SuppliedArtifact::Unavailable,
            RecordSupply::HashMismatch => SuppliedArtifact::Full(b"wrong record"),
        },
    });
    if envelope
        .payload
        .artifacts
        .iter()
        .any(|artifact| artifact.artifact_id == "dependency-proof")
    {
        supplied.push(ArtifactInput {
            artifact_id: "dependency-proof",
            supplied: SuppliedArtifact::Full(DEPENDENCY_PROOF),
        });
    }
    let report = verify_evidence(
        &serde_json::to_vec(&envelope).unwrap(),
        &fixture.registry,
        clock,
        &supplied,
    )
    .unwrap();
    let attempt = CurrentAttemptBinding {
        attempt_id: attempt_id.into(),
        requirement_id: requirement.requirement.requirement_id.clone(),
        check_kind: requirement.requirement.check_kind.clone(),
        subject: requirement.subject.clone(),
        durable_version: 2,
        run_generation: 1,
        request_sha256: fixed(8),
        limits_sha256: fixed(9),
        state: CheckState::Success,
        evidence_sha256: Some(report.envelope_sha256().into()),
        terminal_record_sha256: Some(hash(RECORD)),
    };
    (report, attempt)
}

fn one_issuer() -> Vec<SyntheticIssuer> {
    vec![SyntheticIssuer::known(
        "issuer-a",
        11,
        "domain-a",
        &["root-a"],
        true,
    )]
}

fn check_issues(evaluation: &PolicyEvaluation) -> &[PolicyIssue] {
    &evaluation.checks[0].issues
}

#[test]
fn exact_source_requirement_with_real_signature_is_conditionally_satisfied() {
    let mut fixture = Fixture::source(one_issuer(), 1);
    let pending = AttemptView {
        state: CheckState::Pending,
        version: 0,
        run_generation: 0,
        lease_expires_at_ms: None,
    };
    let AttemptTransitionOutcome::Planned(claim) = plan_attempt_transition(
        &pending,
        pending.fence(),
        AttemptAction::ClaimRun {
            lease_expires_at_ms: NOW + 100,
        },
        Some(NOW),
    )
    .unwrap() else {
        panic!("claim refused")
    };
    let running = AttemptView {
        state: claim.next_state,
        version: claim.next_fence.version,
        run_generation: claim.next_fence.run_generation,
        lease_expires_at_ms: claim.next_lease_expires_at_ms,
    };
    let AttemptTransitionOutcome::Planned(completion) = plan_attempt_transition(
        &running,
        running.fence(),
        AttemptAction::Complete {
            state: CheckState::Success,
        },
        Some(NOW),
    )
    .unwrap() else {
        panic!("completion refused")
    };
    assert_eq!(completion.next_fence.version, 2);
    assert_eq!(completion.next_fence.run_generation, 1);
    fixture.rebuild_one(
        EvaluationClock::Known { unix_ms: NOW },
        RecordSupply::Full,
        |payload| {
            payload.run_generation = running.run_generation;
        },
    );
    fixture.attempts.attempts[0].durable_version = completion.next_fence.version;
    fixture.attempts.attempts[0].run_generation = completion.next_fence.run_generation;
    assert!(fixture.context.required_evidence.is_empty());
    assert_eq!(
        fixture
            .policy
            .derive_requirements(&fixture.context)
            .unwrap()
            .len(),
        1
    );

    let evaluation = fixture.evaluation(EvaluationClock::Known { unix_ms: NOW });

    assert_eq!(evaluation.verdict, PolicyVerdict::Satisfied);
    assert_eq!(evaluation.basis, EvaluationBasis::SuppliedRecordsOnly);
    assert!(!evaluation.escalation_required);
    assert_eq!(evaluation.checks[0].verdict, PolicyVerdict::Satisfied);
    assert_eq!(evaluation.checks[0].independent_issuers, 1);
    assert_eq!(evaluation.checks[0].selected_evidence_sha256.len(), 1);
}

#[test]
fn repeated_receipts_count_distinct_issuers_within_the_separate_evidence_cap() {
    let mut fixture = Fixture::source(one_issuer(), 1);
    let issuer = fixture.issuers[0].clone();
    fixture.reports.clear();
    fixture.attempts.attempts.clear();
    for index in 0..129 {
        let (report, attempt) = make_evidence(
            &fixture,
            &issuer,
            &format!("attempt-{index}"),
            EvaluationClock::Known { unix_ms: NOW },
            RecordSupply::Full,
            |_| {},
        );
        fixture.reports.push(report);
        fixture.attempts.attempts.push(attempt);
    }
    assert!(fixture.attempts.digest().is_ok());
    let evaluation = fixture.evaluation(EvaluationClock::Known { unix_ms: NOW });
    assert_eq!(evaluation.verdict, PolicyVerdict::Satisfied);
    assert_eq!(evaluation.checks[0].independent_issuers, 1);
    assert_eq!(evaluation.checks[0].selected_evidence_sha256.len(), 129);
}

#[test]
fn installed_and_signed_identifiers_use_the_same_bounded_grammar() {
    let mut fixture = Fixture::source(
        vec![SyntheticIssuer::known(
            "org.example:issuer/team",
            11,
            "domain-a",
            &["root-a"],
            true,
        )],
        1,
    );
    fixture.policy.required_checks[0].requirement_id = "agentscm.checks:source-paths/v1".into();
    fixture.rebuild_one(
        EvaluationClock::Known { unix_ms: NOW },
        RecordSupply::Full,
        |payload| {
            payload.attempt_id = "controller.example:attempt/one".into();
        },
    );
    fixture.attempts.attempts[0].attempt_id = "controller.example:attempt/one".into();
    assert_eq!(
        fixture
            .evaluation(EvaluationClock::Known { unix_ms: NOW })
            .verdict,
        PolicyVerdict::Satisfied
    );
    for bad in ["contains space", "nul\0byte", "nonascii-é"] {
        let mut payload = fixture.reports[0]
            .verified_receipt()
            .unwrap()
            .payload()
            .clone();
        payload.requirement_id = bad.into();
        assert!(sign_evidence(payload, &fixture.issuers[0].key).is_err());
    }
}

#[test]
fn candidate_cannot_remove_or_replace_installed_requirements() {
    let mut fixture = Fixture::source(one_issuer(), 1);
    assert_eq!(
        fixture
            .evaluation(EvaluationClock::Known { unix_ms: NOW })
            .verdict,
        PolicyVerdict::Satisfied
    );

    fixture.context.required_evidence = vec![RequiredEvidence {
        identity: "candidate-extra".into(),
        digest: InputIdentity::Digest { sha256: fixed(1) },
    }];
    fixture.rebuild_all(EvaluationClock::Known { unix_ms: NOW });
    let unsupported = fixture.evaluation(EvaluationClock::Known { unix_ms: NOW });
    assert_eq!(unsupported.verdict, PolicyVerdict::NotSatisfied);
    assert!(
        unsupported
            .issues
            .contains(&PolicyIssue::CandidateRequirementUnsupported)
    );

    fixture.context.required_evidence = vec![RequiredEvidence {
        identity: "source-integrity".into(),
        digest: InputIdentity::Digest { sha256: fixed(15) },
    }];
    fixture.rebuild_all(EvaluationClock::Known { unix_ms: NOW });
    let mismatch = fixture.evaluation(EvaluationClock::Known { unix_ms: NOW });
    assert_eq!(mismatch.verdict, PolicyVerdict::NotSatisfied);
    assert!(
        mismatch
            .issues
            .contains(&PolicyIssue::CandidateRequirementMismatch)
    );
}

#[test]
fn common_control_aliases_and_unselected_transitive_bridge_do_not_count_twice() {
    let same_domain = vec![
        SyntheticIssuer::known("issuer-a", 11, "shared", &["root-a"], true),
        SyntheticIssuer::known("issuer-b", 12, "shared", &["root-b"], true),
    ];
    let fixture = Fixture::source(same_domain, 2);
    let evaluation = fixture.evaluation(EvaluationClock::Known { unix_ms: NOW });
    assert_eq!(evaluation.verdict, PolicyVerdict::NotSatisfied);
    assert_eq!(evaluation.checks[0].independent_issuers, 1);
    assert!(check_issues(&evaluation).contains(&PolicyIssue::IndependenceInsufficient));

    let transitive = vec![
        SyntheticIssuer::known("issuer-a", 21, "domain-a", &["root-a"], true),
        SyntheticIssuer::known(
            "issuer-bridge",
            22,
            "domain-bridge",
            &["root-a", "root-c"],
            false,
        ),
        SyntheticIssuer::known("issuer-c", 23, "domain-c", &["root-c"], true),
    ];
    let fixture = Fixture::source(transitive, 2);
    let evaluation = fixture.evaluation(EvaluationClock::Known { unix_ms: NOW });
    assert_eq!(evaluation.checks[0].independent_issuers, 1);
    assert!(check_issues(&evaluation).contains(&PolicyIssue::IndependenceInsufficient));
}

#[test]
fn unknown_control_keeps_independence_indeterminate() {
    let mut issuer = SyntheticIssuer::known("issuer-a", 11, "domain-a", &["root-a"], true);
    issuer.common_control = CommonControl::Unknown {};
    let fixture = Fixture::source(vec![issuer], 1);
    let evaluation = fixture.evaluation(EvaluationClock::Known { unix_ms: NOW });
    assert_eq!(evaluation.verdict, PolicyVerdict::Indeterminate);
    assert!(check_issues(&evaluation).contains(&PolicyIssue::IndependenceUnknown));
}

#[test]
fn signed_wrong_subject_context_and_policy_are_denied() {
    for (mutation, issue) in [
        (0u8, PolicyIssue::SubjectMismatch),
        (1u8, PolicyIssue::SubjectMismatch),
        (2u8, PolicyIssue::PolicyMismatch),
    ] {
        let mut fixture = Fixture::source(one_issuer(), 1);
        fixture.rebuild_one(
            EvaluationClock::Known { unix_ms: NOW },
            RecordSupply::Full,
            |payload| match mutation {
                0 => payload.subject.revision = fixed(14),
                1 => payload.subject.context_digest = fixed(14),
                _ => payload.installed_policy_sha256 = fixed(14),
            },
        );
        let evaluation = fixture.evaluation(EvaluationClock::Known { unix_ms: NOW });
        assert_eq!(evaluation.verdict, PolicyVerdict::NotSatisfied);
        assert!(check_issues(&evaluation).contains(&issue));
    }
}

#[test]
fn cancellation_recovery_generation_changes_and_current_failure_veto_old_success() {
    for state in [
        CheckState::Cancelled,
        CheckState::Incomplete,
        CheckState::Fail,
    ] {
        let mut fixture = Fixture::source(one_issuer(), 1);
        fixture.attempts.attempts[0].state = state;
        fixture.attempts.attempts[0].durable_version += 1;
        fixture.attempts.attempts[0].run_generation += 1;
        let evaluation = fixture.evaluation(EvaluationClock::Known { unix_ms: NOW });
        assert_eq!(evaluation.verdict, PolicyVerdict::NotSatisfied);
        assert!(check_issues(&evaluation).contains(&PolicyIssue::StaleAttempt));
        if state == CheckState::Fail {
            assert!(check_issues(&evaluation).contains(&PolicyIssue::CurrentFailure));
        }
    }
}

#[test]
fn missing_evidence_and_incomplete_attempt_inventory_are_indeterminate() {
    let mut fixture = Fixture::source(one_issuer(), 1);
    fixture.reports.clear();
    let missing = fixture.evaluation(EvaluationClock::Known { unix_ms: NOW });
    assert_eq!(missing.verdict, PolicyVerdict::Indeterminate);
    assert!(check_issues(&missing).contains(&PolicyIssue::MissingEvidence));

    let mut fixture = Fixture::source(one_issuer(), 1);
    fixture.attempts.inventory_complete = false;
    let incomplete = fixture.evaluation(EvaluationClock::Known { unix_ms: NOW });
    assert_eq!(incomplete.verdict, PolicyVerdict::Indeterminate);
    assert!(
        incomplete
            .issues
            .contains(&PolicyIssue::AttemptInventoryIncomplete)
    );
}

#[test]
fn verification_report_cannot_be_reused_under_another_clock_or_registry_snapshot() {
    let fixture = Fixture::source(one_issuer(), 1);
    let changed_clock = fixture.evaluation(EvaluationClock::Known { unix_ms: NOW + 1 });
    assert_eq!(changed_clock.verdict, PolicyVerdict::NotSatisfied);
    assert!(check_issues(&changed_clock).contains(&PolicyIssue::StaleVerification));

    let mut fixture = Fixture::source(one_issuer(), 1);
    fixture.registry.epoch += 1;
    let changed_registry = fixture.evaluation(EvaluationClock::Known { unix_ms: NOW });
    assert_eq!(changed_registry.verdict, PolicyVerdict::NotSatisfied);
    assert!(check_issues(&changed_registry).contains(&PolicyIssue::StaleVerification));
}

#[test]
fn uncertain_clock_never_becomes_satisfied() {
    let mut fixture = Fixture::source(one_issuer(), 1);
    fixture.rebuild_all(EvaluationClock::Uncertain);
    let evaluation = fixture.evaluation(EvaluationClock::Uncertain);
    assert_eq!(evaluation.verdict, PolicyVerdict::Indeterminate);
    assert!(evaluation.issues.contains(&PolicyIssue::ClockUncertain));
}

#[test]
fn expiry_revocation_and_artifact_availability_fail_closed() {
    let mut expired = Fixture::source(one_issuer(), 1);
    expired.rebuild_one(
        EvaluationClock::Known { unix_ms: NOW },
        RecordSupply::Full,
        |payload| payload.expires_at_ms = NOW,
    );
    let evaluation = expired.evaluation(EvaluationClock::Known { unix_ms: NOW });
    assert!(check_issues(&evaluation).contains(&PolicyIssue::EvidenceExpired));

    let mut revoked = Fixture::source(one_issuer(), 1);
    revoked.registry.issuers[0].keys[0].revoked_at_ms = Some(NOW);
    revoked.rebuild_all(EvaluationClock::Known { unix_ms: NOW });
    let evaluation = revoked.evaluation(EvaluationClock::Known { unix_ms: NOW });
    assert!(check_issues(&evaluation).contains(&PolicyIssue::UntrustedIssuer));

    for supply in [
        RecordSupply::Partial,
        RecordSupply::Redacted,
        RecordSupply::Unavailable,
    ] {
        let mut fixture = Fixture::source(one_issuer(), 1);
        fixture.rebuild_one(EvaluationClock::Known { unix_ms: NOW }, supply, |_| {});
        let evaluation = fixture.evaluation(EvaluationClock::Known { unix_ms: NOW });
        assert_eq!(evaluation.verdict, PolicyVerdict::Indeterminate);
        assert!(check_issues(&evaluation).contains(&PolicyIssue::ArtifactUnavailable));
    }
    let mut mismatch = Fixture::source(one_issuer(), 1);
    mismatch.rebuild_one(
        EvaluationClock::Known { unix_ms: NOW },
        RecordSupply::HashMismatch,
        |_| {},
    );
    let evaluation = mismatch.evaluation(EvaluationClock::Known { unix_ms: NOW });
    assert_eq!(evaluation.verdict, PolicyVerdict::NotSatisfied);
    assert!(check_issues(&evaluation).contains(&PolicyIssue::ArtifactMismatch));
}

#[test]
fn receipt_and_artifact_retention_are_both_required() {
    let mut expired_artifact = Fixture::source(one_issuer(), 1);
    expired_artifact.rebuild_one(
        EvaluationClock::Known { unix_ms: NOW },
        RecordSupply::Full,
        |payload| payload.artifacts[0].expires_at_ms = Some(NOW),
    );
    let evaluation = expired_artifact.evaluation(EvaluationClock::Known { unix_ms: NOW });
    assert_eq!(evaluation.verdict, PolicyVerdict::NotSatisfied);
    assert!(check_issues(&evaluation).contains(&PolicyIssue::ArtifactExpired));

    let mut missing = Fixture::source(one_issuer(), 1);
    missing.rebuild_one(
        EvaluationClock::Known { unix_ms: NOW },
        RecordSupply::Full,
        |payload| payload.artifacts[0].expires_at_ms = None,
    );
    let evaluation = missing.evaluation(EvaluationClock::Known { unix_ms: NOW });
    assert_eq!(evaluation.verdict, PolicyVerdict::Indeterminate);
    assert!(check_issues(&evaluation).contains(&PolicyIssue::RetentionUnspecified));

    let mut short = Fixture::source(one_issuer(), 1);
    short.rebuild_one(
        EvaluationClock::Known { unix_ms: NOW },
        RecordSupply::Full,
        |payload| {
            payload.expires_at_ms = NOW + 500;
            payload.artifacts[0].expires_at_ms = Some(NOW + 500);
        },
    );
    let evaluation = short.evaluation(EvaluationClock::Known { unix_ms: NOW });
    assert_eq!(evaluation.verdict, PolicyVerdict::NotSatisfied);
    assert!(check_issues(&evaluation).contains(&PolicyIssue::EvidenceRetentionTooShort));
}

fn qualified_fixture(execution: RequiredExecution) -> Fixture {
    let mut fixture = Fixture::source(one_issuer(), 1);
    let (check_kind, profile, proof) = match &execution {
        RequiredExecution::QualifiedCommand {
            profile,
            qualification_sha256,
        } => ("command/v1", profile.clone(), qualification_sha256.clone()),
        RequiredExecution::QualifiedModel {
            profile,
            qualification_sha256,
        } => (
            "model_review/v1",
            profile.clone(),
            qualification_sha256.clone(),
        ),
        RequiredExecution::SourceIntegrity => unreachable!(),
    };
    fixture.policy.required_checks[0].check_kind = check_kind.into();
    fixture.policy.required_checks[0].execution = execution;
    fixture.registry.issuers[0].allowed_check_kinds = vec![check_kind.into()];
    fixture.qualifications.qualifications = vec![InstalledQualification {
        check_kind: check_kind.into(),
        profile,
        proof_sha256: proof,
        authorized_issuer_ids: vec!["issuer-a".into()],
        not_before_ms: 100,
        not_after_ms: 20_000,
        revoked_at_ms: None,
    }];
    fixture.rebuild_all(EvaluationClock::Known { unix_ms: NOW });
    fixture
}

#[test]
fn exact_qualified_command_and_model_pins_can_satisfy_conditionally() {
    for execution in [
        RequiredExecution::QualifiedCommand {
            profile: "sandbox-v1".into(),
            qualification_sha256: fixed(10),
        },
        RequiredExecution::QualifiedModel {
            profile: "model-runtime-v1".into(),
            qualification_sha256: fixed(11),
        },
    ] {
        let fixture = qualified_fixture(execution);
        assert_eq!(
            fixture
                .evaluation(EvaluationClock::Known { unix_ms: NOW })
                .verdict,
            PolicyVerdict::Satisfied
        );
    }
}

#[test]
fn signed_fixture_unqualified_and_worker_assertions_do_not_establish_qualification() {
    for basis in [
        ExecutionBasis::Fixture,
        ExecutionBasis::Unqualified,
        ExecutionBasis::WorkerAsserted,
    ] {
        let mut fixture = qualified_fixture(RequiredExecution::QualifiedCommand {
            profile: "sandbox-v1".into(),
            qualification_sha256: fixed(10),
        });
        fixture.rebuild_one(
            EvaluationClock::Known { unix_ms: NOW },
            RecordSupply::Full,
            |payload| payload.execution_basis = basis,
        );
        let evaluation = fixture.evaluation(EvaluationClock::Known { unix_ms: NOW });
        assert_eq!(evaluation.verdict, PolicyVerdict::NotSatisfied);
        assert!(check_issues(&evaluation).contains(&PolicyIssue::ExecutionUnqualified));
    }
}

#[test]
fn missing_qualification_is_indeterminate() {
    let mut missing = qualified_fixture(RequiredExecution::QualifiedModel {
        profile: "model-runtime-v1".into(),
        qualification_sha256: fixed(11),
    });
    missing.qualifications.qualifications.clear();
    let evaluation = missing.evaluation(EvaluationClock::Known { unix_ms: NOW });
    assert_eq!(evaluation.verdict, PolicyVerdict::Indeterminate);
    assert!(check_issues(&evaluation).contains(&PolicyIssue::QualificationUnavailable));
}

#[test]
fn qualification_claim_authorization_and_time_failures_are_explicit_denials() {
    let execution = || RequiredExecution::QualifiedCommand {
        profile: "sandbox-v1".into(),
        qualification_sha256: fixed(10),
    };

    let mut mismatched = qualified_fixture(execution());
    mismatched.rebuild_one(
        EvaluationClock::Known { unix_ms: NOW },
        RecordSupply::Full,
        |payload| payload.qualification_claim.as_mut().unwrap().pin_sha256 = fixed(15),
    );
    let evaluation = mismatched.evaluation(EvaluationClock::Known { unix_ms: NOW });
    assert_eq!(evaluation.verdict, PolicyVerdict::NotSatisfied);
    assert!(check_issues(&evaluation).contains(&PolicyIssue::QualificationMismatch));

    let mut unpermitted = qualified_fixture(execution());
    unpermitted.qualifications.qualifications[0].authorized_issuer_ids =
        vec!["different-issuer".into()];
    let evaluation = unpermitted.evaluation(EvaluationClock::Known { unix_ms: NOW });
    assert_eq!(evaluation.verdict, PolicyVerdict::NotSatisfied);
    assert!(check_issues(&evaluation).contains(&PolicyIssue::QualificationIssuerUnpermitted));

    let mut not_yet_valid = qualified_fixture(execution());
    not_yet_valid.qualifications.qualifications[0].not_before_ms = NOW + 1;
    let evaluation = not_yet_valid.evaluation(EvaluationClock::Known { unix_ms: NOW });
    assert_eq!(evaluation.verdict, PolicyVerdict::NotSatisfied);
    assert!(check_issues(&evaluation).contains(&PolicyIssue::QualificationNotYetValid));

    let mut expired = qualified_fixture(execution());
    expired.qualifications.qualifications[0].not_after_ms = NOW;
    let evaluation = expired.evaluation(EvaluationClock::Known { unix_ms: NOW });
    assert_eq!(evaluation.verdict, PolicyVerdict::NotSatisfied);
    assert!(check_issues(&evaluation).contains(&PolicyIssue::QualificationExpired));

    let mut revoked = qualified_fixture(execution());
    revoked.qualifications.qualifications[0].revoked_at_ms = Some(NOW);
    let evaluation = revoked.evaluation(EvaluationClock::Known { unix_ms: NOW });
    assert_eq!(evaluation.verdict, PolicyVerdict::NotSatisfied);
    assert!(check_issues(&evaluation).contains(&PolicyIssue::QualificationRevoked));
}

fn dependency_fixture(state: DependencyState, minimum_basis: DependencyBasis) -> Fixture {
    let revision = fixed(12);
    let mut fixture = Fixture::source(one_issuer(), 1);
    fixture.context = context(vec![DependencyClaim {
        identity: "dependency-a".into(),
        state,
    }]);
    fixture.policy.required_dependencies = vec![DependencyRequirement {
        identity: "dependency-a".into(),
        revision_sha256: revision.clone(),
        minimum_basis,
    }];
    fixture.policy.require_complete_dependency_inventory = true;
    fixture.dependencies.bindings = vec![CurrentDependencyBinding {
        identity: "dependency-a".into(),
        revision_sha256: revision,
    }];
    fixture.rebuild_all(EvaluationClock::Known { unix_ms: NOW });
    fixture
}

#[test]
fn dependency_basis_and_full_observation_proof_are_distinct() {
    let declared = dependency_fixture(DependencyState::Declared {}, DependencyBasis::Resolved);
    let evaluation = declared.evaluation(EvaluationClock::Known { unix_ms: NOW });
    assert_eq!(evaluation.verdict, PolicyVerdict::Indeterminate);
    assert!(
        evaluation
            .issues
            .contains(&PolicyIssue::DependencyBasisInsufficient)
    );

    let resolved = dependency_fixture(
        DependencyState::Resolved {
            revision: fixed(12),
        },
        DependencyBasis::Resolved,
    );
    assert_eq!(
        resolved
            .evaluation(EvaluationClock::Known { unix_ms: NOW })
            .verdict,
        PolicyVerdict::Satisfied
    );

    let observed = dependency_fixture(
        DependencyState::Observed {
            revision: Some(fixed(12)),
            evidence_digest: hash(DEPENDENCY_PROOF),
        },
        DependencyBasis::Observed,
    );
    assert_eq!(
        observed
            .evaluation(EvaluationClock::Known { unix_ms: NOW })
            .verdict,
        PolicyVerdict::Satisfied
    );
}

#[test]
fn observed_dependency_proof_requires_the_full_retention_interval() {
    for (expiry, expected_verdict, expected_issue) in [
        (
            None,
            PolicyVerdict::Indeterminate,
            Some(PolicyIssue::RetentionUnspecified),
        ),
        (
            Some(NOW + 999),
            PolicyVerdict::NotSatisfied,
            Some(PolicyIssue::EvidenceRetentionTooShort),
        ),
        (
            Some(NOW),
            PolicyVerdict::NotSatisfied,
            Some(PolicyIssue::ArtifactExpired),
        ),
        (Some(NOW + 1_000), PolicyVerdict::Satisfied, None),
    ] {
        let mut fixture = dependency_fixture(
            DependencyState::Observed {
                revision: Some(fixed(12)),
                evidence_digest: hash(DEPENDENCY_PROOF),
            },
            DependencyBasis::Observed,
        );
        fixture.rebuild_one(
            EvaluationClock::Known { unix_ms: NOW },
            RecordSupply::Full,
            |payload| {
                payload
                    .artifacts
                    .iter_mut()
                    .find(|a| a.artifact_id == "dependency-proof")
                    .unwrap()
                    .expires_at_ms = expiry;
            },
        );
        let evaluation = fixture.evaluation(EvaluationClock::Known { unix_ms: NOW });
        assert_eq!(evaluation.verdict, expected_verdict, "expiry {expiry:?}");
        if let Some(issue) = expected_issue {
            assert!(evaluation.issues.contains(&issue));
            assert!(
                evaluation
                    .issues
                    .contains(&PolicyIssue::DependencyObservationUnavailable)
            );
        }
    }
}

#[test]
fn valid_alternative_dependency_proof_does_not_keep_rejected_retention_issues() {
    let mut fixture = dependency_fixture(
        DependencyState::Observed {
            revision: Some(fixed(12)),
            evidence_digest: hash(DEPENDENCY_PROOF),
        },
        DependencyBasis::Observed,
    );
    let mut payload = fixture.reports[0]
        .verified_receipt()
        .unwrap()
        .payload()
        .clone();
    let proof = payload
        .artifacts
        .iter_mut()
        .find(|a| a.artifact_id == "dependency-proof")
        .unwrap();
    proof.expires_at_ms = Some(NOW + 999);
    let mut retained = proof.clone();
    retained.artifact_id = "dependency-proof-valid".into();
    retained.expires_at_ms = Some(NOW + 1_000);
    payload.artifacts.push(retained);
    let envelope = sign_evidence(payload, &fixture.issuers[0].key).unwrap();
    let report = verify_evidence(
        &serde_json::to_vec(&envelope).unwrap(),
        &fixture.registry,
        EvaluationClock::Known { unix_ms: NOW },
        &[
            ArtifactInput {
                artifact_id: "record",
                supplied: SuppliedArtifact::Full(RECORD),
            },
            ArtifactInput {
                artifact_id: "dependency-proof",
                supplied: SuppliedArtifact::Full(DEPENDENCY_PROOF),
            },
            ArtifactInput {
                artifact_id: "dependency-proof-valid",
                supplied: SuppliedArtifact::Full(DEPENDENCY_PROOF),
            },
        ],
    )
    .unwrap();
    fixture.attempts.attempts[0].evidence_sha256 = Some(report.envelope_sha256().into());
    fixture.reports = vec![report];
    let evaluation = fixture.evaluation(EvaluationClock::Known { unix_ms: NOW });
    assert_eq!(evaluation.verdict, PolicyVerdict::Satisfied);
    assert!(evaluation.issues.is_empty());
}

#[test]
fn dependency_snapshot_revision_generation_inventory_and_proof_stay_current() {
    let state = DependencyState::Observed {
        revision: Some(fixed(12)),
        evidence_digest: hash(DEPENDENCY_PROOF),
    };
    let mut wrong_revision = dependency_fixture(state.clone(), DependencyBasis::Observed);
    wrong_revision.dependencies.bindings[0].revision_sha256 = fixed(13);
    let evaluation = wrong_revision.evaluation(EvaluationClock::Known { unix_ms: NOW });
    assert_eq!(evaluation.verdict, PolicyVerdict::NotSatisfied);
    assert!(evaluation.issues.contains(&PolicyIssue::DependencyMismatch));

    let mut changed_generation = dependency_fixture(state.clone(), DependencyBasis::Observed);
    changed_generation.dependencies.generation += 1;
    let evaluation = changed_generation.evaluation(EvaluationClock::Known { unix_ms: NOW });
    assert_eq!(evaluation.verdict, PolicyVerdict::Indeterminate);
    assert!(check_issues(&evaluation).contains(&PolicyIssue::DependencySnapshotStale));

    let mut partial_inventory = dependency_fixture(state.clone(), DependencyBasis::Observed);
    partial_inventory.dependencies.inventory_complete = false;
    partial_inventory.rebuild_all(EvaluationClock::Known { unix_ms: NOW });
    let evaluation = partial_inventory.evaluation(EvaluationClock::Known { unix_ms: NOW });
    assert_eq!(evaluation.verdict, PolicyVerdict::Indeterminate);
    assert!(
        evaluation
            .issues
            .contains(&PolicyIssue::DependencyInventoryIncomplete)
    );

    let mut missing_proof = dependency_fixture(state, DependencyBasis::Observed);
    missing_proof.rebuild_one(
        EvaluationClock::Known { unix_ms: NOW },
        RecordSupply::Full,
        |payload| {
            payload
                .artifacts
                .retain(|artifact| artifact.artifact_id != "dependency-proof");
        },
    );
    let evaluation = missing_proof.evaluation(EvaluationClock::Known { unix_ms: NOW });
    assert_eq!(evaluation.verdict, PolicyVerdict::Indeterminate);
    assert!(
        evaluation
            .issues
            .contains(&PolicyIssue::DependencyObservationUnavailable)
    );
}

#[test]
fn canonical_policy_and_snapshot_permutations_are_stable() {
    let mut fixture = Fixture::source(
        vec![
            SyntheticIssuer::known("issuer-a", 11, "domain-a", &["root-a"], true),
            SyntheticIssuer::known("issuer-b", 12, "domain-b", &["root-b"], true),
        ],
        1,
    );
    let mut second_requirement = fixture.policy.required_checks[0].clone();
    second_requirement.requirement_id = "source-integrity-second".into();
    fixture.policy.required_checks.push(second_requirement);
    fixture.policy.required_checks.reverse();
    fixture.policy.required_checks[0]
        .trusted_issuer_ids
        .reverse();
    let first_policy = fixture.policy.digest().unwrap();
    fixture.policy.required_checks.reverse();
    fixture.policy.required_checks[1]
        .trusted_issuer_ids
        .reverse();
    assert_eq!(fixture.policy.digest().unwrap(), first_policy);

    let attempts = fixture.attempts.digest().unwrap();
    fixture.attempts.attempts.reverse();
    assert_eq!(fixture.attempts.digest().unwrap(), attempts);

    fixture.dependencies.bindings = vec![
        CurrentDependencyBinding {
            identity: "z-dependency".into(),
            revision_sha256: fixed(12),
        },
        CurrentDependencyBinding {
            identity: "a-dependency".into(),
            revision_sha256: fixed(13),
        },
    ];
    let dependencies = fixture.dependencies.digest().unwrap();
    fixture.dependencies.bindings.reverse();
    assert_eq!(fixture.dependencies.digest().unwrap(), dependencies);

    fixture.qualifications.qualifications = vec![
        InstalledQualification {
            check_kind: "command/v1".into(),
            profile: "z-profile".into(),
            proof_sha256: fixed(14),
            authorized_issuer_ids: vec!["issuer-b".into(), "issuer-a".into()],
            not_before_ms: 100,
            not_after_ms: 20_000,
            revoked_at_ms: None,
        },
        InstalledQualification {
            check_kind: "model_review/v1".into(),
            profile: "a-profile".into(),
            proof_sha256: fixed(15),
            authorized_issuer_ids: vec!["issuer-a".into()],
            not_before_ms: 100,
            not_after_ms: 20_000,
            revoked_at_ms: None,
        },
    ];
    let qualifications = fixture.qualifications.digest().unwrap();
    fixture.qualifications.qualifications.reverse();
    fixture.qualifications.qualifications[1]
        .authorized_issuer_ids
        .reverse();
    assert_eq!(fixture.qualifications.digest().unwrap(), qualifications);
}

#[test]
fn duplicate_unknown_and_over_limit_policy_inputs_fail_closed() {
    let fixture = Fixture::source(one_issuer(), 1);
    let mut duplicate = fixture.policy.clone();
    duplicate
        .required_checks
        .push(duplicate.required_checks[0].clone());
    assert!(matches!(duplicate.validate(), Err(Error::Invalid(_))));

    let mut too_many = fixture.policy.clone();
    too_many.required_checks = (0..=MAX_POLICY_CHECKS)
        .map(|index| {
            let mut requirement = fixture.policy.required_checks[0].clone();
            requirement.requirement_id = format!("requirement-{index}");
            requirement
        })
        .collect();
    assert!(matches!(too_many.validate(), Err(Error::Limit(_))));

    let mut value = serde_json::to_value(&fixture.policy).unwrap();
    value["candidate_can_waive"] = serde_json::Value::Bool(true);
    assert!(matches!(
        InstalledPolicy::from_slice(&serde_json::to_vec(&value).unwrap()),
        Err(Error::Json(_))
    ));

    let mut duplicate_snapshot = fixture.dependencies.clone();
    duplicate_snapshot.bindings = vec![
        CurrentDependencyBinding {
            identity: "dependency-a".into(),
            revision_sha256: fixed(12),
        },
        CurrentDependencyBinding {
            identity: "dependency-a".into(),
            revision_sha256: fixed(12),
        },
    ];
    assert!(matches!(
        duplicate_snapshot.validate(),
        Err(Error::Invalid(_))
    ));

    let mut attempt_value = serde_json::to_value(&fixture.attempts).unwrap();
    attempt_value["authenticated"] = serde_json::Value::Bool(true);
    assert!(matches!(
        CurrentAttemptSnapshot::from_slice(&serde_json::to_vec(&attempt_value).unwrap()),
        Err(Error::Json(_))
    ));

    let mut dependency_value = serde_json::to_value(&fixture.dependencies).unwrap();
    dependency_value["authenticated"] = serde_json::Value::Bool(true);
    assert!(matches!(
        DependencySnapshot::from_slice(&serde_json::to_vec(&dependency_value).unwrap()),
        Err(Error::Json(_))
    ));

    let mut qualification_value = serde_json::to_value(&fixture.qualifications).unwrap();
    qualification_value["authenticated"] = serde_json::Value::Bool(true);
    assert!(matches!(
        QualificationSnapshot::from_slice(&serde_json::to_vec(&qualification_value).unwrap()),
        Err(Error::Json(_))
    ));

    let oversized = vec![b' '; MAX_POLICY_BYTES + 1];
    assert!(matches!(
        CurrentAttemptSnapshot::from_slice(&oversized),
        Err(Error::Limit(_))
    ));
    assert!(matches!(
        DependencySnapshot::from_slice(&oversized),
        Err(Error::Limit(_))
    ));
    assert!(matches!(
        QualificationSnapshot::from_slice(&oversized),
        Err(Error::Limit(_))
    ));

    let template = fixture.attempts.attempts[0].clone();
    let mut too_many_attempts = fixture.attempts.clone();
    too_many_attempts.attempts = (0..=256)
        .map(|index| {
            let mut attempt = template.clone();
            attempt.attempt_id = format!("attempt-{index}");
            attempt
        })
        .collect();
    assert!(matches!(too_many_attempts.validate(), Err(Error::Limit(_))));
}

#[test]
fn current_attempt_snapshot_rejects_malformed_pending_and_running_rows() {
    let fixture = Fixture::source(one_issuer(), 1);
    let mut snapshot = fixture.attempts.clone();
    let row = &mut snapshot.attempts[0];
    row.state = CheckState::Pending;
    row.durable_version = 0;
    row.run_generation = 0;
    assert!(matches!(
        CurrentAttemptSnapshot::from_slice(&serde_json::to_vec(&snapshot).unwrap()),
        Err(Error::Invalid(_))
    ));

    snapshot.attempts[0].evidence_sha256 = None;
    snapshot.attempts[0].terminal_record_sha256 = None;
    assert_eq!(
        CurrentAttemptSnapshot::from_slice(&serde_json::to_vec(&snapshot).unwrap()).unwrap(),
        snapshot
    );

    snapshot.attempts[0].durable_version = 1;
    assert!(matches!(
        CurrentAttemptSnapshot::from_slice(&serde_json::to_vec(&snapshot).unwrap()),
        Err(Error::Invalid(_))
    ));

    let mut running = fixture.attempts.clone();
    running.attempts[0].state = CheckState::Running;
    assert!(matches!(
        CurrentAttemptSnapshot::from_slice(&serde_json::to_vec(&running).unwrap()),
        Err(Error::Invalid(_))
    ));
}
