#![cfg(unix)]

use agentscm::authority::{Authority, workflows::WorkflowRequirement};
use agentscm::execution::Cancellation;
use agentscm::model::FileChange;
use agentscm::skill_workflow::action::{
    ActionExecutor, ActionOutcome, ActionProfile, CommandProfile,
};
use agentscm::skill_workflow::{RunState, WorkflowDefinition, compile, runtime, start, status};
use agentscm::store::Engine;
use rusqlite::{Connection, params};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use tempfile::TempDir;

struct Fixture {
    root: TempDir,
    work: TempDir,
    definition: WorkflowDefinition,
    profile: ActionProfile,
}
fn executable(root: &Path, name: &str, body: &str) -> std::path::PathBuf {
    let path = root.join(name);
    fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
    path
}
fn fixture(recipe: &str) -> Fixture {
    let root = tempfile::tempdir().unwrap();
    let work = tempfile::tempdir().unwrap();
    fs::set_permissions(work.path(), fs::Permissions::from_mode(0o700)).unwrap();
    let claude = executable(
        work.path(),
        "claude",
        r#"
case "$1" in
--version) printf '2.1.300 (Claude Code)\n'; exit 0;;
auth) printf '{"loggedIn":true,"authMethod":"claude.ai"}\n'; exit 0;;
esac
[ -z "$ANTHROPIC_API_KEY" ] || exit 91
input=$(/bin/cat)
case "$input" in
*'explain-release'*) artifact='Explained release';;
*'draft-release'*) case "$input" in *'Explained release'*) artifact='Release notes from Explained release';; *) exit 93;; esac;;
*'explain-test'*) artifact='Explained test plan';;
*'draft-test'*) case "$input" in *'Explained test plan'*) artifact='Test plan from Explained test plan';; *) exit 93;; esac;;
*) exit 92;;
esac
printf '{"type":"result","subtype":"success","is_error":false,"num_turns":1,"modelUsage":{"claude-sonnet-5":{}},"structured_output":{"artifact":"%s"}}\n' "$artifact"
"#,
    );
    let mut files = vec![
        FileChange {
            path: "skills/pipeline/SKILL.md".into(),
            content: Some(b"# Synthetic productive pipeline\n".to_vec()),
        },
        FileChange {
            path: "src/app.txt".into(),
            content: Some(b"The synthetic application adds a filter.".to_vec()),
        },
    ];
    let mut resources = Vec::new();
    for (id, action) in [
        (
            "explain",
            json!({"version":1,"inputs":{"source":{"kind":"source","path":"src/app.txt"}},"output":{"name":"explanation","media_type":"text/markdown","max_bytes":4096},"action":{"kind":"prompt","prompt":"explain.md","required_tools":[]}}),
        ),
        (
            "draft",
            json!({"version":1,"inputs":{"explanation":{"kind":"artifact","step":"explain","name":"explanation"}},"output":{"name":"draft","media_type":"text/markdown","max_bytes":4096},"action":{"kind":"prompt","prompt":"draft.md","required_tools":[]}}),
        ),
        (
            "package",
            json!({"version":1,"inputs":{"draft":{"kind":"artifact","step":"draft","name":"draft"}},"output":{"name":"bundle","media_type":"application/json","max_bytes":4096},"action":{"kind":"command","profile":"package"}}),
        ),
    ] {
        let path = format!("skills/pipeline/{id}.json");
        resources.push(path.clone());
        files.push(FileChange {
            path,
            content: Some(serde_json::to_vec(&action).unwrap()),
        });
    }
    for id in ["explain", "draft"] {
        let path = format!("skills/pipeline/{id}.md");
        resources.push(path.clone());
        files.push(FileChange {
            path,
            content: Some(format!("{id}-{recipe}").into_bytes()),
        });
    }
    let engine = Engine::open(root.path()).unwrap();
    let revision = engine.checkpoint(None, &files).unwrap();
    drop(engine);
    let authority = Authority::open(root.path()).unwrap();
    let repository = authority.repository_id().unwrap();
    drop(authority);
    let definition=serde_json::from_value(json!({"version":1,"name":format!("{recipe}-pipeline"),"source":{"repository":repository,"revision":revision,"policy_epoch":0},"skill":{"entrypoint":"skills/pipeline/SKILL.md","resources":resources},"steps":[
        {"id":"explain","needs":[],"required":true,"action":{"kind":"agent_action","resource":"explain.json"}},
        {"id":"draft","needs":["explain"],"required":true,"action":{"kind":"agent_action","resource":"draft.json"}},
        {"id":"package","needs":["draft"],"required":true,"action":{"kind":"agent_action","resource":"package.json"}}
    ]})).unwrap();
    let profile = ActionProfile {
        version: 1,
        id: "local".into(),
        provider: "claude_code".into(),
        claude_executable: Some(claude),
        model: Some("sonnet".into()),
        commands: BTreeMap::from([(
            "package".into(),
            CommandProfile {
                executable: "/bin/cat".into(),
                args: vec![],
            },
        )]),
        timeout_ms: 5000,
        max_output_bytes: 4096,
    };
    Fixture {
        root,
        work,
        definition,
        profile,
    }
}
fn run(f: &Fixture, id: &str, n: usize) -> agentscm::Result<agentscm::skill_workflow::RunView> {
    let plan = compile(f.root.path(), &f.definition)?;
    start(f.root.path(), id, &plan)?;
    runtime::advance_with_actions(
        f.root.path(),
        id,
        n,
        &ActionExecutor {
            profile: &f.profile,
            work_dir: f.work.path(),
            cancellation: &Cancellation::default(),
        },
    )
}
fn change_resource(f: &mut Fixture, path: &str, edit: impl FnOnce(&mut Value)) {
    let engine = Engine::open(f.root.path()).unwrap();
    let full = format!("skills/pipeline/{path}");
    let bytes = engine
        .read_file(&f.definition.source.revision, &full)
        .unwrap();
    let mut value: Value = serde_json::from_slice(&bytes).unwrap();
    edit(&mut value);
    f.definition.source.revision = engine
        .checkpoint(
            Some(&f.definition.source.revision),
            &[FileChange {
                path: full,
                content: Some(serde_json::to_vec(&value).unwrap()),
            }],
        )
        .unwrap();
}

