use std::fs;
use std::path::Path;
use std::process::{Command, Output};

use agentscm::authority::Authority;
use agentscm::checks::{PathObservation, SourcePathExpectation};
use agentscm::model::{EntryChange, EntryKind, FileChange};
use agentscm::skill_workflow::{
    RunState, SkillReference, StepAction, StepDefinition, StepState, WorkflowDefinition,
    WorkflowSource, advance, cancel, compile, start, start_compiled, status, verify_current,
};
use agentscm::store::{Engine, digest};
use rusqlite::{Connection, params};
use serde_json::Value;
use tempfile::TempDir;

const SKILL_PATH: &str = "skills/native-workflow/SKILL.md";
const REFERENCE_PATH: &str = "skills/native-workflow/reference.txt";
const SOURCE_PATH: &str = "src/example.rs";
const SKILL_BYTES: &[u8] = b"# Native source workflow\n";
const REFERENCE_BYTES: &[u8] = b"bounded reference\n";
const SOURCE_BYTES: &[u8] = b"pub fn answer() -> u8 { 42 }\n";

struct Fixture {
    root: TempDir,
    repository: String,
    revision: String,
}

fn put(path: &str, bytes: &[u8]) -> FileChange {
    FileChange {
        path: path.to_owned(),
        content: Some(bytes.to_vec()),
    }
}

fn make_fixture() -> Fixture {
    make_fixture_with_source(SOURCE_BYTES)
}

fn make_fixture_with_source(source: &[u8]) -> Fixture {
    let root = TempDir::new().expect("temporary native root");
    let engine = Engine::open(root.path()).expect("native engine");
    let revision = engine
        .checkpoint(
            None,
            &[
                put(SKILL_PATH, SKILL_BYTES),
                put(REFERENCE_PATH, REFERENCE_BYTES),
                put(SOURCE_PATH, source),
            ],
        )
        .expect("native checkpoint");
    let authority = Authority::open(root.path()).expect("native authority");
    let repository = authority.repository_id().expect("repository ID");
    drop(authority);
    drop(engine);
    Fixture {
        root,
        repository,
        revision,
    }
}

fn journal(fixture: &Fixture) -> Connection {
    Connection::open(fixture.root.path().join("skill-workflows/state.sqlite"))
        .expect("workflow journal")
}

fn source_step(id: &str, needs: &[&str], required: bool, expected: &str) -> StepDefinition {
    StepDefinition {
        id: id.to_owned(),
        needs: needs.iter().map(|value| (*value).to_owned()).collect(),
        required,
        action: StepAction::SourcePaths {
            expectations: vec![SourcePathExpectation {
                path: SOURCE_PATH.to_owned(),
                sha256: expected.to_owned(),
            }],
        },
    }
}

fn capability_step(id: &str, needs: &[&str], required: bool) -> StepDefinition {
    StepDefinition {
        id: id.to_owned(),
        needs: needs.iter().map(|value| (*value).to_owned()).collect(),
        required,
        action: StepAction::RequireCapability {
            capability: "model.review".to_owned(),
        },
    }
}

fn make_definition(fixture: &Fixture, steps: Vec<StepDefinition>) -> WorkflowDefinition {
    WorkflowDefinition {
        version: 1,
        name: "native-source-acceptance".to_owned(),
        source: WorkflowSource {
            repository: fixture.repository.clone(),
            revision: fixture.revision.clone(),
            policy_epoch: 0,
        },
        skill: SkillReference {
            entrypoint: SKILL_PATH.to_owned(),
            resources: vec![REFERENCE_PATH.to_owned()],
        },
        steps,
    }
}

#[test]
fn compiles_native_skill_and_resumes_persisted_source_observations() {
    let fixture = make_fixture();
    let definition = make_definition(
        &fixture,
        vec![
            source_step("first", &[], true, &digest(SOURCE_BYTES)),
            source_step("second", &["first"], true, &digest(SOURCE_BYTES)),
        ],
    );
    let plan = compile(fixture.root.path(), &definition).expect("compiled plan");
    assert_eq!(plan.definition(), &definition);
    assert_eq!(plan.ordered_steps()[0].id, "first");
    assert_eq!(plan.ordered_steps()[1].id, "second");
    assert!(plan.digest().starts_with("sha256:"));
    assert!(plan.skill_digest().starts_with("sha256:"));

    let pending = start(fixture.root.path(), "resume-run", &plan).expect("start");
    assert_eq!(pending.state, RunState::Pending);
    let partial = advance(fixture.root.path(), "resume-run", 1).expect("first advance");
    assert_eq!(partial.state, RunState::Running);
    assert_eq!(partial.steps[0].state, StepState::Succeeded);
    assert_eq!(partial.steps[1].state, StepState::Pending);
    assert_eq!(partial.steps[0].observations[0].path, SOURCE_PATH);
    assert_eq!(
        partial.steps[0].observations[0].sha256,
        digest(SOURCE_BYTES)
    );

    let reopened = status(fixture.root.path(), "resume-run").expect("reopened status");
    assert_eq!(reopened, partial);
    let completed = advance(fixture.root.path(), "resume-run", 1).expect("resume");
    assert_eq!(completed.state, RunState::Succeeded);
    assert!(
        completed
            .steps
            .iter()
            .all(|step| step.state == StepState::Succeeded)
    );
    assert_eq!(completed.admission, "not_assessed");
}

