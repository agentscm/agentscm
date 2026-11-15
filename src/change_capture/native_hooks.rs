//! Bounded capture copies of native Cursor, Copilot and Gemini CLI hooks.
//!
//! These adapters grant no capture or sharing authority. Callers keep the original payload for
//! coordination and apply the usual target, sharing, retention and redaction checks to these
//! events. No transcript path, inherited model context or reasoning body is forwarded.

use std::io::{self, Write};

use serde_json::{Map, Value, json};

use super::claude_code::identifier;
use super::{CaptureError, Result};
use crate::change_record::AgentHarnessV1;
use crate::hub::github::parse_rfc3339;

const MAX_HOOK_BYTES: usize = 1024 * 1024;

/// Capture events use the shared hook shapes; native input is never modified.
#[derive(Clone, Debug, PartialEq)]
pub struct NormalizedHook {
    pub harness: AgentHarnessV1,
    pub session_id: String,
    pub events: Vec<Value>,
    /// A bounded model observation. Model hooks emit only a `SetupObserved` metadata event.
    pub model: Option<String>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Event {
    Start,
    Prompt,
    ToolStart,
    ToolFinish,
    ToolFailure,
    Reply,
    ReplyAndStop,
    Stop,
    End,
    Compact,
    Model,
}

impl Event {
    fn capture_name(self) -> &'static str {
        match self {
            Self::Start => "SessionStart",
            Self::Prompt => "UserPromptSubmit",
            Self::ToolStart => "PreToolUse",
            Self::ToolFinish => "PostToolUse",
            Self::ToolFailure => "PostToolUseFailure",
            Self::Reply | Self::ReplyAndStop => "MessageDisplay",
            Self::Stop => "Stop",
            Self::End => "SessionEnd",
            Self::Compact => "PreCompact",
            Self::Model => "SetupObserved",
        }
    }
}

/// Normalize only documented capture events from these three native clients.
///
/// Copilot's camelCase payload does not carry an event name, so its installed command must pass
/// the configured event as `event_hint`. A hint conflicting with the payload is rejected.
/// `None` means an unsupported agent/event, not successful capture. Invalid supported events
/// return an error without falling back to another host or session.
///
/// No additive usage is produced: Cursor/Copilot hooks expose none, while Gemini `AfterModel`
/// exposes uncorrelated streaming snapshots. Summing those as requests would overcount usage.
pub fn normalize(
    agent: &str,
    payload: &Value,
    event_hint: Option<&str>,
) -> Result<Option<NormalizedHook>> {
    let harness = match agent {
        "cursor" => AgentHarnessV1::Cursor,
        "copilot" => AgentHarnessV1::Copilot,
        "gemini" => AgentHarnessV1::GeminiCli,
        _ => return Ok(None),
    };
    bounded(payload)?;
    let object = payload
        .as_object()
        .ok_or_else(|| invalid("native hook input must be an object"))?;
    let named = optional_text(object, "hook_event_name")?;
    if named.zip(event_hint).is_some_and(|(a, b)| a != b) {
        return Err(invalid(
            "native hook event conflicts with the configured event",
        ));
    }
    let name = named
        .or(event_hint.filter(|_| harness == AgentHarnessV1::Copilot))
        .ok_or_else(|| invalid("native hook event name is missing"))?;
    let Some(event) = event_for(harness, name) else {
        return Ok(None);
    };
    refuse_child_payload(object)?;
    let camel_copilot = harness == AgentHarnessV1::Copilot
        && name.as_bytes().first().is_some_and(u8::is_ascii_lowercase);
    let session_key = match harness {
        AgentHarnessV1::Cursor => "conversation_id",
        AgentHarnessV1::Copilot if camel_copilot => "sessionId",
        _ => "session_id",
    };
    let session = required_identifier(object, session_key, 255)?;
    for key in ["session_id", "sessionId", "conversation_id"] {
        if optional_text(object, key)?.is_some_and(|value| value != session) {
            return Err(invalid("native hook session identities conflict"));
        }
    }
    let timestamp = timestamp(object, camel_copilot)?;
    let model = model(object, harness);
    let base = |name| {
        let mut event = json!({"hook_event_name": name, "session_id": session});
        if let Some(timestamp) = &timestamp {
            event["timestamp"] = json!(timestamp);
        }
        if let Some(model) = &model {
            event["model"] = json!(model);
        }
        event
    };
    let mut events = Vec::new();
    if event != Event::Model || model.is_some() {
        let mut normalized = base(event.capture_name());
        match event {
            Event::Start => {
                if let Some(source) = optional_text(object, "source")? {
                    let source = match source {
                        "new" if harness == AgentHarnessV1::Copilot => "startup",
                        source => source,
                    };
                    if matches!(source, "startup" | "resume" | "clear" | "compact") {
                        normalized["source"] = json!(source);
                    }
                }
            }
            Event::End => {
                if let Some(reason) = optional_text(object, "reason")? {
                    normalized["reason"] = json!(match reason {
                        "clear" | "logout" | "prompt_input_exit" => reason,
                        _ => "other",
                    });
                }
            }
            Event::Prompt => normalized["prompt"] = json!(required_text(object, "prompt")?),
            Event::Reply | Event::ReplyAndStop => {
                let key = if harness == AgentHarnessV1::Cursor {
                    "text"
                } else {
                    "prompt_response"
                };
                normalized["delta"] = json!(required_text(object, key)?);
                normalized["final"] = json!(true);
                // Cursor's generation_id names a user generation, not an assistant message.
                // With no genuine message ID, leave occurrence handling to the capture store.
            }
            Event::ToolStart | Event::ToolFinish | Event::ToolFailure => {
                tool_event(object, harness, camel_copilot, event, &mut normalized)?;
            }
            Event::Stop | Event::Compact | Event::Model => {}
        }
        bounded(&normalized)?;
        events.push(normalized);
        if event == Event::ReplyAndStop {
            events.push(base("Stop"));
        }
    }
    Ok(Some(NormalizedHook {
        harness,
        session_id: session.to_owned(),
        events,
        model,
    }))
}

