#![cfg(unix)]
//! Real Hub HTTP controls with a local synthetic forge and source-only native state.
//! Source-only service execution uses an inert socket; no Docker or provider process runs.
#[path = "common/workspace_hub.rs"]
mod workspace_hub;

use std::sync::Arc;

use agentscm::authority::Authority;
use agentscm::hub::store::{mint_agent_token, mint_bound_agent_token};
use agentscm::hub::workspace_execution::{AgentTaskProfile, ExecutionConfig, ExecutionService};
use agentscm::skill_workflow::{
    self, CriterionOutcome, CriterionResult, EvidenceTrustClass, ExecutorEnrollment,
    ObservationSignatureAlgorithm, TaskAttemptView, TaskOutcome, TaskResultAssertion, WorkflowTask,
};
use agentscm::team::auth::Role;
use axum::http::header;
use ed25519_dalek::SigningKey;
use reqwest::{Method, Url};
use serde_json::{Value, json};
use workspace_hub::{Env, NativeSourceProfile, native_source_profile};

const REPO: &str = "/v1/hub/workspace/repositories/synthetic%2Fproject";

async fn execution_env(
    root: &std::path::Path,
    native: &NativeSourceProfile,
) -> (Env, Arc<ExecutionService>) {
    let profile = native.profile.clone();
    let queue = root.join("execution.sqlite");
    let installed = Arc::new(std::sync::OnceLock::new());
    let retain = installed.clone();
    let env = Env::with_state(move |state, workspace| {
        let service = ExecutionService::open(
            &queue,
            ExecutionConfig {
                version: 1,
                source_profiles: vec![],
                agent_task_profiles: vec![],
                profiles: vec![profile],
            },
            workspace,
        )
        .unwrap();
        let service = Arc::new(service);
        retain.set(service.clone()).ok().unwrap();
        state.with_workspace_execution(service)
    })
    .await;
    (env, installed.get().unwrap().clone())
}

async fn agent_only_env(
    root: &std::path::Path,
    native: &NativeSourceProfile,
) -> (Env, Arc<ExecutionService>) {
    let profile = AgentTaskProfile {
        repository_id: native.profile.repository_id.clone(),
        profile_id: native.profile.profile_id.clone(),
        native_repository: native.profile.native_repository.clone(),
        data_dir: native.profile.data_dir.clone(),
        target: native.profile.target.clone(),
        requirement_digest: native.profile.requirement_digest.clone(),
    };
    let queue = root.join("agent-only.sqlite");
    let installed = Arc::new(std::sync::OnceLock::new());
    let retain = installed.clone();
    let env = Env::with_state(move |state, workspace| {
        let service = Arc::new(
            ExecutionService::open(
                &queue,
                ExecutionConfig {
                    version: 1,
                    profiles: vec![],
                    source_profiles: vec![],
                    agent_task_profiles: vec![profile],
                },
                workspace,
            )
            .unwrap(),
        );
        retain.set(service.clone()).ok().unwrap();
        state.with_workspace_execution(service)
    })
    .await;
    (env, installed.get().unwrap().clone())
}

fn install_executor(native: &NativeSourceProfile) -> serde_json::Value {
    let public = SigningKey::from_bytes(&[7_u8; 32])
        .verifying_key()
        .to_bytes();
    let public_hex = public
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    let enrollment = ExecutorEnrollment {
        version: 1,
        registration_id: "registration-http-1".into(),
        executor_id: "executor-http-1".into(),
        operator_id: "operator-http-1".into(),
        repository: native.context.change.repository.clone(),
        step_ids: vec!["source".into()],
        registration_epoch: 1,
        verification_key_id: agentscm::policy::issuer_key_id(&public_hex).unwrap(),
        verification_key_sha256: agentscm::store::digest(&public),
        signature_algorithm: ObservationSignatureAlgorithm::Ed25519,
        issued_at_ms: 1,
    };
    Authority::open(&native.profile.data_dir)
        .unwrap()
        .enroll_workflow_executor(&enrollment, &public_hex)
        .unwrap();
    serde_json::to_value(enrollment.binding().unwrap()).unwrap()
}

async fn agent_client(
    root: &std::path::Path,
    native: &NativeSourceProfile,
) -> (Env, String, Value) {
    let executor = install_executor(native);
    let (env, _) = agent_only_env(root, native).await;
    let (_, owner) = env.signin(101).await;
    let owner_session = env.session(&owner).await;
    let grant = env.member(
        202,
        &[Role::Reader, Role::Contributor],
        true,
        env.workspace.settings().unwrap().generation,
        "add-agent-task-transport-contributor",
    );
    assert_eq!(
        env.request(
            Method::PUT,
            "/v1/hub/workspace/members",
            Some(&owner),
            Some(owner_session["csrf_token"].as_str().unwrap()),
            None,
            Some(grant),
        )
        .await
        .0,
        200
    );
    env.signin(202).await;
    let user = env
        .store
        .list_users()
        .unwrap()
        .into_iter()
        .find(|user| user.identity.user_id == 202)
        .unwrap();
    let (_, token) = mint_agent_token(
        &*env.store,
        &user.id,
        "agent-task-transport-client",
        "synthetic/project",
        3600,
    )
    .unwrap();
    (env, token.expose().to_owned(), executor)
}

fn claim_input(task: &WorkflowTask, executor: &Value, suffix: &str) -> Value {
    json!({
        "version":1,
        "request_id":format!("claim-{suffix}"),
        "execution_operation_id":format!("execute-{suffix}"),
        "task_id":task.task_id,
        "task_digest":task.digest().unwrap(),
        "attempt_id":format!("attempt-{suffix}"),
        "executor":executor,
        "requested_lease_ms":60_000
    })
}

fn mutation_input(view: &TaskAttemptView, request_id: &str, kind: &str) -> Value {
    json!({
        "version":1,
        "request_id":request_id,
        "execution_operation_id":view.attempt.execution_operation_id,
        "task_id":view.attempt.task_id,
        "task_digest":view.attempt.task_digest,
        "attempt_id":view.attempt.attempt_id,
        "attempt_digest":view.attempt.digest().unwrap(),
        "fence":view.attempt.lease.fence,
        "executor":view.attempt.executor,
        "action":{"kind":kind}
    })
}

