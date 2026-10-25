use std::collections::BTreeMap;
use std::path::Path;
use std::process::{Command, Output};

#[cfg(unix)]
use std::fs;
#[cfg(unix)]
use std::io::Write;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
#[cfg(unix)]
use std::os::unix::net::UnixListener;
#[cfg(unix)]
use std::process::Stdio;

use agentscm::authority::Authority;
use agentscm::context::{ChangeId, InputIdentity, RequiredEvidence, ReviewContext};
use agentscm::execution::{
    COMMAND_CAPABILITY, COMMAND_EVIDENCE, CommandRecipe, ExecutionLimits, SyscallPolicy,
    environment_digest,
};
use agentscm::model::FileChange;
use agentscm::skill_workflow::library;
use agentscm::skill_workflow::{RunState, StepAction, StepState, advance, compile, start, status};
use agentscm::store::{Engine, digest};
use serde_json::{Value, json};
use tempfile::TempDir;

#[cfg(unix)]
#[path = "common/fake_docker.rs"]
mod fake_docker;

const SKILL: &str = "skills/reusable/v1/SKILL.md";
const LIBRARY_V1: &str = "skills/reusable/v1/workflow.json";
const LIBRARY_V2: &str = "skills/reusable/v2/workflow.json";
const RUNNER_A: &str = "skills/reusable/v1/runner-a.json";
const RUNNER_B: &str = "skills/reusable/v1/runner-b.json";
const SOURCE: &str = "src/example.rs";
const SOURCE_BYTES: &[u8] = b"pub fn answer() -> u8 { 42 }\n";

fn put(path: &str, bytes: impl Into<Vec<u8>>) -> FileChange {
    FileChange {
        path: path.into(),
        content: Some(bytes.into()),
    }
}

fn source_action(expected: &str) -> Value {
    json!({
        "kind": "source_paths",
        "expectations": [{"path": SOURCE, "sha256": expected}]
    })
}

fn template(name: &str, workflow_version: u32, skill: &str, steps: Vec<Value>) -> Value {
    json!({
        "version": 1,
        "name": name,
        "workflow_version": workflow_version,
        "skill": {"entrypoint": skill, "resources": []},
        "steps": steps
    })
}

fn step(id: &str, needs: &[&str], required: bool, action: Value) -> Value {
    json!({"id": id, "needs": needs, "required": required, "action": action})
}

fn context(repository: &str, change: &str, revision: &str) -> ReviewContext {
    ReviewContext {
        version: 1,
        change: ChangeId {
            repository: repository.into(),
            id: change.into(),
        },
        change_version: 1,
        revision: revision.into(),
        base: None,
        target: "main".into(),
        policy_epoch: 0,
        intent: "run the reusable source checks".into(),
        acceptance_criteria: vec!["the exact source matches".into()],
        scope: vec![SOURCE.into()],
        exclusions: vec![],
        omissions: vec![],
        dependencies: vec![],
        recipe: InputIdentity::Digest {
            sha256: "a".repeat(64),
        },
        toolchain: InputIdentity::Digest {
            sha256: "b".repeat(64),
        },
        environment: InputIdentity::Digest {
            sha256: "c".repeat(64),
        },
        required_evidence: vec![],
        required_capabilities: vec!["agentscm.review-context/v1".into()],
    }
}

fn command_recipe(argv: &[&str]) -> CommandRecipe {
    let environment = BTreeMap::new();
    CommandRecipe {
        version: 1,
        image: format!("sha256:{}", "1".repeat(64)),
        argv: argv.iter().map(|value| (*value).into()).collect(),
        environment_sha256: environment_digest(&environment).unwrap(),
        environment,
        toolchain_sha256: "2".repeat(64),
        limits: ExecutionLimits {
            timeout_ms: 5_000,
            max_output_bytes: 16 * 1024,
            memory_bytes: 64 * 1024 * 1024,
            scratch_bytes: 4 * 1024 * 1024,
            pids: 16,
            cpu_millis: 1_000,
        },
        syscall_policy: SyscallPolicy::DockerBuiltin,
        no_new_privileges: true,
    }
}

fn command_context(
    repository: &str,
    change: &str,
    revision: &str,
    recipe: &CommandRecipe,
) -> ReviewContext {
    let mut context = context(repository, change, revision);
    let recipe_digest = recipe.digest().unwrap();
    context.recipe = InputIdentity::Digest {
        sha256: recipe_digest.clone(),
    };
    context.toolchain = InputIdentity::Digest {
        sha256: recipe.toolchain_sha256.clone(),
    };
    context.environment = InputIdentity::Digest {
        sha256: recipe.environment_sha256.clone(),
    };
    context.required_evidence = vec![RequiredEvidence {
        identity: COMMAND_EVIDENCE.into(),
        digest: InputIdentity::Digest {
            sha256: recipe_digest,
        },
    }];
    context
        .required_capabilities
        .push(COMMAND_CAPABILITY.into());
    context
}

fn invoke(data: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_agentscm"))
        .arg("--data-dir")
        .arg(data)
        .args(args)
        .env("PATH", "")
        .output()
        .unwrap()
}

fn output_json(output: &Output) -> Value {
    serde_json::from_slice(&output.stdout).unwrap()
}

#[cfg(unix)]
fn invoke_mcp_raw(data: &Path, startup: &[&str], input: &str) -> Output {
    let mut child = Command::new(env!("CARGO_BIN_EXE_agentscm"))
        .arg("--data-dir")
        .arg(data)
        .args(["skill-workflow", "mcp"])
        .args(startup)
        .env("PATH", "")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let written = child.stdin.take().unwrap().write_all(input.as_bytes());
    let output = child.wait_with_output().unwrap();
    if output.status.success() {
        written.unwrap();
    }
    output
}

#[cfg(unix)]
fn invoke_mcp(data: &Path, startup: &[&str], requests: &[Value]) -> Output {
    let input = requests
        .iter()
        .map(|request| serde_json::to_string(request).unwrap())
        .collect::<Vec<_>>()
        .join("\n")
        + "\n";
    invoke_mcp_raw(data, startup, &input)
}