#[test]
fn required_unavailable_capability_blocks_without_fabricating_success() {
    let fixture = make_fixture();
    let definition = make_definition(&fixture, vec![capability_step("review", &[], true)]);
    let plan = compile(fixture.root.path(), &definition).expect("compiled plan");
    start(fixture.root.path(), "blocked-run", &plan).expect("start");
    let blocked = advance(fixture.root.path(), "blocked-run", 1).expect("advance");

    assert_eq!(blocked.state, RunState::Blocked);
    assert_eq!(blocked.steps[0].state, StepState::Blocked);
    assert!(blocked.steps[0].observations.is_empty());
    assert_eq!(blocked.admission, "not_assessed");
}

#[test]
fn optional_unavailable_capability_completes_with_a_visible_omission() {
    let fixture = make_fixture();
    let definition = make_definition(
        &fixture,
        vec![
            source_step("source", &[], true, &digest(SOURCE_BYTES)),
            capability_step("review", &["source"], false),
        ],
    );
    let plan = compile(fixture.root.path(), &definition).expect("compiled plan");
    start(fixture.root.path(), "omitted-run", &plan).expect("start");
    let completed = advance(fixture.root.path(), "omitted-run", 32).expect("advance");

    assert_eq!(completed.state, RunState::CompletedWithOmissions);
    assert_eq!(completed.steps[0].state, StepState::Succeeded);
    assert_eq!(completed.steps[1].state, StepState::Blocked);
    assert!(completed.steps[1].observations.is_empty());
}

#[test]
fn digest_failure_blocks_dependent_step_and_remains_observable() {
    let fixture = make_fixture();
    let definition = make_definition(
        &fixture,
        vec![
            source_step("mismatch", &[], true, &digest(b"different source")),
            source_step("dependent", &["mismatch"], true, &digest(SOURCE_BYTES)),
        ],
    );
    let plan = compile(fixture.root.path(), &definition).expect("compiled plan");
    start(fixture.root.path(), "failed-run", &plan).expect("start");
    let failed = advance(fixture.root.path(), "failed-run", 32).expect("advance");

    assert_eq!(failed.state, RunState::Failed);
    assert_eq!(failed.steps[0].state, StepState::Failed);
    assert_eq!(failed.steps[0].observations.len(), 1);
    assert_eq!(failed.steps[0].observations[0].sha256, digest(SOURCE_BYTES));
    assert_eq!(failed.steps[1].state, StepState::Blocked);
    assert!(failed.steps[1].observations.is_empty());
}

#[test]
fn run_id_retry_is_idempotent_and_a_different_plan_conflicts() {
    let fixture = make_fixture();
    let definition = make_definition(
        &fixture,
        vec![source_step("check", &[], true, &digest(SOURCE_BYTES))],
    );
    let plan = compile(fixture.root.path(), &definition).expect("compiled plan");
    let first = start(fixture.root.path(), "stable-run", &plan).expect("start");
    assert_eq!(
        start(fixture.root.path(), "stable-run", &plan).expect("exact retry"),
        first
    );

    let mut changed = definition;
    changed.name = "different-plan".to_owned();
    let changed = compile(fixture.root.path(), &changed).expect("changed plan");
    assert!(start(fixture.root.path(), "stable-run", &changed).is_err());
    assert_eq!(status(fixture.root.path(), "stable-run").unwrap(), first);
}

