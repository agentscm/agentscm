use std::fs::{self, OpenOptions};
use std::path::Path;
use std::process::Command;

use agentscm::Error;
use agentscm::authority::Authority;
use agentscm::checks::{SourcePathsRecipe, SourcePathsRequest};
use agentscm::guided_service::{ReadRequest, WorkflowState, read};
use agentscm::model::FileChange;
use agentscm::store::{Engine, digest};
use rusqlite::Connection;
use serde_json::{Value, json};
use tempfile::TempDir;

fn cli(data: &Path, args: &[&str]) -> Value {
    let result = Command::new(env!("CARGO_BIN_EXE_agentscm"))
        .arg("--data-dir")
        .arg(data)
        .args(args)
        .env("PATH", "")
        .output()
        .unwrap();
    assert!(
        result.status.success(),
        "{args:?}: {}",
        String::from_utf8_lossy(&result.stderr)
    );
    serde_json::from_slice(&result.stdout).unwrap()
}

fn setup(root: &Path, require_check: bool) -> (Engine, String) {
    let data = root.join("data");
    cli(&data, &["init"]);
    let engine = Engine::open(&data).unwrap();
    let revision = engine
        .checkpoint(
            None,
            &[FileChange {
                path: "a.txt".into(),
                content: Some(b"source".to_vec()),
            }],
        )
        .unwrap();
    let work = root.join("work");
    let mut args = vec![
        "workflow",
        "start",
        "--change",
        "bridge",
        "--workspace",
        work.to_str().unwrap(),
        "--revision",
        &revision,
        "--issue",
        "https://example.invalid/issues/23",
        "--intent",
        "Read exact guided facts through the service",
        "--criterion",
        "Do not mutate workflow or grant authority",
        "--scope",
        "a.txt",
        "--recipe",
        "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
        "--toolchain",
        "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
        "--environment",
        "cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc",
    ];
    let expected = format!("a.txt={}", digest(b"source"));
    if require_check {
        args.extend(["--require-source-path", expected.as_str()]);
    }
    cli(&data, &args);
    (engine, revision)
}

fn journal(data: &Path) -> Vec<u8> {
    Connection::open(data.join("workflow.sqlite"))
        .unwrap()
        .query_row("SELECT state FROM session WHERE id=1", [], |row| row.get(0))
        .unwrap()
}

fn save(data: &Path, value: &Value) {
    Connection::open(data.join("workflow.sqlite"))
        .unwrap()
        .execute(
            "UPDATE session SET state=?1 WHERE id=1",
            [serde_json::to_vec(value).unwrap()],
        )
        .unwrap();
}

#[test]
fn missing_journal_and_lock_are_never_created_by_service_read() {
    let temp = TempDir::new().unwrap();
    let data = temp.path().join("data");
    cli(&data, &["init"]);
    let engine = Engine::open(&data).unwrap();
    assert!(matches!(
        read(&data, &engine, ReadRequest::Status),
        Err(Error::NotFound(_))
    ));
    assert!(!data.join("workflow.sqlite").exists());
    assert!(!data.join("workflow.lock").exists());
    let before = Authority::open(&data).unwrap().events(0, 100).unwrap();
    assert!(before.is_empty());
}

#[test]
fn projection_shares_exact_diff_but_never_exposes_a_decision_or_opens_workspace() {
    let temp = TempDir::new().unwrap();
    let (engine, _) = setup(temp.path(), true);
    let data = temp.path().join("data");
    cli(&data, &["workflow", "prepare", "prepare"]);
    cli(&data, &["workflow", "check", "source-check"]);
    cli(
        &data,
        &[
            "workflow",
            "note",
            "--command",
            "cargo test",
            "--environment",
            "operator report",
            "--result",
            "reported pass",
        ],
    );
    let inspected = cli(&data, &["workflow", "inspect"]);
    let token = inspected["inspection"].as_str().unwrap();
    cli(
        &data,
        &["workflow", "approve", "approved", "--inspection", token],
    );
    let before = journal(&data);
    // A GET must not use Workspace::open/status, even for a current prepared tip.
    fs::rename(temp.path().join("work"), temp.path().join("hidden-work")).unwrap();
    let view = read(&data, &engine, ReadRequest::Inspect { limit: 1000 }).unwrap();
    assert_eq!(view.state, WorkflowState::Active);
    assert!(view.context_current);
    assert!(view.local_approval_recorded);
    assert_eq!(view.workspace_observation, "not_observed");
    assert_eq!(view.admission, "not_assessed");
    assert!(view.evidence.selected_success_matches_subject);
    assert_eq!(view.evidence.controller_status, "success");
    assert_eq!(
        view.evidence.controller_basis,
        "source_integrity/controller_builtin"
    );
    assert_eq!(
        view.evidence.operator_notes_basis,
        "unverified_operator_claim"
    );
    assert_eq!(
        view.evidence.execution_proof,
        "not_collected_by_this_workflow"
    );
    assert_eq!(
        view.evidence.model_review_proof,
        "not_collected_by_this_workflow"
    );
    assert_eq!(view.diff.as_ref().unwrap(), &inspected["diff"]);
    let json = serde_json::to_value(&view).unwrap();
    assert!(json.get("inspection").is_none());
    assert!(json.get("reviewable").is_none());
    assert!(!serde_json::to_string(&view).unwrap().contains(token));
    assert_eq!(journal(&data), before);
    assert!(
        Authority::open(&data)
            .unwrap()
            .target("main")
            .unwrap()
            .is_none()
    );
}

