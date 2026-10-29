#![cfg(unix)]
//! Native dispatch-guard integration; no Docker or provider calls are made.
use std::sync::atomic::{AtomicUsize, Ordering};

use agentscm::authority::Authority;
use agentscm::checks::SourcePathExpectation;
use agentscm::model::FileChange;
use agentscm::skill_workflow::runtime::advance_guarded;
use agentscm::skill_workflow::{
    self, RunState, SkillReference, StepAction, StepDefinition, StepState, WorkflowDefinition,
    WorkflowSource,
};
use agentscm::store::{Engine, digest};

#[test]
fn native_runner_rechecks_authorization_before_each_step() {
    let root = tempfile::tempdir().unwrap();
    let engine = Engine::open(root.path()).unwrap();
    let revision = engine
        .checkpoint(
            None,
            &[
                FileChange {
                    path: "skills/check/SKILL.md".into(),
                    content: Some(b"# Check\n".to_vec()),
                },
                FileChange {
                    path: "src/example.rs".into(),
                    content: Some(b"fn example() {}\n".to_vec()),
                },
            ],
        )
        .unwrap();
    let authority = Authority::open(root.path()).unwrap();
    let definition = WorkflowDefinition {
        version: 1,
        name: "guarded-native".into(),
        source: WorkflowSource {
            repository: authority.repository_id().unwrap(),
            revision,
            policy_epoch: 0,
        },
        skill: SkillReference {
            entrypoint: "skills/check/SKILL.md".into(),
            resources: vec![],
        },
        steps: ["first", "second"]
            .into_iter()
            .map(|id| StepDefinition {
                id: id.into(),
                needs: if id == "second" {
                    vec!["first".into()]
                } else {
                    vec![]
                },
                required: true,
                action: StepAction::SourcePaths {
                    expectations: vec![SourcePathExpectation {
                        path: "src/example.rs".into(),
                        sha256: digest(b"fn example() {}\n"),
                    }],
                },
            })
            .collect(),
    };
    let plan = skill_workflow::compile(root.path(), &definition).unwrap();
    skill_workflow::start(root.path(), "authorization-guard", &plan).unwrap();
    let checks = AtomicUsize::new(0);
    let guard = || {
        if checks.fetch_add(1, Ordering::Relaxed) == 0 {
            Ok(())
        } else {
            Err(agentscm::Error::Conflict(
                "synthetic authorization revoked".into(),
            ))
        }
    };
    assert!(advance_guarded(root.path(), "authorization-guard", 2, None, None, &guard).is_err());
    let retained = skill_workflow::status(root.path(), "authorization-guard").unwrap();
    assert_eq!(retained.state, RunState::Running);
    assert_eq!(retained.steps[0].state, StepState::Succeeded);
    assert_eq!(retained.steps[1].state, StepState::Pending);
    assert_eq!(checks.load(Ordering::Relaxed), 2);
}
