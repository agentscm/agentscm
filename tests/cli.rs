use std::fs;
use std::process::{Command, Output, Stdio};

use serde_json::{Value, json};
use tempfile::TempDir;

fn agentscm(data_dir: &TempDir, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_agentscm"))
        .arg("--data-dir")
        .arg(data_dir.path())
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("run agentscm")
}

fn json_output(output: &Output) -> Value {
    serde_json::from_slice(&output.stdout).expect("JSON standard output")
}

#[cfg(unix)]
fn agentscm_with_small_stack(args: &[&str]) -> Output {
    Command::new("/bin/sh")
        .args(["-c", "ulimit -s 1024 && exec \"$@\"", "agentscm"])
        .arg(env!("CARGO_BIN_EXE_agentscm"))
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("run agentscm with a small main-thread stack")
}

#[cfg(unix)]
#[test]
fn cli_parsing_works_with_a_small_main_thread_stack() {
    let version = agentscm_with_small_stack(&["--version"]);
    assert!(
        version.status.success(),
        "{}",
        String::from_utf8_lossy(&version.stderr)
    );
    assert!(String::from_utf8_lossy(&version.stdout).starts_with("agentscm "));

    let data = TempDir::new().unwrap();
    let init = agentscm_with_small_stack(&[
        "--no-telemetry",
        "--data-dir",
        data.path().to_str().unwrap(),
        "init",
    ]);
    assert!(
        init.status.success(),
        "{}",
        String::from_utf8_lossy(&init.stderr)
    );
    assert_eq!(json_output(&init)["ok"], true);

    let invalid = agentscm_with_small_stack(&["init-repo", "--invalid-option"]);
    assert_eq!(invalid.status.code(), Some(2));
    let error: Value = serde_json::from_slice(&invalid.stderr).expect("JSON standard error");
    assert_eq!(error["ok"], false);
    assert_eq!(error["error_kind"], "invalid");

    let capture = agentscm_with_small_stack(&["workspace-client", "hook", "--unknown-option"]);
    assert!(capture.status.success());
    assert_eq!(
        serde_json::from_slice::<Value>(&capture.stdout).unwrap(),
        serde_json::json!({})
    );
    let stop = agentscm_with_small_stack(&["skill-workflow", "claude-stop", "--unknown-option"]);
    assert_eq!(stop.status.code(), Some(1));
    let error: Value = serde_json::from_slice(&stop.stderr).expect("JSON stop-hook error");
    assert_eq!(error["error_kind"], "invalid");
}

#[test]
fn init_checkpoint_and_read_are_machine_readable() {
    let data_dir = TempDir::new().unwrap();
    let init = agentscm(&data_dir, &["init"]);
    assert!(
        init.status.success(),
        "{}",
        String::from_utf8_lossy(&init.stderr)
    );
    assert_eq!(json_output(&init)["ok"], true);

    let request = data_dir.path().join("checkpoint.json");
    fs::write(
        &request,
        r#"{"parent":null,"changes":[{"path":"hello.txt","content":[104,105]}]}"#,
    )
    .unwrap();
    let checkpoint = agentscm(
        &data_dir,
        &[
            "checkpoint",
            request.to_str().expect("UTF-8 temporary path"),
        ],
    );
    assert!(
        checkpoint.status.success(),
        "{}",
        String::from_utf8_lossy(&checkpoint.stderr)
    );
    let revision = json_output(&checkpoint)["revision"]
        .as_str()
        .unwrap()
        .to_owned();

    let read = agentscm(&data_dir, &["read", &revision, "hello.txt"]);
    assert!(
        read.status.success(),
        "{}",
        String::from_utf8_lossy(&read.stderr)
    );
    assert_eq!(json_output(&read)["content"], serde_json::json!([104, 105]));
}

#[test]
fn invalid_limits_fail_with_json_on_standard_error() {
    let data_dir = TempDir::new().unwrap();
    let output = agentscm(&data_dir, &["events", "--limit", "0"]);
    assert!(!output.status.success());
    let error: Value = serde_json::from_slice(&output.stderr).expect("JSON standard error");
    assert_eq!(error["ok"], false);
    assert!(error["error"].as_str().unwrap().contains("between 1 and"));
}

