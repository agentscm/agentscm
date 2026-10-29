#![cfg(unix)]

use std::path::Path;
use std::process::Command;

use agentscm::change_record::AssuranceBasisV1;
use agentscm::intent_review_host::{IntentReviewHost, IntentReviewRequest};
use agentscm::skill_workflow::intent::{IntentNext, IntentView};
use agentscm::workflow_library::*;
use serde_json::{Value, json};

fn git(root: &Path, args: &[&str]) {
    let output = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(args)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn fixture() -> (tempfile::TempDir, tempfile::TempDir, String) {
    let root = tempfile::tempdir().unwrap();
    git(root.path(), &["init", "-q"]);
    git(
        root.path(),
        &["config", "user.email", "fixture@example.invalid"],
    );
    git(root.path(), &["config", "user.name", "Fixture"]);
    git(root.path(), &["config", "commit.gpgsign", "false"]);
    std::fs::write(root.path().join("limit.txt"), "per-account limit\n").unwrap();
    git(root.path(), &["add", "limit.txt"]);
    git(root.path(), &["commit", "-qm", "Synthetic initial source"]);
    let store = tempfile::tempdir().unwrap();
    let mut library = Library::open(store.path()).unwrap();
    let preview = library
        .save_preview(
            PreviewSkill {
                name: "Intent review".into(),
                entrypoint: "SKILL.md".into(),
                provenance: Provenance::Markdown {
                    locator: "pasted_markdown".into(),
                },
            },
            &[PackageFile {
                path: "SKILL.md".into(),
                content: "# Intent review\nCompare human criteria against current evidence.".into(),
            }],
            vec![],
        )
        .unwrap();
    let skill = library
        .accept_import(&AcceptImport {
            preview_id: preview.preview_id,
            digest: preview.digest,
            idempotency_key: "import".into(),
        })
        .unwrap();
    let draft = library
        .create_workflow(&CreateWorkflow {
            name: "Intent check".into(),
            summary: "Synthetic check".into(),
            idempotency_key: "create".into(),
        })
        .unwrap();
    let draft = library
        .update_workflow(
            &draft.id,
            &UpdateWorkflow {
                expected_generation: draft.generation,
                name: None,
                summary: None,
                steps: Some(vec![WorkflowStep {
                    id: "review".into(),
                    label: "Intent review".into(),
                    needs: vec![],
                    required: true,
                    kind: WorkflowStepKind::Skill {
                        skill_version_id: skill.id,
                    },
                }]),
                trigger: None,
                follow_on: None,
            },
        )
        .unwrap();
    let Publication::Published { version, .. } = library
        .publish(
            &draft.id,
            &PublishWorkflow {
                expected_generation: draft.generation,
                idempotency_key: "publish".into(),
            },
        )
        .unwrap()
    else {
        panic!("fixture workflow was not published")
    };
    (root, store, version.id)
}

fn start(version: &str) -> Value {
    json!({
        "operation":"start", "request_id":"start", "work_id":"work", "workflow_version_id":version, "step_id":"review",
        "contract": {"version":1,"previous_digest":null,"request":[{"id":"human-1","kind":"human_message","source":"visible-message-1","author":"caller-supplied-human","recorded_at_ms":1,"sha256":"forged","coverage":"complete","text":"Use one rate limit per account."}],"amendments":[],"criteria":[{"id":"account","description":"Use one rate limit per account.","sources":["human-1"],"required":true,"constraint":false}],"focus":[],"coverage":"complete","gaps":[]},
        "scope":{"parent_work_id":null,"checkpoint":null,"criteria":["account"],"coverage":"complete","gaps":[]},
        "limits":{"max_correction_rounds":1,"max_invocations":4,"wall_time_ms":600000},
        "evidence":[{"id":"tests","text":"Caller reports a per-account check passed.","origin":"caller report","coverage":"complete"}]
    })
}

fn dispatch(host: &IntentReviewHost, value: Value) -> Value {
    host.dispatch(serde_json::from_value(value).unwrap())
        .unwrap()
}

fn view(value: &Value) -> IntentView {
    serde_json::from_value(value["view"].clone()).unwrap()
}

#[test]
fn host_pins_bytes_preserves_assertions_and_rejects_stale_assessment() {
    let (root, store, version) = fixture();
    let host = IntentReviewHost::new(root.path(), store.path(), "fixture/project").unwrap();
    let request = start(&version);
    let created = dispatch(&host, request.clone());
    std::thread::sleep(std::time::Duration::from_millis(2));
    let repeated = dispatch(&host, request);
    assert_eq!(created, repeated);
    let created = view(&created);
    assert_eq!(created.record.subject.repository, "fixture/project");
    assert_eq!(
        created.record.contracts[0].request[0].sha256,
        agentscm::store::digest(b"Use one rate limit per account.")
    );
    let evidence = created
        .record
        .subject
        .evidence
        .iter()
        .find(|row| row.id == "tests")
        .unwrap();
    assert_eq!(evidence.assurance, AssuranceBasisV1::CallerAsserted);
    assert_eq!(
        evidence.sha256,
        agentscm::store::digest(b"Caller reports a per-account check passed.")
    );
    assert_eq!(host.status("work").unwrap().next, IntentNext::Assess);
    assert!(host.status("other").is_err());
    let claimed = dispatch(
        &host,
        json!({"operation":"claim","request_id":"claim","work_id":"work","attempt_id":"attempt","session_id":"session","phase":"assessment","lease_ms":60000}),
    );
    assert_eq!(claimed["task"]["skill"]["files"][0]["path"], "SKILL.md");
    assert!(
        claimed["task"]["artifacts"]
            .as_array()
            .unwrap()
            .iter()
            .any(|row| row["id"] == "tests"
                && row["text"].as_str().unwrap().contains("Caller reports"))
    );
    let claimed_view = view(&claimed);
    let fence = claimed_view.record.attempts[0].fence;
    std::fs::write(root.path().join("limit.txt"), "changed account limit\n").unwrap();
    assert!(!host.status("work").unwrap().current);
    let report = json!({"operation":"report","request_id":"report","work_id":"work","attempt_id":"attempt","session_id":"session","fence":fence,"execution":"succeeded","assessment":{"criteria":[],"coverage":"complete","gaps":[]}});
    assert!(
        host.dispatch(serde_json::from_value(report).unwrap())
            .is_err()
    );
    let stop = dispatch(&host, json!({"operation":"stop","work_id":"work"}));
    assert_eq!(stop["next"], "recovery_required");
    assert!(stop["next_action"].as_str().unwrap().contains("human"));
}

#[test]
fn host_downgrades_missing_context_and_requires_separate_human_capability() {
    let (root, store, version) = fixture();
    let host = IntentReviewHost::new(root.path(), store.path(), "fixture/project").unwrap();
    let mut request = start(&version);
    request["contract"]["request"][0]["text"] = Value::Null;
    let created = dispatch(&host, request);
    assert_eq!(
        created["view"]["record"]["contracts"][0]["coverage"],
        "partial"
    );
    assert_eq!(
        created["view"]["record"]["contracts"][0]["request"][0]["reconstructed"],
        true
    );
    let recover = json!({"operation":"recover","request_id":"recover","work_id":"work","session_id":"session","reason":"Explicit recovery","authorization":{"id":"human-recovery","kind":"human_message","source":"visible-message-2","author":"caller-supplied-human","recorded_at_ms":2,"sha256":"forged","coverage":"complete","text":"Recover this work."}});
    assert!(
        host.dispatch(serde_json::from_value(recover).unwrap())
            .unwrap_err()
            .to_string()
            .contains("human authority")
    );
    let mut forged = start(&version);
    forged["source"] = json!({"head":"invented"});
    assert!(serde_json::from_value::<IntentReviewRequest>(forged).is_err());
}

#[test]
fn completed_report_replays_but_old_evidence_cannot_support_a_new_candidate() {
    let (root, store, version) = fixture();
    let host = IntentReviewHost::new(root.path(), store.path(), "fixture/project").unwrap();
    dispatch(&host, start(&version));
    dispatch(
        &host,
        json!({"operation":"claim","request_id":"claim","work_id":"work","attempt_id":"attempt","session_id":"session","phase":"assessment","lease_ms":60000}),
    );
    let report = json!({"operation":"report","request_id":"report","work_id":"work","attempt_id":"attempt","session_id":"session","fence":1,"execution":"succeeded","assessment":{"criteria":[{"criterion":"account","status":"supported","explanation":"Caller-supplied check supports the recorded criterion.","evidence_ids":["tests"],"corrective_outcome":null,"recheck":null}],"coverage":"complete","gaps":[]}});
    let completed = dispatch(&host, report.clone());
    assert_eq!(view(&completed).next, IntentNext::Complete);
    std::thread::sleep(std::time::Duration::from_millis(2));
    assert_eq!(dispatch(&host, report), completed);
    std::fs::write(root.path().join("limit.txt"), "different limit\n").unwrap();
    git(root.path(), &["add", "limit.txt"]);
    git(
        root.path(),
        &["commit", "-qm", "Synthetic candidate change"],
    );
    let before = view(&completed).record.subject;
    let status = host.status("work").unwrap();
    assert!(!status.current);
    let next = dispatch(
        &host,
        json!({"operation":"claim","request_id":"claim-2","work_id":"work","attempt_id":"attempt-2","session_id":"session","phase":"assessment","lease_ms":60000}),
    );
    let next = view(&next);
    assert_eq!(
        next.record.subject.source.repository(),
        before.source.repository()
    );
    let agentscm::change_record::SourceBindingV1::Git {
        base: prior_base, ..
    } = before.source
    else {
        panic!("expected Git")
    };
    let agentscm::change_record::SourceBindingV1::Git { base, .. } = &next.record.subject.source
    else {
        panic!("expected Git")
    };
    assert_eq!(&prior_base, base);
    let stale = json!({"operation":"report","request_id":"report-2","work_id":"work","attempt_id":"attempt-2","session_id":"session","fence":2,"execution":"succeeded","assessment":{"criteria":[{"criterion":"account","status":"supported","explanation":"Old check should not qualify.","evidence_ids":["tests"],"corrective_outcome":null,"recheck":null}],"coverage":"complete","gaps":[]}});
    assert!(
        host.dispatch(serde_json::from_value(stale).unwrap())
            .is_err()
    );
}

#[test]
fn unknown_can_continue_after_retrieving_new_evidence() {
    let (root, store, version) = fixture();
    let host = IntentReviewHost::new(root.path(), store.path(), "fixture/project").unwrap();
    dispatch(&host, start(&version));
    dispatch(
        &host,
        json!({"operation":"claim","request_id":"claim","work_id":"work","attempt_id":"attempt","session_id":"session","phase":"assessment","lease_ms":60000}),
    );
    let unknown = dispatch(
        &host,
        json!({"operation":"report","request_id":"unknown","work_id":"work","attempt_id":"attempt","session_id":"session","fence":1,"execution":"succeeded","assessment":{"criteria":[{"criterion":"account","status":"unknown","explanation":"Need a check using one account across two clients.","evidence_ids":[],"corrective_outcome":null,"recheck":null}],"coverage":"partial","gaps":["Missing cross-client check"]}}),
    );
    assert_eq!(view(&unknown).next, IntentNext::Investigate);
    let no_evidence = json!({"operation":"claim","request_id":"claim-2","work_id":"work","attempt_id":"attempt-2","session_id":"session","phase":"assessment","lease_ms":60000});
    assert!(
        host.dispatch(serde_json::from_value(no_evidence.clone()).unwrap())
            .is_err()
    );
    let mut retrieved = no_evidence;
    retrieved["evidence"] = json!([{"id":"cross-client","text":"Caller reports two clients share one account allowance.","origin":"caller-retrieved check","coverage":"complete"}]);
    let claimed = dispatch(&host, retrieved);
    assert_eq!(view(&claimed).next, IntentNext::InProgress);
    let result = dispatch(
        &host,
        json!({"operation":"report","request_id":"report-2","work_id":"work","attempt_id":"attempt-2","session_id":"session","fence":2,"execution":"succeeded","assessment":{"criteria":[{"criterion":"account","status":"supported","explanation":"Retrieved check supports account-level behavior.","evidence_ids":["cross-client"],"corrective_outcome":null,"recheck":null}],"coverage":"complete","gaps":[]}}),
    );
    assert_eq!(view(&result).next, IntentNext::Complete);
}

#[test]
fn live_correction_keeps_status_and_stop_in_progress_after_source_edit() {
    let (root, store, version) = fixture();
    let host = IntentReviewHost::new(root.path(), store.path(), "fixture/project").unwrap();
    let mut request = start(&version);
    request["scope"]["source_exclusions"] = json!([{"omission":"ignored_content","reason":"Ignored build products are outside requested source scope.","authorization_reference":"human-1"}]);
    dispatch(&host, request);
    dispatch(
        &host,
        json!({"operation":"claim","request_id":"assess-1","work_id":"work","attempt_id":"assessment-1","session_id":"session","phase":"assessment","lease_ms":60000}),
    );
    let mismatch = dispatch(
        &host,
        json!({"operation":"report","request_id":"mismatch","work_id":"work","attempt_id":"assessment-1","session_id":"session","fence":1,"execution":"succeeded","assessment":{"criteria":[{"criterion":"account","status":"mismatch","explanation":"The reported check found an IP-keyed counter.","evidence_ids":["tests"],"corrective_outcome":"Key the allowance by account.","recheck":"Exercise one account across two IP addresses."}],"coverage":"complete","gaps":[]}}),
    );
    assert_eq!(view(&mismatch).next, IntentNext::Correct);
    dispatch(
        &host,
        json!({"operation":"claim","request_id":"correct","work_id":"work","attempt_id":"correction","session_id":"session","phase":"correction","lease_ms":60000}),
    );
    std::fs::write(
        root.path().join("limit.txt"),
        "corrected per-account limit\n",
    )
    .unwrap();
    let editing = host.status("work").unwrap();
    assert!(!editing.current);
    assert_eq!(editing.next, IntentNext::InProgress);
    let stop = dispatch(&host, json!({"operation":"stop","work_id":"work"}));
    assert_eq!(stop["next"], "in_progress");
    let heartbeat = dispatch(
        &host,
        json!({"operation":"heartbeat","request_id":"editing","work_id":"work","attempt_id":"correction","session_id":"session","fence":2,"lease_ms":60000}),
    );
    assert!(!view(&heartbeat).current);
    assert_eq!(view(&heartbeat).next, IntentNext::InProgress);
    let repaired = dispatch(
        &host,
        json!({"operation":"report","request_id":"repaired","work_id":"work","attempt_id":"correction","session_id":"session","fence":2,"execution":"succeeded"}),
    );
    assert_eq!(view(&repaired).next, IntentNext::Assess);
    dispatch(
        &host,
        json!({"operation":"claim","request_id":"assess-2","work_id":"work","attempt_id":"assessment-2","session_id":"session","phase":"assessment","lease_ms":60000,"evidence":[{"id":"corrected-check","text":"Caller reports one account shares its allowance across two IP addresses.","origin":"caller check","coverage":"complete"}]}),
    );
    let result = dispatch(
        &host,
        json!({"operation":"report","request_id":"reassessed","work_id":"work","attempt_id":"assessment-2","session_id":"session","fence":3,"execution":"succeeded","assessment":{"criteria":[{"criterion":"account","status":"supported","explanation":"Fresh reported check supports the corrected source.","evidence_ids":["corrected-check"],"corrective_outcome":null,"recheck":null}],"coverage":"complete","gaps":[]}}),
    );
    assert_eq!(view(&result).next, IntentNext::Complete);
    assert_eq!(view(&result).record.attempts.len(), 3);
}

#[test]
fn uncaptured_submodule_and_special_file_content_cannot_meet_recorded_criteria() {
    for special in [false, true] {
        let (root, store, version) = fixture();
        if special {
            let tracked = root.path().join("limit.txt");
            std::fs::remove_file(&tracked).unwrap();
            assert!(
                Command::new("mkfifo")
                    .arg(&tracked)
                    .status()
                    .unwrap()
                    .success()
            );
        } else {
            let head = Command::new("git")
                .arg("-C")
                .arg(root.path())
                .args(["rev-parse", "HEAD"])
                .output()
                .unwrap();
            assert!(head.status.success());
            let head = String::from_utf8(head.stdout).unwrap();
            std::fs::create_dir(root.path().join("dependency")).unwrap();
            git(
                root.path(),
                &[
                    "update-index",
                    "--add",
                    "--cacheinfo",
                    "160000",
                    head.trim(),
                    "dependency",
                ],
            );
            git(root.path(), &["commit", "-qm", "Synthetic gitlink"]);
            let clean = Command::new("git")
                .arg("-C")
                .arg(root.path())
                .args(["status", "--porcelain"])
                .output()
                .unwrap();
            assert!(clean.status.success());
            assert!(clean.stdout.is_empty());
        }
        let host = IntentReviewHost::new(root.path(), store.path(), "fixture/project").unwrap();
        let mut request = start(&version);
        request["scope"]["source_exclusions"] = json!([{"omission":"ignored_content","reason":"Ignored build products are outside requested source scope.","authorization_reference":"human-1"}]);
        let created = dispatch(&host, request);
        assert_eq!(
            created["view"]["record"]["subject"]["source"]["snapshot_coverage"],
            "partial"
        );
        dispatch(
            &host,
            json!({"operation":"claim","request_id":"claim","work_id":"work","attempt_id":"attempt","session_id":"session","phase":"assessment","lease_ms":60000}),
        );
        let reported = dispatch(
            &host,
            json!({"operation":"report","request_id":"report","work_id":"work","attempt_id":"attempt","session_id":"session","fence":1,"execution":"succeeded","assessment":{"criteria":[{"criterion":"account","status":"supported","explanation":"Reported check supports account behavior.","evidence_ids":["tests"],"corrective_outcome":null,"recheck":null}],"coverage":"complete","gaps":[]}}),
        );
        assert_eq!(reported["view"]["outcome"], "needs_evidence");
        assert_eq!(view(&reported).next, IntentNext::Investigate);
        let stop = dispatch(&host, json!({"operation":"stop","work_id":"work"}));
        assert_eq!(
            stop["pending_task"]["assessed_source"],
            reported["view"]["record"]["subject"]["source"]
        );
        assert_eq!(
            stop["pending_task"]["source_exclusions"],
            reported["view"]["record"]["subject"]["scope"]["source_exclusions"]
        );
        assert!(
            stop["next_action"]
                .as_str()
                .unwrap()
                .contains("uncaptured source")
        );
    }
}

#[test]
fn shipped_skill_json_examples_match_the_host_protocol() {
    let skill = include_str!("../../skills/intent-review/SKILL.md");
    let mut examples = 0;
    for block in skill.split("```json\n").skip(1) {
        let json = block.split("```").next().unwrap();
        serde_json::from_str::<IntentReviewRequest>(json).unwrap();
        examples += 1;
    }
    assert!(
        examples >= 8,
        "skill must retain concrete workflow examples"
    );
}
