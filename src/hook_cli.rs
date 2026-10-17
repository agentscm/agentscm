//! `agentscm hook <agent>`: the client for command-hook agents. See docs/agents.md.
//!
//! Advisory by construction: whatever goes wrong, the process prints valid JSON for that agent
//! and exits 0, because some agents treat a non-zero exit or invalid JSON as "block the action".
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use agentscm::hub::forge_http::{self, check_base_url, send};
use clap::Args;
use serde_json::{Value, json};

use crate::hub_cli::config_dir;
use crate::intent_cli::{HubFile, parse_hub_file, repo_root, token_for, valid_repo};

const LIMIT: Duration = Duration::from_secs(3);
const STDIN_BYTES: usize = 4 << 20;
const REPLY_BYTES: usize = 1 << 20;

#[derive(Debug, Args)]
pub struct HookArgs {
    #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
    pub args: Vec<String>,
}

pub async fn run(args: HookArgs) {
    run_unparsed(args.args).await
}

/// Never returns: prints the agent's reply and exits 0.
pub async fn run_unparsed(args: Vec<String>) -> ! {
    let (agent, flag_url, repo) = parse_args(&args);
    let reply = hook(&agent, flag_url, repo).await;
    let mut out = std::io::stdout();
    // Ignore write errors (a closed pipe must not panic or change the exit code).
    let _ = writeln!(out, "{reply}");
    let _ = out.flush();
    std::process::exit(0)
}

/// The agent name (first argument that is not a flag), `--hub-url` and `--repo` (kept only when
/// it is a valid `owner/name`). Unknown flags are ignored.
fn parse_args(args: &[String]) -> (String, Option<String>, Option<String>) {
    let mut agent = None;
    let mut url = None;
    let mut repo = None;
    let mut rest = args.iter();
    while let Some(arg) = rest.next() {
        if arg == "--hub-url" {
            url = rest.next().cloned();
        } else if let Some(value) = arg.strip_prefix("--hub-url=") {
            url = Some(value.to_owned());
        } else if arg == "--repo" {
            repo = rest.next().cloned();
        } else if let Some(value) = arg.strip_prefix("--repo=") {
            repo = Some(value.to_owned());
        } else if !arg.starts_with('-') && agent.is_none() {
            agent = Some(arg.clone());
        }
    }
    (
        agent.unwrap_or_default(),
        url,
        repo.filter(|r| valid_repo(r)),
    )
}

async fn hook(agent: &str, flag_url: Option<String>, repo: Option<String>) -> Value {
    let deadline = Instant::now() + LIMIT;
    let read = tokio::time::timeout(LIMIT, tokio::task::spawn_blocking(read_stdin)).await;
    let Ok(Ok(Some(body))) = read else {
        return unknown_output(agent, &Value::Null, "payload unreadable");
    };
    let payload: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
    let remaining = deadline.saturating_duration_since(Instant::now());
    let call = tokio::spawn(call_hub(
        agent.to_owned(),
        flag_url,
        repo,
        payload.clone(),
        body,
    ));
    match tokio::time::timeout(remaining, call).await {
        Ok(Ok(Ok(reply))) => reply,
        Ok(Ok(Err(reason))) => unknown_output(agent, &payload, &reason),
        Ok(Err(_)) => unknown_output(agent, &payload, "bad reply"),
        Err(_) => unknown_output(agent, &payload, "hub unreachable"),
    }
}

/// The whole of stdin, or `None` when it is over the cap or unreadable.
fn read_stdin() -> Option<Vec<u8>> {
    let mut body = Vec::new();
    let limit = STDIN_BYTES as u64 + 1;
    std::io::stdin().take(limit).read_to_end(&mut body).ok()?;
    (body.len() <= STDIN_BYTES).then_some(body)
}

