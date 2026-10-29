#![cfg(unix)]

use std::fs;

use agentscm::authority::Authority;
use agentscm::checks::SourcePathExpectation;
use agentscm::context::ReviewContext;
use agentscm::model::FileChange;
use agentscm::skill_workflow::{
    self, SkillReference, StepAction, StepDefinition, WorkflowDefinition, WorkflowSource,
};
use agentscm::store::{Engine, digest};
use agentscm::workspace::{CheckLinkRequest, CheckLinkResult, Workspace};
use agentscm::workspace_timeline_service::WorkspaceTimelineService;
use serde_json::json;
use tempfile::TempDir;

#[test]
fn exact_check_evidence_distinguishes_source_from_tested_and_missing_run() {
    let temp = TempDir::new().unwrap();
    let data = temp.path().join("source");
    let root = temp.path().join("workspace");
    let engine = Engine::open(&data).unwrap();
    let mut authority = Authority::open(&data).unwrap();
    let repository = authority.repository_id().unwrap();
    let put = |path: &str, bytes: &[u8]| FileChange {
        path: path.into(),
        content: Some(bytes.into()),
    };
    let source = engine
        .checkpoint(
            None,
            &[
                put("a.txt", b"source"),
                put("skills/check/SKILL.md", b"# Check\n"),
            ],
        )
        .unwrap();
    let tested = engine
        .checkpoint(Some(&source), &[put("prepared.txt", b"prepared")])
        .unwrap();
    let context: ReviewContext = serde_json::from_value(json!({
        "version":1,"change":{"repository":repository,"id":"work"},"change_version":1,
        "revision":source,"base":null,"target":"main","policy_epoch":0,
        "intent":"Check source","acceptance_criteria":["Source remains readable"],
        "scope":["a.txt"],"exclusions":[],"omissions":[],"dependencies":[],
        "recipe":{"state":"digest","sha256":"a".repeat(64)},
        "toolchain":{"state":"digest","sha256":"b".repeat(64)},
        "environment":{"state":"digest","sha256":"c".repeat(64)},
        "required_evidence":[],"required_capabilities":["agentscm.review-context/v1"]
    }))
    .unwrap();
    authority.start_change(&context).unwrap();
    let workspace = Workspace::create(&engine, &root, context.change.clone(), &source).unwrap();
    let definition = WorkflowDefinition {
        version: 1,
        name: "source-check".into(),
        source: WorkflowSource {
            repository,
            revision: tested.clone(),
            policy_epoch: 0,
        },
        skill: SkillReference {
            entrypoint: "skills/check/SKILL.md".into(),
            resources: vec![],
        },
        steps: vec![StepDefinition {
            id: "verify".into(),
            needs: vec![],
            required: true,
            action: StepAction::SourcePaths {
                expectations: vec![SourcePathExpectation {
                    path: "a.txt".into(),
                    sha256: digest(b"source"),
                }],
            },
        }],
    };
    let plan = skill_workflow::compile(&data, &definition).unwrap();
    let run_id = format!("loop-{}", "d".repeat(59));
    let request = CheckLinkRequest {
        source_revision: source.clone(),
        tested_revision: tested.clone(),
        change: context.change.clone(),
        change_version: 1,
        context_digest: context.digest().unwrap(),
        policy_epoch: 0,
        target: "main".into(),
        target_revision: None,
        target_version: None,
        requirement_digest: "c".repeat(64),
        run_id: run_id.clone(),
    };
    let CheckLinkResult::Recorded { entry } =
        workspace.record_check_start(&engine, &request).unwrap()
    else {
        panic!("link not recorded")
    };
    let service = WorkspaceTimelineService::new(&data, &root, None).unwrap();
    assert_eq!(
        service.entry(entry.id).unwrap()["evidence"]["state"],
        "unavailable"
    );
    skill_workflow::start(&data, &run_id, &plan).unwrap();
    skill_workflow::advance(&data, &run_id, 1).unwrap();
    let view = service.entry(entry.id).unwrap();
    assert_eq!(view["entry"]["source_revision"], source);
    assert_eq!(view["entry"]["tested_revision"], tested);
    assert_eq!(view["evidence"]["state"], "available");
    assert_eq!(view["evidence"]["workflow"]["run"]["state"], "succeeded");
    assert_eq!(
        view["evidence"]["workflow"]["run"]["source"]["revision"],
        tested
    );
    authority.advance_policy(0).unwrap();
    let stale = service.entry(entry.id).unwrap();
    assert_eq!(stale["evidence"]["workflow"]["run"]["state"], "succeeded");
    assert_eq!(
        stale["evidence"]["workflow"]["present_applicability"]["state"],
        "stale"
    );
    let db = rusqlite::Connection::open(root.join(".agentscm-workspace/state.sqlite")).unwrap();
    db.execute(
        "UPDATE timeline_entries SET tested_revision = ?1",
        [&source],
    )
    .unwrap();
    assert_eq!(
        service.entry(entry.id).unwrap()["evidence"]["state"],
        "binding_mismatch"
    );
    fs::remove_dir_all(data.join("skill-workflows")).unwrap();
    assert_eq!(
        service.entry(entry.id).unwrap()["evidence"]["state"],
        "unavailable"
    );
}