#[test]
fn two_distinct_recipes_use_same_two_prompt_and_command_engine() {
    for (recipe, expected) in [
        ("release", "Release notes from Explained release"),
        ("test", "Test plan from Explained test plan"),
    ] {
        let f = fixture(recipe);
        let view = run(&f, "composition", 32).unwrap();
        assert_eq!(view.state, RunState::Succeeded);
        assert_eq!(view.steps.len(), 3);
        assert!(
            view.steps
                .iter()
                .all(|s| s.action.as_ref().unwrap().verification == "not_independently_verified")
        );
        let artifact =
            runtime::artifact(f.root.path(), "composition", "package", "bundle").unwrap();
        let package: Value = serde_json::from_slice(&artifact).unwrap();
        assert_eq!(package["inputs"]["draft"], expected);
        assert_eq!(
            view.steps[0]
                .action
                .as_ref()
                .unwrap()
                .observed_model
                .as_deref(),
            Some("claude-sonnet-5")
        );
        assert!(runtime::artifact(f.root.path(), "composition", "package", "wrong-name").is_err());
        assert_eq!(run(&f, "composition", 32).unwrap(), view);
    }
}
#[test]
fn productive_plan_cannot_become_native_admission_requirement() {
    let f = fixture("release");
    let plan = compile(f.root.path(), &f.definition).unwrap();
    assert!(WorkflowRequirement::from_plan("main", &plan).is_err());
}
#[test]
fn undeclared_resource_and_wrong_artifact_binding_fail_compilation() {
    let mut f = fixture("release");
    f.definition
        .skill
        .resources
        .retain(|p| !p.ends_with("explain.md"));
    assert!(compile(f.root.path(), &f.definition).is_err());
    let mut f = fixture("release");
    change_resource(&mut f, "draft.json", |v| {
        v["inputs"]["explanation"]["name"] = json!("wrong")
    });
    assert!(compile(f.root.path(), &f.definition).is_err());
    let mut f = fixture("release");
    f.definition.steps[1].needs.clear();
    assert!(compile(f.root.path(), &f.definition).is_err());
}
#[test]
fn unavailable_provider_or_tools_blocks_and_propagates() {
    for provider in ["codex", "claude_code"] {
        let mut f = fixture("release");
        f.profile.provider = provider.into();
        if provider == "claude_code" {
            change_resource(&mut f, "explain.json", |v| {
                v["action"]["required_tools"] = json!(["browser"])
            });
        }
        let view = run(&f, "blocked", 32).unwrap();
        assert_eq!(view.state, RunState::Blocked);
        assert_eq!(
            view.steps[0].action.as_ref().unwrap().outcome,
            Some(ActionOutcome::Blocked)
        );
        assert!(view.steps[1].action.is_none());
        assert!(runtime::artifact(f.root.path(), "blocked", "explain", "explanation").is_err());
    }
}
#[test]
fn nonzero_empty_and_malformed_command_output_fail_durably() {
    for body in [
        "exit 7",
        "/bin/cat >/dev/null",
        "/bin/cat >/dev/null; printf malformed",
    ] {
        let mut f = fixture("release");
        f.profile.commands.get_mut("package").unwrap().executable =
            executable(f.work.path(), "command", body);
        let view = run(&f, "failure", 32).unwrap();
        assert_eq!(view.state, RunState::Failed);
        assert!(runtime::artifact(f.root.path(), "failure", "package", "bundle").is_err());
        assert_eq!(status(f.root.path(), "failure").unwrap(), view);
    }
}
#[test]
fn interrupted_reservation_never_replays_and_blocks_downstream() {
    let f = fixture("release");
    let plan = compile(f.root.path(), &f.definition).unwrap();
    start(f.root.path(), "interrupted", &plan).unwrap();
    let db = Connection::open(f.root.path().join("skill-workflows/state.sqlite")).unwrap();
    db.execute(
        "UPDATE workflow_runs SET state='running' WHERE run_id='interrupted'",
        [],
    )
    .unwrap();
    db.execute("UPDATE workflow_steps SET state='running',generation=1 WHERE run_id='interrupted' AND step_id='explain'",[]).unwrap();
    db.execute(
        "INSERT INTO workflow_action_attempts VALUES('interrupted','explain',?1,NULL,NULL)",
        [f.profile.digest().unwrap()],
    )
    .unwrap();
    drop(db);
    assert!(run(&f, "interrupted", 32).is_err());
    let recovered = runtime::recover_action(f.root.path(), "interrupted", "explain").unwrap();
    assert_eq!(
        recovered.steps[0].action.as_ref().unwrap().outcome,
        Some(ActionOutcome::Uncertain)
    );
    let view = run(&f, "interrupted", 32).unwrap();
    assert_eq!(view.state, RunState::Blocked);
    assert!(view.steps[0].detail.as_ref().unwrap().contains("uncertain"));
}
#[test]
fn profile_drift_is_refused_between_steps() {
    let mut f = fixture("release");
    let partial = run(&f, "drift", 1).unwrap();
    assert_eq!(
        partial.steps[0].state,
        agentscm::skill_workflow::StepState::Succeeded
    );
    f.profile.timeout_ms += 1;
    assert!(run(&f, "drift", 32).is_err());
}
#[test]
fn corrupted_artifact_is_not_consumed() {
    let f = fixture("release");
    run(&f, "corrupt", 1).unwrap();
    let db = Connection::open(f.root.path().join("skill-workflows/state.sqlite")).unwrap();
    db.execute("UPDATE workflow_action_attempts SET artifact=?1 WHERE run_id='corrupt' AND step_id='explain'",params![b"changed".as_slice()]).unwrap();
    drop(db);
    assert!(runtime::artifact(f.root.path(), "corrupt", "explain", "explanation").is_err());
    assert!(run(&f, "corrupt", 32).is_err());
}
#[test]
fn command_timeout_and_cancel_are_non_success() {
    let mut f = fixture("release");
    change_resource(
        &mut f,
        "package.json",
        |v| v["inputs"] = json!({"literal":{"kind":"parameter","value":"bounded timeout subject"}}),
    );
    f.definition.steps.retain(|step| step.id == "package");
    f.definition.steps[0].needs.clear();
    f.profile.timeout_ms = 100;
    f.profile.commands.get_mut("package").unwrap().executable =
        executable(f.work.path(), "slow", "/bin/cat >/dev/null; /bin/sleep 2");
    let view = run(&f, "timeout", 32).unwrap();
    assert_eq!(
        view.steps[0].action.as_ref().unwrap().outcome,
        Some(ActionOutcome::TimedOut)
    );
    assert_eq!(view.state, RunState::Failed);
    let f = fixture("release");
    let plan = compile(f.root.path(), &f.definition).unwrap();
    start(f.root.path(), "cancelled", &plan).unwrap();
    let cancellation = Cancellation::default();
    cancellation.cancel();
    assert!(
        runtime::advance_with_actions(
            f.root.path(),
            "cancelled",
            32,
            &ActionExecutor {
                profile: &f.profile,
                work_dir: f.work.path(),
                cancellation: &cancellation
            },
        )
        .is_err()
    );
    assert!(
        status(f.root.path(), "cancelled")
            .unwrap()
            .steps
            .iter()
            .all(
                |step| step.state == agentscm::skill_workflow::StepState::Pending
                    && step.action.is_none()
            )
    );
    let db = Connection::open(f.root.path().join("skill-workflows/state.sqlite")).unwrap();
    assert_eq!(
        db.query_row(
            "SELECT count(*) FROM workflow_action_attempts WHERE run_id='cancelled'",
            [],
            |row| row.get::<_, i64>(0)
        )
        .unwrap(),
        0
    );
    drop(db);
    assert_eq!(run(&f, "cancelled", 32).unwrap().state, RunState::Succeeded);
}