fn event_for(harness: AgentHarnessV1, name: &str) -> Option<Event> {
    Some(match (harness, name) {
        (AgentHarnessV1::Cursor, "sessionStart")
        | (AgentHarnessV1::Copilot, "sessionStart" | "SessionStart")
        | (AgentHarnessV1::GeminiCli, "SessionStart") => Event::Start,
        (AgentHarnessV1::Cursor, "beforeSubmitPrompt")
        | (AgentHarnessV1::Copilot, "userPromptSubmitted" | "UserPromptSubmit")
        | (AgentHarnessV1::GeminiCli, "BeforeAgent") => Event::Prompt,
        (AgentHarnessV1::Cursor, "preToolUse")
        | (AgentHarnessV1::Copilot, "preToolUse" | "PreToolUse")
        | (AgentHarnessV1::GeminiCli, "BeforeTool") => Event::ToolStart,
        (AgentHarnessV1::Cursor, "postToolUse")
        | (AgentHarnessV1::Copilot, "postToolUse" | "PostToolUse")
        | (AgentHarnessV1::GeminiCli, "AfterTool") => Event::ToolFinish,
        (AgentHarnessV1::Cursor, "postToolUseFailure")
        | (AgentHarnessV1::Copilot, "postToolUseFailure" | "PostToolUseFailure") => {
            Event::ToolFailure
        }
        (AgentHarnessV1::Cursor, "afterAgentResponse") => Event::Reply,
        (AgentHarnessV1::GeminiCli, "AfterAgent") => Event::ReplyAndStop,
        (AgentHarnessV1::Cursor, "stop") | (AgentHarnessV1::Copilot, "agentStop" | "Stop") => {
            Event::Stop
        }
        (AgentHarnessV1::Cursor, "sessionEnd")
        | (AgentHarnessV1::Copilot, "sessionEnd" | "SessionEnd")
        | (AgentHarnessV1::GeminiCli, "SessionEnd") => Event::End,
        (AgentHarnessV1::Cursor, "preCompact")
        | (AgentHarnessV1::Copilot, "preCompact" | "PreCompact")
        | (AgentHarnessV1::GeminiCli, "PreCompress") => Event::Compact,
        (AgentHarnessV1::GeminiCli, "BeforeModel") => Event::Model,
        _ => return None,
    })
}

fn tool_event(
    object: &Map<String, Value>,
    harness: AgentHarnessV1,
    camel: bool,
    event: Event,
    normalized: &mut Value,
) -> Result<()> {
    let name = if camel { "toolName" } else { "tool_name" };
    let input = if camel { "toolArgs" } else { "tool_input" };
    normalized["tool_name"] = json!(required_identifier(object, name, 128)?);
    if let Some(input) = object.get(input) {
        normalized["tool_input"] = parsed_json_string(input);
    } else if event == Event::ToolStart {
        return Err(invalid("native tool input is missing"));
    }
    if harness == AgentHarnessV1::Cursor
        && let Some(id) = optional_text(object, "tool_use_id")?
    {
        if !identifier(id, 255) {
            return Err(invalid("native tool identity is invalid"));
        }
        normalized["tool_use_id"] = json!(id);
    }
    // Copilot and Gemini expose no documented tool-call identity. A timestamp or tool name
    // cannot pair concurrent calls, and must not become a tool_use_id.
    if event == Event::ToolFailure {
        let error = if harness == AgentHarnessV1::Cursor {
            "error_message"
        } else {
            "error"
        };
        normalized["error"] = json!(required_text(object, error)?);
    } else if event == Event::ToolFinish {
        normalized["tool_response"] = match harness {
            AgentHarnessV1::Cursor => {
                parsed_json_string(&json!(required_text(object, "tool_output")?))
            }
            AgentHarnessV1::Copilot => {
                let key = if camel { "toolResult" } else { "tool_result" };
                let result = object
                    .get(key)
                    .and_then(Value::as_object)
                    .ok_or_else(|| invalid("native tool result is missing or invalid"))?;
                let kind = if camel { "resultType" } else { "result_type" };
                let text = if camel {
                    "textResultForLlm"
                } else {
                    "text_result_for_llm"
                };
                let outcome = required_text(result, kind)?;
                if matches!(outcome, "failure" | "denied") {
                    normalized["hook_event_name"] = json!("PostToolUseFailure");
                    normalized["error"] = json!(optional_text(result, text)?.unwrap_or(outcome));
                    return Ok(());
                }
                if outcome != "success" {
                    return Err(invalid("native tool result has an unknown outcome"));
                }
                json!(required_text(result, text)?)
            }
            AgentHarnessV1::GeminiCli => object
                .get("tool_response")
                .cloned()
                .ok_or_else(|| invalid("native tool response is missing"))?,
            _ => unreachable!("only the three additional native hosts are normalized"),
        };
    }
    Ok(())
}

