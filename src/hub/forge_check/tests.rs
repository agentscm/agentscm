//! Synthetic fixtures only; no network, Docker or Git subprocess.
use super::*;
use crate::authority::Authority;
use crate::authority::workflows::WorkflowRequirement;
use crate::context::{ChangeId, ReviewContext};
use crate::model::FileChange;
use crate::skill_workflow::{
    self, SkillReference, StepAction, StepDefinition, WorkflowDefinition, WorkflowSource,
};
use crate::store::{Engine, digest};
use serde_json::json;
use tempfile::TempDir;

const SKILL: &str = "skills/check/SKILL.md";

struct Fixture {
    _root: TempDir,
    profile: AgentTaskProfile,
    context: ReviewContext,
    subject: CandidateSubject,
}

impl Fixture {
    fn new(install: bool) -> Self {
        let root = TempDir::new().unwrap();
        let native = root.path().join("native");
        std::fs::create_dir(&native).unwrap();
        let engine = Engine::open(&native).unwrap();
        let revision = engine
            .checkpoint(
                None,
                &[
                    FileChange {
                        path: SKILL.into(),
                        content: Some(b"# Check synthetic source\n".to_vec()),
                    },
                    FileChange {
                        path: "src/example.rs".into(),
                        content: Some(b"fn example() {}\n".to_vec()),
                    },
                ],
            )
            .unwrap();
        let mut authority = Authority::open(&native).unwrap();
        let repository = authority.repository_id().unwrap();
        let definition = WorkflowDefinition {
            version: 1,
            name: "synthetic-forge-check".into(),
            source: WorkflowSource {
                repository: repository.clone(),
                revision: revision.clone(),
                policy_epoch: 0,
            },
            skill: SkillReference {
                entrypoint: SKILL.into(),
                resources: vec![],
            },
            steps: vec![StepDefinition {
                id: "source".into(),
                needs: vec![],
                required: true,
                action: StepAction::SourcePaths {
                    expectations: vec![crate::checks::SourcePathExpectation {
                        path: "src/example.rs".into(),
                        sha256: digest(b"fn example() {}\n"),
                    }],
                },
            }],
        };
        let plan = skill_workflow::compile(&native, &definition).unwrap();
        let required = WorkflowRequirement::from_plan("main", &plan).unwrap();
        let requirement_digest = required.digest().unwrap();
        if install {
            authority.install_workflow_requirement(&required).unwrap();
        }
        let policy_epoch = authority.policy_epoch().unwrap();
        let context: ReviewContext = serde_json::from_value(json!({
            "version":1,"change":{"repository":repository,"id":"work"},"change_version":1,
            "revision":revision,"base":null,"target":"main","policy_epoch":policy_epoch,
            "intent":"Verify synthetic source","acceptance_criteria":["Source matches"],
            "scope":["src/example.rs"],"exclusions":[],"omissions":[],"dependencies":[],
            "recipe":{"state":"digest","sha256":"a".repeat(64)},
            "toolchain":{"state":"digest","sha256":"b".repeat(64)},
            "environment":{"state":"digest","sha256":"c".repeat(64)},
            "required_evidence":[],"required_capabilities":["agentscm.review-context/v1"]
        }))
        .unwrap();
        authority.start_change(&context).unwrap();
        drop(authority);
        let subject = CandidateSubject {
            id: "candidate".into(),
            attachment: "attachment".into(),
            endpoint: "endpoint".into(),
            output: "agentscm/work".into(),
            change: context.change.clone(),
            change_version: 1,
            context_digest: context.digest().unwrap(),
            policy_epoch,
            revision,
            target: "main".into(),
            baseline_commit: "b".repeat(40),
            parent_commit: "c".repeat(40),
            source_commit: "d".repeat(40),
            source_fingerprint: "e".repeat(64),
            comparison_fingerprint: "f".repeat(64),
        };
        let profile = AgentTaskProfile {
            repository_id: "sample/project".into(),
            profile_id: "checks".into(),
            native_repository: repository,
            data_dir: native,
            target: "main".into(),
            requirement_digest,
            publish_forge_status: true,
        };
        Self {
            _root: root,
            profile,
            context,
            subject,
        }
    }

    fn qualify(&self) -> String {
        let root = &self.profile.data_dir;
        let requirement = Authority::open_read_only(root)
            .unwrap()
            .workflow_requirement("main")
            .unwrap()
            .unwrap();
        let plan = skill_workflow::compile(root, &requirement.instantiate(&self.context).unwrap())
            .unwrap();
        skill_workflow::start(root, "run-1", &plan).unwrap();
        skill_workflow::advance(root, "run-1", 8).unwrap();
        skill_workflow::runtime::finalize(root, "run-1", &self.context).unwrap()
    }
}

