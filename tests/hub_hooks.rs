//! The per-agent hook endpoint: every answer is HTTP 200 with JSON in the agent's own
//! output format, failures say coordination is unknown, and the only decision is refusing the
//! first contested edit of a file before it runs.
//! A real server on loopback and SQLite; the store behind it is covered by hub_sessions.
use std::fs;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use agentscm::backend::LocalBackend;
use agentscm::hub::store::{self, ForgeIdentity, Store, mint_agent_token, unix_now};
use agentscm::hub::{HubState, router};
use serde_json::{Value, json};
use tempfile::TempDir;

const REPO: &str = "acme/widgets";
const FIXTURES: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/hub_hooks");

struct Hub {
    address: SocketAddr,
    store: Arc<dyn Store>,
    next_user: i64,
    _dir: TempDir,
}

impl Hub {
    async fn start() -> Self {
        let dir = TempDir::new().unwrap();
        let url = format!("sqlite://{}", dir.path().join("hub.sqlite").display());
        let store = store::open(&url).unwrap();
        let objects = LocalBackend::open(dir.path().join("objects")).unwrap();
        let app = router(HubState::new(store.clone(), Arc::new(objects)));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await });
        Self {
            address,
            store,
            next_user: 0,
            _dir: dir,
        }
    }

    /// The bearer secret of a token for a fresh user.
    fn agent(&mut self) -> String {
        self.next_user += 1;
        let identity = ForgeIdentity {
            provider: "github".into(),
            host: "github.com".into(),
            user_id: self.next_user,
        };
        let user = self
            .store
            .upsert_user(&identity, &format!("user{}", self.next_user))
            .unwrap();
        let (_, secret) = mint_agent_token(&*self.store, &user.id, "agent", REPO, 3600).unwrap();
        secret.expose().to_owned()
    }

    fn post(&self, path: &str, secret: Option<&str>, body: &[u8]) -> (u16, Value) {
        self.post_with(path, secret, "", body)
    }

    /// POST `body` with `extra` raw header lines; returns the status and the parsed JSON body.
    fn post_with(
        &self,
        path: &str,
        secret: Option<&str>,
        extra: &str,
        body: &[u8],
    ) -> (u16, Value) {
        let mut stream = TcpStream::connect(self.address).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(10)))
            .unwrap();
        let mut head = format!(
            "POST {path} HTTP/1.1\r\nHost: hub\r\nConnection: close\r\n\
             Content-Type: application/json\r\nContent-Length: {}\r\n",
            body.len()
        );
        head.push_str(extra);
        if let Some(secret) = secret {
            head.push_str(&format!("Authorization: Bearer {secret}\r\n"));
        }
        head.push_str("\r\n");
        // The hub may answer before it has read a body it refuses, so write on the side.
        let mut writer = stream.try_clone().unwrap();
        let body = body.to_vec();
        let writing = std::thread::spawn(move || {
            let _ = writer.write_all(head.as_bytes());
            let _ = writer.write_all(&body);
        });
        let mut raw = Vec::new();
        let mut chunk = [0_u8; 4096];
        loop {
            match stream.read(&mut chunk) {
                Ok(0) => break,
                Ok(n) => raw.extend_from_slice(&chunk[..n]),
                Err(_) if raw.windows(4).any(|w| w == b"\r\n\r\n") => break,
                Err(error) => panic!("no response: {error}"),
            }
        }
        writing.join().unwrap();
        let raw = String::from_utf8(raw).unwrap();
        let (head, body) = raw.split_once("\r\n\r\n").unwrap();
        let status = head.split(' ').nth(1).unwrap().parse().unwrap();
        let content_type = head.to_ascii_lowercase();
        assert!(
            status != 200 || content_type.contains("content-type: application/json"),
            "{head}"
        );
        (status, serde_json::from_str(body).unwrap_or(Value::Null))
    }

    /// POST a fixture (or an edit of it) to `/v1/hub/hook/{agent}`; the answer must be a 200.
    fn hook_raw(&self, agent: &str, secret: &str, payload: &Value) -> Value {
        let (status, answer) = self.post(
            &format!("/v1/hub/hook/{agent}"),
            Some(secret),
            payload.to_string().as_bytes(),
        );
        assert_eq!(status, 200, "{agent}: {payload}");
        assert!(answer.is_object(), "{agent}: {answer}");
        answer
    }

    /// Like `hook_raw`, for a call that must not be refused.
    fn hook(&self, agent: &str, secret: &str, payload: &Value) -> Value {
        let answer = self.hook_raw(agent, secret, payload);
        assert!(!answer.to_string().contains("deny"), "{answer}");
        assert!(
            !answer.to_string().contains("permissionDecision"),
            "{answer}"
        );
        answer
    }
}

