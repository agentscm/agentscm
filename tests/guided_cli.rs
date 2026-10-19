use std::fs;
use std::path::Path;
use std::process::{Command, Output};

use agentscm::authority::Authority;
use agentscm::context::ReviewContext;
use agentscm::model::{Approval, FileChange, IntegrateRequest};
use agentscm::store::{Engine, digest};
use rusqlite::Connection;
use serde_json::{Value, json};
use tempfile::TempDir;

fn run(data: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_agentscm"))
        .arg("--data-dir")
        .arg(data)
        .args(args)
        .env("PATH", "")
        .output()
        .unwrap()
}
fn ok(data: &Path, args: &[&str]) -> Value {
    let output = run(data, args);
    assert!(
        output.status.success(),
        "{args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap()
}
fn fail(data: &Path, args: &[&str]) {
    let output = run(data, args);
    assert!(!output.status.success(), "{args:?}");
    let _: Value = serde_json::from_slice(&output.stderr).unwrap();
}
fn initialize(data: &Path) -> String {
    ok(data, &["init"]);
    Engine::open(data)
        .unwrap()
        .checkpoint(
            None,
            &[FileChange {
                path: "a.txt".into(),
                content: Some(b"initial".to_vec()),
            }],
        )
        .unwrap()
}
fn start(data: &Path, work: &Path, change: &str, revision: &str, extra: &[&str]) -> Value {
    let mut args = vec![
        "workflow",
        "start",
        "--change",
        change,
        "--workspace",
        work.to_str().unwrap(),
        "--revision",
        revision,
        "--issue",
        "https://example.invalid/issues/23",
        "--intent",
        "Exercise exact local landing",
        "--criterion",
        "Preserve every source byte",
        "--scope",
        "a.txt",
        "--recipe",
        "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
        "--toolchain",
        "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
        "--environment",
        "cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc",
    ];
    args.extend_from_slice(extra);
    ok(data, &args)
}
fn token(data: &Path) -> String {
    let view = ok(data, &["workflow", "inspect"]);
    assert_eq!(view["diff"]["complete"], true);
    assert_eq!(view["reviewable"], true);
    view["inspection"].as_str().unwrap().to_owned()
}
fn approve(data: &Path, id: &str) -> String {
    let t = token(data);
    ok(data, &["workflow", "approve", id, "--inspection", &t]);
    t
}
fn session(data: &Path) -> Value {
    let db = Connection::open(data.join("workflow.sqlite")).unwrap();
    let bytes: Vec<u8> = db
        .query_row("SELECT state FROM session WHERE id=1", [], |r| r.get(0))
        .unwrap();
    serde_json::from_slice(&bytes).unwrap()
}
fn save_session(data: &Path, value: &Value) {
    Connection::open(data.join("workflow.sqlite"))
        .unwrap()
        .execute(
            "UPDATE session SET state=?1 WHERE id=1",
            [serde_json::to_vec(value).unwrap()],
        )
        .unwrap();
}
fn mark_restored(root: &Path, id: &str) {
    fs::create_dir_all(root.join(".agentscm-recovery")).unwrap();
    fs::write(root.join(".agentscm-recovery/restore-complete.json"), b"{}").unwrap();
    fs::write(
        root.join(".agentscm-recovery/instance.json"),
        serde_json::to_vec(&json!({"version":1,"id":id})).unwrap(),
    )
    .unwrap();
}
#[test]
fn guided_multiple_checkpoints_exact_diff_explicit_approval_and_two_serial_changes() {
    let temp = TempDir::new().unwrap();
    let data = temp.path().join("data");
    let work = temp.path().join("work");
    let initial = initialize(&data);
    start(&data, &work, "first", &initial, &[]);
    fail(
        &data,
        &["workflow", "approve", "implicit", "--inspection", "none"],
    );
    fs::write(work.join("a.txt"), b"one").unwrap();
    ok(&data, &["workflow", "checkpoint", "cp1", "0"]);
    fs::write(work.join("b.txt"), b"two").unwrap();
    let cp2 = ok(&data, &["workflow", "checkpoint", "cp2", "1"]);
    let note = ok(
        &data,
        &[
            "workflow",
            "note",
            "--command",
            "cargo test --locked",
            "--environment",
            "synthetic",
            "--result",
            "reported pass",
        ],
    );
    assert_eq!(
        note["evidence"]["operator_notes"][0]["revision"],
        cp2["workspace"]["base_revision"]
    );
    ok(&data, &["workflow", "prepare", "prepare1"]);
    let limited = ok(&data, &["workflow", "inspect", "--limit", "1"]);
    assert_eq!(limited["diff"]["complete"], false);
    assert!(limited["inspection"].is_null());
    let view = ok(&data, &["workflow", "inspect"]);
    assert_eq!(view["diff"]["entries"].as_array().unwrap().len(), 2);
    assert!(
        view["diff"]["entries"]
            .as_array()
            .unwrap()
            .iter()
            .any(|e| e["after_bytes"] == json!([111, 110, 101]))
    );
    fail(
        &data,
        &[
            "workflow",
            "integrate",
            "without-approval",
            "--inspection",
            view["inspection"].as_str().unwrap(),
        ],
    );
    let t = approve(&data, "approval1");
    let landed = ok(
        &data,
        &["workflow", "integrate", "land1", "--inspection", &t],
    );
    assert_eq!(landed["event"]["kind"], "initialize");
    let first = landed["event"]["after"]["revision"].as_str().unwrap();
    let work2 = temp.path().join("work2");
    start(&data, &work2, "second", first, &[]);
    fs::write(work2.join("a.txt"), b"three").unwrap();
    ok(&data, &["workflow", "checkpoint", "cp3", "0"]);
    fs::remove_file(work2.join("b.txt")).unwrap();
    ok(&data, &["workflow", "checkpoint", "cp4", "1"]);
    let prepared = ok(&data, &["workflow", "prepare", "prepare2"]);
    let revision = prepared["context"]["revision"].as_str().unwrap();
    let parents = Engine::open(&data)
        .unwrap()
        .load_revision(revision)
        .unwrap()
        .parents;
    assert_eq!(parents[0], first);
    assert_eq!(parents.len(), 2);
    let t = approve(&data, "approval2");
    let result = ok(
        &data,
        &["workflow", "integrate", "land2", "--inspection", &t],
    );
    assert_eq!(result["event"]["kind"], "integrate");
    assert_eq!(ok(&data, &["workflow", "resume"])["event"], result["event"]);
    assert_eq!(
        Authority::open(&data)
            .unwrap()
            .events(0, 100)
            .unwrap()
            .len(),
        2
    );
    let current = result["event"]["after"]["revision"].as_str().unwrap();
    let work3 = temp.path().join("work3");
    start(&data, &work3, "no-op", current, &[]);
    let no_op = ok(&data, &["workflow", "prepare", "already-current"]);
    assert_eq!(no_op["already_current"], true);
    assert!(no_op["event"].is_null());
    fail(
        &data,
        &[
            "workflow",
            "approve",
            "no-op-approval",
            "--inspection",
            "none",
        ],
    );
    let work4 = temp.path().join("work4");
    start(&data, &work4, "next", current, &[]);
}
#[test]
fn guided_stale_source_criteria_policy_and_target_cannot_integrate_old_approval() {
    for drift in ["source", "criteria", "policy", "target"] {
        let temp = TempDir::new().unwrap();
        let data = temp.path().join("data");
        let work = temp.path().join("work");
        let initial = initialize(&data);
        start(&data, &work, "work", &initial, &[]);
        ok(&data, &["workflow", "prepare", "prepare"]);
        let t = approve(&data, "approval");
        let saved = session(&data);
        let mut request: IntegrateRequest =
            serde_json::from_value(saved["approved"].clone()).unwrap();
        request.operation_id = "stale".into();
        match drift {
            "source" => {
                fs::write(work.join("a.txt"), b"unreviewed").unwrap();
            }
            "criteria" => {
                ok(
                    &data,
                    &[
                        "workflow",
                        "criteria",
                        "--criterion",
                        "Changed criteria, identical bytes",
                    ],
                );
            }
            "policy" => {
                ok(&data, &["policy", "--advance", "0"]);
            }
            "target" => {
                let mut a = Authority::open(&data).unwrap();
                let e = Engine::open(&data).unwrap();
                let mut context: ReviewContext =
                    serde_json::from_value(saved["context"].clone()).unwrap();
                context.change.id = "other".into();
                context.change_version = 1;
                a.start_change(&context).unwrap();
                let digest = context.digest().unwrap();
                a.approve(&Approval {
                    approval_id: "other-approval".into(),
                    target: context.target.clone(),
                    revision: context.revision.clone(),
                    context_digest: digest.clone(),
                    change: Some(context.change.clone()),
                    change_version: Some(1),
                    policy_epoch: 0,
                })
                .unwrap();
                a.initialize_target(
                    &e,
                    &IntegrateRequest {
                        operation_id: "other-land".into(),
                        target: context.target.clone(),
                        expected_target: None,
                        revision: context.revision.clone(),
                        context_digest: digest,
                        approval_id: "other-approval".into(),
                        change: Some(context.change),
                        change_version: Some(1),
                        policy_epoch: 0,
                    },
                )
                .unwrap();
            }
            _ => unreachable!(),
        }
        fail(
            &data,
            &["workflow", "integrate", "stale", "--inspection", &t],
        );
        if drift != "source" {
            assert!(
                Authority::open(&data)
                    .unwrap()
                    .initialize_target(&Engine::open(&data).unwrap(), &request)
                    .is_err()
            );
        }
        assert_eq!(
            fs::read(work.join("a.txt")).unwrap(),
            if drift == "source" {
                b"unreviewed".as_slice()
            } else {
                b"initial".as_slice()
            }
        );
    }
}
#[test]
fn guided_missing_failed_and_successful_controller_evidence_and_manual_note() {
    for matches in [false, true] {
        let temp = TempDir::new().unwrap();
        let data = temp.path().join("data");
        let work = temp.path().join("work");
        let initial = initialize(&data);
        let expected = format!(
            "a.txt={}",
            digest(if matches { b"initial" } else { b"wrong" })
        );
        start(
            &data,
            &work,
            "checked",
            &initial,
            &["--require-source-path", &expected],
        );
        ok(&data, &["workflow", "prepare", "prepare"]);
        ok(
            &data,
            &[
                "workflow",
                "note",
                "--command",
                "cargo test --locked",
                "--environment",
                "operator machine",
                "--result",
                "reported pass",
            ],
        );
        let missing = ok(&data, &["workflow", "inspect"]);
        assert!(missing["inspection"].is_null());
        assert_eq!(missing["reviewable"], false);
        let t = "missing-inspection";
        fail(
            &data,
            &["workflow", "approve", "missing", "--inspection", t],
        );
        let checked = ok(&data, &["workflow", "check", "check1"]);
        fail(
            &data,
            &["workflow", "approve", "old-inspection", "--inspection", t],
        );
        assert_eq!(checked["evidence"]["operator_notes_authenticated"], false);
        assert_eq!(
            checked["evidence"]["operator_notes"][0]["basis"],
            "unverified_operator_claim"
        );
        assert_eq!(
            checked["evidence"]["controller_attempt"]["state"],
            if matches { "success" } else { "fail" }
        );
        if matches {
            let t = approve(&data, "good");
            ok(
                &data,
                &["workflow", "integrate", "land", "--inspection", &t],
            );
        } else {
            let view = ok(&data, &["workflow", "inspect"]);
            assert!(view["inspection"].is_null());
            fail(
                &data,
                &[
                    "workflow",
                    "approve",
                    "failed",
                    "--inspection",
                    "failed-check",
                ],
            );
        }
    }
}
#[test]
fn guided_restart_replays_recorded_checkpoint_and_completed_integration() {
    let temp = TempDir::new().unwrap();
    let data = temp.path().join("data");
    let work = temp.path().join("work");
    let initial = initialize(&data);
    start(&data, &work, "resume", &initial, &[]);
    fs::write(work.join("a.txt"), b"checkpointed").unwrap();
    let mut saved = session(&data);
    saved["pending"] = json!({"kind":"Checkpoint","operation_id":"interrupted-cp","generation":0});
    save_session(&data, &saved);
    ok(&data, &["workflow", "resume"]);
    // Simulate a lost workflow reply after the underlying workspace committed.
    save_session(&data, &saved);
    let resumed = ok(&data, &["workflow", "resume"]);
    assert_eq!(resumed["workspace"]["generation"], 1);
    ok(&data, &["workflow", "prepare", "prepare"]);
    let t = approve(&data, "approved");
    let mut saved = session(&data);
    let mut request = saved["approved"].clone();
    request["operation_id"] = json!("interrupted-land");
    saved["pending"] = json!({"kind":"Integrate","request":request});
    save_session(&data, &saved);
    let landed = ok(&data, &["workflow", "resume"]);
    assert_eq!(landed["event"]["operation_id"], "interrupted-land");
    // Lost reply plus later filesystem edit must retrieve the recorded event, not land twice.
    fs::write(work.join("a.txt"), b"later edit").unwrap();
    save_session(&data, &saved);
    assert_eq!(ok(&data, &["workflow", "resume"])["event"], landed["event"]);
    assert_eq!(
        Authority::open(&data)
            .unwrap()
            .events(0, 100)
            .unwrap()
            .len(),
        1
    );
    fs::remove_dir_all(&work).unwrap();
    save_session(&data, &saved);
    assert_eq!(ok(&data, &["workflow", "resume"])["event"], landed["event"]);
    assert_eq!(ok(&data, &["workflow", "resume"])["event"], landed["event"]);
    assert!(!t.is_empty());
}
#[test]
fn guided_pending_unexecuted_integration_rejects_new_edits_and_large_diff_never_approves() {
    let temp = TempDir::new().unwrap();
    let data = temp.path().join("data");
    let work = temp.path().join("work");
    let initial = initialize(&data);
    start(&data, &work, "bounded", &initial, &[]);
    ok(&data, &["workflow", "prepare", "prepare"]);
    approve(&data, "approved");
    let mut saved = session(&data);
    let mut request = saved["approved"].clone();
    request["operation_id"] = json!("not-yet-landed");
    saved["pending"] = json!({"kind":"Integrate","request":request});
    save_session(&data, &saved);
    fs::write(work.join("a.txt"), b"later edit").unwrap();
    fail(&data, &["workflow", "resume"]);
    assert!(
        Authority::open(&data)
            .unwrap()
            .target("main")
            .unwrap()
            .is_none()
    );
    // Restore the exact approved bytes; explicit resume can now finish the recorded request.
    fs::write(work.join("a.txt"), b"initial").unwrap();
    ok(&data, &["workflow", "resume"]);
    let head = Authority::open(&data)
        .unwrap()
        .target("main")
        .unwrap()
        .unwrap()
        .revision;
    let work2 = temp.path().join("work2");
    start(&data, &work2, "large", &head, &[]);
    fs::write(work2.join("a.txt"), vec![b'x'; 256 * 1024 + 1]).unwrap();
    ok(&data, &["workflow", "checkpoint", "large-cp", "0"]);
    ok(&data, &["workflow", "prepare", "large-prepare"]);
    let view = ok(&data, &["workflow", "inspect"]);
    assert_eq!(view["diff"]["complete"], false);
    assert!(view["inspection"].is_null());
}
#[test]
fn guided_resume_requires_explicit_exact_workspace_reattachment() {
    let temp = TempDir::new().unwrap();
    let data = temp.path().join("data");
    let work = temp.path().join("work");
    let initial = initialize(&data);
    start(&data, &work, "relocate", &initial, &[]);
    ok(&data, &["workflow", "prepare", "prepare"]);
    let t = approve(&data, "approved");
    let moved = temp.path().join("moved");
    fs::rename(&work, &moved).unwrap();
    fail(&data, &["workflow", "resume"]);
    let resumed = ok(
        &data,
        &["workflow", "resume", "--workspace", moved.to_str().unwrap()],
    );
    assert_eq!(resumed["approval_recorded"], false);
    fail(
        &data,
        &["workflow", "integrate", "old-approval", "--inspection", &t],
    );
    let t = approve(&data, "reattached-approval");
    ok(
        &data,
        &["workflow", "integrate", "reattached", "--inspection", &t],
    );
}
#[test]
fn guided_prepare_retries_exact_request_and_refresh_recovers_failed_checkpoint() {
    let temp = TempDir::new().unwrap();
    let data = temp.path().join("data");
    let work = temp.path().join("work");
    let initial = initialize(&data);
    start(&data, &work, "base", &initial, &[]);
    ok(&data, &["workflow", "prepare", "base-prepare"]);
    let t = approve(&data, "base-approval");
    let first = ok(
        &data,
        &["workflow", "integrate", "base-land", "--inspection", &t],
    );
    let head = first["event"]["after"]["revision"].as_str().unwrap();
    let work2 = temp.path().join("work2");
    start(&data, &work2, "prepared", head, &[]);
    fs::write(work2.join("a.txt"), b"changed").unwrap();
    fail(&data, &["workflow", "checkpoint", "wrong-generation", "99"]);
    // Explicit refresh discards the failed request, not the source edits.
    ok(&data, &["workflow", "refresh"]);
    assert_eq!(fs::read(work2.join("a.txt")).unwrap(), b"changed");
    let cp = ok(&data, &["workflow", "checkpoint", "cp", "0"]);
    let tip = cp["workspace"]["base_revision"].clone();
    let mut saved = session(&data);
    let mut context = saved["context"].clone();
    let version = context["change_version"].as_u64().unwrap();
    context["change_version"] = json!(version + 1);
    context["revision"] = tip.clone();
    let request = json!({"operation_id":"interrupted-prepare","target":"main","expected_target":{"revision":head,"version":1},"source_tip":tip,"change_version":version,"context":context});
    saved["pending"] = json!({"kind":"Prepare","request":request});
    save_session(&data, &saved);
    let prepared = ok(&data, &["workflow", "resume"]);
    assert_eq!(prepared["context"]["change_version"], version + 1);
    save_session(&data, &saved);
    let replayed = ok(&data, &["workflow", "resume"]);
    assert_eq!(replayed["context"], prepared["context"]);
    let t = approve(&data, "prepared-approval");
    ok(
        &data,
        &["workflow", "integrate", "prepared-land", "--inspection", &t],
    );
}
#[test]
fn guided_stale_uncompleted_integration_can_be_explicitly_refreshed_and_reapproved() {
    let temp = TempDir::new().unwrap();
    let data = temp.path().join("data");
    let work = temp.path().join("work");
    let initial = initialize(&data);
    start(&data, &work, "refresh", &initial, &[]);
    ok(&data, &["workflow", "prepare", "prepare"]);
    approve(&data, "approved");
    let mut saved = session(&data);
    let mut request = saved["approved"].clone();
    request["operation_id"] = json!("pending-land");
    saved["pending"] = json!({"kind":"Integrate","request":request});
    save_session(&data, &saved);
    ok(&data, &["policy", "--advance", "0"]);
    fail(&data, &["workflow", "resume"]);
    ok(&data, &["workflow", "refresh"]);
    ok(&data, &["workflow", "prepare", "new-prepare"]);
    let t = approve(&data, "new-approval");
    let result = ok(
        &data,
        &["workflow", "integrate", "new-land", "--inspection", &t],
    );
    assert_eq!(result["event"]["policy_epoch"], 1);
}
#[test]
fn guided_restore_receipt_blocks_original_workspace_until_explicit_reattachment() {
    let temp = TempDir::new().unwrap();
    let data = temp.path().join("data");
    let work = temp.path().join("work");
    let initial = initialize(&data);
    start(&data, &work, "restored", &initial, &[]);
    ok(&data, &["workflow", "prepare", "prepare"]);
    mark_restored(temp.path(), "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa");
    assert_eq!(
        ok(&data, &["workflow", "status"])["restored_reattachment_required"],
        true
    );
    fail(&data, &["workflow", "resume"]);
    fail(&data, &["workflow", "checkpoint", "forbidden", "0"]);
    let moved = temp.path().join("restored-workspace");
    fs::rename(&work, &moved).unwrap();
    fs::create_dir(&work).unwrap();
    fs::write(work.join("sentinel"), b"original checkout stays untouched").unwrap();
    let attached = ok(
        &data,
        &["workflow", "resume", "--workspace", moved.to_str().unwrap()],
    );
    assert_eq!(attached["workspace"]["generation"], 0);
    assert_eq!(
        fs::read(work.join("sentinel")).unwrap(),
        b"original checkout stays untouched"
    );
    ok(&data, &["workflow", "status"]);
}
#[test]
fn guided_cancel_interrupted_start_preserves_partial_workspace_and_allows_new_change() {
    let temp = TempDir::new().unwrap();
    let data = temp.path().join("data");
    let work = temp.path().join("work");
    let initial = initialize(&data);
    start(&data, &work, "template", &initial, &[]);
    let mut pending = session(&data);
    let partial = temp.path().join("partial");
    pending["context"]["change"]["id"] = json!("never-started");
    pending["context"]["change_version"] = json!(1);
    pending["workspace"] = json!(partial);
    pending["workspace_binding"] = Value::Null;
    pending["pending"] = json!({"kind":"Start"});
    save_session(&data, &pending);
    ok(&data, &["policy", "--advance", "0"]);
    fail(&data, &["workflow", "resume"]);
    fs::create_dir(&partial).unwrap();
    fs::write(partial.join("keep"), b"preserve partial creation").unwrap();
    let cancelled = ok(
        &data,
        &[
            "workflow",
            "cancel-start",
            "Target moved before start completed",
        ],
    );
    assert!(cancelled["cancelled_start"].is_string());
    assert_eq!(
        fs::read(partial.join("keep")).unwrap(),
        b"preserve partial creation"
    );
    let new = temp.path().join("new");
    start(&data, &new, "new", &initial, &[]);
}
#[test]
fn guided_source_check_accepts_equals_in_native_paths() {
    let temp = TempDir::new().unwrap();
    let data = temp.path().join("data");
    let work = temp.path().join("work");
    initialize(&data);
    let e = Engine::open(&data).unwrap();
    let rev = e
        .checkpoint(
            None,
            &[FileChange {
                path: "a=b.txt".into(),
                content: Some(b"expected".to_vec()),
            }],
        )
        .unwrap();
    let expected = format!("a=b.txt={}", digest(b"expected"));
    start(
        &data,
        &work,
        "equals",
        &rev,
        &["--require-source-path", &expected],
    );
    ok(&data, &["workflow", "prepare", "prepare"]);
    assert_eq!(
        ok(&data, &["workflow", "check", "equals-check"])["evidence"]["controller_attempt"]["state"],
        "success"
    );
    let t = approve(&data, "equals-approval");
    ok(
        &data,
        &["workflow", "integrate", "equals-land", "--inspection", &t],
    );
}
#[test]
fn guided_cross_kind_operation_collision_can_refresh_without_claiming_completion() {
    let temp = TempDir::new().unwrap();
    let data = temp.path().join("data");
    let work = temp.path().join("work");
    let initial = initialize(&data);
    start(&data, &work, "kind", &initial, &[]);
    ok(&data, &["workflow", "prepare", "prepare"]);
    approve(&data, "approved");
    let mut saved = session(&data);
    let mut request: IntegrateRequest = serde_json::from_value(saved["approved"].clone()).unwrap();
    request.operation_id = "kind-collision".into();
    Authority::open(&data)
        .unwrap()
        .integrate(&Engine::open(&data).unwrap(), &request)
        .unwrap();
    saved["pending"] = json!({"kind":"Integrate","request":request});
    save_session(&data, &saved);
    fail(&data, &["workflow", "resume"]);
    let refreshed = ok(&data, &["workflow", "refresh"]);
    assert!(refreshed["event"].is_null());
    assert!(refreshed["pending"].is_null());
    assert_eq!(
        ok(&data, &["workflow", "prepare", "no-op"])["already_current"],
        true
    );
}
#[cfg(unix)]
#[test]
fn guided_same_path_workspace_replacement_requires_explicit_reattachment() {
    let temp = TempDir::new().unwrap();
    let data = temp.path().join("data");
    let work = temp.path().join("work");
    let initial = initialize(&data);
    start(&data, &work, "physical", &initial, &[]);
    ok(&data, &["workflow", "prepare", "prepare"]);
    let t = approve(&data, "approved");
    let saved = session(&data);
    let moved = temp.path().join("old-work");
    fs::rename(&work, &moved).unwrap();
    let change = serde_json::from_value(saved["context"]["change"].clone()).unwrap();
    agentscm::workspace::Workspace::create(&Engine::open(&data).unwrap(), &work, change, &initial)
        .unwrap();
    fail(
        &data,
        &["workflow", "integrate", "old-token", "--inspection", &t],
    );
    assert!(
        Authority::open(&data)
            .unwrap()
            .target("main")
            .unwrap()
            .is_none()
    );
    ok(
        &data,
        &["workflow", "resume", "--workspace", work.to_str().unwrap()],
    );
    fail(
        &data,
        &["workflow", "approve", "old-token", "--inspection", &t],
    );
    let t = approve(&data, "new-approval");
    ok(
        &data,
        &["workflow", "integrate", "new-land", "--inspection", &t],
    );
    assert_eq!(fs::read(moved.join("a.txt")).unwrap(), b"initial");
}
#[test]
fn guided_reverted_tree_prepare_is_terminal_and_retryable_without_approval() {
    let temp = TempDir::new().unwrap();
    let data = temp.path().join("data");
    let work = temp.path().join("work");
    let initial = initialize(&data);
    start(&data, &work, "first", &initial, &[]);
    ok(&data, &["workflow", "prepare", "prepare"]);
    let t = approve(&data, "approved");
    let first = ok(
        &data,
        &["workflow", "integrate", "land", "--inspection", &t],
    );
    let head = first["event"]["after"]["revision"].as_str().unwrap();
    let work2 = temp.path().join("work2");
    start(&data, &work2, "revert", head, &[]);
    fs::write(work2.join("a.txt"), b"temporary edit").unwrap();
    ok(&data, &["workflow", "checkpoint", "edit", "0"]);
    fs::write(work2.join("a.txt"), b"initial").unwrap();
    let cp = ok(&data, &["workflow", "checkpoint", "revert", "1"]);
    let mut saved = session(&data);
    let mut context = saved["context"].clone();
    context["change_version"] = json!(2);
    context["revision"] = cp["workspace"]["base_revision"].clone();
    saved["pending"] = json!({"kind":"Prepare","request":{"operation_id":"no-op-prepare","target":"main","expected_target":{"revision":head,"version":1},"source_tip":cp["workspace"]["base_revision"],"change_version":1,"context":context}});
    save_session(&data, &saved);
    assert_eq!(ok(&data, &["workflow", "resume"])["already_current"], true);
    save_session(&data, &saved);
    assert_eq!(ok(&data, &["workflow", "resume"])["already_current"], true);
    assert_eq!(
        Authority::open(&data)
            .unwrap()
            .events(0, 100)
            .unwrap()
            .len(),
        1
    );
    start(&data, &temp.path().join("next"), "next", head, &[]);
}
#[test]
fn guided_refresh_adopts_completed_update_and_preparation_without_reusing_approval() {
    for kind in ["Update", "Prepare"] {
        let temp = TempDir::new().unwrap();
        let data = temp.path().join("data");
        let work = temp.path().join("work");
        let initial = initialize(&data);
        start(&data, &work, "first", &initial, &[]);
        ok(&data, &["workflow", "prepare", "prepare"]);
        let t = approve(&data, "approved");
        let first = ok(
            &data,
            &["workflow", "integrate", "land", "--inspection", &t],
        );
        let head = first["event"]["after"]["revision"].as_str().unwrap();
        let work2 = temp.path().join("work2");
        start(&data, &work2, "refresh", head, &[]);
        fs::write(work2.join("a.txt"), b"new work").unwrap();
        let cp = ok(&data, &["workflow", "checkpoint", "cp", "0"]);
        let mut saved = session(&data);
        let mut context = saved["context"].clone();
        context["change_version"] = json!(2);
        context["revision"] = cp["workspace"]["base_revision"].clone();
        let mut a = Authority::open(&data).unwrap();
        if kind == "Update" {
            let context: ReviewContext = serde_json::from_value(context).unwrap();
            a.update_change_context(1, &context).unwrap();
            saved["pending"] = json!({"kind":"Update","expected":1,"context":context});
        } else {
            let request:agentscm::model::PrepareLandingRequest=serde_json::from_value(json!({"operation_id":"prepared-before-refresh","target":"main","expected_target":{"revision":head,"version":1},"source_tip":cp["workspace"]["base_revision"],"change_version":1,"context":context})).unwrap();
            a.prepare_landing(&Engine::open(&data).unwrap(), &request)
                .unwrap();
            saved["pending"] = json!({"kind":"Prepare","request":request});
        }
        drop(a);
        save_session(&data, &saved);
        let refreshed = ok(&data, &["workflow", "refresh"]);
        assert_eq!(refreshed["context"]["change_version"], 3);
        assert_eq!(refreshed["approval_recorded"], false);
        assert!(refreshed["pending"].is_null());
        assert_eq!(fs::read(work2.join("a.txt")).unwrap(), b"new work");
    }
}
#[test]
fn guided_displayed_success_is_required_even_when_authority_has_another_success() {
    let temp = TempDir::new().unwrap();
    let data = temp.path().join("data");
    let work = temp.path().join("work");
    let initial = initialize(&data);
    let expectation = format!("a.txt={}", digest(b"initial"));
    start(
        &data,
        &work,
        "evidence",
        &initial,
        &["--require-source-path", &expectation],
    );
    ok(&data, &["workflow", "prepare", "prepare"]);
    let saved = session(&data);
    let context: ReviewContext = serde_json::from_value(saved["context"].clone()).unwrap();
    let recipe = serde_json::from_value(saved["recipe"].clone()).unwrap();
    let request = agentscm::checks::SourcePathsRequest {
        attempt_id: "external-success".into(),
        change: context.change.clone(),
        change_version: context.change_version,
        context_digest: context.digest().unwrap(),
        revision: context.revision.clone(),
        recipe,
    };
    let mut a = Authority::open(&data).unwrap();
    a.create_source_paths_attempt(&request).unwrap();
    a.run_source_paths_attempt(&Engine::open(&data).unwrap(), "external-success", 0)
        .unwrap();
    drop(a);
    let view = ok(&data, &["workflow", "inspect"]);
    assert_eq!(
        view["evidence"]["controller_status"],
        "not_recorded_in_workflow"
    );
    assert!(view["inspection"].is_null());
    fail(
        &data,
        &[
            "workflow",
            "approve",
            "hidden-success",
            "--inspection",
            "none",
        ],
    );
    ok(&data, &["workflow", "check", "external-success"]);
    let t = approve(&data, "selected-success");
    let mut cancelled = request;
    cancelled.attempt_id = "selected-cancelled".into();
    let mut a = Authority::open(&data).unwrap();
    a.create_source_paths_attempt(&cancelled).unwrap();
    a.cancel_check_attempt("selected-cancelled", 0, "operator cancelled")
        .unwrap();
    drop(a);
    ok(&data, &["workflow", "check", "selected-cancelled"]);
    let view = ok(&data, &["workflow", "inspect"]);
    assert_eq!(view["evidence"]["controller_status"], "cancelled");
    assert!(view["inspection"].is_null());
    fail(
        &data,
        &[
            "workflow",
            "approve",
            "contrary-evidence",
            "--inspection",
            &t,
        ],
    );
    fail(
        &data,
        &[
            "workflow",
            "integrate",
            "contrary-evidence",
            "--inspection",
            &t,
        ],
    );
    ok(&data, &["workflow", "check", "external-success"]);
    let t = approve(&data, "reselected");
    ok(
        &data,
        &["workflow", "integrate", "land", "--inspection", &t],
    );
}
#[test]
fn guided_restored_pending_checkpoint_reconciles_before_and_after_workspace_commit() {
    for completed in [false, true] {
        let temp = TempDir::new().unwrap();
        let data = temp.path().join("data");
        let work = temp.path().join("work");
        let initial = initialize(&data);
        start(&data, &work, "checkpoint", &initial, &[]);
        fs::write(work.join("a.txt"), b"preserved work").unwrap();
        let mut saved = session(&data);
        saved["pending"] = json!({"kind":"Checkpoint","operation_id":"retained-cp","generation":0});
        save_session(&data, &saved);
        if completed {
            agentscm::workspace::Workspace::open(&work)
                .unwrap()
                .checkpoint(&Engine::open(&data).unwrap(), "retained-cp", 0)
                .unwrap();
        }
        mark_restored(temp.path(), "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa");
        let moved = temp.path().join("restored-work");
        fs::rename(&work, &moved).unwrap();
        fs::create_dir(&work).unwrap();
        fs::write(work.join("sentinel"), b"original stays untouched").unwrap();
        let status = ok(&data, &["workflow", "status"]);
        assert_eq!(status["pending"]["kind"], "Checkpoint");
        assert_eq!(status["restored_reattachment_required"], true);
        let resumed = ok(
            &data,
            &["workflow", "resume", "--workspace", moved.to_str().unwrap()],
        );
        assert_eq!(resumed["workspace"]["generation"], 1);
        assert!(resumed["pending"].is_null());
        assert_eq!(fs::read(moved.join("a.txt")).unwrap(), b"preserved work");
        assert_eq!(
            fs::read(work.join("sentinel")).unwrap(),
            b"original stays untouched"
        );
    }
}
#[test]
fn guided_restored_interrupted_start_status_and_cancel_never_touch_partial_workspace() {
    for registered in [false, true] {
        let temp = TempDir::new().unwrap();
        let data = temp.path().join("data");
        let work = temp.path().join("work");
        let initial = initialize(&data);
        start(&data, &work, "template", &initial, &[]);
        let partial = temp.path().join("partial");
        let mut saved = session(&data);
        saved["context"]["change"]["id"] = json!("interrupted");
        saved["context"]["change_version"] = json!(1);
        saved["workspace"] = json!(partial);
        saved["workspace_binding"] = Value::Null;
        saved["workspace_identity"] = Value::Null;
        saved["pending"] = json!({"kind":"Start"});
        if registered {
            let context: ReviewContext = serde_json::from_value(saved["context"].clone()).unwrap();
            Authority::open(&data)
                .unwrap()
                .start_change(&context)
                .unwrap();
            fs::create_dir(&partial).unwrap();
            fs::write(partial.join("keep"), b"partial bytes").unwrap();
        }
        save_session(&data, &saved);
        mark_restored(temp.path(), "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa");
        let status = ok(&data, &["workflow", "status"]);
        assert_eq!(status["pending"]["kind"], "Start");
        assert_eq!(status["change"].is_null(), !registered);
        ok(
            &data,
            &[
                "workflow",
                "cancel-start",
                "Relinquish restored incomplete start",
            ],
        );
        if registered {
            assert_eq!(fs::read(partial.join("keep")).unwrap(), b"partial bytes");
        }
        start(&data, &temp.path().join("next"), "next", &initial, &[]);
    }
}
#[test]
fn guided_restored_completed_receipt_needs_no_workspace_or_reattachment() {
    let temp = TempDir::new().unwrap();
    let data = temp.path().join("data");
    let work = temp.path().join("work");
    let initial = initialize(&data);
    start(&data, &work, "receipt", &initial, &[]);
    ok(&data, &["workflow", "prepare", "prepare"]);
    let t = approve(&data, "approved");
    let mut saved = session(&data);
    let mut request = saved["approved"].clone();
    request["operation_id"] = json!("receipt-land");
    saved["pending"] = json!({"kind":"Integrate","request":request});
    save_session(&data, &saved);
    let receipt = ok(&data, &["workflow", "resume"])["event"].clone();
    fs::remove_dir_all(&work).unwrap();
    mark_restored(temp.path(), "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa");
    assert_eq!(ok(&data, &["workflow", "resume"])["event"], receipt);
    assert_eq!(ok(&data, &["workflow", "status"])["event"], receipt);
    save_session(&data, &saved);
    assert_eq!(ok(&data, &["workflow", "resume"])["event"], receipt);
    assert!(!t.is_empty());
}
#[cfg(unix)]
#[test]
fn guided_uncompleted_integration_replacement_requires_reattachment_and_reapproval() {
    let temp = TempDir::new().unwrap();
    let data = temp.path().join("data");
    let work = temp.path().join("work");
    let initial = initialize(&data);
    start(&data, &work, "pending", &initial, &[]);
    ok(&data, &["workflow", "prepare", "prepare"]);
    let t = approve(&data, "approved");
    let mut saved = session(&data);
    let mut request = saved["approved"].clone();
    request["operation_id"] = json!("not-completed");
    saved["pending"] = json!({"kind":"Integrate","request":request});
    save_session(&data, &saved);
    let moved = temp.path().join("original");
    fs::rename(&work, &moved).unwrap();
    let change = serde_json::from_value(saved["context"]["change"].clone()).unwrap();
    agentscm::workspace::Workspace::create(&Engine::open(&data).unwrap(), &work, change, &initial)
        .unwrap();
    fail(&data, &["workflow", "resume"]);
    assert!(
        Authority::open(&data)
            .unwrap()
            .target("main")
            .unwrap()
            .is_none()
    );
    // Reattachment records the new root, but deliberately invalidates the old decision.
    fail(
        &data,
        &["workflow", "resume", "--workspace", work.to_str().unwrap()],
    );
    fail(
        &data,
        &["workflow", "integrate", "old-token", "--inspection", &t],
    );
    ok(&data, &["workflow", "refresh"]);
    ok(&data, &["workflow", "prepare", "new-prepare"]);
    let t = approve(&data, "new-approval");
    ok(
        &data,
        &["workflow", "integrate", "new-land", "--inspection", &t],
    );
    assert_eq!(fs::read(moved.join("a.txt")).unwrap(), b"initial");
}
#[cfg(unix)]
#[test]
fn guided_completed_start_with_lost_journal_save_requires_explicit_workspace_acknowledgement() {
    let temp = TempDir::new().unwrap();
    let data = temp.path().join("data");
    let work = temp.path().join("work");
    let initial = initialize(&data);
    start(&data, &work, "start-reply", &initial, &[]);
    let mut saved = session(&data);
    saved["pending"] = json!({"kind":"Start"});
    saved["workspace_identity"] = Value::Null;
    saved["workspace_binding"] = Value::Null;
    save_session(&data, &saved);
    let moved = temp.path().join("original");
    fs::rename(&work, &moved).unwrap();
    let change = serde_json::from_value(saved["context"]["change"].clone()).unwrap();
    agentscm::workspace::Workspace::create(&Engine::open(&data).unwrap(), &work, change, &initial)
        .unwrap();
    fail(&data, &["workflow", "resume"]);
    assert_eq!(
        ok(&data, &["workflow", "status"])["pending"]["kind"],
        "Start"
    );
    let resumed = ok(
        &data,
        &["workflow", "resume", "--workspace", work.to_str().unwrap()],
    );
    assert!(resumed["pending"].is_null());
    assert_eq!(resumed["workspace"]["generation"], 0);
    assert_eq!(fs::read(moved.join("a.txt")).unwrap(), b"initial");
}
#[test]
fn guided_pending_start_with_no_workspace_resumes_creation_without_manual_files() {
    let temp = TempDir::new().unwrap();
    let data = temp.path().join("data");
    let work = temp.path().join("work");
    let initial = initialize(&data);
    start(&data, &work, "template", &initial, &[]);
    let created = temp.path().join("created-on-resume");
    let mut saved = session(&data);
    saved["context"]["change"]["id"] = json!("not-yet-started");
    saved["context"]["change_version"] = json!(1);
    saved["workspace"] = json!(created);
    saved["workspace_binding"] = Value::Null;
    saved["workspace_identity"] = Value::Null;
    saved["pending"] = json!({"kind":"Start"});
    save_session(&data, &saved);
    assert!(!created.exists());
    let resumed = ok(&data, &["workflow", "resume"]);
    assert_eq!(resumed["change"]["id"]["id"], "not-yet-started");
    assert!(resumed["pending"].is_null());
    assert_eq!(resumed["workspace"]["generation"], 0);
    assert_eq!(fs::read(created.join("a.txt")).unwrap(), b"initial");
    assert!(
        Authority::open(&data)
            .unwrap()
            .target("main")
            .unwrap()
            .is_none()
    );
}
#[test]
fn guided_engine_page_bound_rechecks_exact_small_approval_and_integration() {
    let temp = TempDir::new().unwrap();
    let data = temp.path().join("data");
    let work = temp.path().join("work");
    let initial = initialize(&data);
    start(&data, &work, "page-bound", &initial, &[]);
    ok(&data, &["workflow", "prepare", "prepare"]);
    fail(&data, &["workflow", "inspect", "--limit", "4097"]);
    let view = ok(&data, &["workflow", "inspect", "--limit", "4096"]);
    assert_eq!(view["diff"]["complete"], true);
    let t = view["inspection"].as_str().unwrap();
    ok(
        &data,
        &["workflow", "approve", "approved", "--inspection", t],
    );
    let landed = ok(&data, &["workflow", "integrate", "land", "--inspection", t]);
    assert_eq!(landed["event"]["after"]["revision"], initial);
}
#[test]
fn guided_reused_restore_path_requires_the_new_instance_and_fresh_approval() {
    let temp = TempDir::new().unwrap();
    let data = temp.path().join("data");
    let work = temp.path().join("work");
    let initial = initialize(&data);
    start(&data, &work, "restore-instance", &initial, &[]);
    ok(&data, &["workflow", "prepare", "prepare"]);
    mark_restored(temp.path(), "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa");
    ok(
        &data,
        &["workflow", "resume", "--workspace", work.to_str().unwrap()],
    );
    let old = approve(&data, "instance-a");
    assert_eq!(
        session(&data)["restored_instance_ack"],
        "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
    );
    // A fresh restore can reuse the same now-deleted destination name and receipt.
    // The instance is the only changed observation; workspace identity stays equal.
    mark_restored(temp.path(), "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb");
    assert_eq!(
        ok(&data, &["workflow", "status"])["restored_reattachment_required"],
        true
    );
    fail(&data, &["workflow", "resume"]);
    fail(&data, &["workflow", "checkpoint", "old-root", "0"]);
    fail(
        &data,
        &[
            "workflow",
            "integrate",
            "old-approval",
            "--inspection",
            &old,
        ],
    );
    assert!(
        Authority::open(&data)
            .unwrap()
            .target("main")
            .unwrap()
            .is_none()
    );
    ok(
        &data,
        &["workflow", "resume", "--workspace", work.to_str().unwrap()],
    );
    fail(
        &data,
        &[
            "workflow",
            "integrate",
            "old-approval",
            "--inspection",
            &old,
        ],
    );
    let current = approve(&data, "instance-b");
    ok(
        &data,
        &[
            "workflow",
            "integrate",
            "new-instance",
            "--inspection",
            &current,
        ],
    );
}
#[test]
fn guided_path_only_restore_ack_requires_instance_acknowledgement() {
    let temp = TempDir::new().unwrap();
    let data = temp.path().join("data");
    let work = temp.path().join("work");
    let initial = initialize(&data);
    start(&data, &work, "legacy-ack", &initial, &[]);
    mark_restored(temp.path(), "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa");
    ok(
        &data,
        &["workflow", "resume", "--workspace", work.to_str().unwrap()],
    );
    let mut saved = session(&data);
    saved["restored_instance_ack"] = Value::Null;
    save_session(&data, &saved);
    fail(&data, &["workflow", "resume"]);
    assert_eq!(
        ok(&data, &["workflow", "status"])["restored_reattachment_required"],
        true
    );
    ok(
        &data,
        &["workflow", "resume", "--workspace", work.to_str().unwrap()],
    );
    assert_eq!(
        session(&data)["restored_instance_ack"],
        "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
    );
}

#[test]
fn guided_inspection_follows_unchanged_scan_pages_before_exact_approval_and_landing() {
    let temp = TempDir::new().unwrap();
    let data = temp.path().join("data");
    ok(&data, &["init"]);
    let engine = Engine::open(&data).unwrap();
    let files: Vec<_> = (0..200)
        .map(|i| FileChange {
            path: format!("file-{i:03}.txt"),
            content: Some(b"before".to_vec()),
        })
        .collect();
    let initial = engine.checkpoint(None, &files).unwrap();
    let baseline = temp.path().join("baseline");
    start(&data, &baseline, "baseline", &initial, &[]);
    ok(&data, &["workflow", "prepare", "baseline-prepare"]);
    let capped = ok(&data, &["workflow", "inspect", "--limit", "1"]);
    assert_eq!(capped["reviewable"], false);
    assert_eq!(capped["diff"]["complete"], false);
    assert_eq!(capped["diff"]["incomplete_reason"], "entry_limit");
    assert!(capped["inspection"].is_null());
    fail(
        &data,
        &["workflow", "approve", "partial", "--inspection", "none"],
    );
    let t = approve(&data, "baseline-approval");
    ok(
        &data,
        &["workflow", "integrate", "baseline-land", "--inspection", &t],
    );
    let mut cursor = None;
    let mut last = None;
    loop {
        let page = engine
            .list_files(&initial, cursor.as_deref(), 4096)
            .unwrap();
        if let Some(file) = page.entries.last() {
            last = Some(file.path.clone());
        }
        cursor = page.next;
        if cursor.is_none() {
            break;
        }
    }
    let path = last.unwrap();
    let work = temp.path().join("edit");
    start(&data, &work, "late-edit", &initial, &[]);
    fs::write(work.join(&path), b"intermediate").unwrap();
    ok(&data, &["workflow", "checkpoint", "late-cp1", "0"]);
    fs::write(work.join(&path), b"after").unwrap();
    ok(&data, &["workflow", "checkpoint", "late-cp2", "1"]);
    let prepared = ok(&data, &["workflow", "prepare", "late-prepare"]);
    let revision = prepared["context"]["revision"].as_str().unwrap();
    // This is the real Engine condition from the full-source demo: the first
    // bounded scan contains no changes, but still has a continuation cursor.
    let first = engine.diff_page(&initial, revision, None, 4096).unwrap();
    assert!(first.entries.is_empty());
    assert!(first.needed_objects.is_empty());
    assert!(first.next.is_some());
    let view = ok(&data, &["workflow", "inspect", "--limit", "1"]);
    assert_eq!(view["reviewable"], true);
    assert_eq!(view["diff"]["complete"], true);
    assert!(view["diff"]["scan_pages"].as_u64().unwrap() > 1);
    assert!(view["diff"]["continuation"].is_null());
    let entries = view["diff"]["entries"].as_array().unwrap();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0]["path"], path);
    assert_eq!(entries[0]["before_bytes"], json!(b"before".to_vec()));
    assert_eq!(entries[0]["after_bytes"], json!(b"after".to_vec()));
    let t = view["inspection"].as_str().unwrap();
    ok(
        &data,
        &["workflow", "approve", "late-approval", "--inspection", t],
    );
    let landed = ok(
        &data,
        &["workflow", "integrate", "late-land", "--inspection", t],
    );
    assert_eq!(landed["event"]["after"]["revision"], revision);
    assert_eq!(engine.read_file(revision, &path).unwrap(), b"after");
    for file in &files {
        if file.path != path {
            assert_eq!(engine.read_file(revision, &file.path).unwrap(), b"before");
        }
    }
}