#[test]
fn stale_authority_subject_never_labels_selected_success_as_current_proof() {
    let temp = TempDir::new().unwrap();
    let (engine, _) = setup(temp.path(), true);
    let data = temp.path().join("data");
    cli(&data, &["workflow", "prepare", "prepare"]);
    cli(&data, &["workflow", "check", "source-check"]);
    let before = read(&data, &engine, ReadRequest::Status).unwrap();
    assert!(before.evidence.selected_success_matches_subject);
    let mut moved = before.recorded_context.clone();
    moved.change_version += 1;
    moved
        .acceptance_criteria
        .push("A different exact review context".into());
    Authority::open(&data)
        .unwrap()
        .update_change_context(before.recorded_context.change_version, &moved)
        .unwrap();
    let bytes = journal(&data);
    let view = read(&data, &engine, ReadRequest::Inspect { limit: 4096 }).unwrap();
    assert!(!view.context_current);
    assert!(!view.evidence.selected_success_matches_subject);
    assert_eq!(view.evidence.controller_status, "stale");
    assert_eq!(
        view.recorded_context.change_version + 1,
        view.current_change.unwrap().version
    );
    assert_eq!(journal(&data), bytes);
}

#[test]
fn cancelled_and_missing_selected_attempts_remain_explicit_without_retry() {
    for missing in [false, true] {
        let temp = TempDir::new().unwrap();
        let (engine, _) = setup(temp.path(), true);
        let data = temp.path().join("data");
        let mut saved: Value = serde_json::from_slice(&journal(&data)).unwrap();
        let context: agentscm::context::ReviewContext =
            serde_json::from_value(saved["context"].clone()).unwrap();
        if !missing {
            let recipe: SourcePathsRecipe =
                serde_json::from_value(saved["recipe"].clone()).unwrap();
            let mut authority = Authority::open(&data).unwrap();
            let context_digest = context.digest().unwrap();
            authority
                .create_source_paths_attempt(&SourcePathsRequest {
                    attempt_id: "selected".into(),
                    change: context.change,
                    change_version: context.change_version,
                    context_digest,
                    revision: context.revision,
                    recipe,
                })
                .unwrap();
            authority
                .cancel_check_attempt("selected", 0, "explicit cancellation")
                .unwrap();
        }
        saved["attempt"] = json!("selected");
        save(&data, &saved);
        let bytes = journal(&data);
        let view = read(&data, &engine, ReadRequest::Status).unwrap();
        assert_eq!(
            view.evidence.controller_status,
            if missing { "missing" } else { "cancelled" }
        );
        assert!(!view.evidence.selected_success_matches_subject);
        assert_eq!(
            view.evidence.execution_proof,
            "not_collected_by_this_workflow"
        );
        assert_eq!(journal(&data), bytes);
    }
}

#[test]
fn lost_workflow_reply_shows_exact_authority_event_without_retry_or_workspace() {
    let temp = TempDir::new().unwrap();
    let (engine, _) = setup(temp.path(), false);
    let data = temp.path().join("data");
    cli(&data, &["workflow", "prepare", "prepare"]);
    let token = cli(&data, &["workflow", "inspect"])["inspection"]
        .as_str()
        .unwrap()
        .to_owned();
    cli(
        &data,
        &["workflow", "approve", "approved", "--inspection", &token],
    );
    let mut saved: Value = serde_json::from_slice(&journal(&data)).unwrap();
    let mut request = saved["approved"].clone();
    request["operation_id"] = json!("land");
    saved["pending"] = json!({"kind":"Integrate","request":request});
    save(&data, &saved);
    let unknown = read(&data, &engine, ReadRequest::Status).unwrap();
    assert_eq!(unknown.state, WorkflowState::Pending);
    assert!(unknown.event.is_none());
    assert_eq!(
        unknown.event_status,
        "no_retained_result_for_pending_operation"
    );
    assert!(
        Authority::open(&data)
            .unwrap()
            .target("main")
            .unwrap()
            .is_none()
    );
    let landed = cli(&data, &["workflow", "resume"]);
    Authority::open(&data).unwrap().advance_policy(0).unwrap();
    save(&data, &saved); // Simulate an authority commit followed by a lost workflow save.
    fs::remove_dir_all(temp.path().join("work")).unwrap();
    let bytes = journal(&data);
    let events = Authority::open(&data).unwrap().events(0, 100).unwrap();
    let view = read(&data, &engine, ReadRequest::Status).unwrap();
    assert_eq!(view.state, WorkflowState::Completed);
    assert!(!view.context_current);
    assert_eq!(view.current_policy_epoch, 1);
    assert_eq!(view.event_status, "retained_authority_event");
    assert_eq!(
        serde_json::to_value(view.event.unwrap()).unwrap(),
        landed["event"]
    );
    assert_eq!(view.pending.unwrap().outcome, "exact_retained_result");
    assert_eq!(journal(&data), bytes);
    assert_eq!(
        Authority::open(&data).unwrap().events(0, 100).unwrap(),
        events
    );
}