#[test]
fn start_compiled_keeps_identity_checks_and_start_still_rejects_stale_plans() {
    let fixture = make_fixture();
    let definition = make_definition(
        &fixture,
        vec![source_step("check", &[], true, &digest(SOURCE_BYTES))],
    );
    let plan = compile(fixture.root.path(), &definition).expect("compiled plan");
    let first = start_compiled(fixture.root.path(), "reuse-run", &plan).expect("start");
    assert_eq!(
        start_compiled(fixture.root.path(), "reuse-run", &plan).expect("exact retry"),
        first
    );
    let mut changed = definition;
    changed.name = "different-plan".to_owned();
    let changed = compile(fixture.root.path(), &changed).expect("changed plan");
    assert!(start_compiled(fixture.root.path(), "reuse-run", &changed).is_err());

    let mut authority = Authority::open(fixture.root.path()).expect("authority");
    authority.advance_policy(0).expect("advance policy");
    drop(authority);
    assert!(start(fixture.root.path(), "stale-run", &plan).is_err());
}

#[test]
fn repository_policy_and_skill_identity_changes_are_detected() {
    let fixture = make_fixture();
    let mut definition = make_definition(
        &fixture,
        vec![source_step("check", &[], true, &digest(SOURCE_BYTES))],
    );
    let original = compile(fixture.root.path(), &definition).expect("original plan");

    definition.source.repository = if fixture.repository == "0".repeat(32) {
        "1".repeat(32)
    } else {
        "0".repeat(32)
    };
    assert!(compile(fixture.root.path(), &definition).is_err());

    let mut authority = Authority::open(fixture.root.path()).expect("authority");
    authority.advance_policy(0).expect("advance policy");
    drop(authority);
    assert!(verify_current(fixture.root.path(), &original).is_err());

    let second_fixture = make_fixture();
    let first_definition = make_definition(
        &second_fixture,
        vec![source_step("check", &[], true, &digest(SOURCE_BYTES))],
    );
    let first_plan = compile(second_fixture.root.path(), &first_definition).expect("first skill");
    let engine = Engine::open(second_fixture.root.path()).expect("engine");
    let changed_revision = engine
        .checkpoint(
            Some(&second_fixture.revision),
            &[put(SKILL_PATH, b"# Changed native source workflow\n")],
        )
        .expect("changed skill checkpoint");
    drop(engine);
    let mut changed_definition = first_definition;
    changed_definition.source.revision = changed_revision;
    let changed_plan =
        compile(second_fixture.root.path(), &changed_definition).expect("changed skill plan");
    assert_ne!(first_plan.skill_digest(), changed_plan.skill_digest());
    assert_ne!(first_plan.digest(), changed_plan.digest());
}

#[test]
fn cancellation_is_terminal_and_preserves_the_recorded_run() {
    let fixture = make_fixture();
    let definition = make_definition(
        &fixture,
        vec![
            source_step("first", &[], true, &digest(SOURCE_BYTES)),
            source_step("second", &["first"], true, &digest(SOURCE_BYTES)),
        ],
    );
    let plan = compile(fixture.root.path(), &definition).expect("compiled plan");
    start(fixture.root.path(), "cancelled-run", &plan).expect("start");
    let cancelled = cancel(fixture.root.path(), "cancelled-run").expect("cancel");
    assert_eq!(cancelled.state, RunState::Cancelled);
    assert!(
        cancelled
            .steps
            .iter()
            .all(|step| step.state == StepState::Cancelled)
    );
    assert_eq!(
        advance(fixture.root.path(), "cancelled-run", 32).expect("terminal retry"),
        cancelled
    );
    assert_eq!(
        status(fixture.root.path(), "cancelled-run").unwrap(),
        cancelled
    );
}

#[test]
fn cancellation_of_only_optional_pending_work_is_consistent() {
    let fixture = make_fixture();
    let definition = make_definition(
        &fixture,
        vec![source_step("optional", &[], false, &digest(SOURCE_BYTES))],
    );
    let plan = compile(fixture.root.path(), &definition).expect("compiled plan");
    start(fixture.root.path(), "optional-cancel", &plan).expect("start");

    let cancelled = cancel(fixture.root.path(), "optional-cancel").expect("cancel");
    assert_eq!(cancelled.state, RunState::Cancelled);
    assert_eq!(cancelled.steps[0].state, StepState::Cancelled);
    assert_eq!(
        status(fixture.root.path(), "optional-cancel").unwrap(),
        cancelled
    );
}

