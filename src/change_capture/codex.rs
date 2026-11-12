//! Codex capture: what Codex exposes, and the parts only Codex has.
//!
//! Codex hooks use Claude Code's event names and fields, so live events go through the shared hook
//! path in [`super::claude_code`] with [`AgentHarnessV1::Codex`](crate::change_record::AgentHarnessV1).
//! Live capture reads only what a hook payload carries. Here are the two facts that reach hooks in
//! a Codex-only shape (a turn's last assistant message, on `Stop`, and a model request's token
//! usage, on `PostModelRequest`) and the import of a session file, the only place Codex keeps the
//! rest of a session. Codex does not promise to keep that file's format, so it is read only when a
//! person names the file.

use std::io::BufRead;

use serde::Deserialize;
use serde_json::{Map, Value, json};

use super::claude_code::{
    CaptureReport, CaptureRequest, SelectedEvent, check_kind, identifier, import_history, tool_fact,
};
use super::{AppendOutcome, CaptureError, LocalSpool, Result};
use crate::change_record::{CaptureEnvelopeV1, CapturedEventKindV1, ToolCompletionV1};
use crate::checks::sha256;
use crate::hub::github::parse_rfc3339;
use crate::hub::insights::{
    AttemptOutcome, AttemptPhase, UsageBasis, UsageMeasurementScope, UsageRecord,
};

/// What capture can record for Codex. Checked against Codex CLI 0.167.1: its hook input schemas,
/// and the session files it and four earlier versions wrote.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Capabilities {
    /// `UserPromptSubmit` carries the prompt.
    pub user_prompts: bool,
    /// `Stop` carries the last assistant message of the turn.
    pub turn_final_assistant_messages: bool,
    /// No hook carries an assistant message written before the turn's last one.
    pub interim_assistant_messages: bool,
    pub tool_starts: bool,
    /// `PostToolUse` carries the tool's response.
    pub tool_finishes: bool,
    /// No hook says that a tool call failed; a finished call is recorded as returned.
    pub tool_failures: bool,
    /// Hosted tools, such as web search, run no hook.
    pub hosted_tool_calls: bool,
    /// `PostModelRequest` carries the token counts of one model request.
    pub request_token_usage: bool,
    /// `SessionEnd` fires, with no reason other than `other`.
    pub session_end: bool,
    /// Reasoning is stored encrypted and is never read.
    pub reasoning: bool,
    /// `import-codex` reads prompts, every assistant message, tool calls with their results and
    /// token usage from a session file.
    pub retrospective_visible_history: bool,
}

pub const CAPABILITIES: Capabilities = Capabilities {
    user_prompts: true,
    turn_final_assistant_messages: true,
    interim_assistant_messages: false,
    tool_starts: true,
    tool_finishes: true,
    tool_failures: false,
    hosted_tool_calls: false,
    request_token_usage: true,
    session_end: true,
    reasoning: false,
    retrospective_visible_history: true,
};

/// The Codex hook events capture reads.
pub const HOOK_EVENTS: [&str; 10] = [
    "SessionStart",
    "UserPromptSubmit",
    "PreToolUse",
    "PostToolUse",
    "PostModelRequest",
    "Stop",
    "PreCompact",
    "SubagentStart",
    "SubagentStop",
    "SessionEnd",
];

/// A `Stop` payload's last assistant message as the hook input the shared path records as an
/// assistant message. A turn that ends twice with the same message records it once.
pub fn last_message_hook(stop: &Map<String, Value>) -> Option<Vec<u8>> {
    let text = stop
        .get("last_assistant_message")
        .and_then(Value::as_str)
        .filter(|text| !text.trim().is_empty())?;
    let turn = stop.get("turn_id").and_then(Value::as_str).unwrap_or("");
    serde_json::to_vec(&json!({
        "hook_event_name": "MessageDisplay",
        "session_id": stop.get("session_id"),
        "message_id": format!("stop:{turn}:{}", sha256(text.as_bytes())),
        "delta": text,
        "index": 0,
        "final": true,
    }))
    .ok()
}

/// The usage observation in a `PostModelRequest` payload, when Codex reported token counts.
pub fn usage_from_hook(
    payload: &Map<String, Value>,
    request: &CaptureRequest,
) -> Option<UsageRecord> {
    usage_record(
        payload.get("turn_id")?.as_str()?,
        payload.get("model").and_then(Value::as_str),
        payload.get("token_usage")?,
        request,
    )
}

