//! Two agents, one file, a real hub: a Claude Code HTTP-hook request announces the file, then the
//! real `agentscm hook cursor` client for a second token is told to stop. Also the repository
//! guard: a token used for another repository records nothing. And the connected client in a
//! checkout where a merge stopped on a conflict: the conflicted file is not checked.
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

/// A hub on loopback with its SQLite store under `dir`: the store and the hub's URL.
async fn serve(dir: &TempDir) -> (Arc<dyn Store>, String) {
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
    (store, hub)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_claude_http_hook_then_a_cursor_hook_on_the_same_file_gets_a_stop() {
    let dir = TempDir::new().unwrap();
    let (store, hub) = serve(&dir).await;
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

/// The connected `agentscm hook claude` in a checkout where a merge stopped on a conflict.
#[cfg(unix)]
mod conflict {
    use std::os::unix::fs::PermissionsExt;

    use agentscm::hub::store::{agent_token_digest, unix_now};
    use agentscm::hub::work::{WorkItem, WorkRole, WorkStart};
    use serde_json::json;

    use super::*;

    const DENY: &str = "/hookSpecificOutput/permissionDecisionReason";

    /// Runs Git in `root` with a fixed identity and no user configuration; false when it fails.
    fn git(root: &Path, args: &[&str]) -> bool {
        Command::new("git")
            .current_dir(root)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .args(["-c", "user.name=t", "-c", "user.email=t@example.invalid"])
            .args(args)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .unwrap()
            .success()
    }

    fn token_id(store: &dyn Store, secret: &str) -> String {
        let digest = agent_token_digest(secret).unwrap();
        let principal = store.authenticate(&digest, unix_now()).unwrap();
        principal.unwrap().token_id
    }

    /// Active Work that `session` of `secret`'s token leads over `scope`.
    fn lead_work(store: &dyn Store, secret: &str, session: &str, scope: &[&str]) -> WorkItem {
        let start = WorkStart {
            issue_choice: None,
            work_id: None,
            source: None,
            request_key: None,
            title: format!("{session}'s task"),
            description: None,
            session: session.into(),
            role: WorkRole::Lead,
            scope: scope.iter().map(|path| (*path).to_owned()).collect(),
            branch: None,
            worktree: None,
            next_action: None,
            depends_on: Vec::new(),
            related_work: Vec::new(),
            takeover_generation: None,
            preflight: None,
        };
        let started = store.work_start_auto(&token_id(store, secret), &start, unix_now());
        started.unwrap().item
    }

    /// The reply of the connected client to the resolver's call of `tool` in `root`.
    async fn client_edit(config: &Path, root: &Path, tool: &str, input: Value) -> Value {
        let payload = json!({"hook_event_name": "PreToolUse", "session_id": "resolver",
            "cwd": root, "tool_name": tool, "tool_input": input});
        let mut command = Command::new(env!("CARGO_BIN_EXE_agentscm"));
        command
            .args(["hook", "claude", "--client-connection"])
            .env_clear()
            .env("XDG_CONFIG_HOME", config)
            .env("HOME", config)
            .env("PATH", std::env::var_os("PATH").unwrap_or_default())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped());
        let run = move || {
            let mut child = command.spawn().unwrap();
            let mut stdin = child.stdin.take().unwrap();
            stdin.write_all(payload.to_string().as_bytes()).unwrap();
            drop(stdin);
            let output = child.wait_with_output().unwrap();
            assert_eq!(output.status.code(), Some(0));
            serde_json::from_slice(&output.stdout).unwrap()
        };
        tokio::task::spawn_blocking(run).await.unwrap()
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn an_unresolved_conflict_is_edited_unchecked_and_other_paths_are_still_checked() {
        let dir = TempDir::new().unwrap();
        let (store, hub) = serve(&dir).await;
        let (holder, resolver) = (token(&*store, 1), token(&*store, 2));
        // Another agent's Work holds both files. The resolver leads Work that covers neither, so
        // its edit of either is refused on every attempt.
        let held = lead_work(&*store, &holder, "holder", &["docs/guide.md", "notes.md"]);
        lead_work(&*store, &resolver, "resolver", &["src/own.rs"]);

        // The resolver's checkout: both sides changed docs/guide.md, so the merge stops on it.
        let root = dir.path().join("repo");
        std::fs::create_dir_all(root.join("docs")).unwrap();
        assert!(git(&root, &["init", "--quiet", "--initial-branch=main"]));
        let origin = "https://github.com/acme/widgets.git";
        assert!(git(&root, &["remote", "add", "origin", origin]));
        std::fs::write(root.join("notes.md"), "the same on both sides\n").unwrap();
        for side in ["main", "theirs", "ours"] {
            if side != "main" {
                assert!(git(&root, &["checkout", "--quiet", "-b", side, "main"]));
            }
            std::fs::write(root.join("docs/guide.md"), side).unwrap();
            assert!(git(&root, &["add", "."]));
            assert!(git(&root, &["commit", "--quiet", "-m", side]));
        }
        assert!(!git(&root, &["merge", "theirs"]));
        let root = root.canonicalize().unwrap();

        let config = dir.path().join("config");
        std::fs::create_dir_all(config.join("agentscm")).unwrap();
        let secret = dir.path().join("client.token");
        std::fs::write(&secret, format!("{resolver}\n")).unwrap();
        std::fs::set_permissions(&secret, std::fs::Permissions::from_mode(0o600)).unwrap();
        let saved = json!({"version": 1, "connections": [{"root": root,
            "origin_host": "github.com", "hub": hub, "repo": REPO, "token_file": secret}]});
        let connections = config.join("agentscm/connections.json");
        std::fs::write(connections, saved.to_string()).unwrap();

        let edit = |file: &str| client_edit(&config, &root, "Edit", json!({"file_path": file}));
        let reason = |reply: &Value| {
            reply
                .pointer(DENY)
                .and_then(Value::as_str)
                .map(str::to_owned)
        };
        let said = |reply: &Value| {
            let context = reply["hookSpecificOutput"]["additionalContext"].as_str();
            context.is_some_and(|text| text.starts_with(
                "AgentSCM: unresolved Git conflict in docs/guide.md in this checkout, so this edit was not checked"
            ))
        };

        // While the conflict is unresolved, the edit that resolves it runs and says why.
        let passed = edit("docs/guide.md").await;
        assert!(reason(&passed).is_none() && said(&passed), "{passed}");
        // It took, extended and weakened nothing: the holder's Work is as it was, and no session
        // holds a file.
        let unchanged = store.work_get(&token_id(&*store, &holder), &held.id, unix_now());
        assert_eq!(unchanged.unwrap(), Some(held));
        assert!(store.live_intents(REPO, unix_now(), 10).unwrap().is_empty());

        // A held file with no conflict is refused as ever in the same merge, alone or in one
        // patch with the conflicted file.
        let unlinked = "EDIT_UNLINKED: this edit was not applied";
        let refused = edit("notes.md").await;
        assert!(
            reason(&refused).is_some_and(|text| text.contains(unlinked)),
            "{refused}"
        );
        let patch = "*** Begin Patch\n*** Update File: docs/guide.md\n*** Update File: notes.md\n*** End Patch";
        let refused = client_edit(&config, &root, "apply_patch", json!(patch)).await;
        let only_notes = r#"a scope that adds ["notes.md"]"#;
        assert!(
            reason(&refused).is_some_and(|text| text.contains(only_notes)),
            "{refused}"
        );
        assert!(said(&refused), "{refused}");

        // Once the merge is over, so is the exception.
        assert!(git(&root, &["merge", "--abort"]));
        let refused = edit("docs/guide.md").await;
        assert!(
            reason(&refused).is_some_and(|text| text.contains(unlinked)),
            "{refused}"
        );
    }
}
