//! What each coding agent gets, end to end: one test for each row of the coverage table in
//! docs/agents.md. Each test runs the command `client setup` installs for that agent
//! (`agentscm --no-telemetry hook <agent> --client-connection`) with the payloads its host
//! sends, against a real Hub, and fails when a claim in that row stops being true. Which events
//! setup installs for each agent is pinned by the wiring tests in `src/client_setup.rs` and
//! `src/client_plugin.rs`. A recorded wait is shown by the Hub only after 60 seconds, so these
//! tests check that a wait is accepted and never answered; the Hub's own hook tests cover what
//! it records.
#![cfg(unix)]

use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;

use agentscm::backend::LocalBackend;
use agentscm::hub::store::{
    self, ForgeIdentity, Store, agent_token_digest, mint_agent_token, unix_now,
};
use agentscm::hub::work::{WorkRole, WorkStart};
use agentscm::hub::{HubState, router};
use serde_json::{Value, json};
use tempfile::TempDir;

const REPO: &str = "acme/widgets";
/// The native session of the agent under test.
const SESSION: &str = "11111111-2222-3333-4444-555555555555";
/// Where Claude Code, Codex and Gemini CLI take context from a hook.
const HOOK_CONTEXT: &str = "/hookSpecificOutput/additionalContext";
/// The file the agent under test leads Work on when a coordination request waits for it.
const LED: &str = "src/widget.rs";

/// A Hub, a checkout connected to it, and two agent tokens: the agent under test and another
/// person's agent.
struct Bench {
    runtime: tokio::runtime::Runtime,
    hub: String,
    store: Arc<dyn Store>,
    home: TempDir,
    root: PathBuf,
    own: String,
    other: String,
}

impl Bench {
    fn start() -> Self {
        let home = TempDir::new().unwrap();
        let store = store::open(&format!(
            "sqlite://{}",
            home.path().join("hub.sqlite").display()
        ))
        .unwrap();
        let objects = LocalBackend::open(home.path().join("objects")).unwrap();
        let app = router(HubState::new(store.clone(), Arc::new(objects)));
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap();
        let listener = runtime
            .block_on(tokio::net::TcpListener::bind("127.0.0.1:0"))
            .unwrap();
        let hub = format!("http://{}", listener.local_addr().unwrap());
        runtime.spawn(async move { axum::serve(listener, app).await });
        let secret = |user_id: i64| {
            let identity = ForgeIdentity {
                provider: "github".into(),
                host: "github.com".into(),
                user_id,
            };
            let user = store
                .upsert_user(&identity, &format!("user{user_id}"))
                .unwrap();
            mint_agent_token(&*store, &user.id, "agent", REPO, 3600)
                .unwrap()
                .1
                .expose()
                .to_owned()
        };
        let (own, other) = (secret(1), secret(2));

        let root = home.path().join("repo");
        std::fs::create_dir_all(root.join("src")).unwrap();
        for args in [
            &["init", "--quiet"][..],
            &[
                "remote",
                "add",
                "origin",
                "https://github.com/acme/widgets.git",
            ],
        ] {
            let done = Command::new("git")
                .current_dir(&root)
                .args(args)
                .output()
                .unwrap();
            assert!(
                done.status.success(),
                "{}",
                String::from_utf8_lossy(&done.stderr)
            );
        }
        let root = root.canonicalize().unwrap();
        let config = home.path().join("config/agentscm");
        std::fs::create_dir_all(&config).unwrap();
        let token = home.path().join("client.token");
        std::fs::write(&token, format!("{own}\n")).unwrap();
        std::fs::set_permissions(&token, std::fs::Permissions::from_mode(0o600)).unwrap();
        let saved = json!({"version": 1, "connections": [{"root": root, "origin_host": "github.com",
            "hub": hub, "repo": REPO, "token_file": token}]});
        std::fs::write(config.join("connections.json"), saved.to_string()).unwrap();
        Self {
            runtime,
            hub,
            store,
            home,
            root,
            own,
            other,
        }
    }