/// One model request's token counts. Codex counts cached input inside `input_tokens`; the record
/// keeps them apart, so cached input is taken out. Reasoning tokens are part of `output_tokens`
/// and are not kept separately.
///
/// The hook does not name the request, so the observation is identified by its turn and counts.
/// The hook and the session file then give one request the same identity, and a session recorded
/// live and imported later is not counted twice.
// shortcut: two requests of one turn with identical counts are recorded once; use the request's
// own identity when Codex puts one in the hook payload.
fn usage_record(
    turn_id: &str,
    model: Option<&str>,
    usage: &Value,
    request: &CaptureRequest,
) -> Option<UsageRecord> {
    let count = |key: &str| usage.get(key).and_then(Value::as_i64).filter(|n| *n >= 0);
    let (input, cached, output) = (
        count("input_tokens"),
        count("cached_input_tokens"),
        count("output_tokens"),
    );
    if !identifier(turn_id, 255) || (input.is_none() && cached.is_none() && output.is_none()) {
        return None;
    }
    let identity = [
        "input_tokens",
        "cached_input_tokens",
        "cache_write_input_tokens",
        "output_tokens",
        "reasoning_output_tokens",
        "total_tokens",
    ]
    .map(|key| format!("{:?}", count(key)))
    .join("\0");
    Some(UsageRecord {
        work_id: request.work_id.clone(),
        observation_id: format!(
            "codex-usage:{}",
            sha256(format!("{turn_id}\0{identity}").as_bytes())
        ),
        generation: request.generation,
        session: request.canonical_session_id.clone(),
        phase: AttemptPhase::Other,
        outcome: AttemptOutcome::Unknown,
        agent: "codex".into(),
        provider: "unknown".into(),
        model: model
            .filter(|model| identifier(model, 255))
            .unwrap_or("unknown")
            .into(),
        usage_basis: UsageBasis::ProviderResult,
        source_commit: None,
        // Unknown unless both counts are known and consistent.
        input_tokens: input
            .zip(cached)
            .and_then(|(input, cached)| input.checked_sub(cached))
            .filter(|uncached| *uncached >= 0),
        output_tokens: output,
        cache_read_input_tokens: cached,
        // Codex writes 0 when the provider reported none, so only a positive count is a fact.
        cache_creation_input_tokens: count("cache_write_input_tokens").filter(|n| *n > 0),
        action_id: None,
        attempt_id: None,
        provider_request_id: None,
        provider_event_id: None,
        measurement_scope: Some(UsageMeasurementScope::Request),
        cost: None,
    })
}

#[derive(Deserialize)]
struct RolloutLine {
    timestamp: Option<String>,
    ordinal: Option<u64>,
    #[serde(rename = "type")]
    kind: Option<String>,
    payload: Option<Value>,
}