#[test]
fn scoped_guided_workflow_expands_checkpoints_and_lands_complete_source() {
    let dir = TempDir::new().unwrap();
    let data = dir.path().join("data");
    fs::create_dir(&data).unwrap();
    ok(&data, &["init"]);
    let engine = Engine::open(&data).unwrap();
    let revision = engine
        .checkpoint(
            None,
            &[
                FileChange {
                    path: "a.txt".into(),
                    content: Some(b"initial".to_vec()),
                },
                FileChange {
                    path: "dependency.txt".into(),
                    content: Some(b"dependency".to_vec()),
                },
                FileChange {
                    path: "unseen.txt".into(),
                    content: Some(b"preserved".to_vec()),
                },
            ],
        )
        .unwrap();
    let work = dir.path().join("work");
    start(&data, &work, "scoped", &revision, &["--path", "a.txt"]);
    assert!(work.join("a.txt").is_file());
    assert!(!work.join("dependency.txt").exists());
    assert!(!work.join("unseen.txt").exists());
    let expanded = ok(
        &data,
        &[
            "workspace",
            "expand",
            work.to_str().unwrap(),
            "dependencies",
            "0",
            "--path",
            "dependency.txt",
        ],
    );
    assert_eq!(expanded["outcome"]["status"], "expanded");
    assert_eq!(expanded["outcome"]["generation"], 1);
    fs::write(work.join("a.txt"), b"edited").unwrap();
    ok(&data, &["workflow", "checkpoint", "edited", "1"]);
    ok(&data, &["workflow", "prepare", "prepare"]);
    let inspection = approve(&data, "approved");
    let outcome = ok(
        &data,
        &["workflow", "integrate", "land", "--inspection", &inspection],
    );
    assert_eq!(outcome["ok"], true);
    let target = Authority::open(&data)
        .unwrap()
        .target("main")
        .unwrap()
        .unwrap();
    assert_eq!(
        engine.read_file(&target.revision, "a.txt").unwrap(),
        b"edited"
    );
    assert_eq!(
        engine
            .read_file(&target.revision, "dependency.txt")
            .unwrap(),
        b"dependency"
    );
    assert_eq!(
        engine.read_file(&target.revision, "unseen.txt").unwrap(),
        b"preserved"
    );
    assert!(!work.join("unseen.txt").exists());
}