#[test]
fn legacy_cancelled_terminal_record_without_cancelled_steps_is_inspectable() {
    let fixture = make_fixture();
    let definition = make_definition(
        &fixture,
        vec![source_step("check", &[], true, &digest(SOURCE_BYTES))],
    );
    let plan = compile(fixture.root.path(), &definition).expect("compiled plan");
    start(fixture.root.path(), "legacy-cancel", &plan).expect("start");
    advance(fixture.root.path(), "legacy-cancel", 1).expect("complete");
    journal(&fixture)
        .execute(
            "UPDATE workflow_runs SET state='cancelled' WHERE run_id='legacy-cancel'",
            [],
        )
        .unwrap();

    let retained = status(fixture.root.path(), "legacy-cancel").expect("legacy status");
    assert_eq!(retained.state, RunState::Cancelled);
    assert_eq!(retained.steps[0].state, StepState::Succeeded);
}

#[test]
fn source_payload_limit_does_not_charge_native_encoding_overhead() {
    let source = vec![b'x'; 8 * 1024 * 1024];
    let fixture = make_fixture_with_source(&source);
    let definition = make_definition(
        &fixture,
        vec![source_step("boundary", &[], true, &digest(&source))],
    );
    let plan = compile(fixture.root.path(), &definition).expect("compiled boundary plan");
    start(fixture.root.path(), "boundary-run", &plan).expect("start");
    let completed = advance(fixture.root.path(), "boundary-run", 1).expect("advance");

    assert_eq!(completed.state, RunState::Succeeded);
    assert_eq!(completed.steps[0].observations[0].bytes, 8 * 1024 * 1024);
}

#[test]
fn source_payload_over_8_mib_blocks_with_a_bounded_observation() {
    let source = vec![b'x'; 8 * 1024 * 1024 + 1];
    let fixture = make_fixture_with_source(&source);
    let definition = make_definition(
        &fixture,
        vec![source_step("over-limit", &[], true, &digest(&source))],
    );
    let plan = compile(fixture.root.path(), &definition).expect("compiled over-limit plan");
    start(fixture.root.path(), "over-limit-run", &plan).expect("start");
    let blocked = advance(fixture.root.path(), "over-limit-run", 1).expect("advance");

    assert_eq!(blocked.state, RunState::Blocked);
    assert_eq!(blocked.steps[0].state, StepState::Blocked);
    assert!(blocked.steps[0].observations.is_empty());
    assert!(
        blocked.steps[0]
            .detail
            .as_deref()
            .is_some_and(|detail| detail.contains("8 MiB byte limit"))
    );
}

#[test]
fn thirty_two_small_source_expectations_fit_the_encoded_read_ceiling() {
    let root = TempDir::new().expect("temporary native root");
    let engine = Engine::open(root.path()).expect("native engine");
    let mut changes = vec![
        put(SKILL_PATH, SKILL_BYTES),
        put(REFERENCE_PATH, REFERENCE_BYTES),
    ];
    let mut expectations = Vec::new();
    for index in 0..32 {
        let path = format!("src/path-{index:02}.txt");
        let bytes = format!("value-{index:02}").into_bytes();
        expectations.push(SourcePathExpectation {
            path: path.clone(),
            sha256: digest(&bytes),
        });
        changes.push(put(&path, &bytes));
    }
    let revision = engine
        .checkpoint(None, &changes)
        .expect("many-path checkpoint");
    let authority = Authority::open(root.path()).expect("native authority");
    let repository = authority.repository_id().expect("repository ID");
    drop(authority);
    drop(engine);
    let definition = WorkflowDefinition {
        version: 1,
        name: "many-paths".to_owned(),
        source: WorkflowSource {
            repository,
            revision,
            policy_epoch: 0,
        },
        skill: SkillReference {
            entrypoint: SKILL_PATH.to_owned(),
            resources: vec![REFERENCE_PATH.to_owned()],
        },
        steps: vec![StepDefinition {
            id: "many".to_owned(),
            needs: Vec::new(),
            required: true,
            action: StepAction::SourcePaths { expectations },
        }],
    };
    let plan = compile(root.path(), &definition).expect("compile many paths");
    start(root.path(), "many-paths", &plan).expect("start");
    let completed = advance(root.path(), "many-paths", 1).expect("advance");

    assert_eq!(completed.state, RunState::Succeeded);
    assert_eq!(completed.steps[0].observations.len(), 32);
}

#[cfg(unix)]
#[test]
fn start_rejects_a_preexisting_workflow_directory_symlink() {
    use std::os::unix::fs::symlink;

    let fixture = make_fixture();
    let definition = make_definition(
        &fixture,
        vec![source_step("check", &[], true, &digest(SOURCE_BYTES))],
    );
    let plan = compile(fixture.root.path(), &definition).expect("compiled plan");
    let outside = TempDir::new().expect("outside directory");
    symlink(outside.path(), fixture.root.path().join("skill-workflows"))
        .expect("workflow directory symlink");

    assert!(start(fixture.root.path(), "escaped-run", &plan).is_err());
    assert_eq!(fs::read_dir(outside.path()).unwrap().count(), 0);
}

