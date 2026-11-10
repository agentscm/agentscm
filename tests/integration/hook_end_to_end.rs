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

/// Exactly what the generated Claude Code hook sends for an edit: bearer header, the repository
/// guard, and the raw PreToolUse payload (no project-root header; the hub uses the payload's cwd).
async fn claude_edit(hub: &str, secret: &str) -> Value {
    let reply = reqwest::Client::new()
        .post(format!("{hub}/v1/hub/hook/claude?repo={REPO}"))
        .bearer_auth(secret)
        .header("content-type", "application/json")
        .body(fixture("claude-PreToolUse-Edit"))
        .send()
        .await
        .unwrap()
        .bytes()
        .await
        .unwrap();
    serde_json::from_slice(&reply).unwrap()
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

    // The session declared no Work: its first edit is refused once, and the repeat passes.
    let refused = claude_edit(&hub, &claude).await;
    assert!(refused.to_string().contains("WORK_UNDECLARED"), "{refused}");
    let reply = claude_edit(&hub, &claude).await;
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

/// Runs `agentscm hook claude` for `secret` against `hub`, with a clean environment.
#[cfg(unix)]
fn claude_hook(hub: &str, secret: &str, payload: &Value) -> Value {
    let mut child = Command::new(env!("CARGO_BIN_EXE_agentscm"))
        .args(["hook", "claude", "--hub-url", hub, "--repo", REPO])
        .env_clear()
        .env("AGENTSCM_HUB_TOKEN", secret)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    stdin.write_all(payload.to_string().as_bytes()).unwrap();
    drop(stdin);
    let output = child.wait_with_output().unwrap();
    assert_eq!(output.status.code(), Some(0));
    serde_json::from_slice(&output.stdout).unwrap()
}

/// `--hub-url` routing sends the agent's own paths with the checkout root beside them. A second
/// agent that reaches the checkout through a symlink is still refused the first agent's file, and
/// an edit through an in-repository symlink, which the Hub would file under another path, gets
/// "coordination unknown" instead of an empty reply.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_hub_url_hook_through_a_symlink_is_refused_a_held_file() {
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
    let (first, second) = (token(&*store, 1), token(&*store, 2));

    let real = dir.path().join("checkout");
    std::fs::create_dir_all(real.join(".git")).unwrap();
    std::fs::create_dir_all(real.join("src")).unwrap();
    let real = real.canonicalize().unwrap();
    let alias = dir.path().join("alias");
    std::os::unix::fs::symlink(&real, &alias).unwrap();
    std::os::unix::fs::symlink("src", real.join("lnk")).unwrap();
    let edit = |session: &str, cwd: &Path, file: &Path| {
        serde_json::json!({"session_id": session, "cwd": cwd, "hook_event_name": "PreToolUse",
            "tool_name": "Edit", "tool_input": {"file_path": file}})
    };
    let run = |secret: &str, payload: Value| {
        let (hub, secret) = (hub.clone(), secret.to_owned());
        tokio::task::spawn_blocking(move || claude_hook(&hub, &secret, &payload))
    };

    // The first agent takes the file (its first edit is refused once for undeclared Work).
    let held = edit("a1", &real, &real.join("src/widget.rs"));
    run(&first, held.clone()).await.unwrap();
    let reply = run(&first, held).await.unwrap();
    assert!(reply.get("hookSpecificOutput").is_none(), "{reply}");

    // The second reaches the same file through the symlink and is refused it.
    let refused = run(&second, edit("b1", &alias, &alias.join("src/widget.rs")))
        .await
        .unwrap();
    let out = &refused["hookSpecificOutput"];
    assert_eq!(out["permissionDecision"], "deny", "{refused}");
    let reason = out["permissionDecisionReason"].as_str().unwrap();
    assert!(
        reason.contains("another agent is working on that file")
            && reason.contains("src/widget.rs"),
        "{refused}"
    );

    // An edit through the in-repository symlink says coordination is unknown.
    let unplaced = run(&second, edit("b2", &real, &real.join("lnk/widget.rs")))
        .await
        .unwrap();
    let text = unplaced["hookSpecificOutput"]["additionalContext"]
        .as_str()
        .unwrap_or_default();
    assert!(
        text.contains("coordination unknown (edit path not matched to the checkout)"),
        "{unplaced}"
    );
}