#[cfg(unix)]
fn mcp_responses(output: &Output) -> Vec<Value> {
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout.clone())
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
}

#[cfg(unix)]
fn mcp_response(responses: &[Value], id: u64) -> &Value {
    responses
        .iter()
        .find(|response| response["id"] == id)
        .unwrap()
}

#[cfg(unix)]
fn initialize(id: u64, client: &str) -> Value {
    json!({
        "jsonrpc":"2.0",
        "id":id,
        "method":"initialize",
        "params":{
            "protocolVersion":"2025-06-18",
            "capabilities":{},
            "clientInfo":{"name":client,"version":"synthetic"}
        }
    })
}

#[cfg(unix)]
fn mcp_call(id: u64, name: &str, arguments: Value) -> Value {
    json!({
        "jsonrpc":"2.0",
        "id":id,
        "method":"tools/call",
        "params":{"name":name,"arguments":arguments}
    })
}

#[cfg(unix)]
#[test]
fn local_mcp_is_client_neutral_read_only_by_default_and_rejects_injected_settings() {
    let root = TempDir::new().unwrap();
    Engine::open(root.path()).unwrap();
    Authority::open(root.path()).unwrap();

    for client in [
        "claude", "codex", "cursor", "copilot", "gemini", "grok", "generic",
    ] {
        let output = invoke_mcp(
            root.path(),
            &[],
            &[
                initialize(1, client),
                json!({"jsonrpc":"2.0","id":2,"method":"tools/list","params":{}}),
            ],
        );
        let responses = mcp_responses(&output);
        assert_eq!(
            mcp_response(&responses, 1)["result"]["serverInfo"]["name"],
            "agentscm-local-workflows"
        );
        let names = mcp_response(&responses, 2)["result"]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|tool| tool["name"].as_str().unwrap())
            .collect::<Vec<_>>();
        assert_eq!(
            names,
            [
                "agentscm_workflow_library_inspect",
                "agentscm_workflow_handoff"
            ]
        );
    }

    for (startup, expected) in [
        (
            &["--allow-configure"][..],
            vec![
                "agentscm_workflow_library_inspect",
                "agentscm_workflow_handoff",
                "agentscm_workflow_library_configure",
            ],
        ),
        (
            &["--allow-execution"][..],
            vec![
                "agentscm_workflow_library_inspect",
                "agentscm_workflow_handoff",
                "agentscm_workflow_check",
            ],
        ),
    ] {
        let output = invoke_mcp(
            root.path(),
            startup,
            &[
                initialize(1, "generic"),
                json!({"jsonrpc":"2.0","id":2,"method":"tools/list","params":{}}),
            ],
        );
        let responses = mcp_responses(&output);
        let names = mcp_response(&responses, 2)["result"]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|tool| tool["name"].as_str().unwrap())
            .collect::<Vec<_>>();
        assert_eq!(names, expected);
    }

    let requests = [
        initialize(1, "generic"),
        mcp_call(
            2,
            "agentscm_workflow_library_configure",
            json!({"change":"missing","library":LIBRARY_V1}),
        ),
        mcp_call(3, "agentscm_workflow_check", json!({"change":"missing"})),
        mcp_call(
            4,
            "agentscm_workflow_library_inspect",
            json!({"change":"missing","library":LIBRARY_V1,"data_dir":"/injected"}),
        ),
    ];
    let input = format!(
        "{{not-json\n{}\n",
        requests
            .iter()
            .map(|request| serde_json::to_string(request).unwrap())
            .collect::<Vec<_>>()
            .join("\n")
    );
    let output = invoke_mcp_raw(root.path(), &[], &input);
    let responses = mcp_responses(&output);
    assert_eq!(responses[0]["id"], Value::Null);
    assert_eq!(responses[0]["error"]["code"], -32700);
    for id in 2..=4 {
        assert_eq!(mcp_response(&responses, id)["error"]["code"], -32602);
    }
    assert!(
        Authority::open(root.path())
            .unwrap()
            .workflow_requirement("main")
            .unwrap()
            .is_none()
    );
}

#[cfg(unix)]
#[test]
fn workflow_inspection_does_not_recreate_a_missing_native_object_store() {
    let root = TempDir::new().unwrap();
    let manifest = template(
        "missing-store",
        1,
        SKILL,
        vec![step(
            "source",
            &[],
            true,
            source_action(&digest(SOURCE_BYTES)),
        )],
    );
    let engine = Engine::open(root.path()).unwrap();
    let revision = engine
        .checkpoint(
            None,
            &[
                put(SKILL, b"# Missing store fixture\n".to_vec()),
                put(SOURCE, SOURCE_BYTES.to_vec()),
                put(LIBRARY_V1, serde_json::to_vec(&manifest).unwrap()),
            ],
        )
        .unwrap();
    let mut authority = Authority::open(root.path()).unwrap();
    let repository = authority.repository_id().unwrap();
    authority
        .start_change(&context(&repository, "missing-store", &revision))
        .unwrap();
    drop(authority);
    drop(engine);
    let objects = root.path().join("objects");
    fs::remove_dir_all(&objects).unwrap();

    let mcp = invoke_mcp(
        root.path(),
        &[],
        &[
            initialize(1, "generic"),
            mcp_call(
                2,
                "agentscm_workflow_library_inspect",
                json!({"change":"missing-store","library":LIBRARY_V1}),
            ),
        ],
    );
    assert!(!mcp.status.success());
    assert!(!objects.exists());

    let cli = invoke(
        root.path(),
        &[
            "skill-workflow",
            "library",
            "inspect",
            "missing-store",
            "--library",
            LIBRARY_V1,
        ],
    );
    assert!(!cli.status.success());
    assert!(!objects.exists());
}