#[test]
fn event_inputs_are_immutable_data_and_missing_field_blocks() {
    let mut f = fixture("release");
    change_resource(&mut f, "explain.json", |v| {
        v["inputs"]["event_target"] = json!({"kind":"event","field":"target"})
    });
    assert_eq!(
        run(&f, "missing-event", 32).unwrap().state,
        RunState::Blocked
    );
    let plan = compile(f.root.path(), &f.definition).unwrap();
    start(f.root.path(), "event", &plan).unwrap();
    let fields = BTreeMap::from([("target".into(), "main".into())]);
    let digest = runtime::bind_inputs(f.root.path(), "event", &fields).unwrap();
    assert_eq!(
        runtime::bind_inputs(f.root.path(), "event", &fields).unwrap(),
        digest
    );
    assert!(
        runtime::bind_inputs(
            f.root.path(),
            "event",
            &BTreeMap::from([("target".into(), "other".into())])
        )
        .is_err()
    );
    assert_eq!(run(&f, "event", 32).unwrap().state, RunState::Succeeded);
    assert!(runtime::bind_inputs(f.root.path(), "event", &BTreeMap::new()).is_err());
}

#[test]
fn live_dispatch_cannot_be_claimed_twice_and_recovery_fences_late_result() {
    let mut f = fixture("release");
    change_resource(&mut f, "explain.json", |v| {
        v["action"] = json!({"kind":"command","profile":"slow"})
    });
    f.profile.timeout_ms = 30_000;
    let started = f.work.path().join("started");
    let slow = executable(
        f.work.path(),
        "slow",
        &format!(
            "/bin/cat >/dev/null; printf started > '{}'; /bin/sleep 30; printf 'Explained release'",
            started.display()
        ),
    );
    f.profile.commands.insert(
        "slow".into(),
        CommandProfile {
            executable: slow,
            args: vec![],
        },
    );
    let plan = compile(f.root.path(), &f.definition).unwrap();
    start(f.root.path(), "concurrent", &plan).unwrap();
    let root = f.root.path().to_path_buf();
    let work = f.work.path().to_path_buf();
    let profile = f.profile.clone();
    let worker = std::thread::spawn(move || {
        runtime::advance_with_actions(
            &root,
            "concurrent",
            1,
            &ActionExecutor {
                profile: &profile,
                work_dir: &work,
                cancellation: &Cancellation::default(),
            },
        )
    });
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        if started.is_file() {
            break;
        }
        assert!(std::time::Instant::now() < deadline);
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    let competing = runtime::advance_with_actions(
        f.root.path(),
        "concurrent",
        1,
        &ActionExecutor {
            profile: &f.profile,
            work_dir: f.work.path(),
            cancellation: &Cancellation::default(),
        },
    );
    assert!(competing.is_err());
    assert!(
        runtime::bind_inputs(
            f.root.path(),
            "concurrent",
            &BTreeMap::from([("late".into(), "data".into())])
        )
        .is_err()
    );
    loop {
        match runtime::recover_action(f.root.path(), "concurrent", "explain") {
            Ok(_) => break,
            Err(agentscm::Error::Conflict(message))
                if message.starts_with("workflow journal is active:")
                    && std::time::Instant::now() < deadline =>
            {
                std::thread::sleep(std::time::Duration::from_millis(5))
            }
            Err(error) => panic!("recovery failed: {error}"),
        }
    }
    worker.join().unwrap().unwrap();
    let view = run(&f, "concurrent", 32).unwrap();
    assert_eq!(view.state, RunState::Blocked);
    assert_eq!(
        view.steps[0].action.as_ref().unwrap().outcome,
        Some(ActionOutcome::Uncertain)
    );
    assert!(view.steps[0].action.as_ref().unwrap().artifact.is_none());
}

