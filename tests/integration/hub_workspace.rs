//! Synthetic forge identities exercise the actual OAuth callback and cookie API.
use crate::workspace_hub;

use agentscm::backend::LocalBackend;
use agentscm::hub::store::{mint_agent_token, mint_bound_agent_token};
use agentscm::hub::workspace::{Identity, Workspace, WorkspaceConfig};
use agentscm::hub::{HubState, router};
use agentscm::team::auth::Role;
use axum::http::{StatusCode, header};
use reqwest::Method;
use serde_json::{Value, json};
use std::sync::Arc;
use workspace_hub::Env;

#[tokio::test]
async fn workspace_two_people_csrf_scope_roles_revocation_and_last_admin() {
    let env = Env::new().await;
    // Numeric membership must be provisioned before a forge user can sign in.
    assert_eq!(env.signin(202).await.0, 403);
    let (status, owner) = env.signin(101).await;
    assert_eq!(status, 302);
    let session = env.session(&owner).await;
    let csrf = session["csrf_token"].as_str().unwrap();
    assert_eq!(session["workspace"]["generation"], 1);
    assert_eq!(session["capabilities"]["manage_workspace"], true);
    let add = env.member(
        202,
        &[Role::Reader, Role::Contributor],
        true,
        1,
        "add-second",
    );
    // Cookies alone and an unrelated CSRF token never authorize mutations.
    for token in [None, Some("not-a-token")] {
        let (status, body) = env
            .request(
                Method::PUT,
                "/v1/hub/workspace/members",
                Some(&owner),
                token,
                None,
                Some(add.clone()),
            )
            .await;
        assert_eq!(status, 403);
        assert_eq!(body["error"], "csrf_required");
    }
    assert_eq!(
        env.request(
            Method::PUT,
            "/v1/hub/workspace/members",
            Some(&owner),
            Some(csrf),
            None,
            Some(add.clone())
        )
        .await
        .0,
        200
    );
    // Exact replay returns the retained generation; changed payload conflicts.
    assert_eq!(
        env.request(
            Method::PUT,
            "/v1/hub/workspace/members",
            Some(&owner),
            Some(csrf),
            None,
            Some(add.clone())
        )
        .await,
        (200, json!({"generation":2}))
    );
    let mut collision = add.clone();
    collision["roles"] = json!(["reader"]);
    assert_eq!(
        env.request(
            Method::PUT,
            "/v1/hub/workspace/members",
            Some(&owner),
            Some(csrf),
            None,
            Some(collision)
        )
        .await,
        (409, json!({"error":"idempotency_conflict"}))
    );
    let (_, second) = env.signin(202).await;
    let second_session = env.session(&second).await;
    let second_csrf = second_session["csrf_token"].as_str().unwrap();
    assert_eq!(
        env.request(
            Method::POST,
            "/synthetic/execute?repository=synthetic/project",
            Some(&second),
            Some(second_csrf),
            None,
            None
        )
        .await
        .0,
        204
    );
    assert_eq!(
        env.request(
            Method::POST,
            "/synthetic/execute?repository=synthetic/other",
            Some(&second),
            Some(second_csrf),
            None,
            None
        )
        .await
        .0,
        403
    );
    assert_eq!(second_session["capabilities"]["manage_workspace"], false);
    assert_eq!(
        env.request(
            Method::GET,
            "/v1/hub/workspace/members",
            Some(&second),
            None,
            None,
            None
        )
        .await
        .0,
        403
    );
    assert_eq!(
        env.request(
            Method::PUT,
            "/v1/hub/workspace/members",
            Some(&second),
            Some(second_csrf),
            None,
            Some(env.member(202, &[Role::Administrator], true, 2, "self-promote"))
        )
        .await
        .0,
        403
    );
    // No removal of the final administrator.
    assert_eq!(
        env.request(
            Method::PUT,
            "/v1/hub/workspace/members",
            Some(&owner),
            Some(csrf),
            None,
            Some(env.member(101, &[Role::Reader], true, 2, "last-admin"))
        )
        .await,
        (409, json!({"error":"last_administrator"}))
    );
    let settings = json!({"request_id":"stale-settings","expected_generation":1,"name":"Renamed workspace","max_concurrent_runs":1});
    assert_eq!(
        env.request(
            Method::PUT,
            "/v1/hub/workspace/settings",
            Some(&owner),
            Some(csrf),
            None,
            Some(settings)
        )
        .await,
        (409, json!({"error":"generation_conflict"}))
    );
    // Removing a member invalidates their existing cookie and future OAuth login.
    assert_eq!(
        env.request(
            Method::PUT,
            "/v1/hub/workspace/members",
            Some(&owner),
            Some(csrf),
            None,
            Some(env.member(202, &[Role::Reader], false, 2, "revoke-second"))
        )
        .await
        .0,
        200
    );
    assert_eq!(
        env.request(
            Method::GET,
            "/v1/hub/workspace/session",
            Some(&second),
            None,
            None,
            None
        )
        .await
        .0,
        401
    );
    assert_eq!(env.signin(202).await.0, 403);
    // Logout is CSRF protected, deletes its persistent digest, and clears access.
    assert_eq!(
        env.request(
            Method::DELETE,
            "/v1/hub/workspace/session",
            Some(&owner),
            None,
            None,
            None
        )
        .await
        .0,
        403
    );
    assert_eq!(
        env.request(
            Method::DELETE,
            "/v1/hub/workspace/session",
            Some(&owner),
            Some(csrf),
            None,
            None
        )
        .await
        .0,
        204
    );
    assert_eq!(
        env.request(
            Method::GET,
            "/v1/hub/workspace/session",
            Some(&owner),
            None,
            None,
            None
        )
        .await
        .0,
        401
    );
}