    /// Runs the command `client setup` installs for `agent` with one payload from its host and
    /// returns what the host reads back.
    fn hook(&self, agent: &str, payload: &Value) -> Value {
        let mut child = Command::new(env!("CARGO_BIN_EXE_agentscm"))
            .args(["--no-telemetry", "hook", agent, "--client-connection"])
            .env_clear()
            .env("XDG_CONFIG_HOME", self.home.path().join("config"))
            .env("HOME", self.home.path())
            .env("PATH", std::env::var_os("PATH").unwrap_or_default())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(payload.to_string().as_bytes())
            .unwrap();
        let output = child.wait_with_output().unwrap();
        assert!(
            output.status.success(),
            "{agent}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        serde_json::from_slice(&output.stdout).unwrap_or_else(|_| {
            panic!(
                "{agent} printed {}",
                String::from_utf8_lossy(&output.stdout)
            )
        })
    }

    /// One request straight to the Hub as `secret`, as a client with no AgentSCM hook sends it.
    fn post(&self, path: &str, secret: &str, body: &Value) -> Value {
        let url = format!("{}{path}", self.hub);
        let bytes = self.runtime.block_on(async {
            reqwest::Client::new()
                .post(url)
                .bearer_auth(secret)
                .header("content-type", "application/json")
                .body(body.to_string())
                .send()
                .await
                .unwrap()
                .bytes()
                .await
                .unwrap()
        });
        serde_json::from_slice(&bytes).unwrap()
    }

    /// Another person's session announces an edit of `file`, so it holds that file.
    fn other_holds(&self, file: &str) {
        // Through the generic hook, so `held_through` never counts it as the agent under test's.
        let edit = json!({"agent": "other", "session": "other-session", "event": "check",
            "cwd": "/r", "paths": [format!("/r/{file}")]});
        let path = format!("/v1/hub/hook/generic?repo={REPO}");
        let reply = self.post(&path, &self.other, &edit);
        assert_ne!(reply["status"], "unknown", "{reply}");
    }

    /// The files the Hub lists as held through `agent`'s hooks.
    fn held_through(&self, agent: &str) -> Vec<String> {
        // Protects: the hook's edit is recorded as coming through that agent's hook. Other
        // routes the same hold also came through may be listed beside it.
        let via = format!("hook:{agent}");
        self.store
            .live_intents(REPO, unix_now(), 10)
            .unwrap()
            .into_iter()
            .filter(|intent| intent.via.contains(&via))
            .flat_map(|intent| intent.paths)
            .collect()
    }

    /// The agent under test leads Work on [`LED`], and another agent's overlapping start queues a
    /// coordination request that its session must answer. Returns the request's id.
    fn request(&self) -> String {
        let token_id = |secret: &str| {
            self.store
                .authenticate(&agent_token_digest(secret).unwrap(), unix_now())
                .unwrap()
                .unwrap()
                .token_id
        };
        let start = |key: &str, session: &str| WorkStart {
            issue_choice: None,
            work_id: None,
            source: None,
            request_key: Some(key.into()),
            title: key.into(),
            description: None,
            session: session.into(),
            role: WorkRole::Lead,
            scope: vec![LED.into()],
            branch: None,
            worktree: None,
            next_action: None,
            depends_on: Vec::new(),
            related_work: Vec::new(),
            takeover_generation: None,
            preflight: None,
        };
        self.store
            .work_start(
                &token_id(&self.own),
                &start("hold-widget", SESSION),
                unix_now(),
            )
            .unwrap();
        self.store
            .work_start(
                &token_id(&self.other),
                &start("needs-widget", "other-session"),
                unix_now(),
            )
            .unwrap()
            .coordination_request
            .expect("an overlapping start queues a request")
            .id
    }

    /// The text of one MCP tool call's result, for a client that has only the Hub's MCP server.
    fn tool(&self, secret: &str, name: &str, arguments: Value) -> String {
        let call = json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call",
            "params": {"name": name, "arguments": arguments}});
        let reply = self.post("/mcp", secret, &call);
        text(&reply, "/result/content/0/text").to_owned()
    }

    fn file(&self, name: &str) -> String {
        self.root.join(name).to_str().unwrap().to_owned()
    }
}