async fn call_hub(
    agent: String,
    flag_url: Option<String>,
    repo: Option<String>,
    payload: Value,
    body: Vec<u8>,
) -> Result<Value, String> {
    let env = |name| std::env::var(name).ok().filter(|v| !v.trim().is_empty());
    let settings = Settings {
        url: env("AGENTSCM_HUB_URL"),
        token: env("AGENTSCM_HUB_TOKEN"),
        token_file: env("AGENTSCM_HUB_TOKEN_FILE"),
    };
    let file = config_dir()
        .ok()
        .and_then(|dir| std::fs::read_to_string(dir.join("agentscm").join("hub.json")).ok())
        .and_then(|text| parse_hub_file(&text).ok());
    let (url, token) = resolve(
        &settings,
        flag_url.as_deref(),
        &cwd_of(&payload),
        file.as_ref(),
    )?;
    let token = match token {
        Token::Value(token) => token,
        Token::File(path) => std::fs::read_to_string(path)
            .map_err(|_| "no token")?
            .trim()
            .to_owned(),
    };
    if token.is_empty() {
        return Err("no token".into());
    }
    let client = forge_http::client().map_err(|_| "hub unreachable")?;
    // The hub answers "coordination unknown" when the repository differs from the token's.
    let query = repo.map_or_else(String::new, |r| format!("?repo={r}"));
    let mut request = client
        .post(format!("{url}/v1/hub/hook/{}{query}", path_segment(&agent)))
        .header(reqwest::header::CONTENT_TYPE, "application/json")
        .bearer_auth(token)
        .body(body);
    if let Some(root) = root_header(&cwd_of(&payload)) {
        request = request.header("X-Agentscm-Root", root);
    }
    // `send` errors carry no URL or credentials.
    let (status, reply) = send(request, REPLY_BYTES)
        .await
        .map_err(|_| "hub unreachable")?;
    match status {
        200 if reply.is_object() => Ok(defanged(
            reply,
            &agent,
            &payload,
            config_dir().ok().as_deref(),
        )),
        200 => Err("bad reply".into()),
        401 | 403 => Err("token rejected".into()),
        _ => Err(format!("hub error {status}")),
    }
}

const PAIR: [&str; 2] = ["permissionDecision", "permissionDecisionReason"];
type Object = serde_json::Map<String, Value>;

/// The hub's reply cut down to an allowlist: context and message fields, Cursor's `permission`
/// (forced to allow) and the one documented refusal, `permissionDecision` "deny" with a
/// non-empty reason. A deny passes only on a pre-tool event and once per identical edit (a
/// repeat is the agent's retry); the rest is dropped, so a wrong or hostile reply cannot
/// block, approve or rewrite a call.
fn defanged(reply: Value, agent: &str, payload: &Value, config: Option<&Path>) -> Value {
    let Value::Object(mut top) = reply else {
        return reply;
    };
    top.retain(|key, _| {
        PAIR.contains(&key.as_str())
            || [
                "hookSpecificOutput",
                "additionalContext",
                "additional_context",
                "systemMessage",
                "permission",
            ]
            .contains(&key.as_str())
    });
    if top.contains_key("permission") {
        top.insert("permission".into(), json!("allow"));
    }
    if !top.get("hookSpecificOutput").is_none_or(Value::is_object) {
        top.remove("hookSpecificOutput");
    }
    let mut inner = top
        .get_mut("hookSpecificOutput")
        .and_then(Value::as_object_mut);
    if let Some(inner) = inner.as_deref_mut() {
        inner.retain(|key, _| {
            PAIR.contains(&key.as_str())
                || ["hookEventName", "additionalContext"].contains(&key.as_str())
        });
    }
    let wants = inner.as_deref().is_some_and(denies);
    let wants = wants || denies(&top);
    let keep = wants && is_pre_tool(payload) && first_deny(config, agent, payload);
    if let Some(inner) = top
        .get_mut("hookSpecificOutput")
        .and_then(Value::as_object_mut)
    {
        settle(inner, keep);
    }
    settle(&mut top, keep);
    Value::Object(top)
}

fn denies(object: &Object) -> bool {
    object.get(PAIR[0]).and_then(Value::as_str) == Some("deny")
        && object
            .get(PAIR[1])
            .and_then(Value::as_str)
            .is_some_and(|reason| !reason.is_empty())
}

/// Keeps a valid deny pair when `keep`, else removes the pair.
fn settle(object: &mut Object, keep: bool) {
    if !(keep && denies(object)) {
        PAIR.iter().for_each(|key| _ = object.remove(*key));
    }
}