#[test]
fn oversize_command_output_and_contradictory_state_are_rejected() {
    let mut f = fixture("release");
    f.profile.commands.get_mut("package").unwrap().executable = executable(
        f.work.path(),
        "large",
        "/bin/cat >/dev/null; i=0; while [ $i -lt 5000 ]; do printf x; i=$((i+1)); done",
    );
    let view = run(&f, "large", 32).unwrap();
    assert_eq!(
        view.steps[2].action.as_ref().unwrap().outcome,
        Some(ActionOutcome::OutputLimit)
    );
    let f = fixture("release");
    run(&f, "contradict", 32).unwrap();
    let db = Connection::open(f.root.path().join("skill-workflows/state.sqlite")).unwrap();
    db.execute(
        "UPDATE workflow_runs SET state='failed' WHERE run_id='contradict'",
        [],
    )
    .unwrap();
    db.execute(
        "UPDATE workflow_steps SET state='failed' WHERE run_id='contradict' AND step_id='package'",
        [],
    )
    .unwrap();
    drop(db);
    assert!(status(f.root.path(), "contradict").is_err());
}

#[test]
fn cli_refuses_candidate_checkout_profile_before_dispatch() {
    let f = fixture("release");
    fs::create_dir(f.work.path().join(".git")).unwrap();
    let profile = f.work.path().join("operator.json");
    fs::write(&profile, serde_json::to_vec(&f.profile).unwrap()).unwrap();
    let definition = f.work.path().join("workflow.json");
    fs::write(&definition, serde_json::to_vec(&f.definition).unwrap()).unwrap();
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_agentscm"))
        .args(["--no-telemetry", "--data-dir"])
        .arg(f.root.path())
        .args(["skill-workflow", "run", "bad-profile"])
        .arg(definition)
        .arg("--action-profile")
        .arg(profile)
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("outside Git checkouts"));
    assert!(
        status(f.root.path(), "bad-profile")
            .unwrap()
            .steps
            .iter()
            .all(|step| step.action.is_none())
    );
}