fn text<'a>(reply: &'a Value, pointer: &str) -> &'a str {
    reply
        .pointer(pointer)
        .and_then(Value::as_str)
        .unwrap_or_else(|| panic!("no {pointer} in {reply}"))
}

/// Rules at session start: the context names the native session and carries every required rule.
fn carries_rules(reply: &Value, pointer: &str) {
    let context = text(reply, pointer);
    assert!(context.contains(SESSION), "{reply}");
    assert!(agentscm::hub::agent_rules::carries_all(context), "{reply}");
}

/// A subagent's edit under its parent's session: it is not refused, and the note it gets points
/// at the lead session without handing it the request addressed to that session.
fn under_parent(reply: &Value, request: &str) -> bool {
    reply
        .pointer("/hookSpecificOutput/permissionDecision")
        .is_none()
        && !text(reply, HOOK_CONTEXT).contains(request)
}

/// An answer that cannot refuse: it carries no field any supported host reads as a refusal or a
/// stop. Protects Cursor's row ("nothing in the answer can refuse") without pinning which other
/// fields, such as a note, the answer has.
fn cannot_refuse(reply: &Value) {
    for field in ["/permission", "/decision", "/permissionDecision"] {
        assert!(reply.pointer(field).is_none(), "{field}: {reply}");
    }
    assert!(
        reply
            .pointer("/hookSpecificOutput/permissionDecision")
            .is_none(),
        "{reply}"
    );
    assert_ne!(reply.get("continue"), Some(&json!(false)), "{reply}");
}

/// A wait is reported to the Hub and never answered: the host reads back nothing.
fn says_nothing(reply: &Value) {
    assert_eq!(*reply, json!({}));
}

/// Session end is reported: the file the session announced is held, and the end releases it.
fn end_releases(bench: &Bench, agent: &str, end: &Value) {
    assert!(
        bench.held_through(agent).contains(&"src/own.rs".to_owned()),
        "{agent}"
    );
    bench.hook(agent, end);
    let held = bench.held_through(agent);
    assert!(held.is_empty(), "{agent} still holds {held:?}");
}

/// A Claude Code or Codex payload: PascalCase event names and snake_case fields.
fn pascal(root: &Path, event: &str) -> Value {
    json!({"hook_event_name": event, "session_id": SESSION, "cwd": root})
}

#[test]
fn claude_code_gets_what_its_row_says() {
    let bench = Bench::start();
    let edit = |bench: &Bench, file: &str| {
        let mut edit = pascal(&bench.root, "PreToolUse");
        edit["tool_name"] = json!("Edit");
        edit["tool_input"] =
            json!({"file_path": bench.file(file), "old_string": "a", "new_string": "b"});
        edit
    };
    carries_rules(
        &bench.hook("claude", &pascal(&bench.root, "SessionStart")),
        HOOK_CONTEXT,
    );
    // The edit check runs before the write and can refuse it.
    bench.other_holds("src/held.rs");
    let refused = bench.hook("claude", &edit(&bench, "src/held.rs"));
    assert_eq!(
        refused.pointer("/hookSpecificOutput/permissionDecision"),
        Some(&json!("deny")),
        "{refused}"
    );
    assert!(text(&refused, "/hookSpecificOutput/permissionDecisionReason").contains("src/held.rs"));
    // Waiting is reported for a permission prompt and for the end of a turn.
    let mut prompt = pascal(&bench.root, "Notification");
    prompt["notification_type"] = json!("permission_prompt");
    says_nothing(&bench.hook("claude", &prompt));
    says_nothing(&bench.hook("claude", &pascal(&bench.root, "Stop")));
    bench.hook("claude", &edit(&bench, "src/own.rs"));
    end_releases(&bench, "claude", &pascal(&bench.root, "SessionEnd"));

    // Messages from the Hub arrive at session start, on each prompt and at an edit, which is
    // held until the session answers.
    let bench = Bench::start();
    let request = bench.request();
    let started = bench.hook("claude", &pascal(&bench.root, "SessionStart"));
    assert!(text(&started, HOOK_CONTEXT).contains(&request), "{started}");
    let prompted = bench.hook("claude", &pascal(&bench.root, "UserPromptSubmit"));
    assert!(
        text(&prompted, HOOK_CONTEXT).contains(&request),
        "{prompted}"
    );
    let held = bench.hook("claude", &edit(&bench, LED));
    assert!(
        text(&held, "/hookSpecificOutput/permissionDecisionReason").contains(&request),
        "{held}"
    );
    // A subagent's call carries its parent's session: its edit is checked under the parent's
    // Work and pointed at the lead session instead of being held.
    let mut subagent = edit(&bench, LED);
    subagent["agent_id"] = json!("a1b2c3");
    subagent["agent_type"] = json!("general-purpose");
    let reply = bench.hook("claude", &subagent);
    assert!(under_parent(&reply, &request), "{reply}");
}