#[test]
fn no_installed_requirement_is_not_qualified() {
    let fixture = Fixture::new(false);
    assert_eq!(
        native_state(&fixture.profile, &fixture.subject),
        NativeState::NotQualified
    );
}

#[test]
fn installed_requirement_without_receipt_is_not_qualified() {
    let fixture = Fixture::new(true);
    assert_eq!(
        native_state(&fixture.profile, &fixture.subject),
        NativeState::NotQualified
    );
}

#[test]
fn requirement_differing_from_pin_is_never_qualified() {
    let mut fixture = Fixture::new(true);
    fixture.qualify();
    fixture.profile.requirement_digest = "0".repeat(64);
    assert!(matches!(
        native_state(&fixture.profile, &fixture.subject),
        NativeState::Stale(_)
    ));
}

#[test]
fn qualified_subject_carries_receipt_and_tree() {
    let fixture = Fixture::new(true);
    let receipt = fixture.qualify();
    let engine = Engine::open_existing(&fixture.profile.data_dir).unwrap();
    let tree_oid = crate::git_export::native_tree_oid(&engine, &fixture.subject.revision).unwrap();
    assert_eq!(
        native_state(&fixture.profile, &fixture.subject),
        NativeState::Qualified {
            receipt,
            requirement_digest: fixture.profile.requirement_digest.clone(),
            tree_oid,
        }
    );
}

#[test]
fn advanced_policy_makes_a_qualified_subject_stale() {
    let fixture = Fixture::new(true);
    fixture.qualify();
    let mut authority = Authority::open(&fixture.profile.data_dir).unwrap();
    authority
        .advance_policy(fixture.subject.policy_epoch)
        .unwrap();
    drop(authority);
    assert!(matches!(
        native_state(&fixture.profile, &fixture.subject),
        NativeState::Stale(_)
    ));
}

#[test]
fn moved_change_and_mismatched_subjects_are_stale() {
    let fixture = Fixture::new(true);
    fixture.qualify();
    let changed = |edit: &dyn Fn(&mut CandidateSubject)| {
        let mut subject = fixture.subject.clone();
        edit(&mut subject);
        native_state(&fixture.profile, &subject)
    };
    assert!(matches!(
        changed(&|s| s.revision = "other".into()),
        NativeState::Stale(_)
    ));
    assert!(matches!(
        changed(&|s| s.change_version = 2),
        NativeState::Stale(_)
    ));
    assert!(matches!(
        changed(&|s| s.target = "release".into()),
        NativeState::Stale(_)
    ));
    assert!(matches!(
        changed(&|s| s.change = ChangeId {
            repository: s.change.repository.clone(),
            id: "other".into()
        }),
        NativeState::Stale(_)
    ));
    let mut next = fixture.context.clone();
    next.change_version = 2;
    next.intent = "a later context".into();
    Authority::open(&fixture.profile.data_dir)
        .unwrap()
        .update_change_context(1, &next)
        .unwrap();
    assert!(matches!(
        native_state(&fixture.profile, &fixture.subject),
        NativeState::Stale(_)
    ));
}

#[test]
fn unreadable_store_or_unknown_context_is_unavailable() {
    let fixture = Fixture::new(true);
    fixture.qualify();
    let mut subject = fixture.subject.clone();
    subject.context_digest = "0".repeat(64);
    assert!(matches!(
        native_state(&fixture.profile, &subject),
        NativeState::Unavailable(_)
    ));
    let mut profile = fixture.profile.clone();
    profile.data_dir = profile.data_dir.join("missing");
    assert!(matches!(
        native_state(&profile, &fixture.subject),
        NativeState::Unavailable(_)
    ));
}

const REPO: u64 = 7;

fn subject() -> CandidateSubject {
    Fixture::new(false).subject
}

fn live(subject: &CandidateSubject) -> LiveHead {
    LiveHead {
        open: true,
        head_sha: "1".repeat(40),
        head_repo_id: REPO,
        base_repo_id: REPO,
        base_ref: subject.target.clone(),
        base_tip: subject.baseline_commit.clone(),
        head_tree: "2".repeat(40),
        head_parents: vec![
            subject.parent_commit.clone(),
            subject.baseline_commit.clone(),
        ],
    }
}

fn qualified() -> NativeState {
    NativeState::Qualified {
        receipt: "receipt".into(),
        requirement_digest: "requirement".into(),
        tree_oid: "2".repeat(40),
    }
}

fn verdict(subject: &CandidateSubject, live: &LiveHead, native: &NativeState) -> Verdict {
    evaluate(REPO, subject, &"1".repeat(40), live, native)
}

#[test]
fn success_requires_every_binding() {
    let subject = subject();
    assert_eq!(
        verdict(&subject, &live(&subject), &qualified()),
        Verdict::Success
    );
    // A baseline equal to the parent yields a single-parent commit.
    let mut single = subject.clone();
    single.baseline_commit = single.parent_commit.clone();
    let mut head = live(&single);
    head.head_parents = vec![single.parent_commit.clone()];
    assert_eq!(verdict(&single, &head, &qualified()), Verdict::Success);
}