fn large_assertion_input(task: &WorkflowTask, view: &TaskAttemptView, request_id: &str) -> Value {
    let assertion = TaskResultAssertion {
        version: 1,
        task_id: task.task_id.clone(),
        task_digest: task.digest().unwrap(),
        attempt_id: view.attempt.attempt_id.clone(),
        attempt_digest: view.attempt.digest().unwrap(),
        request_id: request_id.into(),
        execution_operation_id: view.attempt.execution_operation_id.clone(),
        agent: view.attempt.agent.clone(),
        executor: view.attempt.executor.clone(),
        fence: view.attempt.lease.fence,
        trust_class: EvidenceTrustClass::AgentAssertion,
        outcome: TaskOutcome::Incomplete,
        criteria: vec![CriterionResult {
            criterion: task.entrypoint.required_criteria[0].id.clone(),
            outcome: CriterionOutcome::Incomplete,
            findings: vec!["\u{1}".repeat(700); 16],
            evidence_digests: vec![],
        }],
        executor_provenance: None,
    };
    let canonical = assertion.canonical_bytes().unwrap();
    assert!(canonical.len() > 64 * 1024);
    assert!(canonical.len() <= 256 * 1024);
    let mut value = serde_json::to_value(assertion).unwrap();
    value.as_object_mut().unwrap().remove("agent");
    value
}

fn mcp_call(name: &str, arguments: Value) -> Value {
    json!({"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":name,"arguments":arguments}})
}

fn mcp_envelope(response: &Value) -> Value {
    serde_json::from_str(response["result"]["content"][0]["text"].as_str().unwrap()).unwrap()
}