/// A before-the-tool event: Claude/Codex/Copilot name it, Copilot CLI's camelCase payload has
/// a tool name and no result.
fn is_pre_tool(payload: &Value) -> bool {
    match payload["hook_event_name"].as_str() {
        Some(name) => matches!(name, "PreToolUse" | "preToolUse"),
        None => payload.get("toolName").is_some() && payload.get("toolResult").is_none(),
    }
}

/// True the first time this exact edit of the session is refused. Any missing session id or
/// filesystem error passes the deny: the hub already limits refusals.
fn first_deny(config: Option<&Path>, agent: &str, payload: &Value) -> bool {
    let (Some(config), Some(session)) = (config, session_of(payload)) else {
        return true;
    };
    // Object keys are sorted (serde_json without preserve_order), so this is canonical.
    let input = payload.get("tool_input").or(payload.get("toolArgs"));
    first_marker(
        config,
        &format!(
            "deny\0{agent}\0{session}\0{}",
            input.unwrap_or(&Value::Null)
        ),
    )
}

/// The agent name is argv, so keep it to a safe single path segment.
fn path_segment(agent: &str) -> String {
    let safe = agent
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
    if safe && !agent.is_empty() {
        agent.to_owned()
    } else {
        "generic".to_owned()
    }
}

/// The Git root of `cwd` as a header value, or `None` (no `.git`, or not valid UTF-8 / header text).
fn root_header(cwd: &Path) -> Option<reqwest::header::HeaderValue> {
    let root = repo_root(cwd)?;
    reqwest::header::HeaderValue::from_str(root.to_str()?).ok()
}

/// Where the agent is working: the payload's `cwd`, else its first workspace root, else here.
fn cwd_of(payload: &Value) -> PathBuf {
    let first_root = payload["workspace_roots"][0].as_str();
    payload["cwd"].as_str().or(first_root).map_or_else(
        || std::env::current_dir().unwrap_or_default(),
        PathBuf::from,
    )
}

struct Settings {
    url: Option<String>,
    token: Option<String>,
    token_file: Option<String>,
}

#[derive(Clone, Debug, PartialEq)]
enum Token {
    Value(String),
    File(PathBuf),
}

/// The hub URL and where its token comes from. A token stored in hub.json goes only to the hub
/// that file names (or the one `AGENTSCM_HUB_URL` names): a checked-in `--hub-url` must not be
/// able to redirect it.
fn resolve(
    env: &Settings,
    flag_url: Option<&str>,
    cwd: &Path,
    file: Option<&HubFile>,
) -> Result<(String, Token), &'static str> {
    let raw = env
        .url
        .as_deref()
        .or(flag_url)
        .or(file.map(|f| f.url.as_str()))
        .ok_or("no hub configured")?;
    let url = check_base_url(raw).map_err(|_| "bad hub url")?;
    if let Some(token) = &env.token {
        return Ok((url, Token::Value(token.clone())));
    }
    if let Some(path) = &env.token_file {
        return Ok((url, Token::File(PathBuf::from(path))));
    }
    let file = file.ok_or("no token")?;
    let stored = check_base_url(&file.url).ok();
    if env.url.is_none() && stored.as_deref() != Some(url.as_str()) {
        return Err("no token for this hub; set AGENTSCM_HUB_TOKEN");
    }
    let path = token_for(file, cwd);
    if !path.is_absolute() {
        return Err("no token");
    }
    Ok((url, Token::File(path)))
}

/// What the agent gets when the hub cannot be consulted: valid JSON that never blocks. The note
/// is printed once per (agent, session); later calls in that session get the silent no-op.
fn unknown_output(agent: &str, payload: &Value, reason: &str) -> Value {
    let note = match (session_of(payload), config_dir()) {
        (Some(session), Ok(dir)) => first_note(&dir, agent, session),
        _ => true,
    };
    fail_open(agent, payload, note.then_some(reason))
}

/// The payload's session id, under the names the supported agents use.
fn session_of(payload: &Value) -> Option<&str> {
    ["session_id", "conversation_id", "sessionId"]
        .into_iter()
        .filter_map(|key| payload[key].as_str())
        .find(|id| !id.is_empty())
}