#[test]
fn active_writer_foreign_schema_and_oversized_journal_fail_closed() {
    let temp = TempDir::new().unwrap();
    let (engine, _) = setup(temp.path(), false);
    let data = temp.path().join("data");
    let lock = OpenOptions::new()
        .read(true)
        .write(true)
        .open(data.join("workflow.lock"))
        .unwrap();
    lock.try_lock().unwrap();
    assert!(matches!(
        read(&data, &engine, ReadRequest::Status),
        Err(Error::Conflict(_))
    ));
    drop(lock);
    let db = Connection::open(data.join("workflow.sqlite")).unwrap();
    db.pragma_update(None, "application_id", 1).unwrap();
    assert!(matches!(
        read(&data, &engine, ReadRequest::Status),
        Err(Error::Invalid(_))
    ));
    db.pragma_update(None, "application_id", 0x41534746)
        .unwrap();
    db.execute("UPDATE session SET state=zeroblob(262145) WHERE id=1", [])
        .unwrap();
    assert!(matches!(
        read(&data, &engine, ReadRequest::Status),
        Err(Error::Limit(_))
    ));
}

#[test]
fn quarantine_precedes_journal_read_and_active_restore_only_reports_reattachment() {
    let temp = TempDir::new().unwrap();
    let (engine, _) = setup(temp.path(), false);
    let data = temp.path().join("data");
    let control = temp.path().join(".agentscm-recovery");
    fs::create_dir(&control).unwrap();
    fs::write(control.join("quarantine.json"), b"{}").unwrap();
    fs::write(
        data.join("workflow.sqlite"),
        b"invalid journal after quarantine",
    )
    .unwrap();
    assert!(matches!(
        read(&data, &engine, ReadRequest::Status),
        Err(Error::Conflict(_))
    ));
    // This test's damaged journal is discarded; a separate valid fixture tests
    // post-activation occurrence binding without asserting real restore execution.
    let other = TempDir::new().unwrap();
    let (other_engine, _) = setup(other.path(), false);
    let other_data = other.path().join("data");
    let other_control = other.path().join(".agentscm-recovery");
    fs::create_dir(&other_control).unwrap();
    fs::write(other_control.join("restore-complete.json"), b"{}").unwrap();
    fs::write(
        other_control.join("instance.json"),
        br#"{"version":1,"id":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"}"#,
    )
    .unwrap();
    fs::remove_dir_all(other.path().join("work")).unwrap();
    let bytes = journal(&other_data);
    let view = read(&other_data, &other_engine, ReadRequest::Status).unwrap();
    assert!(view.restored_reattachment_required);
    assert_eq!(view.workspace_observation, "not_observed");
    assert_eq!(journal(&other_data), bytes);
}

#[cfg(unix)]
#[test]
fn missing_existing_access_lock_is_not_recreated_by_projection() {
    let temp = TempDir::new().unwrap();
    let (engine, _) = setup(temp.path(), false);
    let data = temp.path().join("data");
    let lock = data.join(".agentscm-local-access.lock");
    fs::remove_file(&lock).unwrap();
    assert!(read(&data, &engine, ReadRequest::Status).is_err());
    assert!(!lock.exists());
}

#[test]
fn service_inspection_entry_and_request_bounds_do_not_become_admission_claims() {
    let temp = TempDir::new().unwrap();
    let (engine, _) = setup(temp.path(), false);
    let data = temp.path().join("data");
    fs::write(temp.path().join("work/b.txt"), b"another exact file").unwrap();
    cli(&data, &["workflow", "checkpoint", "two-files", "0"]);
    cli(&data, &["workflow", "prepare", "two-files-prepare"]);
    let bytes = journal(&data);
    for limit in [0, 4097] {
        assert!(matches!(
            read(&data, &engine, ReadRequest::Inspect { limit }),
            Err(Error::Limit(_))
        ));
    }
    let bounded = read(&data, &engine, ReadRequest::Inspect { limit: 1 }).unwrap();
    assert_eq!(bounded.diff.as_ref().unwrap()["complete"], false);
    assert_eq!(
        bounded.diff.as_ref().unwrap()["incomplete_reason"],
        "entry_limit"
    );
    assert_eq!(bounded.admission, "not_assessed");
    let complete = read(&data, &engine, ReadRequest::Inspect { limit: 4096 }).unwrap();
    assert_eq!(complete.diff.as_ref().unwrap()["complete"], true);
    assert_eq!(
        complete.diff.as_ref().unwrap()["entries"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
    assert_eq!(complete.admission, "not_assessed");
    assert_eq!(journal(&data), bytes);
    assert!(
        Authority::open(&data)
            .unwrap()
            .target("main")
            .unwrap()
            .is_none()
    );
}
