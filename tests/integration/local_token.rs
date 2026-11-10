//! `agentscm hub local-token` against a Hub's own database, then that Hub: the token's hook calls
//! are answered for its repository and refused for another.
use std::process::Command;
use std::sync::Arc;

use agentscm::backend::LocalBackend;
use agentscm::hub::store;
use agentscm::hub::{HubState, router};
use serde_json::{Value, json};
use tempfile::TempDir;

fn local_token(url: &str, repo: &str) -> String {
    let output = Command::new(env!("CARGO_BIN_EXE_agentscm"))
        .args([
            "--no-telemetry",
            "hub",
            "local-token",
            "--repo",
            repo,
            "--login",
            "maya",
        ])
        .env_clear()
        .env("AGENTSCM_DATABASE_URL", url)
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    String::from_utf8(output.stdout).unwrap().trim().to_owned()
}

async fn edit(hub: &str, token: &str, repo: &str) -> Value {
    let body = json!({"session_id": "s1", "cwd": "/work/widgets", "hook_event_name": "PreToolUse",
        "tool_name": "Edit", "tool_input": {"file_path": "/work/widgets/src/a.rs"}});
    let reply = reqwest::Client::new()
        .post(format!("{hub}/v1/hub/hook/claude?repo={repo}"))
        .bearer_auth(token)
        .header("content-type", "application/json")
        .body(body.to_string())
        .send()
        .await
        .unwrap();
    assert!(reply.status().is_success());
    serde_json::from_slice(&reply.bytes().await.unwrap()).unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_local_token_works_in_its_repository_on_the_hub_that_serves_the_database() {
    let dir = TempDir::new().unwrap();
    let url = format!("sqlite://{}", dir.path().join("hub.sqlite").display());
    // `hub serve` created and migrated the database.
    let store = store::open(&url).unwrap();
    let token = tokio::task::spawn_blocking({
        let url = url.clone();
        move || local_token(&url, "acme/widgets")
    })
    .await
    .unwrap();
    assert!(!token.is_empty());

    let objects = LocalBackend::open(dir.path().join("objects")).unwrap();
    let app = router(HubState::new(store, Arc::new(objects)));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let hub = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await });

    let answered = edit(&hub, &token, "acme/widgets").await;
    assert!(
        !answered.to_string().contains("coordination unknown"),
        "{answered}"
    );
    let other = edit(&hub, &token, "acme/other").await;
    assert!(
        other.to_string().contains("coordination unknown"),
        "{other}"
    );
}