/// Whether to print the note: true the first time this (agent, session) is seen. The marker is a
/// file named by a hash, so no session id or agent text is written. Any filesystem error prints.
fn first_note(config: &Path, agent: &str, session: &str) -> bool {
    first_marker(config, &format!("{agent}\0{session}"))
}

/// True the first time `key` is seen, remembered as a marker file named by its hash.
fn first_marker(config: &Path, key: &str) -> bool {
    use sha2::{Digest, Sha256};
    const KEEP: Duration = Duration::from_secs(2 * 24 * 60 * 60);
    let hash = Sha256::digest(key);
    let dir = config.join("agentscm").join("hook-notes");
    let mut builder = std::fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    std::os::unix::fs::DirBuilderExt::mode(&mut builder, 0o700);
    let made = builder.create(&dir);
    if made.is_err() {
        return true;
    }
    let marker = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(dir.join(format!("{hash:x}")));
    if let Err(e) = &marker {
        return e.kind() != std::io::ErrorKind::AlreadyExists;
    }
    for entry in std::fs::read_dir(&dir).into_iter().flatten().flatten() {
        let old = (entry.metadata().and_then(|m| m.modified()))
            .ok()
            .and_then(|time| time.elapsed().ok())
            .is_some_and(|age| age > KEEP);
        if old {
            let _ = std::fs::remove_file(entry.path());
        }
    }
    true
}