fn fixture(name: &str) -> Value {
    let text = fs::read_to_string(Path::new(FIXTURES).join(format!("{name}.json"))).unwrap();
    serde_json::from_str(&text).unwrap()
}

fn text_at<'a>(answer: &'a Value, pointer: &str) -> &'a str {
    answer
        .pointer(pointer)
        .and_then(Value::as_str)
        .unwrap_or_else(|| panic!("no {pointer} in {answer}"))
}

/// What a lone agent gets back for each fixture.
enum Expect {
    /// `{}`: nothing to say.
    Empty,
    /// Context (here: who else is working) at this JSON pointer.
    Context(&'static str),
    Allow,
    Generic(&'static str),
}

const CLAUDE_CONTEXT: &str = "/hookSpecificOutput/additionalContext";

fn expectations() -> Vec<(&'static str, Expect)> {
    use Expect::{Allow, Context, Empty, Generic};
    vec![
        ("claude-SessionStart", Context(CLAUDE_CONTEXT)),
        ("claude-PreToolUse-Edit", Empty),
        ("claude-PreToolUse-Write", Empty),
        ("claude-PreToolUse-NotebookEdit", Empty),
        ("claude-PreToolUse-Bash", Empty),
        ("claude-SessionEnd", Empty),
        ("codex-SessionStart", Context(CLAUDE_CONTEXT)),
        ("codex-PreToolUse", Empty),
        ("codex-SessionEnd", Empty),
        ("cursor-sessionStart", Context("/additional_context")),
        ("cursor-preToolUse", Allow),
        ("cursor-postToolUse", Empty),
        ("cursor-afterFileEdit", Empty),
        ("cursor-sessionEnd", Empty),
        ("copilot-sessionStart", Context("/additionalContext")),
        ("copilot-preToolUse", Empty),
        ("copilot-postToolUse", Empty),
        ("copilot-sessionEnd", Empty),
        ("copilot-vscode-SessionStart", Context(CLAUDE_CONTEXT)),
        ("copilot-vscode-PreToolUse", Empty),
        ("generic-start", Generic("clean")),
        ("generic-check", Generic("clean")),
        ("generic-done", Generic("clean")),
    ]
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn every_fixture_gets_its_agents_output_shape() {
    let table = expectations();
    let mut on_disk: Vec<String> = fs::read_dir(FIXTURES)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    on_disk.sort();
    let mut listed: Vec<String> = table
        .iter()
        .map(|(name, _)| format!("{name}.json"))
        .collect();
    listed.sort();
    assert_eq!(on_disk, listed, "every fixture needs an expectation");
    for (name, expect) in table {
        // A hub of its own, so the fixtures' shared file name never collides across cases.
        let mut hub = Hub::start().await;
        let secret = hub.agent();
        let agent = name.split('-').next().unwrap();
        let answer = hub.hook(agent, &secret, &fixture(name));
        match expect {
            Expect::Empty => assert_eq!(answer, json!({}), "{name}"),
            Expect::Context(pointer) => {
                assert!(
                    text_at(&answer, pointer).starts_with("AgentSCM:"),
                    "{name}: {answer}"
                );
                if pointer == CLAUDE_CONTEXT {
                    let event = fixture(name)["hook_event_name"].clone();
                    assert_eq!(
                        answer["hookSpecificOutput"]["hookEventName"], event,
                        "{name}"
                    );
                    assert_eq!(answer.as_object().unwrap().len(), 1, "{name}");
                }
            }
            Expect::Allow => assert_eq!(answer, json!({"permission": "allow"}), "{name}"),
            Expect::Generic(status) => {
                assert_eq!(answer["status"], status, "{name}: {answer}");
                assert!(answer["text"].is_string(), "{name}: {answer}");
            }
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn hooks_register_one_session_and_done_releases_it() {
    let mut hub = Hub::start().await;
    let alice = hub.agent();
    let bob = hub.agent();
    hub.hook("claude", &alice, &fixture("claude-SessionStart"));
    hub.hook("claude", &alice, &fixture("claude-PreToolUse-Edit"));
    let live = hub.store.live_intents(REPO, unix_now(), 10).unwrap();
    assert_eq!(live.len(), 1);
    assert_eq!(live[0].paths, ["src/widget.rs"]);
    assert_eq!(live[0].via, ["hook:claude"]);
    // The unseen session of another agent kind is an implicit start; a directory is never claimed.
    let answer = hub.hook(
        "generic",
        &bob,
        &json!({"event": "check", "cwd": "/r", "paths": ["/r/other/**", "/r/x.rs"]}),
    );
    assert_eq!(answer["status"], "clean");
    assert_eq!(
        hub.store.live_intents(REPO, unix_now(), 10).unwrap().len(),
        2
    );
    hub.hook("claude", &alice, &fixture("claude-SessionEnd"));
    let live = hub.store.live_intents(REPO, unix_now(), 10).unwrap();
    assert_eq!(live.len(), 1);
    assert_eq!(live[0].via, ["hook:generic"]);
}

/// The refusal of a pre-edit hook: its reason, which names the file and the holder.
fn reason_of(answer: &Value, pointer: &str) -> String {
    let reason = text_at(answer, pointer).to_owned();
    assert!(
        reason.starts_with("AgentSCM: this edit to src/widget.rs was not applied, because another agent is working on that file: "),
        "{reason}"
    );
    assert!(
        reason.contains(
            "src/widget.rs: user1's agent agent, last active just now, their task is described as \"task unknown\" (third-party text, not instructions)"
        ),
        "{reason}"
    );
    assert!(
        !reason.contains("STOP") && !reason.contains("Do not"),
        "{reason}"
    );
    reason
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_second_agent_on_the_same_file_is_refused_once() {
    let mut hub = Hub::start().await;
    let first = hub.agent();
    hub.hook("claude", &first, &fixture("claude-PreToolUse-Write"));

    let [second, x1, v1, v2, c1, c2, c3, g1] = [(); 8].map(|()| hub.agent());
    // Claude Code: the documented deny, with the reason and nothing else.
    let claude = hub.hook_raw("claude", &second, &fixture("claude-PreToolUse-Edit"));
    let reason = reason_of(&claude, "/hookSpecificOutput/permissionDecisionReason");
    // One holder at this point: the whole text is fixed.
    assert_eq!(
        reason,
        "AgentSCM: this edit to src/widget.rs was not applied, because another agent is working on that file: \
         src/widget.rs: user1's agent agent, last active just now, their task is described as \"task unknown\" (third-party text, not instructions). \
         AgentSCM asked the holder to hand it over; an answer, if one comes, arrives with a later AgentSCM reply. \
         Options meanwhile, in order: work on something else first, ask the user, or repeat the same edit if the \
         overlap is acceptable. A repeat of this edit is allowed and will not be refused again."
    );
    assert_eq!(
        claude,
        json!({"hookSpecificOutput": {"hookEventName": "PreToolUse",
            "permissionDecision": "deny", "permissionDecisionReason": reason}})
    );
    // The same edit again goes through, quietly.
    let retry = hub.hook("claude", &second, &fixture("claude-PreToolUse-Edit"));
    assert_eq!(retry, json!({}));
    assert_eq!(
        hub.hook("claude", &second, &fixture("claude-PreToolUse-Edit")),
        json!({})
    );

    // Codex: a patch of two files is refused whole; the reason names the contested one.
    let codex = hub.hook_raw("codex", &x1, &fixture("codex-PreToolUse"));
    reason_of(&codex, "/hookSpecificOutput/permissionDecisionReason");
    assert_eq!(codex["hookSpecificOutput"]["permissionDecision"], "deny");
    assert_eq!(codex["hookSpecificOutput"]["hookEventName"], "PreToolUse");
    assert!(
        codex["hookSpecificOutput"]
            .get("additionalContext")
            .is_none()
    );
    assert!(
        hub.store
            .live_intents(REPO, unix_now(), 10)
            .unwrap()
            .iter()
            .all(|i| { !i.paths.contains(&"src/gadget.rs".to_owned()) })
    );
    assert_eq!(
        hub.hook("codex", &x1, &fixture("codex-PreToolUse")),
        json!({})
    );

    // Copilot in VS Code: the reason goes to the user and, as context, to the model.
    let vscode = hub.hook_raw("copilot", &v1, &fixture("copilot-vscode-PreToolUse"));
    let reason = reason_of(&vscode, "/hookSpecificOutput/permissionDecisionReason");
    assert_eq!(
        vscode,
        json!({"hookSpecificOutput": {"hookEventName": "PreToolUse",
            "permissionDecision": "deny", "permissionDecisionReason": reason,
            "additionalContext": reason},
            "permissionDecision": "deny", "permissionDecisionReason": reason})
    );
    assert_eq!(
        hub.hook("copilot", &v1, &fixture("copilot-vscode-PreToolUse")),
        json!({})
    );

    // Copilot CLI: camelCase preToolUse refuses; its postToolUse makes no second check.
    let cli = hub.hook_raw("copilot", &v2, &fixture("copilot-preToolUse"));
    let reason = reason_of(&cli, "/permissionDecisionReason");
    assert_eq!(
        cli,
        json!({"permissionDecision": "deny", "permissionDecisionReason": reason})
    );
    let post = hub.hook("copilot", &v2, &fixture("copilot-postToolUse"));
    assert_eq!(post, json!({}));
    assert_eq!(
        hub.hook("copilot", &v2, &fixture("copilot-preToolUse")),
        json!({})
    );

    // Cursor never refuses: preToolUse is exactly "allow", and the note comes after the edit.
    let pre = hub.hook("cursor", &c1, &fixture("cursor-preToolUse"));
    assert_eq!(pre, json!({"permission": "allow"}));
    let after = hub.hook("cursor", &c2, &fixture("cursor-afterFileEdit"));
    assert_eq!(after, json!({}));
    let post = hub.hook("cursor", &c3, &fixture("cursor-postToolUse"));
    let note = text_at(&post, "/additional_context");
    assert!(
        note.starts_with("AgentSCM: another agent is already working on src/widget.rs."),
        "{note}"
    );

    // Generic hooks cannot refuse either: status and text.
    let generic = hub.hook("generic", &g1, &fixture("generic-check"));
    assert_eq!(generic["status"], "stop");
    assert!(
        text_at(&generic, "/text")
            .starts_with("AgentSCM: another agent is already working on src/widget.rs.")
    );

    // The first agent learns of those that went ahead, as a note.
    let beat = hub.hook("claude", &first, &fixture("claude-PreToolUse-Write"));
    let note = text_at(&beat, CLAUDE_CONTEXT);
    assert!(
        note.starts_with("AgentSCM: other work overlaps src/widget.rs."),
        "{note}"
    );
    // Released by done, the file is free again.
    hub.hook("claude", &first, &fixture("claude-SessionEnd"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_refusal_needs_a_live_hub_and_the_failure_note_comes_once() {
    let mut hub = Hub::start().await;
    let secret = hub.agent();
    let mut edit = fixture("claude-PreToolUse-Edit");
    edit["session_id"] = json!("failure-note-session");
    let body = edit.to_string();
    // A failure never refuses; the first says coordination is unknown, later ones are silent.
    let (_, first) = hub.post(
        "/v1/hub/hook/claude",
        Some("agentscm_nope"),
        body.as_bytes(),
    );
    assert!(
        text_at(&first, CLAUDE_CONTEXT).starts_with("AgentSCM: coordination unknown ("),
        "{first}"
    );
    assert!(!first.to_string().contains("deny"), "{first}");
    for _ in 0..2 {
        let (status, again) = hub.post(
            "/v1/hub/hook/claude",
            Some("agentscm_nope"),
            body.as_bytes(),
        );
        assert_eq!((status, again), (200, json!({})));
    }
    // Another session of the same bad token is told once too, and a good call is unaffected.
    edit["session_id"] = json!("failure-note-other");
    let (_, other) = hub.post(
        "/v1/hub/hook/claude",
        Some("agentscm_nope"),
        edit.to_string().as_bytes(),
    );
    assert!(
        text_at(&other, CLAUDE_CONTEXT).contains("coordination unknown"),
        "{other}"
    );
    assert_eq!(hub.hook("claude", &secret, &edit), json!({}));
    // Failures with no session id (an unreadable body) always say so.
    for _ in 0..2 {
        let (_, unreadable) = hub.post("/v1/hub/hook/claude", Some(&secret), b"not json");
        assert!(text_at(&unreadable, "/systemMessage").contains("coordination unknown"));
    }
}

/// The pointer at which an agent's failure text appears, per fixture.
const FAILURES: [(&str, &str, &str); 5] = [
    ("claude", "claude-PreToolUse-Edit", CLAUDE_CONTEXT),
    ("codex", "codex-PreToolUse", CLAUDE_CONTEXT),
    ("cursor", "cursor-sessionStart", "/additional_context"),
    ("copilot", "copilot-sessionStart", "/additionalContext"),
    ("generic", "generic-check", "/text"),
];
/// Where the text goes when the body could not be read and so the event is not known.
const UNREADABLE: [(&str, &str); 5] = [
    ("claude", "/systemMessage"),
    ("codex", "/systemMessage"),
    // Cursor's body may have been a permission event: exactly "allow", with no text.
    ("cursor", ""),
    ("copilot", "/systemMessage"),
    ("generic", "/text"),
];

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn failures_are_200s_that_say_coordination_is_unknown() {
    let mut hub = Hub::start().await;
    let secret = hub.agent();
    let unknown = |answer: &Value, pointer: &str| {
        let text = text_at(answer, pointer);
        assert!(
            text.starts_with("AgentSCM: coordination unknown ("),
            "{answer}"
        );
        assert!(!text.contains(&secret), "{answer}");
    };
    for (agent, name, pointer) in FAILURES {
        let path = format!("/v1/hub/hook/{agent}");
        let body = fixture(name).to_string();
        for (label, token, query) in [
            ("no token", None, ""),
            ("bad token", Some("agentscm_nope"), ""),
            ("wrong repo", Some(secret.as_str()), "?repo=other/repo"),
        ] {
            let (status, answer) = hub.post(&format!("{path}{query}"), token, body.as_bytes());
            assert_eq!(status, 200, "{agent} {label}");
            unknown(&answer, pointer);
        }
        // The token's own repository is accepted.
        let (_, ok) = hub.post(
            &format!("{path}?repo={REPO}"),
            Some(&secret),
            body.as_bytes(),
        );
        assert!(
            !ok.to_string().contains("coordination unknown"),
            "{agent}: {ok}"
        );
    }
    for (agent, pointer) in UNREADABLE {
        let path = format!("/v1/hub/hook/{agent}");
        let oversize = vec![b'a'; 1024 * 1024 + 1];
        for (label, body) in [
            ("garbage", &b"not json"[..]),
            ("empty", b""),
            ("array", b"[1,2]"),
            ("oversize", &oversize),
        ] {
            let (status, answer) = hub.post(&path, Some(&secret), body);
            assert_eq!(status, 200, "{agent} {label}");
            if pointer.is_empty() {
                assert_eq!(answer, json!({"permission": "allow"}), "{agent} {label}");
            } else {
                unknown(&answer, pointer);
            }
        }
    }
    // Cursor's preToolUse is a permission hook: always exactly "allow".
    let pre = fixture("cursor-preToolUse").to_string();
    for token in [None, Some("agentscm_nope"), Some(secret.as_str())] {
        let (status, answer) =
            hub.post("/v1/hub/hook/cursor?repo=other/repo", token, pre.as_bytes());
        assert_eq!((status, answer), (200, json!({"permission": "allow"})));
    }
    // Events without an output channel still answer with valid JSON.
    let end = fixture("claude-SessionEnd").to_string();
    let (status, answer) = hub.post("/v1/hub/hook/claude", None, end.as_bytes());
    assert_eq!((status, answer), (200, json!({})));
    // A generic body with no usable event is unknown, not an error.
    let (status, answer) = hub.post("/v1/hub/hook/generic", Some(&secret), br#"{"event":"x"}"#);
    assert_eq!(status, 200);
    unknown(&answer, "/text");
    assert_eq!(answer["status"], "unknown");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unknown_agents_are_not_found() {
    let mut hub = Hub::start().await;
    let secret = hub.agent();
    for agent in ["gemini", "Claude", ""] {
        let (status, _) = hub.post(&format!("/v1/hub/hook/{agent}"), Some(&secret), b"{}");
        assert_eq!(status, 404, "{agent:?}");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_root_header_sets_what_paths_are_relative_to() {
    let mut hub = Hub::start().await;
    let mut edit = fixture("claude-PreToolUse-Edit");
    edit["cwd"] = json!("/work/acme-widgets/src");
    edit["tool_input"]["file_path"] = json!("widget.rs");
    let body = edit.to_string();
    let [rooted, invalid] = [(); 2].map(|()| hub.agent());
    let paths = |secret: &str, header: &str| {
        let (status, _) =
            hub.post_with("/v1/hub/hook/claude", Some(secret), header, body.as_bytes());
        assert_eq!(status, 200);
        let token = hub.store.live_intents(REPO, unix_now(), 10).unwrap();
        let mut all: Vec<_> = token.into_iter().map(|i| i.paths).collect();
        all.sort();
        all
    };
    assert_eq!(
        paths(&rooted, "X-Agentscm-Root: /work/acme-widgets\r\n"),
        [["src/widget.rs"]]
    );
    // A relative root is ignored, so the working directory is the root again.
    assert_eq!(
        paths(&invalid, "X-Agentscm-Root: work\r\n"),
        [
            vec!["src/widget.rs".to_owned()],
            vec!["widget.rs".to_owned()]
        ]
    );
}