fn parsed_json_string(value: &Value) -> Value {
    value
        .as_str()
        .and_then(|text| serde_json::from_str(text).ok())
        .unwrap_or_else(|| value.clone())
}

fn model(object: &Map<String, Value>, harness: AgentHarnessV1) -> Option<String> {
    let value = match harness {
        AgentHarnessV1::Cursor => object.get("model").or_else(|| object.get("model_id")),
        AgentHarnessV1::GeminiCli => object
            .get("llm_request")
            .and_then(|request| request.get("model"))
            .or_else(|| object.get("model")),
        _ => None,
    };
    value
        .and_then(Value::as_str)
        .filter(|value| identifier(value, 255))
        .map(str::to_owned)
}

fn refuse_child_payload(object: &Map<String, Value>) -> Result<()> {
    for key in ["isSidechain", "is_sidechain"] {
        if object
            .get(key)
            .is_some_and(|value| !value.is_null() && value != &Value::Bool(false))
        {
            return Err(invalid("native child-session capture is unsupported"));
        }
    }
    for key in [
        "agentId",
        "agent_id",
        "parentAgentId",
        "parent_agent_id",
        "parent_session_id",
        "parentSessionId",
        "parent_conversation_id",
    ] {
        if object
            .get(key)
            .is_some_and(|value| !value.is_null() && value != "")
        {
            return Err(invalid("native child-session capture is unsupported"));
        }
    }
    Ok(())
}

fn optional_text<'a>(object: &'a Map<String, Value>, key: &str) -> Result<Option<&'a str>> {
    match object.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(text)) => Ok(Some(text)),
        Some(_) => Err(invalid("native hook text field has another type")),
    }
}

fn required_text<'a>(object: &'a Map<String, Value>, key: &str) -> Result<&'a str> {
    optional_text(object, key)?.ok_or_else(|| invalid("native hook text field is missing"))
}

fn required_identifier<'a>(
    object: &'a Map<String, Value>,
    key: &str,
    max: usize,
) -> Result<&'a str> {
    let text = required_text(object, key)?;
    if !identifier(text, max) {
        return Err(invalid("native hook identity is invalid"));
    }
    Ok(text)
}

fn timestamp(object: &Map<String, Value>, milliseconds: bool) -> Result<Option<String>> {
    let Some(value) = object.get("timestamp").filter(|value| !value.is_null()) else {
        return Ok(None);
    };
    if milliseconds {
        let millis = value
            .as_i64()
            .filter(|value| (0..=253_402_300_799_999).contains(value))
            .ok_or_else(|| invalid("native hook timestamp is invalid"))?;
        let (year, month, day, hour, minute, second) = crate::civil::utc_civil(millis / 1000);
        return Ok(Some(format!(
            "{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}.{:03}Z",
            millis % 1000
        )));
    }
    let text = value
        .as_str()
        .filter(|text| text.len() <= 40)
        .ok_or_else(|| invalid("native hook timestamp is invalid"))?;
    let utc = text
        .strip_suffix('Z')
        .or_else(|| text.strip_suffix("+00:00"))
        .ok_or_else(|| invalid("native hook timestamp must be UTC"))?;
    let (seconds, fraction) = utc
        .split_once('.')
        .map_or((utc, None), |(a, b)| (a, Some(b)));
    if seconds.len() != 19
        || !seconds.bytes().enumerate().all(|(i, b)| match i {
            4 | 7 => b == b'-',
            10 => b == b'T',
            13 | 16 => b == b':',
            _ => b.is_ascii_digit(),
        })
        || fraction.is_some_and(|fraction| {
            fraction.is_empty() || !fraction.bytes().all(|b| b.is_ascii_digit())
        })
    {
        return Err(invalid("native hook timestamp is invalid"));
    }
    let epoch = parse_rfc3339(text).ok_or_else(|| invalid("native hook timestamp is invalid"))?;
    let (year, month, day, hour, minute, second) = crate::civil::utc_civil(epoch);
    if seconds != format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}") {
        return Err(invalid("native hook timestamp is invalid"));
    }
    Ok(Some(text.to_owned()))
}

fn invalid(message: &str) -> CaptureError {
    CaptureError::Invalid(message.into())
}