#[test]
fn sparse_diff_follows_empty_pages_without_exposing_prefix_neighbours() {
    let temp = TempDir::new().unwrap();
    let data = temp.path().join("source");
    let root = temp.path().join("workspace");
    let engine = Engine::open(&data).unwrap();
    let mut authority = Authority::open(&data).unwrap();
    let repository = authority.repository_id().unwrap();
    let put = |path: &str, bytes: &[u8]| FileChange {
        path: path.into(),
        content: Some(bytes.into()),
    };
    let base = engine
        .checkpoint(
            None,
            &[
                put("aaa/outside.txt", b"old"),
                put("src/a/file.txt", b"base"),
                put("src/ab/file.txt", b"old"),
            ],
        )
        .unwrap();
    let context: ReviewContext = serde_json::from_value(json!({
        "version":1,"change":{"repository":repository,"id":"work"},"change_version":1,
        "revision":base,"base":null,"target":"main","policy_epoch":0,
        "intent":"Inspect selected source","acceptance_criteria":["Selection is preserved"],
        "scope":["src/a"],"exclusions":[],"omissions":[],"dependencies":[],
        "recipe":{"state":"digest","sha256":"a".repeat(64)},
        "toolchain":{"state":"digest","sha256":"b".repeat(64)},
        "environment":{"state":"digest","sha256":"c".repeat(64)},
        "required_evidence":[],"required_capabilities":["agentscm.review-context/v1"]
    }))
    .unwrap();
    authority.start_change(&context).unwrap();
    Workspace::create_scoped(&engine, &root, context.change, &base, &["src/a".into()]).unwrap();
    let service = WorkspaceTimelineService::new(&data, &root, None).unwrap();
    fs::write(root.join("src/a/file.txt"), b"first edit").unwrap();
    let first = service
        .capture(agentscm::workspace::SnapshotTrigger::Manual)
        .unwrap()["outcome"]["entry"]["id"]
        .as_u64()
        .unwrap();
    let advanced = engine
        .checkpoint(
            Some(&base),
            &[
                put("aaa/outside.txt", b"new"),
                put("src/ab/file.txt", b"new"),
            ],
        )
        .unwrap();
    let db = rusqlite::Connection::open(root.join(".agentscm-workspace/state.sqlite")).unwrap();
    db.execute(
        "UPDATE workspace_state SET base_revision=?1, generation=1",
        [&advanced],
    )
    .unwrap();
    drop(db);
    fs::write(root.join("src/a/file.txt"), b"second edit").unwrap();
    let second = service
        .capture(agentscm::workspace::SnapshotTrigger::Manual)
        .unwrap()["outcome"]["entry"]["id"]
        .as_u64()
        .unwrap();
    let mut after = None;
    let mut paths = vec![];
    let mut empty_with_continuation = false;
    for _ in 0..8 {
        let page = service.diff(first, second, 1, after).unwrap();
        let entries = page["diff"]["entries"].as_array().unwrap();
        paths.extend(
            entries
                .iter()
                .map(|entry| entry["path"].as_str().unwrap().to_owned()),
        );
        after = page["diff"]["next"].as_str().map(str::to_owned);
        empty_with_continuation |= entries.is_empty() && after.is_some();
        if after.is_none() {
            break;
        }
    }
    assert!(after.is_none(), "pagination did not terminate");
    assert!(empty_with_continuation);
    assert_eq!(paths, ["src/a/file.txt"]);
}
