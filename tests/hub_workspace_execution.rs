#![cfg(unix)]
//! Real Hub HTTP controls with a local synthetic forge and source-only native state.
//! Source-only service execution uses an inert socket; no Docker or provider process runs.
#[path = "common/workspace_hub.rs"]
mod workspace_hub;

use std::sync::Arc;

use agentscm::authority::Authority;
use agentscm::hub::store::{mint_agent_token, mint_bound_agent_token};
use agentscm::hub::workspace_execution::{ExecutionConfig, ExecutionService};
use agentscm::skill_workflow;
use agentscm::team::auth::Role;
use axum::http::header;
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