#[test]
fn interrupted_scoped_start_only_reattaches_the_requested_projection() {
    use agentscm::workspace::Workspace;
    let dir = TempDir::new().unwrap();
    let data = dir.path().join("data");
    fs::create_dir(&data).unwrap();
    let base = initialize(&data);
    let engine = Engine::open(&data).unwrap();
    let revision = engine
        .checkpoint(
            Some(&base),
            &[
                FileChange {
                    path: ".gitignore".into(),
                    content: Some(b"ignored\n".to_vec()),
                },
                FileChange {
                    path: "other.txt".into(),
                    content: Some(b"other".to_vec()),
                },
            ],
        )
        .unwrap();
    let work = dir.path().join("requested");
    start(
        &data,
        &work,
        "recover-scoped",
        &revision,
        &["--path", "a.txt"],
    );
    let mut retained = session(&data);
    retained["pending"] = json!({"kind":"Start"});
    retained["workspace_binding"] = Value::Null;
    retained["workspace_identity"] = Value::Null;
    save_session(&data, &retained);
    let context: ReviewContext = serde_json::from_value(retained["context"].clone()).unwrap();
    let full = dir.path().join("full");
    Workspace::create(&engine, &full, context.change.clone(), &revision).unwrap();
    fail(
        &data,
        &["workflow", "resume", "--workspace", full.to_str().unwrap()],
    );
    let other = dir.path().join("other");
    Workspace::create_scoped(
        &engine,
        &other,
        context.change,
        &revision,
        &["other.txt".into()],
    )
    .unwrap();
    fail(
        &data,
        &["workflow", "resume", "--workspace", other.to_str().unwrap()],
    );
    ok(
        &data,
        &["workflow", "resume", "--workspace", work.to_str().unwrap()],
    );
    assert!(session(&data)["pending"].is_null());
    assert!(work.join(".gitignore").is_file());
    assert!(!work.join("other.txt").exists());
}