#[test]
fn codex_gets_what_its_row_says() {
    let bench = Bench::start();
    let edit = |bench: &Bench, file: &str| {
        let mut edit = pascal(&bench.root, "PreToolUse");
        edit["tool_name"] = json!("apply_patch");
        edit["tool_input"] = json!({"command": format!(
            "*** Begin Patch\n*** Update File: {file}\n@@\n-a\n+b\n*** End Patch\n"
        )});
        edit
    };
    carries_rules(
        &bench.hook("codex", &pascal(&bench.root, "SessionStart")),
        HOOK_CONTEXT,
    );
    // The edit check runs before the patch is applied and can refuse it.
    bench.other_holds("src/held.rs");
    let refused = bench.hook("codex", &edit(&bench, "src/held.rs"));
    assert_eq!(
        refused.pointer("/hookSpecificOutput/permissionDecision"),
        Some(&json!("deny")),
        "{refused}"
    );
    assert!(text(&refused, "/hookSpecificOutput/permissionDecisionReason").contains("src/held.rs"));
    // Waiting is reported for an approval request and for the end of a turn.
    says_nothing(&bench.hook("codex", &pascal(&bench.root, "PermissionRequest")));
    says_nothing(&bench.hook("codex", &pascal(&bench.root, "Stop")));
    bench.hook("codex", &edit(&bench, "src/own.rs"));
    end_releases(&bench, "codex", &pascal(&bench.root, "SessionEnd"));

    // Messages from the Hub arrive at session start and at an edit, which is held until the
    // session answers. No prompt hook is installed for Codex.
    let bench = Bench::start();
    let request = bench.request();
    let started = bench.hook("codex", &pascal(&bench.root, "SessionStart"));
    assert!(text(&started, HOOK_CONTEXT).contains(&request), "{started}");
    let held = bench.hook("codex", &edit(&bench, LED));
    assert!(
        text(&held, "/hookSpecificOutput/permissionDecisionReason").contains(&request),
        "{held}"
    );
    // A call that names a subagent is handled as Claude Code's is. Whether Codex sends that
    // field was not checked, which is what the table says.
    let mut subagent = edit(&bench, LED);
    subagent["agent_id"] = json!("a1b2c3");
    let reply = bench.hook("codex", &subagent);
    assert!(under_parent(&reply, &request), "{reply}");
}

