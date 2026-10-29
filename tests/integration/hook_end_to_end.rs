//! Two agents, one file, a real hub: a Claude Code HTTP-hook request announces the file, then the
//! real `agentscm hook cursor` client for a second token is told to stop. Also the repository
//! guard: a token used for another repository records nothing.
use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::Arc;

use agentscm::backend::LocalBackend;
use agentscm::hub::store::{self, ForgeIdentity, Store, mint_agent_token};
use agentscm::hub::{HubState, router};
use serde_json::Value;
use tempfile::TempDir;

const REPO: &str = "acme/widgets";
const FIXTURES: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/hub_hooks");

fn token(store: &dyn Store, user_id: i64) -> String {
    let identity = ForgeIdentity {
        provider: "github".into(),
        host: "github.com".into(),
        user_id,
    };
    let user = store
        .upsert_user(&identity, &format!("user{user_id}"))
        .unwrap();
    mint_agent_token(store, &user.id, "agent", REPO, 3600)
        .unwrap()
        .1
        .expose()
        .to_owned()
}

fn fixture(name: &str) -> String {
    std::fs::read_to_string(Path::new(FIXTURES).join(format!("{name}.json"))).unwrap()
}

/// Runs `agentscm hook cursor` for `secret` against `hub`, with a clean environment.
fn cursor_hook(hub: &str, repo: &str, secret: &str, payload: &str) -> Value {
    let mut child = Command::new(env!("CARGO_BIN_EXE_agentscm"))
        .args(["hook", "cursor", "--hub-url", hub, "--repo", repo])
        .env_clear()
        .envs(std::env::var_os("SystemRoot").map(|v| ("SystemRoot", v)))
        .env("AGENTSCM_HUB_TOKEN", secret)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(payload.as_bytes())
        .unwrap();
    let output = child.wait_with_output().unwrap();
    assert_eq!(output.status.code(), Some(0));
    serde_json::from_slice(&output.stdout).unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_claude_http_hook_then_a_cursor_hook_on_the_same_file_gets_a_stop() {
    let dir = TempDir::new().unwrap();
    let store = store::open(&format!(
        "sqlite://{}",
        dir.path().join("hub.sqlite").display()
    ))
    .unwrap();
    let objects = LocalBackend::open(dir.path().join("objects")).unwrap();
    let app = router(HubState::new(store.clone(), Arc::new(objects)));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let hub = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await });
    let (claude, cursor) = (token(&*store, 1), token(&*store, 2));

    // Exactly what the generated Claude Code hook sends: bearer header, the repository guard,
    // and the raw PreToolUse payload (no project-root header; the hub uses the payload's cwd).
    let reply = reqwest::Client::new()
        .post(format!("{hub}/v1/hub/hook/claude?repo={REPO}"))
        .bearer_auth(&claude)
        .header("content-type", "application/json")
        .body(fixture("claude-PreToolUse-Edit"))
        .send()
        .await
        .unwrap()
        .bytes()
        .await
        .unwrap();
    let reply: Value = serde_json::from_slice(&reply).unwrap();
    assert!(reply.get("hookSpecificOutput").is_none(), "{reply}");

    let (hub_url, secret, payload) = (hub.clone(), cursor.clone(), fixture("cursor-postToolUse"));
    let warned =
        tokio::task::spawn_blocking(move || cursor_hook(&hub_url, REPO, &secret, &payload))
            .await
            .unwrap();
    let text = warned["additional_context"].as_str().unwrap();
    assert!(
        text.contains("already working on src/widget.rs"),
        "{warned}"
    );
    assert!(warned.get("permission").is_none(), "{warned}");

    // A token for another repository than the guard names records nothing.
    let (hub_url, payload) = (hub.clone(), fixture("cursor-postToolUse"));
    let guarded =
        tokio::task::spawn_blocking(move || cursor_hook(&hub_url, "other/repo", &cursor, &payload))
            .await
            .unwrap();
    let text = guarded["additional_context"].as_str().unwrap();
    assert!(
        text.contains("coordination unknown") && !text.contains("already working"),
        "{guarded}"
    );
}