#[cfg(unix)]
#[test]
fn start_rejects_preexisting_journal_child_symlinks() {
    use std::os::unix::fs::symlink;

    for child in ["journal.lock", "state.sqlite"] {
        let fixture = make_fixture();
        let definition = make_definition(
            &fixture,
            vec![source_step("check", &[], true, &digest(SOURCE_BYTES))],
        );
        let plan = compile(fixture.root.path(), &definition).expect("compiled plan");
        let directory = fixture.root.path().join("skill-workflows");
        fs::create_dir(&directory).expect("workflow directory");
        let outside = TempDir::new().expect("outside directory");
        let target = outside.path().join("target");
        fs::write(&target, b"outside").expect("outside file");
        if child == "state.sqlite" {
            fs::write(directory.join("journal.lock"), b"").expect("regular journal lock");
        }
        symlink(&target, directory.join(child)).expect("journal child symlink");

        assert!(
            start(fixture.root.path(), &format!("symlink-{child}"), &plan).is_err(),
            "accepted {child} symlink"
        );
        assert_eq!(fs::read(&target).unwrap(), b"outside");
    }
}

#[test]
fn start_rejects_nonregular_journal_paths() {
    for child in [None, Some("journal.lock"), Some("state.sqlite")] {
        let fixture = make_fixture();
        let definition = make_definition(
            &fixture,
            vec![source_step("check", &[], true, &digest(SOURCE_BYTES))],
        );
        let plan = compile(fixture.root.path(), &definition).expect("compiled plan");
        let directory = fixture.root.path().join("skill-workflows");
        match child {
            None => fs::write(&directory, b"not a directory").expect("workflow path file"),
            Some(child) => {
                fs::create_dir(&directory).expect("workflow directory");
                if child == "state.sqlite" {
                    fs::write(directory.join("journal.lock"), b"").expect("regular journal lock");
                }
                fs::create_dir(directory.join(child)).expect("journal child directory");
            }
        }

        assert!(
            start(fixture.root.path(), "nonregular-run", &plan).is_err(),
            "accepted nonregular journal path {child:?}"
        );
    }
}

#[test]
fn retained_success_requires_exact_source_observations() {
    let fixture = make_fixture();
    let definition = make_definition(
        &fixture,
        vec![source_step("check", &[], true, &digest(SOURCE_BYTES))],
    );
    let plan = compile(fixture.root.path(), &definition).expect("compiled plan");
    start(fixture.root.path(), "forged-source", &plan).expect("start");
    let connection = journal(&fixture);
    connection
        .execute(
            "UPDATE workflow_steps SET state='succeeded',detail='forged' WHERE run_id='forged-source'",
            [],
        )
        .unwrap();
    connection
        .execute(
            "UPDATE workflow_runs SET state='succeeded' WHERE run_id='forged-source'",
            [],
        )
        .unwrap();

    assert!(status(fixture.root.path(), "forged-source").is_err());
}

#[test]
fn retained_capability_step_cannot_claim_success() {
    let fixture = make_fixture();
    let definition = make_definition(&fixture, vec![capability_step("review", &[], true)]);
    let plan = compile(fixture.root.path(), &definition).expect("compiled plan");
    start(fixture.root.path(), "forged-capability", &plan).expect("start");
    let connection = journal(&fixture);
    connection
        .execute(
            "UPDATE workflow_steps SET state='succeeded',detail='forged' WHERE run_id='forged-capability'",
            [],
        )
        .unwrap();
    connection
        .execute(
            "UPDATE workflow_runs SET state='succeeded' WHERE run_id='forged-capability'",
            [],
        )
        .unwrap();

    assert!(status(fixture.root.path(), "forged-capability").is_err());
}

#[test]
fn retained_terminal_run_state_must_match_its_step_results() {
    let fixture = make_fixture();
    let definition = make_definition(
        &fixture,
        vec![source_step("check", &[], true, &digest(SOURCE_BYTES))],
    );
    let plan = compile(fixture.root.path(), &definition).expect("compiled plan");
    start(fixture.root.path(), "wrong-terminal", &plan).expect("start");
    let observations = serde_json::to_vec(&vec![PathObservation {
        path: SOURCE_PATH.to_owned(),
        sha256: digest(SOURCE_BYTES),
        bytes: SOURCE_BYTES.len() as u64,
    }])
    .unwrap();
    let connection = journal(&fixture);
    connection
        .execute(
            "UPDATE workflow_steps SET state='succeeded',generation=1,observations_json=?1,detail='matched' WHERE run_id='wrong-terminal'",
            params![observations],
        )
        .unwrap();
    connection
        .execute(
            "UPDATE workflow_runs SET state='failed' WHERE run_id='wrong-terminal'",
            [],
        )
        .unwrap();

    assert!(status(fixture.root.path(), "wrong-terminal").is_err());
}