#[cfg(unix)]
#[test]
fn local_mcp_runs_the_source_only_library_lifecycle_without_a_backend() {
    let root = TempDir::new().unwrap();
    let manifest = template(
        "mcp-source-only",
        1,
        SKILL,
        vec![step(
            "source",
            &[],
            true,
            source_action(&digest(SOURCE_BYTES)),
        )],
    );
    let engine = Engine::open(root.path()).unwrap();
    let revision = engine
        .checkpoint(
            None,
            &[
                put(SKILL, b"# MCP source check\n".to_vec()),
                put(SOURCE, SOURCE_BYTES.to_vec()),
                put(LIBRARY_V1, serde_json::to_vec(&manifest).unwrap()),
            ],
        )
        .unwrap();
    let mut authority = Authority::open(root.path()).unwrap();
    let repository = authority.repository_id().unwrap();
    let original_context = context(&repository, "mcp-change", &revision);
    let original_state = authority.start_change(&original_context).unwrap();
    drop(authority);
    let snapshot = library::load(root.path(), &revision, LIBRARY_V1).unwrap();
    let expected_definition = snapshot.instantiate(&original_context).unwrap();

    let configure = json!({
        "jsonrpc":"2.0",
        "method":"tools/call",
        "params":{
            "name":"agentscm_workflow_library_configure",
            "arguments":{"change":"mcp-change","library":LIBRARY_V1}
        }
    });
    let check = json!({
        "jsonrpc":"2.0",
        "method":"tools/call",
        "params":{
            "name":"agentscm_workflow_check",
            "arguments":{"change":"mcp-change"}
        }
    });
    let mut null_configure = configure.clone();
    null_configure["id"] = Value::Null;
    let mut null_check = check.clone();
    null_check["id"] = Value::Null;
    let notification_output = invoke_mcp(
        root.path(),
        &["--allow-configure", "--allow-execution"],
        &[
            initialize(1, "generic"),
            configure,
            check,
            null_configure,
            null_check,
            mcp_call(
                2,
                "agentscm_workflow_library_inspect",
                json!({"change":"mcp-change","library":LIBRARY_V1}),
            ),
        ],
    );
    let notification_responses = mcp_responses(&notification_output);
    assert_eq!(notification_responses.len(), 4);
    for response in &notification_responses[1..3] {
        assert_eq!(response["id"], Value::Null);
        assert!(response["error"].is_object());
    }
    assert_eq!(
        mcp_response(&notification_responses, 2)["result"]["structuredContent"]["definition"],
        serde_json::to_value(&expected_definition).unwrap()
    );
    let authority = Authority::open(root.path()).unwrap();
    assert_eq!(
        authority.change(&original_state.id).unwrap(),
        original_state
    );
    assert!(authority.workflow_requirement("main").unwrap().is_none());
    drop(authority);

    let output = invoke_mcp(
        root.path(),
        &["--allow-configure", "--allow-execution"],
        &[
            initialize(1, "grok"),
            json!({"jsonrpc":"2.0","id":2,"method":"tools/list","params":{}}),
            mcp_call(
                3,
                "agentscm_workflow_library_inspect",
                json!({"change":"mcp-change","library":LIBRARY_V1}),
            ),
            mcp_call(
                4,
                "agentscm_workflow_library_configure",
                json!({"change":"mcp-change","library":LIBRARY_V1}),
            ),
            mcp_call(
                5,
                "agentscm_workflow_check",
                json!({"change":"mcp-change","docker":"/injected"}),
            ),
            mcp_call(6, "agentscm_workflow_check", json!({"change":"mcp-change"})),
            mcp_call(7, "agentscm_workflow_check", json!({"change":"mcp-change"})),
        ],
    );
    let responses = mcp_responses(&output);
    let names = mcp_response(&responses, 2)["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|tool| tool["name"].as_str().unwrap())
        .collect::<Vec<_>>();
    assert_eq!(
        names,
        [
            "agentscm_workflow_library_inspect",
            "agentscm_workflow_handoff",
            "agentscm_workflow_library_configure",
            "agentscm_workflow_check",
        ]
    );

    let inspected = &mcp_response(&responses, 3)["result"];
    assert_eq!(inspected["isError"], false);
    assert_eq!(
        inspected["structuredContent"]["reference"],
        serde_json::to_value(snapshot.reference()).unwrap()
    );
    assert_eq!(
        inspected["structuredContent"]["definition"],
        serde_json::to_value(expected_definition).unwrap()
    );
    assert_eq!(
        serde_json::from_str::<Value>(inspected["content"][0]["text"].as_str().unwrap()).unwrap(),
        inspected["structuredContent"]
    );

    let configured = &mcp_response(&responses, 4)["result"];
    assert_eq!(configured["isError"], false);
    assert!(configured["structuredContent"]["requirement_digest"].is_string());
    assert_eq!(mcp_response(&responses, 5)["error"]["code"], -32602);

    let checked = &mcp_response(&responses, 6)["result"];
    let checked_again = &mcp_response(&responses, 7)["result"];
    for result in [checked, checked_again] {
        assert_eq!(result["isError"], false);
        assert_eq!(result["structuredContent"]["run"]["state"], "succeeded");
        assert!(result["structuredContent"]["receipt"].is_string());
    }
    assert_eq!(
        checked["structuredContent"]["run"]["run_id"],
        checked_again["structuredContent"]["run"]["run_id"]
    );
    assert_eq!(
        checked["structuredContent"]["receipt"],
        checked_again["structuredContent"]["receipt"]
    );
    assert!(!root.path().join("execution").exists());

    let run_id = checked["structuredContent"]["run"]["run_id"]
        .as_str()
        .unwrap();
    let retained = status(root.path(), run_id).unwrap();
    assert_eq!(
        serde_json::to_value(&retained).unwrap(),
        checked["structuredContent"]["run"]
    );
    let authority = Authority::open(root.path()).unwrap();
    let requirement = authority.workflow_requirement("main").unwrap().unwrap();
    assert_eq!(
        requirement.digest().unwrap(),
        configured["structuredContent"]["requirement_digest"]
            .as_str()
            .unwrap()
    );
    drop(authority);

    let handoff_output = invoke_mcp(
        root.path(),
        &[],
        &[
            initialize(1, "generic"),
            mcp_call(2, "agentscm_workflow_handoff", json!({"run_id":run_id})),
        ],
    );
    let handoff_responses = mcp_responses(&handoff_output);
    let handoff = &mcp_response(&handoff_responses, 2)["result"];
    assert_eq!(handoff["isError"], false);
    assert_eq!(
        handoff["structuredContent"]["run"],
        serde_json::to_value(retained).unwrap()
    );
    assert_eq!(handoff["structuredContent"]["admission"], "not_assessed");
    assert_eq!(
        handoff["structuredContent"]["subject_scope"],
        "source_revision_only"
    );
}

#[test]
fn one_template_is_reused_by_distinct_changes_and_versions_coexist() {
    let root = TempDir::new().unwrap();
    let engine = Engine::open(root.path()).unwrap();
    let v1 = template(
        "verify",
        1,
        SKILL,
        vec![step(
            "source",
            &[],
            true,
            source_action(&digest(SOURCE_BYTES)),
        )],
    );
    let revision_one = engine
        .checkpoint(
            None,
            &[
                put(SKILL, b"# Reusable checks\n".to_vec()),
                put(SOURCE, SOURCE_BYTES.to_vec()),
                put(LIBRARY_V1, serde_json::to_vec(&v1).unwrap()),
            ],
        )
        .unwrap();
    let v2 = template(
        "verify",
        2,
        "skills/reusable/v2/SKILL.md",
        vec![step(
            "source",
            &[],
            true,
            source_action(&digest(SOURCE_BYTES)),
        )],
    );
    let revision_two = engine
        .checkpoint(
            Some(&revision_one),
            &[
                put("notes/revision.txt", b"second subject\n".to_vec()),
                put(
                    "skills/reusable/v2/SKILL.md",
                    b"# Reusable checks v2\n".to_vec(),
                ),
                put(LIBRARY_V2, serde_json::to_vec(&v2).unwrap()),
            ],
        )
        .unwrap();
    let mut authority = Authority::open(root.path()).unwrap();
    let repository = authority.repository_id().unwrap();
    let first_context = context(&repository, "first", &revision_one);
    let second_context = context(&repository, "second", &revision_two);
    authority.start_change(&first_context).unwrap();
    authority.start_change(&second_context).unwrap();
    drop(authority);

    let first = library::load(root.path(), &revision_one, LIBRARY_V1).unwrap();
    let second = library::load(root.path(), &revision_two, LIBRARY_V1).unwrap();
    assert_eq!(first.reference().sha256, second.reference().sha256);
    assert_ne!(first.reference().revision, second.reference().revision);

    for (run_id, snapshot, context) in [
        ("first-template-run", &first, &first_context),
        ("second-template-run", &second, &second_context),
    ] {
        let definition = snapshot.instantiate(context).unwrap();
        assert_eq!(definition.name, "verify:v1");
        assert_eq!(definition.source.revision, context.revision);
        let plan = compile(root.path(), &definition).unwrap();
        start(root.path(), run_id, &plan).unwrap();
        assert_eq!(
            advance(root.path(), run_id, 32).unwrap().state,
            RunState::Succeeded
        );
    }

    let second_version = library::load(root.path(), &revision_two, LIBRARY_V2).unwrap();
    assert_eq!(second_version.reference().workflow_version, 2);
    assert_eq!(second_version.reference().name, "verify");
    assert_ne!(second_version.reference().sha256, second.reference().sha256);
    assert_eq!(
        library::load(root.path(), &revision_two, LIBRARY_V1)
            .unwrap()
            .reference()
            .sha256,
        first.reference().sha256
    );
}

#[test]
fn command_and_review_steps_bind_context_and_legacy_configure_refreshes_policy() {
    let root = TempDir::new().unwrap();
    let recipe = command_recipe(&["/bin/true"]);
    let mut value = template(
        "test-and-review",
        3,
        SKILL,
        vec![
            step(
                "tests",
                &[],
                true,
                json!({"kind":"command", "runner":"runner-a.json"}),
            ),
            step(
                "review",
                &["tests"],
                true,
                json!({"kind":"claude_code_review", "paths":[SOURCE], "model":"sonnet"}),
            ),
        ],
    );
    value["skill"]["resources"] = json!([RUNNER_A]);
    let engine = Engine::open(root.path()).unwrap();
    let revision = engine
        .checkpoint(
            None,
            &[
                put(SKILL, b"Review the selected source.\n".to_vec()),
                put(SOURCE, SOURCE_BYTES.to_vec()),
                put(RUNNER_A, serde_json::to_vec(&recipe).unwrap()),
                put(LIBRARY_V1, serde_json::to_vec(&value).unwrap()),
            ],
        )
        .unwrap();
    let mut authority = Authority::open(root.path()).unwrap();
    let repository = authority.repository_id().unwrap();
    let bound_context = command_context(&repository, "bound", &revision, &recipe);
    let state = authority.start_change(&bound_context).unwrap();
    drop(authority);

    let snapshot = library::load(root.path(), &revision, LIBRARY_V1).unwrap();
    let definition = snapshot.instantiate(&bound_context).unwrap();
    assert_eq!(definition.name, "test-and-review:v3");
    assert!(definition.skill.resources.contains(&LIBRARY_V1.into()));
    match &definition.steps[0].action {
        StepAction::Command {
            runner,
            change,
            change_version,
            context_digest,
        } => {
            assert_eq!(runner, "runner-a.json");
            assert_eq!(change, &bound_context.change);
            assert_eq!(*change_version, state.version);
            assert_eq!(context_digest, &state.context_digest);
        }
        action => panic!("unexpected action: {action:?}"),
    }
    assert_eq!(
        definition.steps[1].action,
        StepAction::ClaudeCodeReview {
            paths: vec![SOURCE.into()],
            model: "sonnet".into(),
        }
    );
    compile(root.path(), &definition).unwrap();

    let mismatched = context(&repository, "other", &"f".repeat(64));
    assert!(snapshot.instantiate(&mismatched).is_err());

    #[cfg(unix)]
    {
        let mut authority = Authority::open(root.path()).unwrap();
        authority.advance_policy(0).unwrap();
        drop(authority);
        let configured = invoke(
            root.path(),
            &[
                "skill-workflow",
                "configure",
                "bound",
                "--skill",
                SKILL,
                "--runner",
                RUNNER_A,
                "--review-path",
                SOURCE,
                "--model",
                "sonnet",
            ],
        );
        assert!(
            configured.status.success(),
            "{}",
            String::from_utf8_lossy(&configured.stderr)
        );
        let authority = Authority::open(root.path()).unwrap();
        let refreshed = authority.change(&bound_context.change).unwrap();
        let refreshed_context = authority.review_context(&refreshed.context_digest).unwrap();
        let requirement = authority.workflow_requirement("main").unwrap().unwrap();
        assert!(refreshed.version > state.version);
        assert_eq!(
            refreshed_context.policy_epoch,
            authority.policy_epoch().unwrap()
        );
        drop(authority);

        let rebound_definition = requirement.instantiate(&refreshed_context).unwrap();
        let rebound_plan = compile(root.path(), &rebound_definition).unwrap();
        assert_eq!(
            agentscm::authority::workflows::WorkflowRequirement::from_plan("main", &rebound_plan,)
                .unwrap(),
            requirement
        );
    }
}

#[test]
fn malformed_templates_fail_before_they_become_plans() {
    let root = TempDir::new().unwrap();
    let valid_step = step("source", &[], true, source_action(&digest(SOURCE_BYTES)));
    let mut unknown = template("unknown", 1, SKILL, vec![valid_step.clone()]);
    unknown["extra"] = json!(true);
    let mut unknown_action = template("unknown-action", 1, SKILL, vec![valid_step.clone()]);
    unknown_action["steps"][0]["action"]["script"] = json!("./untrusted.sh");
    let cycle = template(
        "cycle",
        1,
        SKILL,
        vec![
            step("a", &["b"], true, source_action(&digest(SOURCE_BYTES))),
            step("b", &["a"], true, source_action(&digest(SOURCE_BYTES))),
        ],
    );
    let duplicate = template(
        "duplicate",
        1,
        SKILL,
        vec![valid_step.clone(), valid_step.clone()],
    );
    let bad_name = template("bad name", 1, SKILL, vec![valid_step.clone()]);
    let empty_name = template("", 1, SKILL, vec![valid_step.clone()]);
    let bad_version = template("zero", 0, SKILL, vec![valid_step]);
    let claude_only = template(
        "claude-only",
        1,
        SKILL,
        vec![step(
            "review",
            &[],
            true,
            json!({"kind":"claude_code_review", "paths":[SOURCE], "model":"sonnet"}),
        )],
    );
    let mut traversal = template(
        "traversal",
        1,
        SKILL,
        vec![step(
            "source",
            &[],
            true,
            source_action(&digest(SOURCE_BYTES)),
        )],
    );
    traversal["skill"]["resources"] = json!(["skills/reusable/v1/../escape.json"]);
    let engine = Engine::open(root.path()).unwrap();
    let revision = engine
        .checkpoint(
            None,
            &[
                put(SKILL, b"# Validation fixture\n".to_vec()),
                put(SOURCE, SOURCE_BYTES.to_vec()),
                put(
                    "skills/reusable/v1/unknown.json",
                    serde_json::to_vec(&unknown).unwrap(),
                ),
                put(
                    "skills/reusable/v1/unknown-action.json",
                    serde_json::to_vec(&unknown_action).unwrap(),
                ),
                put(
                    "skills/reusable/v1/cycle.json",
                    serde_json::to_vec(&cycle).unwrap(),
                ),
                put(
                    "skills/reusable/v1/duplicate.json",
                    serde_json::to_vec(&duplicate).unwrap(),
                ),
                put(
                    "skills/reusable/v1/bad-name.json",
                    serde_json::to_vec(&bad_name).unwrap(),
                ),
                put(
                    "skills/reusable/v1/empty-name.json",
                    serde_json::to_vec(&empty_name).unwrap(),
                ),
                put(
                    "skills/reusable/v1/bad-version.json",
                    serde_json::to_vec(&bad_version).unwrap(),
                ),
                put(
                    "skills/reusable/v1/traversal.json",
                    serde_json::to_vec(&traversal).unwrap(),
                ),
                put(
                    "skills/reusable/v1/claude-only.json",
                    serde_json::to_vec(&claude_only).unwrap(),
                ),
            ],
        )
        .unwrap();
    let authority = Authority::open(root.path()).unwrap();
    let repository = authority.repository_id().unwrap();
    drop(authority);
    let context = context(&repository, "validation", &revision);

    assert!(library::load(root.path(), &revision, "../workflow.json").is_err());
    assert!(library::load(root.path(), &revision, "skills/reusable/v1/unknown.json").is_err());
    assert!(
        library::load(
            root.path(),
            &revision,
            "skills/reusable/v1/unknown-action.json"
        )
        .is_err()
    );
    assert!(
        library::load(
            root.path(),
            &revision,
            "skills/reusable/v1/bad-version.json"
        )
        .is_err()
    );
    assert!(library::load(root.path(), &revision, "skills/reusable/v1/empty-name.json").is_err());
    for path in [
        "skills/reusable/v1/cycle.json",
        "skills/reusable/v1/duplicate.json",
        "skills/reusable/v1/bad-name.json",
        "skills/reusable/v1/traversal.json",
        "skills/reusable/v1/claude-only.json",
    ] {
        if let Ok(snapshot) = library::load(root.path(), &revision, path) {
            assert!(snapshot.instantiate(&context).is_err(), "accepted {path}");
        }
    }
}

#[cfg(unix)]
#[test]
fn source_only_configure_rejects_command_context_requirements_before_mutation() {
    let source_only = template(
        "source-only",
        1,
        SKILL,
        vec![step(
            "source",
            &[],
            true,
            source_action(&digest(SOURCE_BYTES)),
        )],
    );
    for (case, command_capability, command_evidence) in
        [("capability", true, false), ("evidence", false, true)]
    {
        let root = TempDir::new().unwrap();
        let engine = Engine::open(root.path()).unwrap();
        let revision = engine
            .checkpoint(
                None,
                &[
                    put(SKILL, b"# Source only\n".to_vec()),
                    put(SOURCE, SOURCE_BYTES.to_vec()),
                    put(LIBRARY_V1, serde_json::to_vec(&source_only).unwrap()),
                ],
            )
            .unwrap();
        let mut authority = Authority::open(root.path()).unwrap();
        let repository = authority.repository_id().unwrap();
        let mut required_context = context(&repository, case, &revision);
        if command_capability {
            required_context
                .required_capabilities
                .push(COMMAND_CAPABILITY.into());
        }
        if command_evidence {
            required_context.required_evidence.push(RequiredEvidence {
                identity: COMMAND_EVIDENCE.into(),
                digest: InputIdentity::Digest {
                    sha256: "d".repeat(64),
                },
            });
        }
        let before = authority.start_change(&required_context).unwrap();
        drop(authority);

        let rejected = invoke(
            root.path(),
            &[
                "skill-workflow",
                "library",
                "configure",
                case,
                "--library",
                LIBRARY_V1,
            ],
        );
        assert!(!rejected.status.success(), "accepted {case}");
        assert!(
            String::from_utf8_lossy(&rejected.stderr).contains("without command steps"),
            "{}",
            String::from_utf8_lossy(&rejected.stderr)
        );
        let authority = Authority::open(root.path()).unwrap();
        assert_eq!(authority.change(&before.id).unwrap(), before);
        assert!(authority.workflow_requirement("main").unwrap().is_none());
    }
}

#[cfg(unix)]
#[test]
fn configure_rejects_mixed_recipes_and_existing_requirements_without_partial_change_updates() {
    let mixed = TempDir::new().unwrap();
    let recipe_a = command_recipe(&["/bin/true"]);
    let recipe_b = command_recipe(&["/bin/false"]);
    let mut mixed_template = template(
        "mixed",
        1,
        SKILL,
        vec![
            step(
                "a",
                &[],
                true,
                json!({"kind":"command", "runner":"runner-a.json"}),
            ),
            step(
                "b",
                &["a"],
                true,
                json!({"kind":"command", "runner":"runner-b.json"}),
            ),
        ],
    );
    mixed_template["skill"]["resources"] = json!([RUNNER_A, RUNNER_B]);
    let engine = Engine::open(mixed.path()).unwrap();
    let revision = engine
        .checkpoint(
            None,
            &[
                put(SKILL, b"# Mixed recipes\n".to_vec()),
                put(SOURCE, SOURCE_BYTES.to_vec()),
                put(RUNNER_A, serde_json::to_vec(&recipe_a).unwrap()),
                put(RUNNER_B, serde_json::to_vec(&recipe_b).unwrap()),
                put(LIBRARY_V1, serde_json::to_vec(&mixed_template).unwrap()),
            ],
        )
        .unwrap();
    let mut authority = Authority::open(mixed.path()).unwrap();
    let repository = authority.repository_id().unwrap();
    let original_context = context(&repository, "mixed", &revision);
    let original = authority.start_change(&original_context).unwrap();
    drop(authority);
    let rejected = invoke(
        mixed.path(),
        &[
            "skill-workflow",
            "library",
            "configure",
            "mixed",
            "--library",
            LIBRARY_V1,
        ],
    );
    assert!(!rejected.status.success());
    let authority = Authority::open(mixed.path()).unwrap();
    assert_eq!(authority.change(&original.id).unwrap(), original);
    assert!(authority.workflow_requirement("main").unwrap().is_none());
    drop(authority);

    let stale = TempDir::new().unwrap();
    let engine = Engine::open(stale.path()).unwrap();
    let source_only = template(
        "source-only",
        1,
        SKILL,
        vec![step(
            "source",
            &[],
            true,
            source_action(&digest(SOURCE_BYTES)),
        )],
    );
    let stale_revision = engine
        .checkpoint(
            None,
            &[
                put(SKILL, b"# Stale policy\n".to_vec()),
                put(SOURCE, SOURCE_BYTES.to_vec()),
                put(LIBRARY_V1, serde_json::to_vec(&source_only).unwrap()),
            ],
        )
        .unwrap();
    let mut authority = Authority::open(stale.path()).unwrap();
    let stale_repository = authority.repository_id().unwrap();
    let stale_context = context(&stale_repository, "stale", &stale_revision);
    let stale_before = authority.start_change(&stale_context).unwrap();
    authority.advance_policy(0).unwrap();
    drop(authority);
    let configured = invoke(
        stale.path(),
        &[
            "skill-workflow",
            "library",
            "configure",
            "stale",
            "--library",
            LIBRARY_V1,
        ],
    );
    assert!(
        configured.status.success(),
        "{}",
        String::from_utf8_lossy(&configured.stderr)
    );
    let authority = Authority::open(stale.path()).unwrap();
    let stale_after = authority.change(&stale_before.id).unwrap();
    let refreshed_context = authority
        .review_context(&stale_after.context_digest)
        .unwrap();
    assert!(stale_after.version > stale_before.version);
    assert_eq!(
        refreshed_context.policy_epoch,
        authority.policy_epoch().unwrap()
    );
    assert!(authority.workflow_requirement("main").unwrap().is_some());
    drop(authority);
    let checked = invoke(stale.path(), &["skill-workflow", "check", "stale"]);
    assert!(
        checked.status.success(),
        "{}",
        String::from_utf8_lossy(&checked.stderr)
    );
    let checked = output_json(&checked);
    assert_eq!(checked["run"]["state"], "succeeded");
    assert!(checked["receipt"].is_string());

    let configured = TempDir::new().unwrap();
    let engine = Engine::open(configured.path()).unwrap();
    let revision = engine
        .checkpoint(
            None,
            &[
                put(SKILL, b"# Source only\n".to_vec()),
                put(SOURCE, SOURCE_BYTES.to_vec()),
                put(LIBRARY_V1, serde_json::to_vec(&source_only).unwrap()),
            ],
        )
        .unwrap();
    let mut authority = Authority::open(configured.path()).unwrap();
    let repository = authority.repository_id().unwrap();
    let source_context = context(&repository, "configured", &revision);
    let before_inspect = authority.start_change(&source_context).unwrap();
    drop(authority);
    let inspected = invoke(
        configured.path(),
        &[
            "skill-workflow",
            "library",
            "inspect",
            "configured",
            "--library",
            LIBRARY_V1,
        ],
    );
    assert!(
        inspected.status.success(),
        "{}",
        String::from_utf8_lossy(&inspected.stderr)
    );
    assert_eq!(output_json(&inspected)["admission"], "not_assessed");
    let authority = Authority::open(configured.path()).unwrap();
    assert_eq!(
        authority.change(&before_inspect.id).unwrap(),
        before_inspect
    );
    assert!(authority.workflow_requirement("main").unwrap().is_none());
    drop(authority);
    let first = invoke(
        configured.path(),
        &[
            "skill-workflow",
            "library",
            "configure",
            "configured",
            "--library",
            LIBRARY_V1,
        ],
    );
    assert!(
        first.status.success(),
        "{}",
        String::from_utf8_lossy(&first.stderr)
    );
    assert!(!configured.path().join("execution").exists());
    let checked = invoke(
        configured.path(),
        &[
            "skill-workflow",
            "check",
            "configured",
            "--docker",
            "/synthetic/missing-docker",
            "--docker-socket",
            "/synthetic/missing-docker.sock",
            "--claude",
            "/synthetic/missing-claude",
        ],
    );
    assert!(
        checked.status.success(),
        "{}",
        String::from_utf8_lossy(&checked.stderr)
    );
    let checked = output_json(&checked);
    assert_eq!(checked["run"]["state"], "succeeded");
    assert!(checked["receipt"].is_string());
    assert!(!configured.path().join("execution").exists());
    let checked_without_flags = invoke(
        configured.path(),
        &["skill-workflow", "check", "configured"],
    );
    assert!(
        checked_without_flags.status.success(),
        "{}",
        String::from_utf8_lossy(&checked_without_flags.stderr)
    );
    assert_eq!(
        output_json(&checked_without_flags)["run"]["state"],
        "succeeded"
    );
    assert!(!configured.path().join("execution").exists());
    let run_id = checked["run"]["run_id"].as_str().unwrap();
    let authority = Authority::open(configured.path()).unwrap();
    let change = ChangeId {
        repository,
        id: "configured".into(),
    };
    let before_handoff = authority.change(&change).unwrap();
    let target_before_handoff = authority.target("main").unwrap();
    drop(authority);
    let handed_off = invoke(configured.path(), &["skill-workflow", "handoff", run_id]);
    assert!(handed_off.status.success());
    let handed_off = output_json(&handed_off);
    assert_eq!(handed_off["run"]["state"], "succeeded");
    assert_eq!(handed_off["admission"], "not_assessed");
    assert_eq!(handed_off["subject_scope"], "source_revision_only");
    assert_eq!(handed_off["command_context_bindings"], json!([]));
    let authority = Authority::open(configured.path()).unwrap();
    assert_eq!(authority.change(&change).unwrap(), before_handoff);
    assert_eq!(authority.target("main").unwrap(), target_before_handoff);
    let before_retry = authority.change(&change).unwrap();
    let requirement_before = authority.workflow_requirement("main").unwrap().unwrap();
    drop(authority);
    let repeated = invoke(
        configured.path(),
        &[
            "skill-workflow",
            "library",
            "configure",
            "configured",
            "--library",
            LIBRARY_V1,
        ],
    );
    assert!(!repeated.status.success());
    let authority = Authority::open(configured.path()).unwrap();
    assert_eq!(authority.change(&change).unwrap(), before_retry);
    assert_eq!(
        authority.workflow_requirement("main").unwrap().unwrap(),
        requirement_before
    );
}

#[cfg(unix)]
#[test]
fn command_only_check_requires_its_runtime_but_not_a_claude_executable() {
    let root = TempDir::new().unwrap();
    let recipe = command_recipe(&["/bin/true"]);
    let mut manifest = template(
        "command-only",
        1,
        SKILL,
        vec![step(
            "tests",
            &[],
            true,
            json!({"kind":"command", "runner":"runner-a.json"}),
        )],
    );
    manifest["skill"]["resources"] = json!([RUNNER_A]);
    let engine = Engine::open(root.path()).unwrap();
    let revision = engine
        .checkpoint(
            None,
            &[
                put(SKILL, b"# Command only\n".to_vec()),
                put(SOURCE, SOURCE_BYTES.to_vec()),
                put(RUNNER_A, serde_json::to_vec(&recipe).unwrap()),
                put(LIBRARY_V1, serde_json::to_vec(&manifest).unwrap()),
            ],
        )
        .unwrap();
    let mut authority = Authority::open(root.path()).unwrap();
    let repository = authority.repository_id().unwrap();
    authority
        .start_change(&context(&repository, "command-only", &revision))
        .unwrap();
    drop(authority);
    let configured = invoke(
        root.path(),
        &[
            "skill-workflow",
            "library",
            "configure",
            "command-only",
            "--library",
            LIBRARY_V1,
        ],
    );
    assert!(
        configured.status.success(),
        "{}",
        String::from_utf8_lossy(&configured.stderr)
    );
    let check = invoke(root.path(), &["skill-workflow", "check", "command-only"]);
    assert!(!check.status.success());
    let error = String::from_utf8_lossy(&check.stderr);
    assert!(error.contains("--docker") && error.contains("--docker-socket"));
    assert!(!error.contains("--claude"));

    let docker = root.path().join("fake-docker.py");
    fs::write(&docker, fake_docker::FAKE_DOCKER).unwrap();
    fs::set_permissions(&docker, fs::Permissions::from_mode(0o700)).unwrap();
    let socket = root.path().join("docker.sock");
    let _listener = UnixListener::bind(&socket).unwrap();
    let passed = invoke(
        root.path(),
        &[
            "skill-workflow",
            "check",
            "command-only",
            "--docker",
            docker.to_str().unwrap(),
            "--docker-socket",
            socket.to_str().unwrap(),
        ],
    );
    assert!(
        passed.status.success(),
        "{}",
        String::from_utf8_lossy(&passed.stderr)
    );
    let passed = output_json(&passed);
    assert_eq!(passed["run"]["state"], "succeeded");
    assert!(passed["receipt"].is_string());
    let run_id = passed["run"]["run_id"].as_str().unwrap();
    let handoff = invoke(root.path(), &["skill-workflow", "handoff", run_id]);
    assert!(handoff.status.success());
    let handoff = output_json(&handoff);
    assert_eq!(
        handoff["subject_scope"],
        "source_revision_and_command_contexts"
    );
    assert_eq!(
        handoff["command_context_bindings"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    assert_eq!(
        handoff["command_context_bindings"][0]["change"]["id"],
        "command-only"
    );
    assert_eq!(
        handoff["command_context_bindings"][0]["context_digest"],
        handoff["definition"]["steps"][0]["action"]["context_digest"]
    );
}

#[test]
fn handoff_preserves_pending_failed_and_omitted_results_without_advancing() {
    let root = TempDir::new().unwrap();
    let engine = Engine::open(root.path()).unwrap();
    let manifest = template(
        "handoff",
        1,
        SKILL,
        vec![
            step("source", &[], true, source_action(&digest(SOURCE_BYTES))),
            step(
                "optional",
                &["source"],
                false,
                json!({"kind":"require_capability", "capability":"model.review"}),
            ),
        ],
    );
    let revision = engine
        .checkpoint(
            None,
            &[
                put(SKILL, b"# Handoff fixture\n".to_vec()),
                put(SOURCE, SOURCE_BYTES.to_vec()),
                put(LIBRARY_V1, serde_json::to_vec(&manifest).unwrap()),
            ],
        )
        .unwrap();
    let authority = Authority::open(root.path()).unwrap();
    let repository = authority.repository_id().unwrap();
    drop(authority);
    let context = context(&repository, "handoff", &revision);
    let snapshot = library::load(root.path(), &revision, LIBRARY_V1).unwrap();
    let definition = snapshot.instantiate(&context).unwrap();
    let plan = compile(root.path(), &definition).unwrap();

    start(root.path(), "pending", &plan).unwrap();
    start(root.path(), "omitted", &plan).unwrap();
    let omitted = advance(root.path(), "omitted", 32).unwrap();
    assert_eq!(omitted.state, RunState::CompletedWithOmissions);
    assert_eq!(omitted.steps[1].state, StepState::Blocked);

    let mut failed_definition = definition;
    let StepAction::SourcePaths { expectations } = &mut failed_definition.steps[0].action else {
        unreachable!()
    };
    expectations[0].sha256 = "0".repeat(64);
    let failed_plan = compile(root.path(), &failed_definition).unwrap();
    start(root.path(), "failed", &failed_plan).unwrap();
    let failed = advance(root.path(), "failed", 32).unwrap();
    assert_eq!(failed.state, RunState::Failed);

    for (run_id, expected) in [
        ("pending", RunState::Pending),
        ("omitted", RunState::CompletedWithOmissions),
        ("failed", RunState::Failed),
    ] {
        let before = status(root.path(), run_id).unwrap();
        let output = invoke(root.path(), &["skill-workflow", "handoff", run_id]);
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let handoff = output_json(&output);
        assert_eq!(
            handoff["run"]["state"],
            serde_json::to_value(expected).unwrap()
        );
        assert_eq!(handoff["admission"], "not_assessed");
        assert_eq!(handoff["subject_scope"], "source_revision_only");
        assert_eq!(handoff["command_context_bindings"], json!([]));
        assert!(handoff["definition"].is_object());
        assert!(handoff["next_action"].is_string());
        assert_eq!(status(root.path(), run_id).unwrap(), before);
    }
}

#[cfg(unix)]
#[test]
fn productive_library_configure_preserves_change_and_context() {
    let root = TempDir::new().unwrap();
    let engine = Engine::open(root.path()).unwrap();
    let manifest = template(
        "productive",
        1,
        SKILL,
        vec![step(
            "draft",
            &[],
            true,
            json!({"kind":"agent_action","resource":"draft.json"}),
        )],
    );
    let mut manifest = manifest;
    manifest["skill"]["resources"] = json!([
        "skills/reusable/v1/draft.json",
        "skills/reusable/v1/draft.md"
    ]);
    let action = json!({"version":1,"inputs":{},"output":{"name":"draft","media_type":"text/markdown","max_bytes":1024},"action":{"kind":"prompt","prompt":"draft.md","required_tools":[]}});
    let revision = engine
        .checkpoint(
            None,
            &[
                put(SKILL, b"# Productive\n".to_vec()),
                put(LIBRARY_V1, serde_json::to_vec(&manifest).unwrap()),
                put(
                    "skills/reusable/v1/draft.json",
                    serde_json::to_vec(&action).unwrap(),
                ),
                put(
                    "skills/reusable/v1/draft.md",
                    b"Draft synthetic release notes.\n".to_vec(),
                ),
            ],
        )
        .unwrap();
    let mut authority = Authority::open(root.path()).unwrap();
    let repository = authority.repository_id().unwrap();
    let before_context = context(&repository, "productive", &revision);
    let before = authority.start_change(&before_context).unwrap();
    drop(authority);
    let output = invoke(
        root.path(),
        &[
            "skill-workflow",
            "library",
            "configure",
            "productive",
            "--library",
            LIBRARY_V1,
        ],
    );
    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr)
            .contains("productive actions cannot be native admission")
    );
    let authority = Authority::open(root.path()).unwrap();
    assert_eq!(authority.change(&before.id).unwrap(), before);
    assert_eq!(
        authority.review_context(&before.context_digest).unwrap(),
        before_context
    );
    assert!(authority.workflow_requirement("main").unwrap().is_none());
}