fn bounded(value: &Value) -> Result<()> {
    struct Budget(usize);
    impl Write for Budget {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.0 = self
                .0
                .checked_sub(bytes.len())
                .ok_or_else(|| io::Error::other("native hook exceeds capture byte limit"))?;
            Ok(bytes.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
    serde_json::to_writer(Budget(MAX_HOOK_BYTES), value)
        .map_err(|_| CaptureError::Limit("native hook exceeds capture byte limit".into()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::change_capture::{LocalSpool, claude_code};
    use crate::change_record::{CaptureEnvelopeV1, CaptureModeV1, CapturedEventKindV1};

    fn normalized(agent: &str, payload: Value, hint: Option<&str>) -> NormalizedHook {
        normalize(agent, &payload, hint).unwrap().unwrap()
    }

    fn request(harness: AgentHarnessV1, mode: CaptureModeV1) -> claude_code::CaptureRequest {
        claude_code::CaptureRequest {
            operation_prefix: "native-test".into(),
            work_id: "wrk_native".into(),
            generation: 1,
            observed_session_id: "session_native".into(),
            canonical_session_id: "canonical_native".into(),
            parent_session_id: None,
            capture_mode: mode,
            live_capture_mode: mode,
            harness,
        }
    }

    #[test]
    fn harness_wire_ids_remain_distinct() {
        for (harness, wire) in [
            (AgentHarnessV1::ClaudeCode, "claude_code"),
            (AgentHarnessV1::Codex, "codex"),
            (AgentHarnessV1::Cursor, "cursor"),
            (AgentHarnessV1::Copilot, "copilot"),
            (AgentHarnessV1::GeminiCli, "gemini_cli"),
        ] {
            assert_eq!(serde_json::to_value(harness).unwrap(), wire);
            assert_eq!(
                serde_json::from_value::<AgentHarnessV1>(json!(wire)).unwrap(),
                harness
            );
        }
    }

    #[test]
    fn cursor_captures_visible_text_without_claiming_generation_is_message_identity() {
        let input = json!({
            "hook_event_name": "afterAgentResponse", "conversation_id": "session_native",
            "generation_id": "generation-1", "text": "The complete visible reply.",
            "model": "example-model", "system_prompt": "excluded-system",
            "transcript_path": "/excluded/path", "thought": "excluded-thought",
        });
        let original = input.clone();
        let hook = normalize("cursor", &input, None).unwrap().unwrap();
        assert_eq!(input, original);
        assert_eq!(hook.harness, AgentHarnessV1::Cursor);
        assert_eq!(
            hook.events,
            vec![json!({
                "hook_event_name": "MessageDisplay", "session_id": "session_native",
                "delta": "The complete visible reply.", "final": true, "model": "example-model",
            })]
        );
        assert!(
            !serde_json::to_string(&hook.events)
                .unwrap()
                .contains("excluded")
        );
        let prompt = normalized(
            "cursor",
            json!({
                "hook_event_name": "beforeSubmitPrompt", "conversation_id": "session_native",
                "generation_id": "generation-1", "prompt": "Continue from this context.",
            }),
            None,
        );
        assert_eq!(prompt.events[0]["prompt"], "Continue from this context.");
        assert!(prompt.events[0].get("prompt_id").is_none());
    }

    #[test]
    fn cursor_tool_phases_keep_the_real_id_and_results() {
        let mut input = json!({
            "hook_event_name": "preToolUse", "conversation_id": "session_native",
            "tool_name": "Shell", "tool_use_id": "call-1", "tool_input": {"command": "true"},
        });
        let pre = normalize("cursor", &input, None).unwrap().unwrap();
        assert_eq!(pre.events[0]["hook_event_name"], "PreToolUse");
        assert_eq!(pre.events[0]["tool_use_id"], "call-1");
        input["hook_event_name"] = json!("postToolUse");
        input["tool_output"] = json!("{\"exit_code\":0,\"stdout\":\"done\"}");
        let post = normalize("cursor", &input, None).unwrap().unwrap();
        assert_eq!(
            post.events[0]["tool_response"],
            json!({"exit_code":0,"stdout":"done"})
        );
        assert_eq!(post.events[0]["tool_use_id"], "call-1");
        input["hook_event_name"] = json!("postToolUseFailure");
        input["error_message"] = json!("Tool was interrupted.");
        let failed = normalize("cursor", &input, None).unwrap().unwrap();
        assert_eq!(failed.events[0]["hook_event_name"], "PostToolUseFailure");
        assert_eq!(failed.events[0]["error"], "Tool was interrupted.");
        assert!(failed.events[0].get("tool_response").is_none());
    }

    #[test]
    fn copilot_requires_event_identity_and_preserves_milliseconds() {
        let input = json!({"sessionId":"session_native","timestamp":1_780_000_000_123_i64,
            "prompt":"Keep this prompt", "model":"not-a-documented-copilot-field"});
        assert!(normalize("copilot", &input, None).is_err());
        let hook = normalize("copilot", &input, Some("userPromptSubmitted"))
            .unwrap()
            .unwrap();
        assert_eq!(
            hook.events,
            vec![json!({
                "session_id":"session_native","hook_event_name":"UserPromptSubmit",
                "timestamp":"2026-05-28T20:26:40.123Z","prompt":"Keep this prompt",
            })]
        );
        assert!(hook.model.is_none());
        let conflict = json!({"sessionId":"session_native","hook_event_name":"sessionStart"});
        assert!(normalize("copilot", &conflict, Some("sessionEnd")).is_err());
    }

    #[test]
    fn copilot_native_and_pascal_tool_formats_keep_results_without_fake_ids() {
        for (input, hint) in [
            (
                json!({"sessionId":"session_native","toolName":"bash",
                "toolArgs":"{\"command\":\"true\"}",
                "toolResult":{"resultType":"success","textResultForLlm":"done"},
                "tool_use_id":"invented"}),
                Some("postToolUse"),
            ),
            (
                json!({"session_id":"session_native","hook_event_name":"PostToolUse",
                "tool_name":"bash","tool_input":{"command":"true"},
                "tool_result":{"result_type":"success","text_result_for_llm":"done"},
                "tool_use_id":"invented"}),
                None,
            ),
        ] {
            let hook = normalized("copilot", input, hint);
            assert_eq!(
                hook.events[0],
                json!({"hook_event_name":"PostToolUse",
                "session_id":"session_native","tool_name":"bash",
                "tool_input":{"command":"true"},"tool_response":"done"})
            );
        }
        let failure = normalized(
            "copilot",
            json!({"sessionId":"session_native",
            "toolName":"bash","error":"Tool failed"}),
            Some("postToolUseFailure"),
        );
        assert_eq!(failure.events[0]["error"], "Tool failed");
        let input = normalized(
            "copilot",
            json!({"sessionId":"session_native",
            "toolName":"custom","toolArgs":"plain text"}),
            Some("preToolUse"),
        );
        assert_eq!(input.events[0]["tool_input"], "plain text");
    }

    #[test]
    fn copilot_failed_and_denied_results_are_retained_without_fake_call_ids() {
        for outcome in ["failure", "denied"] {
            for text in [None, Some("The tool did not run.")] {
                for camel in [true, false] {
                    let (mut input, hint, result_key, text_key) = if camel {
                        (
                            json!({"sessionId":"session_native","toolName":"bash",
                                "toolResult":{"resultType":outcome}}),
                            Some("postToolUse"),
                            "toolResult",
                            "textResultForLlm",
                        )
                    } else {
                        (
                            json!({"session_id":"session_native","hook_event_name":"PostToolUse",
                                "tool_name":"bash","tool_result":{"result_type":outcome}}),
                            None,
                            "tool_result",
                            "text_result_for_llm",
                        )
                    };
                    if let Some(text) = text {
                        input[result_key][text_key] = json!(text);
                    }
                    let hook = normalized("copilot", input, hint);
                    assert_eq!(
                        hook.events[0],
                        json!({
                            "hook_event_name":"PostToolUseFailure", "session_id":"session_native",
                            "tool_name":"bash", "error":text.unwrap_or(outcome),
                        })
                    );
                    let temp = tempfile::TempDir::new().unwrap();
                    let spool = LocalSpool::open(temp.path().join("spool")).unwrap();
                    claude_code::capture_hook(
                        &serde_json::to_vec(&hook.events[0]).unwrap(),
                        &request(AgentHarnessV1::Copilot, CaptureModeV1::LocalConversation),
                        &spool,
                    )
                    .unwrap();
                    assert!(!spool.pending(10).unwrap().is_empty());
                }
            }
        }
    }

    #[test]
    fn streamed_after_model_snapshots_do_not_enter_capture() {
        assert!(
            normalize(
                "gemini",
                &json!({
                    "hook_event_name":"AfterModel", "session_id":"session_native",
                    "llm_request":{"model":"gemini-example"},
                    "llm_response":{"usageMetadata":{"totalTokenCount":42}},
                }),
                None
            )
            .unwrap()
            .is_none()
        );
    }

    #[test]
    fn gemini_captures_prompt_reply_and_tool_results_from_their_own_events() {
        let prompt = normalized(
            "gemini",
            json!({"hook_event_name":"BeforeAgent",
            "session_id":"session_native","prompt":"User prompt"}),
            None,
        );
        assert_eq!(prompt.events[0]["prompt"], "User prompt");
        let reply = normalized(
            "gemini",
            json!({"hook_event_name":"AfterAgent",
            "session_id":"session_native","prompt_response":"Visible reply",
            "prompt":"excluded-duplicate-prompt"}),
            None,
        );
        assert_eq!(
            reply.events,
            vec![
                json!({"hook_event_name":"MessageDisplay","session_id":"session_native",
                "delta":"Visible reply","final":true}),
                json!({"hook_event_name":"Stop","session_id":"session_native"}),
            ]
        );
        for (native, canonical, extra) in [
            (
                "BeforeTool",
                "PreToolUse",
                json!({"tool_input":{"command":"true"}}),
            ),
            (
                "AfterTool",
                "PostToolUse",
                json!({"tool_response":{"output":"done"}}),
            ),
        ] {
            let mut input = json!({"hook_event_name":native,"session_id":"session_native",
                "tool_name":"run_shell_command","tool_use_id":"undocumented-id"});
            input
                .as_object_mut()
                .unwrap()
                .extend(extra.as_object().unwrap().clone());
            let hook = normalize("gemini", &input, None).unwrap().unwrap();
            assert_eq!(hook.events[0]["hook_event_name"], canonical);
            assert!(hook.events[0].get("tool_use_id").is_none());
        }
    }

    #[test]
    fn model_hooks_retain_setup_without_request_context_thoughts_or_token_totals() {
        let hook = normalized(
            "gemini",
            json!({"hook_event_name":"BeforeModel",
                "session_id":"session_native","timestamp":"2026-05-28T20:26:40.123Z",
                "llm_request":{"model":"gemini-example", "messages":["excluded-context"],
                    "systemInstruction":"excluded-system"},
                "llm_response":{"candidates":[{"content":{"parts":[{"text":"excluded-thought"}]}}],
                    "usageMetadata":{"promptTokenCount":20,"totalTokenCount":30}},
            }),
            None,
        );
        assert_eq!(hook.model.as_deref(), Some("gemini-example"));
        assert_eq!(
            hook.events,
            vec![json!({"hook_event_name":"SetupObserved",
            "session_id":"session_native","timestamp":"2026-05-28T20:26:40.123Z",
            "model":"gemini-example"})]
        );
        for model in [
            Value::Null,
            json!("../invalid/model"),
            json!("x".repeat(256)),
        ] {
            let hook = normalized(
                "gemini",
                json!({"hook_event_name":"BeforeModel",
                "session_id":"session_native","llm_request":{"model":model},
                "llm_response":{"usageMetadata":{"totalTokenCount":30}}}),
                None,
            );
            assert!(hook.events.is_empty());
            assert!(hook.model.is_none());
        }
    }

    #[test]
    fn lifecycle_and_compaction_do_not_copy_initial_prompts_or_transcripts() {
        for (agent, native, canonical, session_key, hint) in [
            (
                "cursor",
                "sessionStart",
                "SessionStart",
                "conversation_id",
                false,
            ),
            ("cursor", "stop", "Stop", "conversation_id", false),
            (
                "cursor",
                "sessionEnd",
                "SessionEnd",
                "conversation_id",
                false,
            ),
            (
                "cursor",
                "preCompact",
                "PreCompact",
                "conversation_id",
                false,
            ),
            ("copilot", "sessionStart", "SessionStart", "sessionId", true),
            ("copilot", "agentStop", "Stop", "sessionId", true),
            ("copilot", "sessionEnd", "SessionEnd", "sessionId", true),
            (
                "gemini",
                "SessionStart",
                "SessionStart",
                "session_id",
                false,
            ),
            ("gemini", "SessionEnd", "SessionEnd", "session_id", false),
            ("gemini", "PreCompress", "PreCompact", "session_id", false),
        ] {
            let mut input = json!({session_key:"session_native","source":"resume",
                "initialPrompt":"excluded-prompt","transcriptPath":"/excluded/path",
                "transcript_path":"/excluded/path","context_tokens":1000});
            if !hint {
                input["hook_event_name"] = json!(native);
            }
            let hook = normalized(agent, input, hint.then_some(native));
            assert_eq!(hook.events[0]["hook_event_name"], canonical);
            assert!(
                !serde_json::to_string(&hook.events)
                    .unwrap()
                    .contains("excluded")
            );
            assert!(hook.events[0].get("context_tokens").is_none());
        }
    }

    #[test]
    fn unsupported_events_and_agents_do_not_cross_adapters() {
        for (agent, event) in [
            ("cursor", "afterAgentThought"),
            ("copilot", "userPromptTransformed"),
            ("gemini", "Notification"),
            ("claude", "SessionStart"),
            ("codex", "SessionStart"),
            ("other", "SessionStart"),
            ("cursor", "SessionStart"),
        ] {
            assert!(
                normalize(agent, &json!({"hook_event_name":event}), None)
                    .unwrap()
                    .is_none()
            );
        }
    }

    #[test]
    fn child_markers_and_conflicting_session_aliases_are_refused() {
        let base = json!({"hook_event_name":"UserPromptSubmit","session_id":"session_native",
            "prompt":"must not be attributed to the parent"});
        for (key, value) in [
            ("agent_id", json!("child")),
            ("agentId", json!("child")),
            ("parent_agent_id", json!("parent")),
            ("parent_session_id", json!("parent")),
            ("parent_conversation_id", json!("parent")),
            ("isSidechain", json!(true)),
            ("is_sidechain", json!("false")),
            ("sessionId", json!("another_session")),
        ] {
            let mut input = base.clone();
            input[key] = value;
            assert!(
                normalize("copilot", &input, None).is_err(),
                "accepted {key}"
            );
        }
        for identity in [
            json!(""),
            json!("../session"),
            json!("x".repeat(256)),
            json!(12),
        ] {
            let mut input = base.clone();
            input["session_id"] = identity;
            assert!(normalize("copilot", &input, None).is_err());
        }
    }

    #[test]
    fn malformed_fields_and_oversized_payloads_are_refused() {
        let base = json!({"hook_event_name":"beforeSubmitPrompt","conversation_id":"session_native",
            "prompt":"Prompt"});
        for (key, value) in [
            ("prompt", json!({"text":"unexpected shape"})),
            ("hook_event_name", json!(1)),
            ("timestamp", json!("2026-02-30T00:00:00Z")),
            ("timestamp", json!("2026-05-28T20:26:40+01:00")),
            ("timestamp", json!("999999999999999999-01-01T00:00:00Z")),
        ] {
            let mut input = base.clone();
            input[key] = value;
            assert!(normalize("cursor", &input, None).is_err());
        }
        for millis in [json!(-1), json!(1.5), json!(i64::MAX)] {
            assert!(
                normalize(
                    "copilot",
                    &json!({"sessionId":"session_native",
                "prompt":"Prompt","timestamp":millis}),
                    Some("userPromptSubmitted")
                )
                .is_err()
            );
        }
        let mut large = base;
        large["ignored_extra"] = json!("x".repeat(MAX_HOOK_BYTES));
        assert!(matches!(
            normalize("cursor", &large, None),
            Err(CaptureError::Limit(_))
        ));
        assert!(normalize("gemini", &json!([]), None).is_err());
    }

    #[test]
    fn repeated_cursor_replies_with_only_a_timestamp_remain_separate_occurrences() {
        let temp = tempfile::TempDir::new().unwrap();
        let spool = LocalSpool::open(temp.path().join("spool")).unwrap();
        let hook = normalized(
            "cursor",
            json!({"hook_event_name":"afterAgentResponse",
            "conversation_id":"session_native","generation_id":"same-generation","text":"Repeated reply",
            "timestamp":"2026-05-28T20:26:40.123Z"}),
            None,
        );
        let request = request(hook.harness, CaptureModeV1::LocalConversation);
        for _ in 0..2 {
            claude_code::capture_hook(
                &serde_json::to_vec(&hook.events[0]).unwrap(),
                &request,
                &spool,
            )
            .unwrap();
        }
        let replies: Vec<_> = spool
            .pending(100)
            .unwrap()
            .into_iter()
            .flat_map(|envelope| match envelope {
                CaptureEnvelopeV1::EventBatch(batch) => batch.events,
                _ => vec![],
            })
            .filter(|event| event.kind == CapturedEventKindV1::AssistantMessage)
            .collect();
        assert_eq!(replies.len(), 2);
        assert_ne!(replies[0].event_id, replies[1].event_id);
    }

    fn copilot_same_millisecond_events() -> Vec<Value> {
        [
            (
                "preToolUse",
                json!({"sessionId":"session_native","timestamp":1_780_000_000_123_i64,
                "toolName":"bash","toolArgs":{"command":"first command"}}),
            ),
            (
                "preToolUse",
                json!({"sessionId":"session_native","timestamp":1_780_000_000_123_i64,
                "toolName":"bash","toolArgs":{"command":"second command"}}),
            ),
            (
                "userPromptSubmitted",
                json!({"sessionId":"session_native","timestamp":1_780_000_000_123_i64,
                "prompt":"Repeated prompt"}),
            ),
            (
                "userPromptSubmitted",
                json!({"sessionId":"session_native","timestamp":1_780_000_000_123_i64,
                "prompt":"Repeated prompt"}),
            ),
        ]
        .into_iter()
        .flat_map(|(event, payload)| normalized("copilot", payload, Some(event)).events)
        .collect()
    }

    #[test]
    fn copilot_same_millisecond_tools_and_repeated_prompts_keep_each_occurrence_and_gap() {
        use crate::change_record::GapReasonV1;
        let temp = tempfile::tempdir().unwrap();
        let spool = LocalSpool::open(temp.path().join("spool")).unwrap();
        let request = request(AgentHarnessV1::Copilot, CaptureModeV1::LocalConversation);
        for event in copilot_same_millisecond_events() {
            claude_code::capture_hook(&serde_json::to_vec(&event).unwrap(), &request, &spool)
                .unwrap();
        }
        let mut observations = Vec::new();
        let mut uncertain = 0;
        for envelope in spool.pending(100).unwrap() {
            let CaptureEnvelopeV1::EventBatch(batch) = envelope else {
                panic!("expected event evidence only");
            };
            for event in batch.events {
                if event
                    .gap
                    .as_ref()
                    .is_some_and(|gap| gap.reason == GapReasonV1::IdentityUncertain)
                {
                    uncertain += 1;
                }
                if matches!(
                    event.kind,
                    CapturedEventKindV1::ToolStarted | CapturedEventKindV1::UserPrompt
                ) {
                    assert_eq!(event.observed_at, 1_780_000_000);
                    observations.push(event);
                }
            }
        }
        assert_eq!(observations.len(), 4);
        assert_eq!(uncertain, 4);
        let ids: std::collections::HashSet<_> =
            observations.iter().map(|event| &event.event_id).collect();
        assert_eq!(ids.len(), 4);
        let bodies: Vec<Value> = observations
            .iter()
            .map(|event| {
                serde_json::from_slice(
                    &spool
                        .read_artifact(event.artifact.as_ref().unwrap())
                        .unwrap(),
                )
                .unwrap()
            })
            .collect();
        assert_eq!(
            bodies
                .iter()
                .filter(|body| body["content"] == "Repeated prompt")
                .count(),
            2
        );
        for command in ["first command", "second command"] {
            assert_eq!(
                bodies
                    .iter()
                    .filter(|body| body["tool_input"]["command"] == command)
                    .count(),
                1
            );
        }
    }

    #[test]
    fn deferred_native_events_preserve_same_millisecond_occurrences_without_bodies() {
        use crate::change_capture::{DeferredReasonV1, GitSourceRequest};
        let temp = tempfile::tempdir().unwrap();
        let spool = LocalSpool::open(temp.path().join("spool")).unwrap();
        let actions = claude_code::ActionCaptureConfig {
            source: GitSourceRequest {
                repository: "example/repository".into(),
                worktree_id: "worktree_native".into(),
                repository_path: temp.path().to_path_buf(),
                base_ref: "HEAD".into(),
            },
            setup_digest: "a".repeat(64),
            rules: Vec::new(),
            managed_invocations: Vec::new(),
        };
        for event in copilot_same_millisecond_events() {
            claude_code::deferred_native_hook(
                &serde_json::to_vec(&event).unwrap(),
                AgentHarnessV1::Copilot,
                DeferredReasonV1::NoCurrentAuthorizedWork,
                &actions,
                &spool,
            )
            .unwrap();
        }
        let deferred = spool.deferred(100).unwrap();
        assert_eq!(deferred.len(), 4);
        assert!(
            deferred
                .iter()
                .all(|event| event.observed_at == 1_780_000_000)
        );
        let ids: std::collections::HashSet<_> =
            deferred.iter().map(|event| &event.occurrence_id).collect();
        assert_eq!(ids.len(), 4);
        let digests: std::collections::HashSet<_> =
            deferred.iter().map(|event| &event.input_digest).collect();
        assert_eq!(
            digests.len(),
            3,
            "identical prompts remain separate occurrences"
        );
        let retained = serde_json::to_string(&deferred).unwrap();
        for private in ["first command", "second command", "Repeated prompt"] {
            assert!(!retained.contains(private));
        }
    }

    #[test]
    fn legacy_timestamp_identity_and_real_cursor_tool_identity_still_deduplicate() {
        use crate::change_record::GapReasonV1;
        for harness in [
            AgentHarnessV1::ClaudeCode,
            AgentHarnessV1::Codex,
            AgentHarnessV1::Cursor,
        ] {
            let temp = tempfile::tempdir().unwrap();
            let spool = LocalSpool::open(temp.path().join("spool")).unwrap();
            let request = request(harness, CaptureModeV1::LocalConversation);
            let mut event = json!({"hook_event_name":"UserPromptSubmit","session_id":"session_native",
                "timestamp":"2026-05-28T20:26:40.123Z","prompt":"Legacy prompt"});
            if harness == AgentHarnessV1::Cursor {
                event = json!({"hook_event_name":"PreToolUse","session_id":"session_native",
                    "timestamp":"2026-05-28T20:26:40.123Z","tool_name":"Shell",
                    "tool_use_id":"genuine-call-id","tool_input":{"command":"true"}});
            }
            for _ in 0..2 {
                claude_code::capture_hook(&serde_json::to_vec(&event).unwrap(), &request, &spool)
                    .unwrap();
            }
            let events: Vec<_> = spool
                .pending(100)
                .unwrap()
                .into_iter()
                .flat_map(|envelope| match envelope {
                    CaptureEnvelopeV1::EventBatch(batch) => batch.events,
                    _ => Vec::new(),
                })
                .collect();
            assert_eq!(
                events
                    .iter()
                    .filter(|event| matches!(
                        event.kind,
                        CapturedEventKindV1::UserPrompt | CapturedEventKindV1::ToolStarted
                    ))
                    .count(),
                1
            );
            assert!(!events.iter().any(|event| {
                event
                    .gap
                    .as_ref()
                    .is_some_and(|gap| gap.reason == GapReasonV1::IdentityUncertain)
            }));
        }
    }

    #[test]
    fn metadata_mode_keeps_setup_but_no_conversation_bodies_or_usage() {
        let temp = tempfile::TempDir::new().unwrap();
        let spool = LocalSpool::open(temp.path().join("spool")).unwrap();
        let request = request(AgentHarnessV1::GeminiCli, CaptureModeV1::MetadataOnly);
        for payload in [
            json!({"hook_event_name":"BeforeAgent","session_id":"session_native","prompt":"Private prompt"}),
            json!({"hook_event_name":"BeforeModel","session_id":"session_native",
                "llm_request":{"model":"gemini-example"},
                "llm_response":{"usageMetadata":{"totalTokenCount":42}}}),
        ] {
            let hook = normalized("gemini", payload, None);
            for event in hook.events {
                claude_code::capture_hook(&serde_json::to_vec(&event).unwrap(), &request, &spool)
                    .unwrap();
            }
        }
        let mut setup_seen = false;
        for envelope in spool.pending(100).unwrap() {
            envelope.validate().unwrap();
            assert!(!matches!(&envelope, CaptureEnvelopeV1::Usage(_)));
            if let CaptureEnvelopeV1::EventBatch(batch) = envelope {
                assert_eq!(batch.session.harness, AgentHarnessV1::GeminiCli);
                if batch
                    .events
                    .iter()
                    .any(|event| event.kind == CapturedEventKindV1::SetupObserved)
                {
                    assert!(batch.events.iter().all(|event| event.gap.is_none()));
                }
                for event in batch.events {
                    assert!(event.artifact.is_none());
                    if event.kind == CapturedEventKindV1::SetupObserved {
                        let setup = event.setup.unwrap();
                        assert_eq!(setup.model.as_deref(), Some("gemini-example"));
                        setup_seen = true;
                    }
                }
            }
        }
        assert!(setup_seen);
    }
}