#[test]
fn completed_step_requires_successful_declared_dependencies() {
    let fixture = make_fixture();
    let definition = make_definition(
        &fixture,
        vec![
            source_step("first", &[], true, &digest(SOURCE_BYTES)),
            source_step("second", &["first"], true, &digest(SOURCE_BYTES)),
        ],
    );
    let plan = compile(fixture.root.path(), &definition).expect("compiled plan");
    start(fixture.root.path(), "wrong-dependency", &plan).expect("start");
    let observations = serde_json::to_vec(&vec![PathObservation {
        path: SOURCE_PATH.to_owned(),
        sha256: digest(SOURCE_BYTES),
        bytes: SOURCE_BYTES.len() as u64,
    }])
    .unwrap();
    let connection = journal(&fixture);
    connection
        .execute(
            "UPDATE workflow_steps SET state='succeeded',generation=1,observations_json=?1,detail='matched' WHERE run_id='wrong-dependency' AND step_id='second'",
            params![observations],
        )
        .unwrap();
    connection
        .execute(
            "UPDATE workflow_runs SET state='running' WHERE run_id='wrong-dependency'",
            [],
        )
        .unwrap();
    drop(connection);

    assert!(status(fixture.root.path(), "wrong-dependency").is_err());
    assert!(cancel(fixture.root.path(), "wrong-dependency").is_err());
    let state: String = journal(&fixture)
        .query_row(
            "SELECT state FROM workflow_runs WHERE run_id='wrong-dependency'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(state, "running");
}

#[test]
fn retained_plan_digest_and_topological_positions_are_revalidated() {
    let fixture = make_fixture();
    let definition = make_definition(
        &fixture,
        vec![
            source_step("first", &[], true, &digest(SOURCE_BYTES)),
            source_step("second", &["first"], true, &digest(SOURCE_BYTES)),
        ],
    );
    let plan = compile(fixture.root.path(), &definition).expect("compiled plan");
    start(fixture.root.path(), "mutated-digest", &plan).expect("start");
    let connection = journal(&fixture);
    connection
        .execute(
            "UPDATE workflow_runs SET plan_digest=?1 WHERE run_id='mutated-digest'",
            [format!("sha256:{}", "0".repeat(64))],
        )
        .unwrap();
    assert!(status(fixture.root.path(), "mutated-digest").is_err());

    start(fixture.root.path(), "mutated-order", &plan).expect("start order");
    let transaction = connection.unchecked_transaction().unwrap();
    transaction
        .execute(
            "UPDATE workflow_steps SET position=31 WHERE run_id='mutated-order' AND position=0",
            [],
        )
        .unwrap();
    transaction
        .execute(
            "UPDATE workflow_steps SET position=0 WHERE run_id='mutated-order' AND position=1",
            [],
        )
        .unwrap();
    transaction
        .execute(
            "UPDATE workflow_steps SET position=1 WHERE run_id='mutated-order' AND position=31",
            [],
        )
        .unwrap();
    transaction.commit().unwrap();
    assert!(status(fixture.root.path(), "mutated-order").is_err());
    assert!(advance(fixture.root.path(), "mutated-order", 1).is_err());
}

#[test]
fn interrupted_running_source_read_is_safely_retried() {
    let fixture = make_fixture();
    let definition = make_definition(
        &fixture,
        vec![source_step("check", &[], true, &digest(SOURCE_BYTES))],
    );
    let plan = compile(fixture.root.path(), &definition).expect("compiled plan");
    start(fixture.root.path(), "interrupted-run", &plan).expect("start");
    let connection = journal(&fixture);
    connection
        .execute(
            "UPDATE workflow_runs SET state='running' WHERE run_id='interrupted-run'",
            [],
        )
        .unwrap();
    connection
        .execute(
            "UPDATE workflow_steps SET state='running',generation=1,observations_json=?1,detail=NULL WHERE run_id='interrupted-run'",
            [b"[]".as_slice()],
        )
        .unwrap();
    drop(connection);

    let completed = advance(fixture.root.path(), "interrupted-run", 1).expect("resume");
    assert_eq!(completed.state, RunState::Succeeded);
    assert_eq!(completed.steps[0].state, StepState::Succeeded);
}

#[test]
fn terminal_step_commit_before_run_refresh_is_reconciled() {
    let fixture = make_fixture();
    let definition = make_definition(
        &fixture,
        vec![source_step("check", &[], true, &digest(SOURCE_BYTES))],
    );
    let plan = compile(fixture.root.path(), &definition).expect("compiled plan");
    start(fixture.root.path(), "lagged-run", &plan).expect("start");
    let observations = serde_json::to_vec(&vec![PathObservation {
        path: SOURCE_PATH.to_owned(),
        sha256: digest(SOURCE_BYTES),
        bytes: SOURCE_BYTES.len() as u64,
    }])
    .unwrap();
    let connection = journal(&fixture);
    connection
        .execute(
            "UPDATE workflow_steps SET state='succeeded',generation=1,observations_json=?1,detail='matched' WHERE run_id='lagged-run'",
            params![observations],
        )
        .unwrap();
    drop(connection);

    let lagged = status(fixture.root.path(), "lagged-run").expect("lagged status");
    assert_eq!(lagged.state, RunState::Pending);
    assert_eq!(lagged.steps[0].state, StepState::Succeeded);
    let repaired = advance(fixture.root.path(), "lagged-run", 1).expect("repair");
    assert_eq!(repaired.state, RunState::Succeeded);
}

#[test]
fn terminal_step_commit_is_finalized_offline_after_policy_changes() {
    let fixture = make_fixture();
    let definition = make_definition(
        &fixture,
        vec![source_step("check", &[], true, &digest(SOURCE_BYTES))],
    );
    let plan = compile(fixture.root.path(), &definition).expect("compiled plan");
    start(fixture.root.path(), "offline-finalize", &plan).expect("start");
    let observations = serde_json::to_vec(&vec![PathObservation {
        path: SOURCE_PATH.to_owned(),
        sha256: digest(SOURCE_BYTES),
        bytes: SOURCE_BYTES.len() as u64,
    }])
    .unwrap();
    let connection = journal(&fixture);
    connection
        .execute(
            "UPDATE workflow_runs SET state='running' WHERE run_id='offline-finalize'",
            [],
        )
        .unwrap();
    connection
        .execute(
            "UPDATE workflow_steps SET state='succeeded',generation=1,observations_json=?1,detail='matched' WHERE run_id='offline-finalize'",
            params![observations],
        )
        .unwrap();
    drop(connection);
    let mut authority = Authority::open(fixture.root.path()).expect("authority");
    authority.advance_policy(0).expect("advance policy");
    drop(authority);

    let completed = advance(fixture.root.path(), "offline-finalize", 1)
        .expect("offline historical finalization");
    assert_eq!(completed.state, RunState::Succeeded);
}

#[test]
fn unfinished_run_after_policy_change_is_unchanged_and_cannot_advance() {
    let fixture = make_fixture();
    let definition = make_definition(
        &fixture,
        vec![source_step("check", &[], true, &digest(SOURCE_BYTES))],
    );
    let plan = compile(fixture.root.path(), &definition).expect("compiled plan");
    let before = start(fixture.root.path(), "stale-unfinished", &plan).expect("start");
    let mut authority = Authority::open(fixture.root.path()).expect("authority");
    authority.advance_policy(0).expect("advance policy");
    drop(authority);

    assert!(advance(fixture.root.path(), "stale-unfinished", 1).is_err());
    assert_eq!(
        status(fixture.root.path(), "stale-unfinished").unwrap(),
        before
    );
}

#[test]
fn non_regular_source_refusal_remains_clear_in_read_projection() {
    let fixture = make_fixture();
    let engine = Engine::open(fixture.root.path()).unwrap();
    let revision = engine
        .checkpoint_entries(
            Some(&fixture.revision),
            &[EntryChange {
                path: SOURCE_PATH.into(),
                content: Some(SOURCE_BYTES.to_vec()),
                kind: EntryKind::Executable,
            }],
        )
        .unwrap();
    let mut definition = make_definition(
        &fixture,
        vec![source_step("source", &[], true, &digest(SOURCE_BYTES))],
    );
    definition.source.revision = revision;
    let plan = compile(fixture.root.path(), &definition).unwrap();
    start(fixture.root.path(), "nonregular", &plan).unwrap();
    let observed = advance(fixture.root.path(), "nonregular", 32).unwrap();
    assert_eq!(observed.state, RunState::Blocked);
    assert!(
        observed.steps[0]
            .detail
            .as_deref()
            .unwrap()
            .contains("executable")
    );
    let projected =
        agentscm::skill_workflow_service::read(fixture.root.path(), &engine, "nonregular").unwrap();
    assert_eq!(projected.run.steps[0].detail, observed.steps[0].detail);
    assert!(projected.run.steps[0].observations.is_empty());
}

#[test]
fn strict_json_and_semantic_validation_reject_malformed_input() {
    let fixture = make_fixture();
    let definition = make_definition(
        &fixture,
        vec![source_step("check", &[], true, &digest(SOURCE_BYTES))],
    );
    let mut value = serde_json::to_value(&definition).expect("definition JSON");
    value
        .as_object_mut()
        .unwrap()
        .insert("unexpected".to_owned(), Value::Bool(true));
    assert!(serde_json::from_value::<WorkflowDefinition>(value).is_err());
    assert!(
        serde_json::from_str::<StepAction>(
            r#"{"kind":"source_paths","expectations":[],"command":"true"}"#,
        )
        .is_err()
    );

    let mut invalid = definition;
    let StepAction::SourcePaths { expectations } = &mut invalid.steps[0].action else {
        unreachable!();
    };
    expectations[0].sha256 = "ABC".to_owned();
    assert!(compile(fixture.root.path(), &invalid).is_err());
}

fn invoke(data_dir: &Path, arguments: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_agentscm"))
        .arg("--data-dir")
        .arg(data_dir)
        .args(arguments)
        .env("PATH", "")
        .output()
        .expect("invoke skill workflow CLI")
}

fn output_json(output: &Output) -> Value {
    serde_json::from_slice(&output.stdout).expect("JSON stdout")
}

#[test]
fn cli_plans_runs_inspects_resumes_cancels_and_reports_blocking() {
    let fixture = make_fixture();
    let definition = make_definition(
        &fixture,
        vec![source_step("check", &[], true, &digest(SOURCE_BYTES))],
    );
    let definition_path = fixture.root.path().join("workflow.json");
    fs::write(
        &definition_path,
        serde_json::to_vec(&definition).expect("definition JSON"),
    )
    .expect("definition file");
    let definition_path = definition_path.to_str().expect("UTF-8 temporary path");

    let planned = invoke(
        fixture.root.path(),
        &["skill-workflow", "plan", definition_path],
    );
    assert!(planned.status.success());
    assert!(output_json(&planned)["digest"].as_str().is_some());

    let run = invoke(
        fixture.root.path(),
        &["skill-workflow", "run", "cli-success", definition_path],
    );
    assert!(run.status.success());
    assert_eq!(output_json(&run)["state"], "succeeded");
    let status_output = invoke(
        fixture.root.path(),
        &["skill-workflow", "status", "cli-success"],
    );
    assert!(status_output.status.success());
    assert_eq!(output_json(&status_output)["state"], "succeeded");
    let resume = invoke(
        fixture.root.path(),
        &[
            "skill-workflow",
            "resume",
            "cli-success",
            "--max-steps",
            "1",
        ],
    );
    assert!(resume.status.success());
    assert_eq!(output_json(&resume)["state"], "succeeded");

    let plan = compile(fixture.root.path(), &definition).expect("compiled cancel plan");
    start(fixture.root.path(), "cli-cancel", &plan).expect("start cancel run");
    let cancel_output = invoke(
        fixture.root.path(),
        &["skill-workflow", "cancel", "cli-cancel"],
    );
    assert!(cancel_output.status.success());
    assert_eq!(output_json(&cancel_output)["state"], "cancelled");

    let blocked_definition =
        make_definition(&fixture, vec![capability_step("unsupported", &[], true)]);
    let blocked_path = fixture.root.path().join("blocked.json");
    fs::write(
        &blocked_path,
        serde_json::to_vec(&blocked_definition).expect("blocked definition JSON"),
    )
    .expect("blocked definition file");
    let blocked = invoke(
        fixture.root.path(),
        &[
            "skill-workflow",
            "run",
            "cli-blocked",
            blocked_path.to_str().expect("UTF-8 temporary path"),
        ],
    );
    assert!(!blocked.status.success());
    assert_eq!(output_json(&blocked)["state"], "blocked");
    let blocked_status = invoke(
        fixture.root.path(),
        &["skill-workflow", "status", "cli-blocked"],
    );
    assert!(blocked_status.status.success());
    assert_eq!(output_json(&blocked_status)["state"], "blocked");
}