#[test]
fn invalid_scoped_start_does_not_reserve_a_session_or_change() {
    let dir = TempDir::new().unwrap();
    let data = dir.path().join("data");
    fs::create_dir(&data).unwrap();
    let revision = initialize(&data);
    let work = dir.path().join("work");
    // Build a valid command once using the same arguments as the start helper.
    let args = [
        "workflow",
        "start",
        "--change",
        "invalid-scope",
        "--workspace",
        work.to_str().unwrap(),
        "--revision",
        &revision,
        "--issue",
        "synthetic-task",
        "--intent",
        "Test rejected scope",
        "--criterion",
        "No durable start on invalid input",
        "--scope",
        "a.txt",
        "--recipe",
        "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
        "--toolchain",
        "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
        "--environment",
        "cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc",
        "--path",
        ".agentscm-workspace/secret",
    ];
    fail(&data, &args);
    assert!(!work.exists());
    let db = Connection::open(data.join("workflow.sqlite")).unwrap();
    assert_eq!(
        db.query_row("SELECT count(*) FROM session", [], |row| row
            .get::<_, i64>(0))
            .unwrap(),
        0
    );
    let authority = Authority::open(&data).unwrap();
    let change = agentscm::context::ChangeId {
        repository: authority.repository_id().unwrap(),
        id: "invalid-scope".into(),
    };
    assert!(matches!(
        authority.change(&change),
        Err(agentscm::Error::NotFound(_))
    ));
    drop(authority);
    drop(db);
    start(
        &data,
        &work,
        "invalid-scope",
        &revision,
        &["--path", "a.txt"],
    );
}