#[tokio::test]
async fn workspace_settings_persist_bootstrap_once_and_agent_management_denied() {
    let env = Env::new().await;
    let (_, owner) = env.signin(101).await;
    let session = env.session(&owner).await;
    let csrf = session["csrf_token"].as_str().unwrap();
    let settings = json!({"request_id":"rename","expected_generation":1,"name":"Shared workspace","max_concurrent_runs":1});
    assert_eq!(
        env.request(
            Method::PUT,
            "/v1/hub/workspace/settings",
            Some(&owner),
            Some(csrf),
            None,
            Some(settings.clone())
        )
        .await,
        (200, json!({"generation":2}))
    );
    assert_eq!(
        env.request(
            Method::PUT,
            "/v1/hub/workspace/settings",
            Some(&owner),
            Some(csrf),
            None,
            Some(settings)
        )
        .await,
        (200, json!({"generation":2}))
    );
    // Reopening with another seed owner cannot grant that owner access or reset settings.
    let reopened = Workspace::open(
        WorkspaceConfig {
            workspace_id: "synthetic-workspace".into(),
            name: "Ignored seed name".into(),
            database: env.dir.path().join("workspace.sqlite"),
            initial_owner: Identity {
                provider: "github".into(),
                host: env.host.clone(),
                user_id: 999,
            },
            forge_repositories: vec![],
        },
        env.store.clone(),
    )
    .unwrap();
    assert_eq!(reopened.settings().unwrap().name, "Shared workspace");
    assert!(
        !reopened
            .allows_sign_in(
                &Identity {
                    provider: "github".into(),
                    host: env.host.clone(),
                    user_id: 999
                }
                .forge()
            )
            .unwrap()
    );
    // A new Hub/Workspace instance accepts the retained short-lived digest;
    // OAuth handoff and legacy in-memory human sessions are not required.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let restarted_base = format!("http://{}", listener.local_addr().unwrap());
    let restarted = HubState::new(
        env.store.clone(),
        Arc::new(LocalBackend::open(env.dir.path().join("objects")).unwrap()),
    )
    .with_workspace(reopened);
    let task = tokio::spawn(async move {
        axum::serve(listener, router(restarted)).await.unwrap();
    });
    let response = env
        .client
        .get(format!("{restarted_base}/v1/hub/workspace/session"))
        .header(header::COOKIE, &owner)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        serde_json::from_str::<Value>(&response.text().await.unwrap()).unwrap()["workspace"]["name"],
        "Shared workspace"
    );
    task.abort();
    let conn = rusqlite::Connection::open(env.dir.path().join("workspace.sqlite")).unwrap();
    let count: i64 = conn
        .query_row(
            "SELECT count(*) FROM workspace_audit WHERE kind='settings_changed'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(
        count, 1,
        "idempotency replay must not append another mutation"
    );
    let retained: String = conn
        .query_row("SELECT digest FROM workspace_sessions LIMIT 1", [], |row| {
            row.get(0)
        })
        .unwrap();
    assert!(
        !owner.ends_with(&retained),
        "only session digests are retained"
    );
    let user = env
        .store
        .list_users()
        .unwrap()
        .into_iter()
        .find(|u| u.identity.user_id == 101)
        .unwrap();
    let (write_id, write) = mint_agent_token(
        &*env.store,
        &user.id,
        "synthetic-agent",
        "synthetic/project",
        3600,
    )
    .unwrap();
    let (_, read, _) = mint_bound_agent_token(
        &*env.store,
        &user.id,
        "synthetic-reader",
        "synthetic/project",
        77,
        3600,
        true,
    )
    .unwrap();
    assert_eq!(
        env.request(
            Method::POST,
            "/synthetic/execute?repository=synthetic/project",
            None,
            None,
            Some(write.expose()),
            None
        )
        .await
        .0,
        204
    );
    assert_eq!(
        env.request(
            Method::POST,
            "/synthetic/execute?repository=synthetic/other",
            None,
            None,
            Some(write.expose()),
            None
        )
        .await
        .0,
        403
    );
    assert_eq!(
        env.request(
            Method::POST,
            "/synthetic/execute?repository=synthetic/project",
            None,
            None,
            Some(read.expose()),
            None
        )
        .await
        .0,
        403
    );
    for token in [&write, &read] {
        assert_eq!(env.request(Method::PUT,"/v1/hub/workspace/settings",None,None,Some(token.expose()),Some(json!({"request_id":"agent-rename","expected_generation":2,"name":"Agent rename","max_concurrent_runs":1}))).await.0,403);
        assert_eq!(
            env.request(
                Method::GET,
                "/v1/hub/workspace/members",
                None,
                None,
                Some(token.expose()),
                None
            )
            .await
            .0,
            403
        );
    }
    env.store.revoke_agent_token(&write_id).unwrap();
    assert_eq!(
        env.request(
            Method::POST,
            "/synthetic/execute?repository=synthetic/project",
            None,
            None,
            Some(write.expose()),
            None
        )
        .await
        .0,
        401
    );
    // A global Hub disable is checked against exact current identity on every operation.
    env.store.set_user_disabled(&user.id, true).unwrap();
    assert_eq!(
        env.request(
            Method::GET,
            "/v1/hub/workspace/session",
            Some(&owner),
            None,
            None,
            None
        )
        .await
        .0,
        401
    );
    assert!(env.workspace.settings().is_ok());
}