#[test]
fn executable_trust_precedes_provider_preflight_and_rejects_data_overlap() {
    let mut f = fixture("release");
    fs::create_dir(f.work.path().join(".git")).unwrap();
    let marker = f.work.path().join("provider-was-started");
    let bad = executable(
        f.work.path(),
        "candidate-claude",
        &format!("printf started > '{}'", marker.display()),
    );
    f.profile.claude_executable = Some(bad);
    assert!(run(&f, "unsafe-provider", 32).is_err());
    assert!(
        status(f.root.path(), "unsafe-provider")
            .unwrap()
            .steps
            .iter()
            .all(
                |step| step.state == agentscm::skill_workflow::StepState::Pending
                    && step.action.is_none()
            )
    );
    assert!(!marker.exists());
    let mut f = fixture("release");
    let marker = f.work.path().join("command-was-started");
    let bad = executable(
        f.root.path(),
        "native-executable",
        &format!("printf started > '{}'", marker.display()),
    );
    f.profile.commands.get_mut("package").unwrap().executable = bad;
    assert!(run(&f, "unsafe-data", 32).is_err());
    assert!(
        status(f.root.path(), "unsafe-data")
            .unwrap()
            .steps
            .iter()
            .all(
                |step| step.state == agentscm::skill_workflow::StepState::Pending
                    && step.action.is_none()
            )
    );
    assert!(!marker.exists());
}

#[test]
fn executable_bytes_and_file_replacement_change_pinned_profile() {
    let f = fixture("release");
    run(&f, "changed-exe", 1).unwrap();
    let path = f.profile.claude_executable.as_ref().unwrap();
    let mut bytes = fs::read(path).unwrap();
    bytes.extend_from_slice(b"\n# changed installed runtime\n");
    fs::write(path, bytes).unwrap();
    assert!(run(&f, "changed-exe", 32).is_err());
    let f = fixture("release");
    let before = f.profile.digest().unwrap();
    let path = f.profile.claude_executable.as_ref().unwrap();
    let bytes = fs::read(path).unwrap();
    let replacement = f.work.path().join("replacement");
    fs::write(&replacement, bytes).unwrap();
    fs::set_permissions(&replacement, fs::Permissions::from_mode(0o700)).unwrap();
    fs::rename(&replacement, path).unwrap();
    assert_ne!(before, f.profile.digest().unwrap());
}