#[test]
fn cursor_gets_what_its_row_says() {
    let bench = Bench::start();
    let event = |bench: &Bench, name: &str| {
        json!({"hook_event_name": name, "conversation_id": SESSION, "session_id": SESSION,
            "workspace_roots": [bench.root]})
    };
    // The only edit event installed for Cursor comes after the write.
    let edited = |bench: &Bench, file: &str| {
        let mut edit = event(bench, "postToolUse");
        edit["cwd"] = json!(bench.root);
        edit["tool_name"] = json!("Write");
        edit["tool_input"] = json!({"file_path": bench.file(file), "contents": "b"});
        edit
    };
    carries_rules(
        &bench.hook("cursor", &event(&bench, "sessionStart")),
        "/additional_context",
    );
    // The check arrives just after the write, as a note: nothing in the answer can refuse.
    bench.other_holds("src/held.rs");
    let noted = bench.hook("cursor", &edited(&bench, "src/held.rs"));
    assert!(
        text(&noted, "/additional_context").contains("src/held.rs"),
        "{noted}"
    );
    cannot_refuse(&noted);
    // Waiting is reported for the end of a turn only: Cursor reports no permission prompt. A
    // reply's text is kept on the machine until its turn ends; neither call says anything back.
    let mut replied = event(&bench, "afterAgentResponse");
    replied["text"] = json!("Shall I merge it?");
    says_nothing(&bench.hook("cursor", &replied));
    says_nothing(&bench.hook("cursor", &event(&bench, "stop")));
    bench.hook("cursor", &edited(&bench, "src/own.rs"));
    end_releases(&bench, "cursor", &event(&bench, "sessionEnd"));

    // Messages from the Hub arrive at session start and just after an edit, never holding it.
    let bench = Bench::start();
    let request = bench.request();
    let started = bench.hook("cursor", &event(&bench, "sessionStart"));
    assert!(
        text(&started, "/additional_context").contains(&request),
        "{started}"
    );
    let noted = bench.hook("cursor", &edited(&bench, LED));
    assert!(
        text(&noted, "/additional_context").contains(&request),
        "{noted}"
    );
    cannot_refuse(&noted);
    // Subagents are not told apart: a call that names one is handled as the session's own.
    let mut subagent = edited(&bench, LED);
    subagent["agent_id"] = json!("a1b2c3");
    let reply = bench.hook("cursor", &subagent);
    assert!(
        text(&reply, "/additional_context").contains(&request),
        "{reply}"
    );
}

#[test]
fn github_copilot_gets_what_its_row_says() {
    // Copilot CLI's camelCase payloads, which name no event.
    let bench = Bench::start();
    let event = |bench: &Bench| json!({"sessionId": SESSION, "cwd": bench.root});
    let edit = |bench: &Bench, file: &str| {
        let mut edit = event(bench);
        edit["toolName"] = json!("edit");
        edit["toolArgs"] = json!({"path": bench.file(file), "old_str": "a", "new_str": "b"});
        edit
    };
    let mut start = event(&bench);
    start["source"] = json!("new");
    carries_rules(&bench.hook("copilot", &start), "/additionalContext");
    // The edit check runs before the write and can refuse it.
    bench.other_holds("src/held.rs");
    let refused = bench.hook("copilot", &edit(&bench, "src/held.rs"));
    assert_eq!(refused["permissionDecision"], "deny", "{refused}");
    assert!(text(&refused, "/permissionDecisionReason").contains("src/held.rs"));
    // Waiting is reported for a permission prompt and for the end of a turn.
    let mut prompt = event(&bench);
    prompt["notification_type"] = json!("permission_prompt");
    says_nothing(&bench.hook("copilot", &prompt));
    let mut stop = event(&bench);
    stop["stopReason"] = json!("end_turn");
    says_nothing(&bench.hook("copilot", &stop));
    bench.hook("copilot", &edit(&bench, "src/own.rs"));
    let mut end = event(&bench);
    end["reason"] = json!("complete");
    end_releases(&bench, "copilot", &end);

    // Messages from the Hub arrive at session start. Before an edit Copilot CLI takes no note,
    // so a request reaches it there only as the refusal that holds the edit.
    let bench = Bench::start();
    let request = bench.request();
    let mut start = event(&bench);
    start["source"] = json!("new");
    let started = bench.hook("copilot", &start);
    assert!(
        text(&started, "/additionalContext").contains(&request),
        "{started}"
    );
    let held = bench.hook("copilot", &edit(&bench, LED));
    assert!(
        text(&held, "/permissionDecisionReason").contains(&request),
        "{held}"
    );
    assert!(held.get("additionalContext").is_none(), "{held}");
    // A call that names a subagent is put under its parent: its edit is not held for the
    // parent's request. Copilot CLI takes no note there, so the pointer to the lead is not shown.
    let mut subagent = edit(&bench, LED);
    subagent["agentId"] = json!("a1b2c3");
    says_nothing(&bench.hook("copilot", &subagent));
}