#[test]
fn authority_commands_complete_a_bound_local_integration() {
    let data_dir = TempDir::new().unwrap();
    let checkpoint_request = data_dir.path().join("checkpoint.json");
    fs::write(
        &checkpoint_request,
        r#"{"parent":null,"changes":[{"path":"a.txt","content":[97]}]}"#,
    )
    .unwrap();
    let checkpoint = agentscm(
        &data_dir,
        &[
            "checkpoint",
            checkpoint_request.to_str().expect("UTF-8 temporary path"),
        ],
    );
    assert!(checkpoint.status.success());
    let revision = json_output(&checkpoint)["revision"]
        .as_str()
        .unwrap()
        .to_owned();

    let init = agentscm(&data_dir, &["init"]);
    assert!(init.status.success());
    let repository = json_output(&init)["repository"]
        .as_str()
        .unwrap()
        .to_owned();
    let context_request = data_dir.path().join("context.json");
    let mut context = json!({
        "version": 1, "change": {"repository": repository, "id": "change-1"},
        "change_version": 1, "revision": revision, "base": null,
        "target": "main", "policy_epoch": 0, "intent": "Add a.txt",
        "acceptance_criteria": ["File contains a"], "scope": ["a.txt"],
        "exclusions": [], "omissions": [], "dependencies": [],
        "recipe": {"state": "unknown"},
        "toolchain": {"state": "digest", "sha256": "c".repeat(64)},
        "environment": {"state": "digest", "sha256": "d".repeat(64)},
        "required_evidence": [], "required_capabilities": ["agentscm.review-context/v1"]
    });
    fs::write(&context_request, serde_json::to_vec(&context).unwrap()).unwrap();
    let started = agentscm(&data_dir, &["start", context_request.to_str().unwrap()]);
    assert!(
        started.status.success(),
        "{}",
        String::from_utf8_lossy(&started.stderr)
    );
    assert_eq!(json_output(&started)["readiness"]["status"], "incomplete");
    let old_digest = json_output(&started)["change"]["context_digest"]
        .as_str()
        .unwrap()
        .to_owned();
    let incomplete_approval = data_dir.path().join("incomplete-approval.json");
    fs::write(
        &incomplete_approval,
        serde_json::to_vec(&json!({
            "approval_id": "incomplete", "target": "main", "revision": revision,
            "context_digest": old_digest, "change": context["change"],
            "change_version": 1, "policy_epoch": 0
        }))
        .unwrap(),
    )
    .unwrap();
    let denied = agentscm(
        &data_dir,
        &["approve", incomplete_approval.to_str().unwrap()],
    );
    assert!(!denied.status.success());
    let denied_json: Value = serde_json::from_slice(&denied.stderr).unwrap();
    assert!(denied_json["error"].as_str().unwrap().contains("recipe"));
    context["recipe"] = json!({"state": "digest", "sha256": "b".repeat(64)});
    context["change_version"] = json!(2);
    context["acceptance_criteria"] = json!(["File contains exactly a"]);
    fs::write(&context_request, serde_json::to_vec(&context).unwrap()).unwrap();
    let updated = agentscm(
        &data_dir,
        &["context", "1", context_request.to_str().unwrap()],
    );
    assert!(
        updated.status.success(),
        "{}",
        String::from_utf8_lossy(&updated.stderr)
    );
    assert_eq!(json_output(&updated)["readiness"]["status"], "complete");
    let context_digest = json_output(&updated)["change"]["context_digest"]
        .as_str()
        .unwrap()
        .to_owned();
    assert_ne!(old_digest, context_digest);
    let stale = agentscm(
        &data_dir,
        &["context", "1", context_request.to_str().unwrap()],
    );
    assert!(!stale.status.success());
    let shown = agentscm(&data_dir, &["show", &repository, "change-1"]);
    assert!(shown.status.success());
    assert_eq!(
        json_output(&shown)["context"]["acceptance_criteria"],
        context["acceptance_criteria"]
    );
    let historical = agentscm(&data_dir, &["review-context", &old_digest]);
    assert!(historical.status.success());
    assert_eq!(json_output(&historical)["context"]["change_version"], 1);

    let approval_request = data_dir.path().join("approval.json");
    fs::write(
        &approval_request,
        serde_json::to_vec(&serde_json::json!({
            "approval_id": "local-approval-1",
            "target": "main",
            "revision": revision,
            "context_digest": context_digest,
            "change": {"repository": repository, "id": "change-1"},
            "change_version": 2,
            "policy_epoch": 0
        }))
        .unwrap(),
    )
    .unwrap();
    let approve = agentscm(
        &data_dir,
        &[
            "approve",
            approval_request.to_str().expect("UTF-8 temporary path"),
        ],
    );
    assert!(
        approve.status.success(),
        "{}",
        String::from_utf8_lossy(&approve.stderr)
    );

    let integration_request = data_dir.path().join("integration.json");
    fs::write(
        &integration_request,
        serde_json::to_vec(&serde_json::json!({
            "operation_id": "operation-1",
            "target": "main",
            "expected_target": null,
            "revision": revision,
            "context_digest": context_digest,
            "change": {"repository": repository, "id": "change-1"},
            "change_version": 2,
            "approval_id": "local-approval-1",
            "policy_epoch": 0
        }))
        .unwrap(),
    )
    .unwrap();
    let integrate = agentscm(
        &data_dir,
        &[
            "integrate",
            integration_request.to_str().expect("UTF-8 temporary path"),
        ],
    );
    assert!(
        integrate.status.success(),
        "{}",
        String::from_utf8_lossy(&integrate.stderr)
    );
    assert_eq!(
        json_output(&integrate)["event"]["after"]["revision"],
        revision
    );

    assert_eq!(json_output(&integrate)["event"]["semantics_version"], 2);
    let retry = agentscm(
        &data_dir,
        &["integrate", integration_request.to_str().unwrap()],
    );
    assert!(retry.status.success());
    assert_eq!(json_output(&retry), json_output(&integrate));
    let target = agentscm(&data_dir, &["target", "main"]);
    assert_eq!(json_output(&target)["target"]["revision"], revision);
    let events = agentscm(&data_dir, &["events", "--after", "0", "--limit", "10"]);
    assert_eq!(json_output(&events)["events"].as_array().unwrap().len(), 1);
    let diff = agentscm(&data_dir, &["diff", &revision, &revision, "--limit", "10"]);
    assert_eq!(json_output(&diff)["diff"]["entries"], serde_json::json!([]));
    let policy = agentscm(&data_dir, &["policy", "--advance", "0"]);
    assert_eq!(json_output(&policy)["policy_epoch"], 1);
}