/// Imports the visible portion of one Codex session file (`rollout-*.jsonl`): the person's
/// prompts, assistant messages, tool calls with their results, and each model request's token
/// usage. Reasoning, images and the instructions Codex adds around a prompt are not selected. A
/// subagent's file starts with a copy of its parent's history, which is left to the parent's own
/// file. Returns the report and the number of usage observations newly recorded.
pub fn import_rollout(
    input: impl BufRead,
    request: &CaptureRequest,
    spool: &LocalSpool,
) -> Result<(CaptureReport, usize)> {
    // Where the file's own records start, known once its first record named this session.
    let mut own_from = None;
    let mut model: Option<String> = None;
    let mut usage = Vec::new();
    let report = import_history(input, request, spool, |line, number| {
        let Ok(line) = serde_json::from_slice::<RolloutLine>(line) else {
            return Ok(None);
        };
        let (Some(kind), Some(payload)) = (line.kind.as_deref(), line.payload.as_ref()) else {
            return Ok(Some((Vec::new(), 0)));
        };
        let Some(own_from) = own_from else {
            if kind != "session_meta" || payload["id"] != request.observed_session_id.as_str() {
                return Err(CaptureError::Invalid(
                    "Codex history does not start with this session's record".into(),
                ));
            }
            own_from = Some(
                payload["subagent_history_start_ordinal"]
                    .as_u64()
                    .unwrap_or(0),
            );
            return Ok(Some((Vec::new(), 0)));
        };
        if line.ordinal.is_some_and(|ordinal| ordinal < own_from) {
            return Ok(Some((Vec::new(), 0)));
        }
        match kind {
            "turn_context" => model = payload["model"].as_str().map(str::to_owned),
            "token_usage_record" => {
                usage.extend(payload["turn_id"].as_str().and_then(|turn| {
                    usage_record(turn, model.as_deref(), &payload["usage"], request)
                }))
            }
            _ => {}
        }
        let Some(observed_at) = line.timestamp.as_deref().and_then(parse_rfc3339) else {
            return Ok(Some((Vec::new(), 0)));
        };
        // A record's place in the file names its events. Its own ids can repeat: Codex writes
        // several outputs for one call under one call id.
        let place = line
            .ordinal
            .map_or_else(|| format!("line{number}"), |ordinal| ordinal.to_string());
        Ok(Some(select(kind, payload, observed_at, &place)))
    })?;
    let mut recorded = 0;
    for record in usage {
        match spool.append(&CaptureEnvelopeV1::Usage(record)) {
            Ok(AppendOutcome::Stored) => recorded += 1,
            // Already recorded, here or for other Work: not counted again.
            Ok(AppendOutcome::Duplicate) | Err(CaptureError::Conflict(_)) => {}
            Err(error) => return Err(error),
        }
    }
    Ok((report, recorded))
}

/// The events of one of the session's own records, and how many private blocks it left out.
fn select(
    kind: &str,
    payload: &Value,
    observed_at: i64,
    place: &str,
) -> (Vec<SelectedEvent>, usize) {
    let event_id = |index: usize| format!("evt:rollout:{place}:{index}");
    let texts = |content: &Value| -> Vec<Value> {
        content
            .as_array()
            .into_iter()
            .flatten()
            .filter(|block| block["text"].is_string())
            .map(|block| block["text"].clone())
            .collect()
    };
    let messages = |item: &Value, kind| {
        texts(&item["content"])
            .into_iter()
            .enumerate()
            .map(|(index, text)| {
                SelectedEvent::history(
                    event_id(index),
                    observed_at,
                    kind,
                    None,
                    json!({"content": text, "phase": item["phase"]}),
                )
            })
            .collect()
    };
    let record = payload["type"].as_str().unwrap_or("");
    match (kind, record) {
        ("event_msg", "item_completed") => {
            let item = &payload["item"];
            match item["type"].as_str() {
                Some("UserMessage") => (messages(item, CapturedEventKindV1::UserPrompt), 0),
                Some("AgentMessage") => (messages(item, CapturedEventKindV1::AssistantMessage), 0),
                _ => (Vec::new(), 0),
            }
        }
        ("response_item", "function_call" | "custom_tool_call") => {
            let name = payload["name"].as_str();
            // A function call's arguments are JSON in a string; a custom tool's input is text.
            let input = payload["arguments"]
                .as_str()
                .and_then(|arguments| serde_json::from_str::<Value>(arguments).ok())
                .unwrap_or_else(|| payload["input"].clone());
            let call = SelectedEvent::history(
                event_id(0),
                observed_at,
                check_kind(CapturedEventKindV1::ToolStarted, name),
                Some(tool_fact(name, payload["call_id"].as_str(), None)),
                json!({
                    "tool_name": name,
                    "namespace": payload["namespace"],
                    "tool_input": input,
                }),
            );
            (vec![call], 0)
        }
        ("response_item", "function_call_output" | "custom_tool_call_output") => {
            // Text only: an output's images are not selected.
            let output = match &payload["output"] {
                Value::String(text) => json!(text),
                blocks => json!(texts(blocks)),
            };
            let finish = SelectedEvent::history(
                event_id(0),
                observed_at,
                CapturedEventKindV1::ToolFinished,
                Some(tool_fact(
                    None,
                    payload["call_id"].as_str(),
                    Some(ToolCompletionV1::Returned),
                )),
                json!({"tool_result": output}),
            );
            (vec![finish], 0)
        }
        ("response_item", "reasoning") => (Vec::new(), 1),
        _ => (Vec::new(), 0),
    }
}