#[test]
fn productive_cli_interrupts_stop_owned_child_group_and_retain_cancellation() {
    use std::os::unix::process::CommandExt;
    use std::process::Stdio;
    for signal in [libc::SIGINT, libc::SIGTERM] {
        let mut f = fixture("release");
        change_resource(&mut f, "explain.json", |v| {
            v["action"] = json!({"kind":"command","profile":"slow"})
        });
        f.definition.steps.truncate(1);
        f.profile.timeout_ms = 30_000;
        let marker = f.work.path().join("child-pid");
        let program = executable(
            f.work.path(),
            "slow-signal",
            &format!(
                "set -e; /bin/cat >/dev/null; printf '%s' $$ > '{}.pending'; /bin/mv '{}.pending' '{}'; /bin/sleep 30",
                marker.display(),
                marker.display(),
                marker.display()
            ),
        );
        f.profile.commands.insert(
            "slow".into(),
            CommandProfile {
                executable: program,
                args: vec![],
            },
        );
        let profile = f.work.path().join("signal-profile.json");
        fs::write(&profile, serde_json::to_vec(&f.profile).unwrap()).unwrap();
        let definition = f.work.path().join("signal-workflow.json");
        fs::write(&definition, serde_json::to_vec(&f.definition).unwrap()).unwrap();
        let mut child = std::process::Command::new(env!("CARGO_BIN_EXE_agentscm"))
            .args(["--no-telemetry", "--data-dir"])
            .arg(f.root.path())
            .args(["skill-workflow", "run", "signal"])
            .arg(definition)
            .arg("--action-profile")
            .arg(profile)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .process_group(0)
            .spawn()
            .unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while !marker.is_file() {
            if let Some(exit) = child.try_wait().unwrap() {
                let output = child.wait_with_output().unwrap();
                panic!(
                    "controller exited {exit}: {}",
                    String::from_utf8_lossy(&output.stderr)
                );
            }
            if std::time::Instant::now() > deadline {
                let _ = child.kill();
                panic!("child did not start");
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        let group: i32 = fs::read_to_string(&marker).unwrap().parse().unwrap();
        // Both integer IDs name processes started and owned by this test.
        assert_eq!(unsafe { libc::kill(child.id() as i32, signal) }, 0);
        loop {
            if child.try_wait().unwrap().is_some() {
                break;
            }
            if std::time::Instant::now() > deadline {
                unsafe {
                    libc::kill(-group, libc::SIGKILL);
                }
                let _ = child.kill();
                panic!("controller did not retain interruption");
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        let output = child.wait_with_output().unwrap();
        assert!(!output.status.success());
        let view = status(f.root.path(), "signal").unwrap();
        assert_eq!(view.state, RunState::Cancelled);
        assert_eq!(
            view.steps[0].action.as_ref().unwrap().outcome,
            Some(ActionOutcome::Cancelled)
        );
        assert!(view.steps[0].action.as_ref().unwrap().artifact.is_none());
        let cleanup = std::time::Instant::now() + std::time::Duration::from_secs(2);
        // Signal zero inspects only this test-owned child group; kill cleans up a failed test.
        while unsafe { libc::kill(-group, 0) } == 0 {
            if std::time::Instant::now() > cleanup {
                unsafe {
                    libc::kill(-group, libc::SIGKILL);
                }
                panic!("action child group survived controller interruption");
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    }
}

#[test]
fn missing_runtime_before_reservation_resumes_only_after_exact_identity_restored() {
    let f = fixture("release");
    let before = f.profile.digest().unwrap();
    let runtime = f.profile.claude_executable.as_ref().unwrap();
    let held = f.work.path().join("held-runtime");
    fs::rename(runtime, &held).unwrap();
    assert!(run(&f, "retry-safe", 32).is_err());
    let view = status(f.root.path(), "retry-safe").unwrap();
    assert!(view.steps.iter().all(|step| step.state
        == agentscm::skill_workflow::StepState::Pending
        && step.action.is_none()));
    let db = Connection::open(f.root.path().join("skill-workflows/state.sqlite")).unwrap();
    assert_eq!(
        db.query_row(
            "SELECT count(*) FROM workflow_action_attempts WHERE run_id='retry-safe'",
            [],
            |row| row.get::<_, i64>(0)
        )
        .unwrap(),
        0
    );
    drop(db);
    fs::rename(&held, runtime).unwrap();
    assert_eq!(f.profile.digest().unwrap(), before);
    let view = run(&f, "retry-safe", 32).unwrap();
    assert_eq!(view.run_id, "retry-safe");
    assert_eq!(view.state, RunState::Succeeded);
}