#[test]
fn review_context_input_is_bounded_before_json_parsing() {
    let data_dir = TempDir::new().unwrap();
    let request = data_dir.path().join("oversize.json");
    fs::write(&request, vec![b' '; 65_537]).unwrap();
    let output = agentscm(&data_dir, &["start", request.to_str().unwrap()]);
    assert!(!output.status.success());
    let error: Value = serde_json::from_slice(&output.stderr).unwrap();
    assert!(error["error"].as_str().unwrap().contains("65536 bytes"));
    assert!(!data_dir.path().join("state.sqlite").exists());
}

#[test]
fn partial_change_binding_is_a_json_error_instead_of_a_panic() {
    let data_dir = TempDir::new().unwrap();
    let path = data_dir.path().join("partial.json");
    fs::write(
        &path,
        serde_json::to_vec(&json!({
            "operation_id": "partial", "target": "main", "expected_target": null,
            "revision": "a".repeat(64), "context_digest": "b".repeat(64),
            "approval_id": "approval", "change_version": 1, "policy_epoch": 0
        }))
        .unwrap(),
    )
    .unwrap();
    let output = agentscm(&data_dir, &["integrate", path.to_str().unwrap()]);
    assert!(!output.status.success());
    let error: Value = serde_json::from_slice(&output.stderr).unwrap();
    assert_eq!(error["ok"], false);
    assert!(
        error["error"]
            .as_str()
            .unwrap()
            .contains("supplied together")
    );
}