#[test]
fn gemini_cli_gets_what_its_row_says() {
    let bench = Bench::start();
    let edit = |bench: &Bench, file: &str| {
        let mut edit = pascal(&bench.root, "BeforeTool");
        edit["tool_name"] = json!("write_file");
        edit["tool_input"] = json!({"file_path": bench.file(file), "content": "b"});
        edit
    };
    carries_rules(
        &bench.hook("gemini", &pascal(&bench.root, "SessionStart")),
        HOOK_CONTEXT,
    );
    // The edit check runs before the write and can refuse it.
    bench.other_holds("src/held.rs");
    let refused = bench.hook("gemini", &edit(&bench, "src/held.rs"));
    assert_eq!(refused["decision"], "deny", "{refused}");
    assert!(
        text(&refused, "/reason").contains("src/held.rs"),
        "{refused}"
    );
    // Waiting is reported for a tool permission prompt and for the end of a turn.
    let mut prompt = pascal(&bench.root, "Notification");
    prompt["notification_type"] = json!("ToolPermission");
    says_nothing(&bench.hook("gemini", &prompt));
    says_nothing(&bench.hook("gemini", &pascal(&bench.root, "AfterAgent")));
    bench.hook("gemini", &edit(&bench, "src/own.rs"));
    end_releases(&bench, "gemini", &pascal(&bench.root, "SessionEnd"));

    // Messages from the Hub arrive at session start, and before an edit as Gemini CLI's
    // `systemMessage`, which does not hold the edit.
    let bench = Bench::start();
    let request = bench.request();
    let started = bench.hook("gemini", &pascal(&bench.root, "SessionStart"));
    assert!(text(&started, HOOK_CONTEXT).contains(&request), "{started}");
    let noted = bench.hook("gemini", &edit(&bench, LED));
    assert!(text(&noted, "/systemMessage").contains(&request), "{noted}");
    assert!(noted.get("decision").is_none(), "{noted}");
    // Subagents are not told apart: a call that names one is handled as the session's own.
    let mut subagent = edit(&bench, LED);
    subagent["agent_id"] = json!("a1b2c3");
    let reply = bench.hook("gemini", &subagent);
    assert!(text(&reply, "/systemMessage").contains(&request), "{reply}");
}

#[test]
fn a_client_with_no_hooks_gets_what_its_row_says() {
    let bench = Bench::start();
    // The rules reach it in the MCP server's instructions, with no session ID: no hook ran.
    let init = json!({"jsonrpc": "2.0", "id": 1, "method": "initialize",
        "params": {"protocolVersion": "2025-06-18", "capabilities": {}}});
    let reply = bench.post("/mcp", &bench.own, &init);
    let instructions = text(&reply, "/result/instructions");
    assert!(
        agentscm::hub::agent_rules::carries_all(instructions),
        "{reply}"
    );
    assert!(
        !instructions.contains("AgentSCM native session ID"),
        "{reply}"
    );

    // Nothing is checked unless the agent asks: what it learns comes back in its own tool calls.
    bench.tool(
        &bench.other,
        "agentscm_start",
        json!({"task": "fix billing", "paths": ["src/billing.rs"]}),
    );
    let started = bench.tool(
        &bench.own,
        "agentscm_start",
        json!({"task": "refactor docs", "paths": ["docs/a.md"]}),
    );
    assert!(started.contains("fix billing"), "{started}");
    let checked = bench.tool(
        &bench.own,
        "agentscm_check",
        json!({"paths": ["docs/a.md"]}),
    );
    assert!(!checked.is_empty());

    // Its session end is reported only by its own `agentscm_done`: until then the other agent
    // is told of its task, and afterwards it is not.
    let seen = || bench.tool(&bench.other, "agentscm_status", json!({}));
    assert!(seen().contains("refactor docs"), "{}", seen());
    bench.tool(&bench.own, "agentscm_done", json!({}));
    assert!(!seen().contains("refactor docs"), "{}", seen());
}