#[test]
fn every_binding_failure_is_a_non_success_failure() {
    let subject = subject();
    let edits: [&dyn Fn(&mut LiveHead); 12] = [
        &|l| l.open = false,
        &|l| l.head_repo_id = REPO + 1,
        &|l| l.base_repo_id = REPO + 1,
        &|l| l.base_ref = "release".into(),
        &|l| l.base_tip = "9".repeat(40),
        &|l| l.head_sha = "3".repeat(40),
        &|l| l.head_tree = "3".repeat(40),
        &|l| l.head_parents.reverse(),
        &|l| l.head_parents.pop().map(drop).unwrap_or_default(),
        &|l| l.head_parents.push("4".repeat(40)),
        &|l| l.head_sha = "NOT-AN-OID".into(),
        &|l| l.base_tip = String::new(),
    ];
    for (index, edit) in edits.iter().enumerate() {
        let mut head = live(&subject);
        edit(&mut head);
        assert!(
            matches!(verdict(&subject, &head, &qualified()), Verdict::Failure(_)),
            "edit {index}"
        );
    }
    assert!(matches!(
        evaluate(
            REPO,
            &subject,
            &"5".repeat(40),
            &live(&subject),
            &qualified()
        ),
        Verdict::Failure(_)
    ));
    assert!(matches!(
        evaluate(REPO, &subject, "", &live(&subject), &qualified()),
        Verdict::Failure(_)
    ));
}

#[test]
fn native_state_maps_to_status_states() {
    let subject = subject();
    let head = live(&subject);
    assert!(matches!(
        verdict(&subject, &head, &NativeState::NotQualified),
        Verdict::Pending(_)
    ));
    assert!(matches!(
        verdict(&subject, &head, &NativeState::Stale("x")),
        Verdict::Failure(_)
    ));
    assert!(matches!(
        verdict(&subject, &head, &NativeState::Unavailable("x")),
        Verdict::Error(_)
    ));
    // A mismatched pull request is a failure even before qualification exists.
    let mut forked = head.clone();
    forked.head_repo_id = REPO + 1;
    assert!(matches!(
        verdict(&subject, &forked, &NativeState::NotQualified),
        Verdict::Failure(_)
    ));
}

#[test]
fn binding_digest_is_deterministic_and_sensitive() {
    let subject = subject();
    let head = live(&subject);
    let base = binding_digest(REPO, 5, &head, &subject, "native", "req", "receipt");
    assert_eq!(
        base,
        binding_digest(REPO, 5, &head, &subject, "native", "req", "receipt")
    );
    let mut variants = vec![
        binding_digest(REPO + 1, 5, &head, &subject, "native", "req", "receipt"),
        binding_digest(REPO, 6, &head, &subject, "native", "req", "receipt"),
        binding_digest(REPO, 5, &head, &subject, "other", "req", "receipt"),
        binding_digest(REPO, 5, &head, &subject, "native", "req2", "receipt"),
        binding_digest(REPO, 5, &head, &subject, "native", "req", "receipt2"),
        // Field boundaries are length-prefixed.
        binding_digest(REPO, 5, &head, &subject, "nativer", "eq", "receipt"),
    ];
    type Edit = dyn Fn(&mut LiveHead, &mut CandidateSubject);
    let edits: [&Edit; 8] = [
        &|l, _| l.head_sha = "3".repeat(40),
        &|l, _| l.base_ref = "release".into(),
        &|l, _| l.base_tip = "9".repeat(40),
        &|_, s| s.change.id = "other".into(),
        &|_, s| s.change_version = 2,
        &|_, s| s.context_digest = "9".repeat(64),
        &|_, s| s.policy_epoch = 9,
        &|_, s| s.revision = "other".into(),
    ];
    for edit in edits {
        let (mut head, mut subject) = (head.clone(), subject.clone());
        edit(&mut head, &mut subject);
        variants.push(binding_digest(
            REPO, 5, &head, &subject, "native", "req", "receipt",
        ));
    }
    variants.push(base);
    let unique: std::collections::BTreeSet<_> = variants.iter().collect();
    assert_eq!(unique.len(), variants.len());
}

#[test]
fn status_context_validates_target() {
    assert_eq!(status_context("main").unwrap(), "agentscm/workflow/main");
    assert_eq!(
        status_context("release/1.x").unwrap(),
        "agentscm/workflow/release/1.x"
    );
    for bad in ["", "a b", "a..b", "x\n", "é", "a:b", "/a", &"a".repeat(101)] {
        assert!(status_context(bad).is_err(), "{bad:?}");
    }
}