#[tokio::test]
async fn agent_only_http_and_workspace_mcp_expose_the_same_side_effect_free_requirements() {
    let root = tempfile::tempdir().unwrap();
    let native = native_source_profile(root.path());
    let (env, service) = agent_only_env(root.path(), &native).await;
    assert!(
        !service.server_execution_enabled(),
        "agent-only configuration must not start the server executor"
    );
    let (_, owner) = env.signin(101).await;
    let owner_session = env.session(&owner).await;
    let grant = env.member(
        202,
        &[Role::Reader, Role::Contributor],
        true,
        env.workspace.settings().unwrap().generation,
        "add-agent-task-contributor",
    );
    assert_eq!(
        env.request(
            Method::PUT,
            "/v1/hub/workspace/members",
            Some(&owner),
            Some(owner_session["csrf_token"].as_str().unwrap()),
            None,
            Some(grant),
        )
        .await
        .0,
        200
    );
    let (_, member_cookie) = env.signin(202).await;
    let user = env
        .store
        .list_users()
        .unwrap()
        .into_iter()
        .find(|user| user.identity.user_id == 202)
        .unwrap();
    let (_, token) = mint_agent_token(
        &*env.store,
        &user.id,
        "agent-task-client",
        "synthetic/project",
        3600,
    )
    .unwrap();
    let task_path = format!("{REPO}/profiles/checks/agent-tasks");
    let body = json!({"operation":"requirements","subject":native.subject});
    let journal = native.profile.data_dir.join("skill-workflows/state.sqlite");
    assert!(!journal.exists());
    let http = env
        .request(
            Method::POST,
            &task_path,
            None,
            None,
            Some(token.expose()),
            Some(body.clone()),
        )
        .await;
    assert_eq!(http.0, 200, "{http:?}");
    assert_eq!(http.1["capability"], "agentscm.agent-workflow-tasks/v1");
    assert_eq!(http.1["schema_version"], 1);
    assert!(
        http.1["result"]
            .as_array()
            .is_some_and(|tasks| !tasks.is_empty())
    );
    assert!(
        !journal.exists(),
        "requirements must not initialize the task journal"
    );

    let mcp = env
        .request(
            Method::POST,
            "/v1/hub/workspace/mcp?repository=synthetic%2Fproject&profile=checks",
            None,
            None,
            Some(token.expose()),
            Some(json!({"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"agentscm_task_requirements","arguments":{"subject":native.subject}}})),
        )
        .await;
    assert_eq!(mcp.0, 200, "{mcp:?}");
    assert_eq!(mcp.1["result"]["isError"], false);
    let envelope: Value =
        serde_json::from_str(mcp.1["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
    assert_eq!(envelope, http.1);
    assert!(
        !journal.exists(),
        "MCP requirements must remain side-effect free"
    );
    let digest = "0".repeat(64);
    let executor = json!({"registration_id":"missing-registration","executor_id":"missing-executor","registration_epoch":1,"verification_key_id":"missing-key"});
    let status = json!({"version":1,"task_id":"missing-task","task_digest":digest.clone(),"attempt_id":"missing-attempt","attempt_digest":digest.clone(),"fence":1,"executor":executor.clone()});
    let response = env
        .request(
            Method::POST,
            &task_path,
            None,
            None,
            Some(token.expose()),
            Some(json!({"operation":"status","subject":native.subject,"status":status})),
        )
        .await;
    assert_eq!(response.0, 200, "{response:?}");
    assert_eq!(response.1["result"], Value::Null);
    let recovery = env
        .request(
            Method::POST,
            &task_path,
            None,
            None,
            Some(token.expose()),
            Some(json!({"operation":"recovery","subject":native.subject,"mutation":{
                "version":1,"request_id":"missing-recovery","execution_operation_id":"missing-execution",
                "task_id":"missing-task","task_digest":digest.clone(),"attempt_id":"missing-attempt",
                "attempt_digest":digest,"fence":1,"executor":executor,"action":{"kind":"heartbeat"}
            }})),
        )
        .await;
    assert_eq!(recovery.0, 200, "{recovery:?}");
    assert_eq!(recovery.1["result"]["disposition"], "conclusive_rejection");
    assert_eq!(recovery.1["result"]["observed_state"], "unknown");
    assert!(
        !journal.exists(),
        "status and recovery must not initialize the task journal"
    );

    let mut injected_actor = body.clone();
    injected_actor["actor"] = json!({"owner_id":"caller","agent_id":"caller"});
    assert_eq!(
        env.request(
            Method::POST,
            &task_path,
            None,
            None,
            Some(token.expose()),
            Some(injected_actor),
        )
        .await
        .0,
        400,
        "the wire contract must not accept a caller-selected actor"
    );
    let (_, read_token, _) = mint_bound_agent_token(
        &*env.store,
        &user.id,
        "read-only-task-client",
        "synthetic/project",
        7,
        3600,
        true,
    )
    .unwrap();
    assert_eq!(
        env.request(
            Method::POST,
            &task_path,
            None,
            None,
            Some(read_token.expose()),
            Some(body.clone()),
        )
        .await
        .0,
        403,
        "legacy read-only scope must not gain task tools"
    );

    let browser = env
        .request(
            Method::POST,
            &task_path,
            Some(&member_cookie),
            Some("not-a-valid-csrf-token"),
            None,
            Some(body),
        )
        .await;
    assert_eq!(browser.0, 403);
}

#[tokio::test]
async fn task_http_accepts_a_claimed_assertion_above_64_kib_and_rejects_over_route_bound() {
    let root = tempfile::tempdir().unwrap();
    let native = native_source_profile(root.path());
    let (env, token, executor) = agent_client(root.path(), &native).await;
    let task_path = format!("{REPO}/profiles/checks/agent-tasks");
    let requirements = env
        .request(
            Method::POST,
            &task_path,
            None,
            None,
            Some(&token),
            Some(json!({"operation":"requirements","subject":native.subject})),
        )
        .await;
    assert_eq!(requirements.0, 200, "{requirements:?}");
    let task: WorkflowTask = serde_json::from_value(requirements.1["result"][0].clone()).unwrap();
    let claim = env
        .request(
            Method::POST,
            &task_path,
            None,
            None,
            Some(&token),
            Some(json!({"operation":"claim","subject":native.subject,"claim":claim_input(&task,&executor,"http-large")})),
        )
        .await;
    assert_eq!(claim.0, 200, "{claim:?}");
    let view: TaskAttemptView = serde_json::from_value(claim.1["result"].clone()).unwrap();
    let assertion = large_assertion_input(&task, &view, "result-http-large");
    let request = json!({"operation":"result","subject":native.subject,"assertion":assertion});
    assert!(request.to_string().len() > 64 * 1024);
    let result = env
        .request(
            Method::POST,
            &task_path,
            None,
            None,
            Some(&token),
            Some(request),
        )
        .await;
    assert_eq!(result.0, 200, "{result:?}");
    assert_eq!(result.1["result"]["state"], "finished");

    let over_bound = env
        .request(
            Method::POST,
            &task_path,
            None,
            None,
            Some(&token),
            Some(json!({"padding":"x".repeat(400 * 1024)})),
        )
        .await;
    assert_eq!(over_bound.0, 400, "{over_bound:?}");
}

#[tokio::test]
async fn workspace_mcp_accepts_real_mutation_and_large_result_and_rejects_over_route_bound() {
    let root = tempfile::tempdir().unwrap();
    let native = native_source_profile(root.path());
    let (env, token, executor) = agent_client(root.path(), &native).await;
    let mcp_path = "/v1/hub/workspace/mcp?repository=synthetic%2Fproject&profile=checks";
    let requirements = env
        .request(
            Method::POST,
            mcp_path,
            None,
            None,
            Some(&token),
            Some(mcp_call(
                "agentscm_task_requirements",
                json!({"subject":native.subject}),
            )),
        )
        .await;
    assert_eq!(requirements.0, 200, "{requirements:?}");
    let requirements = mcp_envelope(&requirements.1);
    let task: WorkflowTask = serde_json::from_value(requirements["result"][0].clone()).unwrap();
    let claim = env
        .request(
            Method::POST,
            mcp_path,
            None,
            None,
            Some(&token),
            Some(mcp_call(
                "agentscm_task_claim",
                json!({"subject":native.subject,"claim":claim_input(&task,&executor,"mcp-large")}),
            )),
        )
        .await;
    assert_eq!(claim.0, 200, "{claim:?}");
    let claim = mcp_envelope(&claim.1);
    let view: TaskAttemptView = serde_json::from_value(claim["result"].clone()).unwrap();
    let heartbeat = env
        .request(
            Method::POST,
            mcp_path,
            None,
            None,
            Some(&token),
            Some(mcp_call(
                "agentscm_task_heartbeat",
                json!({"subject":native.subject,"mutation":mutation_input(&view,"heartbeat-mcp-large","heartbeat")}),
            )),
        )
        .await;
    assert_eq!(heartbeat.0, 200, "{heartbeat:?}");
    assert_eq!(heartbeat.1["result"]["isError"], false);
    let heartbeat = mcp_envelope(&heartbeat.1);
    let view: TaskAttemptView = serde_json::from_value(heartbeat["result"].clone()).unwrap();
    let assertion = large_assertion_input(&task, &view, "result-mcp-large");
    let request = mcp_call(
        "agentscm_task_result",
        json!({"subject":native.subject,"assertion":assertion}),
    );
    assert!(request.to_string().len() > 64 * 1024);
    let result = env
        .request(
            Method::POST,
            mcp_path,
            None,
            None,
            Some(&token),
            Some(request),
        )
        .await;
    assert_eq!(result.0, 200, "{result:?}");
    assert_eq!(result.1["result"]["isError"], false);
    assert_eq!(mcp_envelope(&result.1)["result"]["state"], "finished");

    let over_bound = env
        .request(
            Method::POST,
            mcp_path,
            None,
            None,
            Some(&token),
            Some(json!({"padding":"x".repeat(400 * 1024)})),
        )
        .await;
    assert_ne!(over_bound.0, 200, "{over_bound:?}");
}

#[tokio::test]
async fn task_http_and_mcp_distinguish_invalid_claims_from_unavailable_journal() {
    let root = tempfile::tempdir().unwrap();
    let native = native_source_profile(root.path());
    let (env, token, executor) = agent_client(root.path(), &native).await;
    let task_path = format!("{REPO}/profiles/checks/agent-tasks");
    let mcp_path = "/v1/hub/workspace/mcp?repository=synthetic%2Fproject&profile=checks";
    let requirements = env
        .request(
            Method::POST,
            &task_path,
            None,
            None,
            Some(&token),
            Some(json!({"operation":"requirements","subject":native.subject})),
        )
        .await;
    assert_eq!(requirements.0, 200, "{requirements:?}");
    let task: WorkflowTask = serde_json::from_value(requirements.1["result"][0].clone()).unwrap();
    let mut unknown = claim_input(&task, &executor, "unknown");
    unknown["task_id"] = json!("unknown-task");
    unknown["task_digest"] = json!("0".repeat(64));
    let invalid_http = env
        .request(
            Method::POST,
            &task_path,
            None,
            None,
            Some(&token),
            Some(json!({"operation":"claim","subject":native.subject,"claim":unknown.clone()})),
        )
        .await;
    assert_eq!(invalid_http, (400, json!({"error":"invalid_request"})));
    let invalid_mcp = env
        .request(
            Method::POST,
            mcp_path,
            None,
            None,
            Some(&token),
            Some(mcp_call(
                "agentscm_task_claim",
                json!({"subject":native.subject,"claim":unknown}),
            )),
        )
        .await;
    assert_eq!(invalid_mcp.0, 200, "{invalid_mcp:?}");
    assert_eq!(invalid_mcp.1["result"]["isError"], true);
    assert_eq!(
        invalid_mcp.1["result"]["content"][0]["text"],
        "AgentSCM task request was invalid or not found"
    );

    std::fs::write(
        native.profile.data_dir.join("skill-workflows"),
        b"block journal directory creation",
    )
    .unwrap();
    let unavailable_http = env
        .request(
            Method::POST,
            &task_path,
            None,
            None,
            Some(&token),
            Some(json!({"operation":"claim","subject":native.subject,"claim":claim_input(&task,&executor,"unavailable-http")})),
        )
        .await;
    assert_eq!(unavailable_http, (503, json!({"error":"unavailable"})));
    let unavailable_mcp = env
        .request(
            Method::POST,
            mcp_path,
            None,
            None,
            Some(&token),
            Some(mcp_call(
                "agentscm_task_claim",
                json!({"subject":native.subject,"claim":claim_input(&task,&executor,"unavailable-mcp")}),
            )),
        )
        .await;
    assert_eq!(unavailable_mcp.0, 200, "{unavailable_mcp:?}");
    assert_eq!(unavailable_mcp.1["error"]["code"], -32603);
    assert_eq!(
        unavailable_mcp.1["error"]["message"],
        "temporarily unavailable; inspect task status or recovery before retrying"
    );
    assert!(
        !unavailable_mcp
            .1
            .to_string()
            .contains(root.path().to_str().unwrap())
    );
}

fn start_request(native: &NativeSourceProfile, generation: &Value, id: &str) -> Value {
    json!({"request_id":id,"expected_generation":generation,"profile_id":"checks","subject":native.subject})
}

#[tokio::test]
async fn http_queue_is_exact_idempotent_scoped_and_cancellable_with_retained_read_projection() {
    let root = tempfile::tempdir().unwrap();
    let native = native_source_profile(root.path());
    let (env, _service) = execution_env(root.path(), &native).await;
    let (_, cookie) = env.signin(101).await;
    let session = env.session(&cookie).await;
    let csrf = session["csrf_token"].as_str().unwrap();
    let generation = &session["actor"]["generation"];
    let repos = env
        .request(
            Method::GET,
            "/v1/hub/workspace/repositories",
            Some(&cookie),
            None,
            None,
            None,
        )
        .await;
    assert_eq!(repos.0, 200);
    assert_eq!(
        repos.1["repositories"][0]["repository_id"],
        "synthetic/project"
    );
    assert_eq!(
        repos.1["repositories"][0]["profiles"][0]["readiness"],
        "configured"
    );
    assert!(
        !repos.1.to_string().contains(root.path().to_str().unwrap()),
        "read views must not expose operator paths"
    );
    let subjects = env
        .request(
            Method::GET,
            &format!("{REPO}/profiles/checks/subjects?limit=1"),
            Some(&cookie),
            None,
            None,
            None,
        )
        .await;
    assert_eq!(
        subjects,
        (200, json!({"subjects":[native.subject],"next_cursor":null}))
    );
    let subject = env
        .request(
            Method::GET,
            &format!("{REPO}/profiles/checks/subject?change_id=work"),
            Some(&cookie),
            None,
            None,
            None,
        )
        .await;
    assert_eq!(subject, (200, json!({"subject":native.subject})));
    let path = format!("{REPO}/runs");
    let request = start_request(&native, generation, "queued-http");
    assert_eq!(
        env.request(
            Method::POST,
            &path,
            Some(&cookie),
            None,
            None,
            Some(request.clone())
        )
        .await
        .0,
        403
    );
    for field in ["data_dir", "executable", "docker_socket", "provider_url"] {
        let mut unknown = request.clone();
        unknown[field] = json!("untrusted-request-value");
        assert_eq!(
            env.request(
                Method::POST,
                &path,
                Some(&cookie),
                Some(csrf),
                None,
                Some(unknown)
            )
            .await
            .0,
            400
        );
    }
    let mut unknown = request.clone();
    unknown["subject"]["policy_epoch"] = json!(0);
    assert_eq!(
        env.request(
            Method::POST,
            &path,
            Some(&cookie),
            Some(csrf),
            None,
            Some(unknown)
        )
        .await
        .0,
        400
    );
    let mut stale = request.clone();
    stale["request_id"] = json!("stale-http");
    stale["subject"]["context_digest"] = json!("0".repeat(64));
    assert_eq!(
        env.request(
            Method::POST,
            &path,
            Some(&cookie),
            Some(csrf),
            None,
            Some(stale)
        )
        .await
        .0,
        400
    );
    let mut stale = request.clone();
    stale["expected_generation"] = json!(0);
    assert_eq!(
        env.request(
            Method::POST,
            &path,
            Some(&cookie),
            Some(csrf),
            None,
            Some(stale)
        )
        .await,
        (409, json!({"error":"generation_conflict"}))
    );
    let (status, run) = env
        .request(
            Method::POST,
            &path,
            Some(&cookie),
            Some(csrf),
            None,
            Some(request.clone()),
        )
        .await;
    assert_eq!(status, 200, "{run}");
    assert_eq!(run["state"], "queued");
    assert_eq!(run["receipt"], Value::Null);
    assert_eq!(
        env.request(
            Method::POST,
            &path,
            Some(&cookie),
            Some(csrf),
            None,
            Some(request.clone())
        )
        .await,
        (200, run.clone())
    );
    let mut collision = request;
    collision["profile_id"] = json!("different-profile");
    assert_eq!(
        env.request(
            Method::POST,
            &path,
            Some(&cookie),
            Some(csrf),
            None,
            Some(collision)
        )
        .await,
        (409, json!({"error":"idempotency_conflict"}))
    );
    let run_id = run["run_id"].as_str().unwrap();
    assert_eq!(
        env.request(
            Method::GET,
            &format!("{REPO}/runs/{run_id}"),
            Some(&cookie),
            None,
            None,
            None
        )
        .await,
        (200, run.clone())
    );
    assert_eq!(
        env.request(
            Method::GET,
            &format!("{REPO}/runs?limit=1"),
            Some(&cookie),
            None,
            None,
            None
        )
        .await,
        (200, json!({"runs":[run.clone()],"next_cursor":null}))
    );
    // Populate retained source-only native observations through the existing local
    // runtime. This is not queue dispatch or controller admission finalization.
    let native_run = run["native_run_id"].as_str().unwrap();
    skill_workflow::start(&native.profile.data_dir, native_run, &native.plan).unwrap();
    skill_workflow::advance(&native.profile.data_dir, native_run, 1).unwrap();
    let projection = env
        .request(
            Method::GET,
            &format!("{REPO}/runs/{run_id}/workflow"),
            Some(&cookie),
            None,
            None,
            None,
        )
        .await;
    assert_eq!(projection.0, 200, "{}", projection.1);
    assert_eq!(projection.1["availability"], "available");
    assert_eq!(projection.1["projection"]["run"]["state"], "succeeded");
    assert!(
        !projection
            .1
            .to_string()
            .contains(root.path().to_str().unwrap())
    );
    let cancel_path = format!("{REPO}/runs/{run_id}/cancel");
    let mut cancel = json!({"request_id":"cancel-http","expected_generation":0});
    assert_eq!(
        env.request(
            Method::POST,
            &cancel_path,
            Some(&cookie),
            Some(csrf),
            None,
            Some(cancel.clone())
        )
        .await,
        (409, json!({"error":"generation_conflict"}))
    );
    cancel["expected_generation"] = run["generation"].clone();
    let (status, cancelled) = env
        .request(
            Method::POST,
            &cancel_path,
            Some(&cookie),
            Some(csrf),
            None,
            Some(cancel.clone()),
        )
        .await;
    assert_eq!(status, 200);
    assert_eq!(cancelled["state"], "cancelled");
    assert_eq!(cancelled["receipt"], Value::Null);
    assert_eq!(
        env.request(
            Method::POST,
            &cancel_path,
            Some(&cookie),
            Some(csrf),
            None,
            Some(cancel.clone())
        )
        .await,
        (200, cancelled)
    );
    cancel["expected_generation"] = json!(999);
    assert_eq!(
        env.request(
            Method::POST,
            &cancel_path,
            Some(&cookie),
            Some(csrf),
            None,
            Some(cancel)
        )
        .await,
        (409, json!({"error":"idempotency_conflict"}))
    );
    assert!(
        Authority::open(&native.profile.data_dir)
            .unwrap()
            .target("main")
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn http_native_actions_require_explicit_human_roles_exact_subject_and_native_evidence() {
    let root = tempfile::tempdir().unwrap();
    let native = native_source_profile(root.path());
    let (env, _service) = execution_env(root.path(), &native).await;
    let (_, owner) = env.signin(101).await;
    let owner_session = env.session(&owner).await;
    let owner_csrf = owner_session["csrf_token"].as_str().unwrap();
    let approve = format!("{REPO}/profiles/checks/approve");
    let integrate = format!("{REPO}/profiles/checks/integrate");
    let request = json!({"request_id":"approval-http","expected_generation":owner_session["actor"]["generation"],"subject":native.subject});
    // Administrator does not imply either independent native action role.
    assert_eq!(
        env.request(
            Method::POST,
            &approve,
            Some(&owner),
            Some(owner_csrf),
            None,
            Some(request.clone())
        )
        .await
        .0,
        403
    );
    let mut integration = request.clone();
    integration["approval_id"] = json!("not-issued");
    assert_eq!(
        env.request(
            Method::POST,
            &integrate,
            Some(&owner),
            Some(owner_csrf),
            None,
            Some(integration)
        )
        .await
        .0,
        403
    );
    let grant = env.member(
        202,
        &[
            Role::Reader,
            Role::Contributor,
            Role::Reviewer,
            Role::Integrator,
        ],
        true,
        env.workspace.settings().unwrap().generation,
        "add-native-reviewer",
    );
    assert_eq!(
        env.request(
            Method::PUT,
            "/v1/hub/workspace/members",
            Some(&owner),
            Some(owner_csrf),
            None,
            Some(grant)
        )
        .await
        .0,
        200
    );
    let (_, reviewer) = env.signin(202).await;
    let reviewer_session = env.session(&reviewer).await;
    let csrf = reviewer_session["csrf_token"].as_str().unwrap();
    let mut request = request;
    request["expected_generation"] = reviewer_session["actor"]["generation"].clone();
    let mut integration = request.clone();
    integration["approval_id"] = json!("not-issued");
    for (path, body) in [
        (&approve, request.clone()),
        (&integrate, integration.clone()),
    ] {
        assert_eq!(
            env.request(
                Method::POST,
                path,
                Some(&reviewer),
                None,
                None,
                Some(body.clone())
            )
            .await
            .0,
            403
        );
        for field in ["actor_id", "policy_epoch", "data_dir"] {
            let mut unknown = body.clone();
            unknown[field] = json!("request-cannot-supply-this");
            assert_eq!(
                env.request(
                    Method::POST,
                    path,
                    Some(&reviewer),
                    Some(csrf),
                    None,
                    Some(unknown)
                )
                .await
                .0,
                400
            );
        }
        let mut stale = body.clone();
        stale["subject"]["change_version"] = json!(99);
        assert_eq!(
            env.request(
                Method::POST,
                path,
                Some(&reviewer),
                Some(csrf),
                None,
                Some(stale)
            )
            .await
            .0,
            409
        );
        let mut stale = body;
        stale["expected_generation"] = json!(0);
        assert_eq!(
            env.request(
                Method::POST,
                path,
                Some(&reviewer),
                Some(csrf),
                None,
                Some(stale)
            )
            .await,
            (409, json!({"error":"generation_conflict"}))
        );
    }
    let user = env
        .store
        .list_users()
        .unwrap()
        .into_iter()
        .find(|user| user.identity.user_id == 202)
        .unwrap();
    let (_, token) = mint_agent_token(
        &*env.store,
        &user.id,
        "synthetic-run-agent",
        "synthetic/project",
        3600,
    )
    .unwrap();
    let (_, read_token, _) = mint_bound_agent_token(
        &*env.store,
        &user.id,
        "synthetic-read-agent",
        "synthetic/project",
        42,
        3600,
        true,
    )
    .unwrap();
    for secret in [&token, &read_token] {
        assert_eq!(
            env.request(
                Method::POST,
                &approve,
                None,
                None,
                Some(secret.expose()),
                Some(request.clone())
            )
            .await
            .0,
            403
        );
        assert_eq!(
            env.request(
                Method::POST,
                &integrate,
                None,
                None,
                Some(secret.expose()),
                Some(integration.clone())
            )
            .await
            .0,
            403
        );
    }
    assert_eq!(
        env.request(
            Method::GET,
            &format!("{REPO}/profiles/checks/subject?change_id=work"),
            None,
            None,
            Some(read_token.expose()),
            None
        )
        .await
        .0,
        200
    );
    assert_eq!(
        env.request(
            Method::POST,
            &format!("{REPO}/runs"),
            None,
            None,
            Some(read_token.expose()),
            Some(start_request(
                &native,
                &reviewer_session["actor"]["generation"],
                "read-token-run"
            ))
        )
        .await
        .0,
        403
    );
    for suffix in ["runs", "profiles/checks/subjects", "runs/unknown/workflow"] {
        assert_eq!(
            env.request(
                Method::GET,
                &format!("/v1/hub/workspace/repositories/synthetic%2Fother/{suffix}"),
                Some(&reviewer),
                None,
                None,
                None
            )
            .await
            .0,
            403
        );
    }
    // Passing authentication and roles cannot bypass the existing native gate:
    // no controller-finalized receipt or approval exists for this exact subject.
    let approval = env
        .request(
            Method::POST,
            &approve,
            Some(&reviewer),
            Some(csrf),
            None,
            Some(request),
        )
        .await;
    assert_eq!(approval.0, 409, "{approval:?}");
    let landing = env
        .request(
            Method::POST,
            &integrate,
            Some(&reviewer),
            Some(csrf),
            None,
            Some(integration),
        )
        .await;
    assert_eq!(landing.0, 409, "{landing:?}");
    assert!(
        Authority::open(&native.profile.data_dir)
            .unwrap()
            .target("main")
            .unwrap()
            .is_none()
    );
    let audit = rusqlite::Connection::open(env.dir.path().join("workspace.sqlite")).unwrap();
    let failed: i64 = audit
        .query_row(
            "SELECT count(*) FROM workspace_audit WHERE kind='native_action_failed'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert!(failed >= 2);
}

#[tokio::test]
async fn synthetic_browser_authorize_page_uses_exact_registered_loopback_callback() {
    let env = Env::new().await;
    let start = env
        .client
        .get(format!("{}/v1/hub/login/github?workspace=1", env.base))
        .send()
        .await
        .unwrap();
    let cookie = start.headers()[header::SET_COOKIE]
        .to_str()
        .unwrap()
        .split(';')
        .next()
        .unwrap()
        .to_owned();
    let authorize = Url::parse(start.headers()[header::LOCATION].to_str().unwrap()).unwrap();
    let page = env.client.get(authorize.clone()).send().await.unwrap();
    assert_eq!(page.status().as_u16(), 200);
    let html = page.text().await.unwrap();
    assert!(
        html.contains("Sign in as synthetic user 101")
            && html.contains("Sign in as synthetic user 202")
    );
    let callback = html
        .split("href=\"")
        .nth(1)
        .unwrap()
        .split('"')
        .next()
        .unwrap()
        .replace("&amp;", "&");
    let selected = env
        .client
        .get(callback)
        .header(header::COOKIE, cookie)
        .send()
        .await
        .unwrap();
    assert_eq!(selected.status().as_u16(), 302);
    assert_eq!(selected.headers()[header::LOCATION], "/ui/#workspace");
    let mut wrong = authorize.clone();
    let params: Vec<_> = authorize
        .query_pairs()
        .map(|(key, value)| (key.into_owned(), value.into_owned()))
        .collect();
    wrong.set_query(None);
    for (key, value) in params {
        wrong.query_pairs_mut().append_pair(
            &key,
            if key == "redirect_uri" {
                "https://external.example/callback"
            } else {
                &value
            },
        );
    }
    let refused = env.client.get(wrong).send().await.unwrap();
    assert_eq!(refused.status().as_u16(), 400);
    assert!(!refused.text().await.unwrap().contains("href="));
}

#[tokio::test]
async fn source_only_native_receipt_allows_explicit_http_approval_and_landing_after_current_role_check()
 {
    let root = tempfile::tempdir().unwrap();
    let native = native_source_profile(root.path());
    let socket = std::os::unix::net::UnixListener::bind(&native.profile.docker_socket).unwrap();
    socket.set_nonblocking(true).unwrap();
    let (env, service) = execution_env(root.path(), &native).await;
    let (_, owner) = env.signin(101).await;
    let session = env.session(&owner).await;
    let owner_csrf = session["csrf_token"].as_str().unwrap();
    let roles = [Role::Reader, Role::Reviewer, Role::Integrator];
    let grant = env.member(
        202,
        &roles,
        true,
        env.workspace.settings().unwrap().generation,
        "grant-landing-roles",
    );
    assert_eq!(
        env.request(
            Method::PUT,
            "/v1/hub/workspace/members",
            Some(&owner),
            Some(owner_csrf),
            None,
            Some(grant)
        )
        .await
        .0,
        200
    );
    let (_, reviewer) = env.signin(202).await;
    let reviewer_session = env.session(&reviewer).await;
    let reviewer_csrf = reviewer_session["csrf_token"].as_str().unwrap();
    // Process the real shared queue with a source-only plan. OciBackend only
    // validates these local paths; no workflow step invokes its executable or
    // the inert Unix listener, and there is no provider step.
    let queued = env
        .request(
            Method::POST,
            &format!("{REPO}/runs"),
            Some(&owner),
            Some(owner_csrf),
            None,
            Some(start_request(
                &native,
                &session["actor"]["generation"],
                "source-only-native-http",
            )),
        )
        .await;
    assert_eq!(queued.0, 200, "{queued:?}");
    let run_id = queued.1["run_id"].as_str().unwrap();
    let executor = service.clone();
    assert!(
        tokio::task::spawn_blocking(move || executor.process_next())
            .await
            .unwrap()
            .unwrap()
    );
    let completed = env
        .request(
            Method::GET,
            &format!("{REPO}/runs/{run_id}"),
            Some(&owner),
            None,
            None,
            None,
        )
        .await;
    assert_eq!(completed.0, 200);
    assert_eq!(completed.1["state"], "succeeded", "{}", completed.1);
    assert!(
        completed.1["receipt"]
            .as_str()
            .is_some_and(|receipt| !receipt.is_empty())
    );
    assert!(matches!(socket.accept(),Err(error) if error.kind()==std::io::ErrorKind::WouldBlock));
    let approve_path = format!("{REPO}/profiles/checks/approve");
    let approval_request = json!({"request_id":"approve-source-http","expected_generation":reviewer_session["actor"]["generation"],"subject":native.subject});
    let approval = env
        .request(
            Method::POST,
            &approve_path,
            Some(&reviewer),
            Some(reviewer_csrf),
            None,
            Some(approval_request.clone()),
        )
        .await;
    assert_eq!(approval.0, 200, "{approval:?}");
    assert_eq!(
        env.request(
            Method::POST,
            &approve_path,
            Some(&reviewer),
            Some(reviewer_csrf),
            None,
            Some(approval_request.clone())
        )
        .await,
        approval
    );
    assert!(
        Authority::open(&native.profile.data_dir)
            .unwrap()
            .target("main")
            .unwrap()
            .is_none(),
        "Approval must not automatically land the revision"
    );
    // Idempotent approval retry acknowledges history without unrevoking it.
    let approval_id = approval.1["approval_id"].as_str().unwrap();
    Authority::open(&native.profile.data_dir)
        .unwrap()
        .revoke_approval(approval_id)
        .unwrap();
    assert_eq!(
        env.request(
            Method::POST,
            &approve_path,
            Some(&reviewer),
            Some(reviewer_csrf),
            None,
            Some(approval_request.clone())
        )
        .await,
        approval
    );
    let revoked_landing = json!({"request_id":"revoked-approval-landing","expected_generation":reviewer_session["actor"]["generation"],"subject":native.subject,"approval_id":approval_id});
    assert_eq!(
        env.request(
            Method::POST,
            &format!("{REPO}/profiles/checks/integrate"),
            Some(&reviewer),
            Some(reviewer_csrf),
            None,
            Some(revoked_landing)
        )
        .await
        .0,
        409
    );
    assert!(
        Authority::open(&native.profile.data_dir)
            .unwrap()
            .target("main")
            .unwrap()
            .is_none()
    );
    let mut fresh_approval = approval_request;
    fresh_approval["request_id"] = json!("approve-current-source-http");
    let approval = env
        .request(
            Method::POST,
            &approve_path,
            Some(&reviewer),
            Some(reviewer_csrf),
            None,
            Some(fresh_approval.clone()),
        )
        .await;
    assert_eq!(approval.0, 200, "{approval:?}");
    let integrate_path = format!("{REPO}/profiles/checks/integrate");
    let mut integration = json!({"request_id":"land-source-http","expected_generation":reviewer_session["actor"]["generation"],"subject":native.subject,"approval_id":approval.1["approval_id"]});
    let revoke = env.member(
        202,
        &[Role::Reader, Role::Reviewer],
        true,
        env.workspace.settings().unwrap().generation,
        "remove-integrator",
    );
    assert_eq!(
        env.request(
            Method::PUT,
            "/v1/hub/workspace/members",
            Some(&owner),
            Some(owner_csrf),
            None,
            Some(revoke)
        )
        .await
        .0,
        200
    );
    assert_eq!(
        env.request(
            Method::POST,
            &integrate_path,
            Some(&reviewer),
            Some(reviewer_csrf),
            None,
            Some(integration.clone())
        )
        .await
        .0,
        401
    );
    let (_, limited) = env.signin(202).await;
    let limited_session = env.session(&limited).await;
    integration["expected_generation"] = limited_session["actor"]["generation"].clone();
    assert_eq!(
        env.request(
            Method::POST,
            &integrate_path,
            Some(&limited),
            limited_session["csrf_token"].as_str(),
            None,
            Some(integration.clone())
        )
        .await
        .0,
        403
    );
    assert!(
        Authority::open(&native.profile.data_dir)
            .unwrap()
            .target("main")
            .unwrap()
            .is_none()
    );
    let restore = env.member(
        202,
        &roles,
        true,
        env.workspace.settings().unwrap().generation,
        "restore-integrator",
    );
    assert_eq!(
        env.request(
            Method::PUT,
            "/v1/hub/workspace/members",
            Some(&owner),
            Some(owner_csrf),
            None,
            Some(restore)
        )
        .await
        .0,
        200
    );
    let (_, current) = env.signin(202).await;
    let current_session = env.session(&current).await;
    integration["expected_generation"] = current_session["actor"]["generation"].clone();
    let landed = env
        .request(
            Method::POST,
            &integrate_path,
            Some(&current),
            current_session["csrf_token"].as_str(),
            None,
            Some(integration.clone()),
        )
        .await;
    assert_eq!(landed.0, 200, "{landed:?}");
    assert_eq!(
        env.request(
            Method::POST,
            &integrate_path,
            Some(&current),
            current_session["csrf_token"].as_str(),
            None,
            Some(integration.clone())
        )
        .await,
        landed
    );
    let original_integration = integration.clone();
    integration["approval_id"] = json!("different-approval");
    let collision = env
        .request(
            Method::POST,
            &integrate_path,
            Some(&current),
            current_session["csrf_token"].as_str(),
            None,
            Some(integration),
        )
        .await;
    assert_eq!(collision.0, 409, "{collision:?}");
    let target = Authority::open(&native.profile.data_dir)
        .unwrap()
        .target("main")
        .unwrap()
        .unwrap();
    assert_eq!(target.revision, native.subject.revision);
    assert_eq!(target.version, 1);
    let audit = rusqlite::Connection::open(env.dir.path().join("workspace.sqlite")).unwrap();
    let successful: i64 = audit
        .query_row(
            "SELECT count(*) FROM workspace_audit WHERE kind='native_action_succeeded'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert!(successful >= 2);
    // Retained native results survive a benign role regrant and fresh session;
    // generation remains a precondition only for a genuinely new mutation.
    let regrant = env.member(
        202,
        &roles,
        true,
        env.workspace.settings().unwrap().generation,
        "benign-regrant",
    );
    assert_eq!(
        env.request(
            Method::PUT,
            "/v1/hub/workspace/members",
            Some(&owner),
            Some(owner_csrf),
            None,
            Some(regrant)
        )
        .await
        .0,
        200
    );
    let (_, reauthenticated) = env.signin(202).await;
    let reauthenticated_session = env.session(&reauthenticated).await;
    let reauthenticated_csrf = reauthenticated_session["csrf_token"].as_str();
    assert_eq!(
        env.request(
            Method::POST,
            &approve_path,
            Some(&reauthenticated),
            reauthenticated_csrf,
            None,
            Some(fresh_approval.clone())
        )
        .await,
        approval
    );
    assert_eq!(
        env.request(
            Method::POST,
            &integrate_path,
            Some(&reauthenticated),
            reauthenticated_csrf,
            None,
            Some(original_integration.clone())
        )
        .await,
        landed
    );
    let mut changed = original_integration.clone();
    changed["approval_id"] = json!("changed-body-after-reauth");
    assert_eq!(
        env.request(
            Method::POST,
            &integrate_path,
            Some(&reauthenticated),
            reauthenticated_csrf,
            None,
            Some(changed)
        )
        .await
        .0,
        409
    );
    for (path, mut body) in [
        (&approve_path, fresh_approval.clone()),
        (&integrate_path, original_integration.clone()),
    ] {
        body["request_id"] = json!("new-request-old-generation");
        assert_eq!(
            env.request(
                Method::POST,
                path,
                Some(&reauthenticated),
                reauthenticated_csrf,
                None,
                Some(body)
            )
            .await,
            (409, json!({"error":"generation_conflict"}))
        );
    }
    // Diagnostic only: the public authority API keeps requirements immutable.
    // An offline direct DB edit simulates unsupported requirement removal so
    // recovery/history must not accidentally require today's installed profile.
    let native_db =
        rusqlite::Connection::open(native.profile.data_dir.join("state.sqlite")).unwrap();
    native_db
        .pragma_update(None, "foreign_keys", "OFF")
        .unwrap();
    native_db
        .execute("DELETE FROM workflow_requirements WHERE target='main'", [])
        .unwrap();
    drop(native_db);
    let retained = env
        .request(
            Method::GET,
            &format!("{REPO}/runs/{run_id}/workflow"),
            Some(&reauthenticated),
            None,
            None,
            None,
        )
        .await;
    assert_eq!(retained.0, 200, "{retained:?}");
    assert_eq!(
        retained.1["projection"]["run"]["source"]["revision"],
        native.subject.revision
    );
    assert_eq!(
        env.request(
            Method::POST,
            &approve_path,
            Some(&reauthenticated),
            reauthenticated_csrf,
            None,
            Some(fresh_approval.clone())
        )
        .await,
        approval
    );
    assert_eq!(
        env.request(
            Method::POST,
            &integrate_path,
            Some(&reauthenticated),
            reauthenticated_csrf,
            None,
            Some(original_integration.clone())
        )
        .await,
        landed
    );
    for (path, mut body) in [
        (&approve_path, fresh_approval.clone()),
        (&integrate_path, original_integration.clone()),
    ] {
        body["request_id"] = json!("new-request-obsolete-profile");
        body["expected_generation"] = reauthenticated_session["actor"]["generation"].clone();
        assert_eq!(
            env.request(
                Method::POST,
                path,
                Some(&reauthenticated),
                reauthenticated_csrf,
                None,
                Some(body)
            )
            .await
            .0,
            400
        );
    }
    let remove = env.member(
        202,
        &[Role::Reader],
        true,
        env.workspace.settings().unwrap().generation,
        "remove-native-roles",
    );
    assert_eq!(
        env.request(
            Method::PUT,
            "/v1/hub/workspace/members",
            Some(&owner),
            Some(owner_csrf),
            None,
            Some(remove)
        )
        .await
        .0,
        200
    );
    let (_, reader) = env.signin(202).await;
    let reader_session = env.session(&reader).await;
    for (path, body) in [
        (&approve_path, fresh_approval),
        (&integrate_path, original_integration),
    ] {
        assert_eq!(
            env.request(
                Method::POST,
                path,
                Some(&reader),
                reader_session["csrf_token"].as_str(),
                None,
                Some(body)
            )
            .await
            .0,
            403
        );
    }
}

#[tokio::test]
async fn exact_queue_retry_survives_membership_regrant_but_new_old_generation_request_fails() {
    let root = tempfile::tempdir().unwrap();
    let native = native_source_profile(root.path());
    let (env, _service) = execution_env(root.path(), &native).await;
    let (_, owner) = env.signin(101).await;
    let owner_session = env.session(&owner).await;
    let owner_csrf = owner_session["csrf_token"].as_str();
    let grant = env.member(
        202,
        &[Role::Reader, Role::Contributor],
        true,
        env.workspace.settings().unwrap().generation,
        "queue-member",
    );
    assert_eq!(
        env.request(
            Method::PUT,
            "/v1/hub/workspace/members",
            Some(&owner),
            owner_csrf,
            None,
            Some(grant)
        )
        .await
        .0,
        200
    );
    let (_, member) = env.signin(202).await;
    let member_session = env.session(&member).await;
    let request = start_request(
        &native,
        &member_session["actor"]["generation"],
        "retained-queue-request",
    );
    let path = format!("{REPO}/runs");
    let retained = env
        .request(
            Method::POST,
            &path,
            Some(&member),
            member_session["csrf_token"].as_str(),
            None,
            Some(request.clone()),
        )
        .await;
    assert_eq!(retained.0, 200);
    let grant = env.member(
        202,
        &[Role::Reader, Role::Contributor, Role::Reviewer],
        true,
        env.workspace.settings().unwrap().generation,
        "queue-member-regrant",
    );
    assert_eq!(
        env.request(
            Method::PUT,
            "/v1/hub/workspace/members",
            Some(&owner),
            owner_csrf,
            None,
            Some(grant)
        )
        .await
        .0,
        200
    );
    let (_, member) = env.signin(202).await;
    let session = env.session(&member).await;
    let csrf = session["csrf_token"].as_str();
    assert_eq!(
        env.request(
            Method::POST,
            &path,
            Some(&member),
            csrf,
            None,
            Some(request.clone())
        )
        .await,
        retained
    );
    let mut changed = request.clone();
    changed["subject"]["change_version"] = json!(99);
    assert_eq!(
        env.request(
            Method::POST,
            &path,
            Some(&member),
            csrf,
            None,
            Some(changed)
        )
        .await,
        (409, json!({"error":"idempotency_conflict"}))
    );
    let mut new = request.clone();
    new["request_id"] = json!("new-queue-old-generation");
    assert_eq!(
        env.request(Method::POST, &path, Some(&member), csrf, None, Some(new))
            .await,
        (409, json!({"error":"generation_conflict"}))
    );
    let revoke = env.member(
        202,
        &[Role::Reader],
        true,
        env.workspace.settings().unwrap().generation,
        "remove-queue-role",
    );
    assert_eq!(
        env.request(
            Method::PUT,
            "/v1/hub/workspace/members",
            Some(&owner),
            owner_csrf,
            None,
            Some(revoke)
        )
        .await
        .0,
        200
    );
    let (_, reader) = env.signin(202).await;
    let reader_session = env.session(&reader).await;
    assert_eq!(
        env.request(
            Method::POST,
            &path,
            Some(&reader),
            reader_session["csrf_token"].as_str(),
            None,
            Some(request)
        )
        .await
        .0,
        403
    );
}