#[test]
fn multi_parent_checkpoint_and_paged_inspection_resume_from_json_cursor_file() {
    let data_dir = TempDir::new().unwrap();
    let input = data_dir.path().join("input.json");
    fs::write(&input, r#"{"parent":null,"changes":[]}"#).unwrap();
    let output = agentscm(&data_dir, &["checkpoint", input.to_str().unwrap()]);
    assert!(output.status.success());
    let base = json_output(&output)["revision"]
        .as_str()
        .unwrap()
        .to_owned();
    fs::write(&input, json!({"parent":base,"changes":[]}).to_string()).unwrap();
    let output = agentscm(&data_dir, &["checkpoint", input.to_str().unwrap()]);
    assert!(output.status.success());
    let tip = json_output(&output)["revision"]
        .as_str()
        .unwrap()
        .to_owned();
    fs::write(
        &input,
        json!({"parents":[base,tip],"changes":[]}).to_string(),
    )
    .unwrap();
    let output = agentscm(&data_dir, &["checkpoint", input.to_str().unwrap()]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let merge = json_output(&output)["revision"]
        .as_str()
        .unwrap()
        .to_owned();
    let first = agentscm(&data_dir, &["history", &merge, "--limit", "1"]);
    assert!(first.status.success());
    let first = json_output(&first);
    assert_eq!(first["page"]["entries"][0]["id"], merge);
    let cursor_path = data_dir.path().join("cursor.json");
    fs::write(&cursor_path, first["page"]["next"].to_string()).unwrap();
    let resumed = agentscm(
        &data_dir,
        &["history", &merge, "--cursor", cursor_path.to_str().unwrap()],
    );
    assert!(resumed.status.success());
    assert!(json_output(&resumed)["page"]["next"].is_null());
    let related = agentscm(&data_dir, &["relation", &tip, &merge]);
    assert!(related.status.success());
    assert_eq!(json_output(&related)["page"]["relation"], "left_ancestor");
    let diff = agentscm(&data_dir, &["diff-page", &base, &merge]);
    assert!(diff.status.success());
    assert_eq!(json_output(&diff)["page"]["entries"], json!([]));
    fs::write(
        &input,
        json!({"parent":base,"parents":[base,tip],"changes":[]}).to_string(),
    )
    .unwrap();
    let invalid = agentscm(&data_dir, &["checkpoint", input.to_str().unwrap()]);
    assert!(!invalid.status.success());
    assert!(invalid.stdout.is_empty());
}

#[test]
fn read_reports_the_entry_kind() {
    use agentscm::model::{EntryChange, EntryKind};
    let data_dir = TempDir::new().unwrap();
    let revision = agentscm::store::Engine::open(data_dir.path())
        .unwrap()
        .checkpoint_entries(
            None,
            &[
                EntryChange {
                    path: "plain".into(),
                    content: Some(b"a".to_vec()),
                    kind: EntryKind::Regular,
                },
                EntryChange {
                    path: "tool".into(),
                    content: Some(b"b".to_vec()),
                    kind: EntryKind::Executable,
                },
            ],
        )
        .unwrap();
    for (path, kind) in [("plain", "regular"), ("tool", "executable")] {
        let read = agentscm(&data_dir, &["read", &revision, path]);
        assert!(read.status.success());
        assert_eq!(json_output(&read)["kind"], kind);
    }
}

#[test]
fn source_exploration_pages_metadata_before_checkout_and_types_unavailable_reads() {
    use agentscm::model::FileChange;
    use agentscm::store::Engine;
    let data = TempDir::new().unwrap();
    let engine = Engine::open(data.path()).unwrap();
    let revision = engine
        .checkpoint(
            None,
            &[
                FileChange {
                    path: "src/main.rs".into(),
                    content: Some(b"main".to_vec()),
                },
                FileChange {
                    path: "src2/other.rs".into(),
                    content: Some(b"other".to_vec()),
                },
                FileChange {
                    path: "docs/readme.md".into(),
                    content: Some(b"docs".to_vec()),
                },
            ],
        )
        .unwrap();
    let mut cursor = None;
    let mut found = Vec::new();
    loop {
        let mut args = vec![
            "files",
            &revision,
            "--limit",
            "1",
            "--prefix",
            "src",
            "--contains",
            ".rs",
        ];
        if cursor.is_some() {
            args.extend(["--cursor", "-"]);
        }
        let output = if let Some(cursor) = &cursor {
            let file = data.path().join("cursor.json");
            fs::write(&file, serde_json::to_vec(cursor).unwrap()).unwrap();
            args.pop();
            args.push(file.to_str().unwrap());
            agentscm(&data, &args)
        } else {
            agentscm(&data, &args)
        };
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let page = json_output(&output);
        assert!(page["scanned"].as_u64().unwrap() <= 1);
        found.extend(
            page["entries"]
                .as_array()
                .unwrap()
                .iter()
                .map(|entry| entry["path"].as_str().unwrap().to_owned()),
        );
        if page["complete"] == true {
            break;
        }
        cursor = Some(page["next"].as_str().unwrap().to_owned());
    }
    assert_eq!(found, ["src/main.rs"]);
    let stat = agentscm(&data, &["stat", &revision, "src/main.rs"]);
    assert!(stat.status.success());
    assert_eq!(json_output(&stat)["state"], "present");
    assert_eq!(
        json_output(&agentscm(&data, &["stat", &revision, "missing"]))["state"],
        "nonexistent"
    );
    let blob = engine
        .stat_file(&revision, "src/main.rs")
        .unwrap()
        .unwrap()
        .blob;
    fs::remove_file(
        data.path()
            .join("objects")
            .join(&blob[..2])
            .join(&blob[2..]),
    )
    .unwrap();
    assert!(
        agentscm(&data, &["stat", &revision, "src/main.rs"])
            .status
            .success()
    );
    let unavailable = agentscm(&data, &["read", &revision, "src/main.rs"]);
    assert!(!unavailable.status.success());
    let error: Value = serde_json::from_slice(&unavailable.stderr).unwrap();
    assert_eq!(error["error_kind"], "unavailable");
    let absent = agentscm(&data, &["read", &revision, "missing"]);
    assert_eq!(
        serde_json::from_slice::<Value>(&absent.stderr).unwrap()["error_kind"],
        "nonexistent"
    );
}