/// The reply for an unreachable hub; `note` is the reason to show, or `None` for the silent form.
fn fail_open(agent: &str, payload: &Value, note: Option<&str>) -> Value {
    let event = payload["hook_event_name"].as_str();
    let text = |reason| {
        format!(
            "AgentSCM: coordination unknown ({reason}). Other agents' work on these files is not visible."
        )
    };
    match (agent, event, note) {
        ("claude" | "codex" | "copilot", Some(name @ ("PreToolUse" | "SessionStart")), Some(r)) => {
            json!({"hookSpecificOutput": {"hookEventName": name, "additionalContext": text(r)}})
        }
        // Cursor blocks on invalid JSON, so a tool call it did not name an event for still
        // gets an explicit allow.
        ("cursor", Some("preToolUse"), _) => json!({"permission": "allow"}),
        ("cursor", Some("sessionStart" | "postToolUse"), Some(r)) => {
            json!({"additional_context": text(r)})
        }
        ("cursor", None, _) if !payload.is_object() || payload.get("tool_name").is_some() => {
            json!({"permission": "allow"})
        }
        _ => json!({}),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| (*s).to_owned()).collect()
    }

    fn pre(session: &str, input: Value) -> Value {
        json!({"hook_event_name": "PreToolUse", "session_id": session, "tool_input": input})
    }

    fn deny(reason: &str) -> Value {
        json!({"hookSpecificOutput": {"hookEventName": "PreToolUse",
            "permissionDecision": "deny", "permissionDecisionReason": reason}})
    }

    #[test]
    fn a_hub_reply_is_cut_to_an_allowlist() {
        let dir = tempfile::TempDir::new().unwrap();
        let cut = |reply| defanged(reply, "claude", &pre("s", json!({})), Some(dir.path()));
        let reply = cut(json!({
            "permission": "deny", "continue": false, "decision": "block", "stopReason": "x",
            "updatedInput": {}, "updated_input": {}, "followup_message": "x",
            "modifiedArgs": {}, "updatedMCPToolOutput": {}, "systemMessage": "m",
            "additional_context": "keep", "additionalContext": "keep",
            "hookSpecificOutput": {"hookEventName": "PreToolUse", "permissionDecision": "deny",
                "updatedInput": {}, "updatedMCPToolOutput": {}, "modifiedArgs": {},
                "decision": "block", "additionalContext": "keep"}
        }));
        // The deny has no reason, so it goes too; `permission` is forced to allow.
        assert_eq!(
            reply,
            json!({"permission": "allow", "systemMessage": "m", "additional_context": "keep",
                "additionalContext": "keep",
                "hookSpecificOutput": {"hookEventName": "PreToolUse", "additionalContext": "keep"}})
        );
        // Cursor's permission is forced to allow even with a deny and a reason.
        let cursor = cut(json!({"permission": "deny", "agent_message": "r"}));
        assert_eq!(cursor, json!({"permission": "allow"}));
        assert_eq!(cut(json!(["x"])), json!(["x"]));
        assert_eq!(cut(json!({"hookSpecificOutput": "x"})), json!({}));
    }

    #[test]
    fn only_a_deny_with_a_reason_on_a_pre_tool_event_survives() {
        let dir = tempfile::TempDir::new().unwrap();
        let run = |reply, payload: &Value| defanged(reply, "claude", payload, Some(dir.path()));
        let bare = json!({"hookSpecificOutput": {"hookEventName": "PreToolUse"}});
        for (decision, reason) in [
            (json!("deny"), json!("")),
            (json!("deny"), Value::Null),
            (json!("deny"), json!(1)),
            (json!("ask"), json!("r")),
            (json!("allow"), json!("r")),
            (json!("defer"), json!("r")),
            (json!("Deny"), json!("r")),
        ] {
            let mut input = bare.clone();
            input["hookSpecificOutput"]["permissionDecision"] = decision.clone();
            input["hookSpecificOutput"]["permissionDecisionReason"] = reason.clone();
            assert_eq!(
                run(input, &pre("s", json!({}))),
                bare,
                "{decision} {reason}"
            );
        }
        // Not before a tool: dropped.
        let post = json!({"hook_event_name": "PostToolUse", "session_id": "s"});
        assert_eq!(run(deny("r"), &post), bare);
        let first = pre("s", json!({"file_path": "a.rs"}));
        assert_eq!(run(deny("r"), &first), deny("r"));
        // The repeat of the same edit passes; another edit or session is refused once.
        assert_eq!(run(deny("r"), &first), bare);
        assert_eq!(
            run(deny("r"), &pre("s", json!({"file_path": "b.rs"}))),
            deny("r")
        );
        assert_eq!(
            run(deny("r"), &pre("t", json!({"file_path": "a.rs"}))),
            deny("r")
        );
        // Copilot CLI's camelCase form is top level, with the tool args as the identity.
        let cli = json!({"sessionId": "c", "toolName": "edit", "toolArgs": "{\"p\":1}"});
        let top = json!({"permissionDecision": "deny", "permissionDecisionReason": "r"});
        assert_eq!(run(top.clone(), &cli), top);
        assert_eq!(run(top, &cli), json!({}));
        let result = json!({"toolName": "edit", "toolResult": {}});
        let ask = json!({"permissionDecision": "ask", "permissionDecisionReason": "r", "k": 1});
        assert_eq!(run(ask, &cli), json!({}));
        let again = json!({"permissionDecision": "deny", "permissionDecisionReason": "r"});
        assert_eq!(run(again, &result), json!({}));
        // No session id or no place for a marker: the deny passes, the hub limits refusals.
        let anon = json!({"hook_event_name": "PreToolUse", "tool_input": {}});
        assert_eq!(run(deny("r"), &anon), deny("r"));
        assert_eq!(run(deny("r"), &anon), deny("r"));
        let blocked = tempfile::NamedTempFile::new().unwrap();
        let once = |reply| defanged(reply, "claude", &first, Some(blocked.path()));
        assert_eq!(once(deny("r")), deny("r"));
        assert_eq!(once(deny("r")), deny("r"));
    }

    #[test]
    fn the_unknown_note_is_printed_once_per_agent_and_session() {
        let dir = tempfile::TempDir::new().unwrap();
        let first = |agent, session| first_note(dir.path(), agent, session);
        assert!(first("claude", "s1"));
        assert!(!first("claude", "s1"));
        assert!(first("claude", "s2"));
        assert!(first("codex", "s1"));
        let markers = dir.path().join("agentscm").join("hook-notes");
        let names: Vec<_> = std::fs::read_dir(&markers).unwrap().flatten().collect();
        assert_eq!(names.len(), 3);
        assert!(names.iter().all(|e| e.file_name().len() == 64));
        // A marker cannot be created: print the note rather than fail.
        let blocked = tempfile::NamedTempFile::new().unwrap();
        assert!(first_note(blocked.path(), "claude", "s1"));
        assert!(first_note(blocked.path(), "claude", "s1"));
    }

    #[test]
    fn a_repeat_note_is_the_agents_silent_no_op() {
        let pre = event("PreToolUse");
        let note = fail_open("claude", &pre, Some("hub unreachable"));
        assert!(note["hookSpecificOutput"]["additionalContext"].is_string());
        assert_eq!(fail_open("claude", &pre, None), json!({}));
        let cursor = event("preToolUse");
        assert_eq!(
            fail_open("cursor", &cursor, None),
            json!({"permission": "allow"})
        );
        let post = event("postToolUse");
        assert!(fail_open("cursor", &post, Some("no token"))["additional_context"].is_string());
        assert_eq!(fail_open("cursor", &post, None), json!({}));
        let id = |key: &str| json!({ key: "s" });
        for key in ["session_id", "conversation_id", "sessionId"] {
            assert_eq!(session_of(&id(key)), Some("s"));
        }
        assert_eq!(session_of(&json!({"session_id": ""})), None);
        assert_eq!(session_of(&Value::Null), None);
    }

    fn hub_file(url: &str, token_file: &str) -> HubFile {
        parse_hub_file(&json!({"url": url, "token_file": token_file}).to_string()).unwrap()
    }

    fn settings() -> Settings {
        Settings {
            url: None,
            token: None,
            token_file: None,
        }
    }

    fn abs(name: &str) -> String {
        std::env::temp_dir().join(name).display().to_string()
    }

    #[test]
    fn arguments_parse_leniently() {
        let parsed = |list: &[&str]| parse_args(&args(list));
        assert_eq!(parsed(&["claude"]), ("claude".into(), None, None));
        assert_eq!(
            parsed(&["codex", "--hub-url", "https://h.test"]),
            ("codex".into(), Some("https://h.test".into()), None)
        );
        assert_eq!(
            parsed(&["--hub-url=https://h.test", "cursor", "--wat", "x"]),
            ("cursor".into(), Some("https://h.test".into()), None)
        );
        assert_eq!(parsed(&["--hub-url"]), (String::new(), None, None));
        assert_eq!(parsed(&[]), (String::new(), None, None));
        let repo = |list: &[&str]| parsed(list).2;
        assert_eq!(
            repo(&["x", "--repo", "acme/widgets"]),
            Some("acme/widgets".into())
        );
        assert_eq!(
            repo(&["x", "--repo=acme/widgets"]),
            Some("acme/widgets".into())
        );
        for bad in ["acme", "a b/c", "a/b?x=1", "a//b", ""] {
            assert_eq!(repo(&["x", "--repo", bad]), None, "{bad}");
        }
    }

    #[test]
    fn agent_names_are_one_path_segment() {
        assert_eq!(path_segment("claude"), "claude");
        assert_eq!(path_segment("../x"), "generic");
        assert_eq!(path_segment(""), "generic");
    }

    #[test]
    fn nothing_configured_is_no_hub() {
        let cwd = Path::new(".");
        assert_eq!(
            resolve(&settings(), None, cwd, None),
            Err("no hub configured")
        );
        let flag = "https://hub.example.test";
        assert_eq!(resolve(&settings(), Some(flag), cwd, None), Err("no token"));
    }

    #[test]
    fn environment_wins() {
        let cwd = Path::new(".");
        let file = hub_file("https://stored.example.test/", &abs("stored-token"));
        let env = Settings {
            url: Some("https://env.example.test/".into()),
            token: Some("synthetic-token".into()),
            token_file: Some(abs("ignored")),
        };
        let want = (
            "https://env.example.test".into(),
            Token::Value("synthetic-token".into()),
        );
        assert_eq!(
            resolve(&env, Some("https://flag.example.test"), cwd, Some(&file)),
            Ok(want)
        );
        let env = Settings { token: None, ..env };
        let want = (
            "https://env.example.test".into(),
            Token::File(abs("ignored").into()),
        );
        assert_eq!(resolve(&env, None, cwd, Some(&file)), Ok(want));
        // AGENTSCM_HUB_URL names the hub, so the stored token may go there.
        let env = Settings {
            token_file: None,
            ..env
        };
        let want = (
            "https://env.example.test".into(),
            Token::File(abs("stored-token").into()),
        );
        assert_eq!(resolve(&env, None, cwd, Some(&file)), Ok(want));
    }

    #[test]
    fn stored_token_only_goes_to_the_stored_hub() {
        let cwd = Path::new(".");
        let file = hub_file("https://stored.example.test", &abs("stored-token"));
        let stored = (
            "https://stored.example.test".to_owned(),
            Token::File(abs("stored-token").into()),
        );
        assert_eq!(
            resolve(&settings(), None, cwd, Some(&file)),
            Ok(stored.clone())
        );
        // A trailing slash is the same hub.
        let flag = Some("https://stored.example.test/");
        assert_eq!(resolve(&settings(), flag, cwd, Some(&file)), Ok(stored));
        let other = Some("https://other.example.test");
        assert_eq!(
            resolve(&settings(), other, cwd, Some(&file)),
            Err("no token for this hub; set AGENTSCM_HUB_TOKEN")
        );
        // With an environment token the other hub is fine.
        let env = Settings {
            token: Some("synthetic-token".into()),
            ..settings()
        };
        assert!(resolve(&env, other, cwd, Some(&file)).is_ok());
    }

    #[test]
    fn bad_urls_and_relative_token_files_are_refused() {
        let cwd = Path::new(".");
        let env = Settings {
            token: Some("synthetic-token".into()),
            ..settings()
        };
        assert_eq!(
            resolve(&env, Some("http://example.test"), cwd, None),
            Err("bad hub url")
        );
        assert!(resolve(&env, Some("http://127.0.0.1:9"), cwd, None).is_ok());
        let file = hub_file("https://stored.example.test", "relative-token");
        assert_eq!(
            resolve(&settings(), None, cwd, Some(&file)),
            Err("no token")
        );
    }

    #[test]
    fn payload_directory_comes_from_cwd_then_workspace_roots() {
        assert_eq!(
            cwd_of(&json!({"cwd": "/a", "workspace_roots": ["/b"]})),
            PathBuf::from("/a")
        );
        assert_eq!(
            cwd_of(&json!({"workspace_roots": ["/b", "/c"]})),
            PathBuf::from("/b")
        );
        assert_eq!(
            cwd_of(&Value::Null),
            std::env::current_dir().unwrap_or_default()
        );
    }

    fn event(name: &str) -> Value {
        json!({"hook_event_name": name})
    }

    #[test]
    fn claude_codex_and_copilot_get_context_on_two_events() {
        for agent in ["claude", "codex", "copilot"] {
            for name in ["PreToolUse", "SessionStart"] {
                let out = unknown_output(agent, &event(name), "hub unreachable");
                assert_eq!(out["hookSpecificOutput"]["hookEventName"], name);
                let text = out["hookSpecificOutput"]["additionalContext"]
                    .as_str()
                    .unwrap();
                assert_eq!(
                    text,
                    "AgentSCM: coordination unknown (hub unreachable). \
                     Other agents' work on these files is not visible."
                );
                assert_eq!(out.as_object().unwrap().len(), 1);
            }
            for other in [
                event("SessionEnd"),
                event("preToolUse"),
                Value::Null,
                json!({}),
            ] {
                assert_eq!(unknown_output(agent, &other, "no token"), json!({}));
            }
        }
    }

    #[test]
    fn cursor_never_gets_invalid_or_blocking_output() {
        let out = |payload: &Value| unknown_output("cursor", payload, "no token");
        assert_eq!(out(&event("preToolUse")), json!({"permission": "allow"}));
        assert_eq!(
            out(&json!({"tool_name": "Write"})),
            json!({"permission": "allow"})
        );
        assert_eq!(out(&Value::Null), json!({"permission": "allow"}));
        for name in ["sessionStart", "postToolUse"] {
            let reply = out(&event(name));
            assert!(
                reply["additional_context"]
                    .as_str()
                    .unwrap()
                    .contains("coordination unknown (no token)")
            );
        }
        assert_eq!(out(&event("sessionEnd")), json!({}));
        assert_eq!(out(&json!({"conversation_id": "c"})), json!({}));
    }

    #[test]
    fn generic_and_unknown_agents_get_an_empty_object() {
        for agent in ["generic", "mystery", ""] {
            assert_eq!(
                unknown_output(agent, &event("PreToolUse"), "no token"),
                json!({})
            );
        }
    }
}
