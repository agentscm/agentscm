//! Codex capture: what Codex exposes, and the parts only Codex has.
//!
//! Codex hooks use Claude Code's event names and fields, so live events go through the shared hook
//! path in [`super::claude_code`] with [`AgentHarnessV1::Codex`](crate::change_record::AgentHarnessV1).
//! The native hook payload carries a turn's last assistant message on `Stop` and request usage
//! on `PostModelRequest`. Explicit Work associations also recover bounded visible history from
//! the exact native session file. The host does not promise a stable file format: unknown or
//! conflicting records preserve uncertainty rather than becoming proof of complete capture.

use std::io::BufRead;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

use serde::Deserialize;
use serde_json::{Map, Value, json};

use super::claude_code::{
    CaptureReport, CaptureRequest, SelectedEvent, VisibleHistoryFile, add_report,
    automatic_history_event, capture_history_record, identifier, import_history, native_tool_phase,
    open_history_file, read_visible_page, record_provider_history_gap, tool_operation,
};
use super::redact::redact_json;
use super::spool::{ProviderHistoryCursorV1, VisibleHistoryCursorV1};
use super::{AppendOutcome, CaptureError, LocalSpool, Result};
use crate::change_record::{
    CaptureEnvelopeV1, CaptureModeV1, CapturedEventKindV1, ToolCompletionV1, ToolFactV1, ToolKindV1,
};
use crate::checks::sha256;
use crate::hub::github::parse_rfc3339;
use crate::hub::insights::{
    AttemptOutcome, AttemptPhase, UsageBasis, UsageMeasurementScope, UsageRecord,
};
use crate::hub::session_waiting::{PendingQuestion, QuestionSnapshot};

const MAX_ROLLOUT_FILES: usize = 16_384;

pub use super::claude_code::VisibleHistoryReport as RolloutCaptureReport;

/// Finds only the exact native session, never the newest file. The caller supplies the host's
/// configured sessions directory; discovery neither follows links nor searches outside it.
pub fn find_rollout(history_root: &Path, native_session: &str) -> Result<Option<PathBuf>> {
    if !identifier(native_session, 128) {
        return Err(CaptureError::Invalid(
            "Codex native session is invalid".into(),
        ));
    }
    let metadata = match std::fs::symlink_metadata(history_root) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
        return Err(CaptureError::Invalid(
            "Codex history root is not a regular directory".into(),
        ));
    }
    let root = history_root.canonicalize()?;
    let mut directories = vec![(root.clone(), 0)];
    let mut entries = 0usize;
    let mut found = None;
    let suffix = format!("-{native_session}.jsonl");
    while let Some((directory, depth)) = directories.pop() {
        for entry in std::fs::read_dir(directory)? {
            let entry = entry?;
            entries += 1;
            if entries > MAX_ROLLOUT_FILES {
                return Err(CaptureError::Limit(
                    "Codex history discovery reached its entry bound".into(),
                ));
            }
            let kind = entry.file_type()?;
            if kind.is_dir() && depth < 3 {
                directories.push((entry.path(), depth + 1));
            } else if kind.is_file()
                && entry
                    .file_name()
                    .to_str()
                    .is_some_and(|name| name.starts_with("rollout-") && name.ends_with(&suffix))
            {
                let opened = open_rollout(&entry.path(), native_session)?;
                if !opened.path.starts_with(&root) {
                    return Err(CaptureError::Invalid(
                        "Codex history left its configured root".into(),
                    ));
                }
                if found.replace(opened.path).is_some() {
                    return Err(CaptureError::Conflict(
                        "multiple Codex files name this native session".into(),
                    ));
                }
            }
        }
    }
    Ok(found)
}

fn open_rollout(path: &Path, session: &str) -> Result<VisibleHistoryFile> {
    open_history_file(path, |line| {
        if line["type"] != "session_meta" || line["payload"]["id"] != session {
            return Err(CaptureError::Invalid(
                "Codex history does not start with this native session".into(),
            ));
        }
        Ok(line["payload"]["subagent_history_start_ordinal"]
            .as_u64()
            .unwrap_or(0))
    })
}

fn own_record(line: &RolloutLine, own_from: u64, session: &str) -> Result<bool> {
    if line.kind.as_deref() == Some("session_meta") {
        return Ok(false);
    }
    if own_from > 0 && line.ordinal.is_none() {
        return Err(CaptureError::Invalid(
            "forked Codex history record has no source ordinal".into(),
        ));
    }
    if line.ordinal.is_some_and(|ordinal| ordinal < own_from) {
        return Ok(false);
    }
    if line
        .payload
        .as_ref()
        .and_then(|payload| payload["thread_id"].as_str())
        .is_some_and(|thread| thread != session)
    {
        return Err(CaptureError::Invalid(
            "Codex history contains another native session".into(),
        ));
    }
    Ok(true)
}

/// Proves an explicit successful AgentSCM start in this native session's own records.
pub fn verify_history_start(
    path: &Path,
    request: &CaptureRequest,
    repository: &str,
    spool: &LocalSpool,
) -> Result<super::claude_code::HistoryStartReport> {
    if request.harness != crate::change_record::AgentHarnessV1::Codex {
        return Err(CaptureError::Invalid(
            "Codex start proof requires Codex capture".into(),
        ));
    }
    let mut opened = open_rollout(path, &request.observed_session_id)?;
    let own_from = opened.own_from;
    super::claude_code::verify_start_page(&mut opened, request, repository, spool, |raw, _| {
        let line: RolloutLine = serde_json::from_slice(raw)?;
        if !own_record(&line, own_from, &request.observed_session_id)?
            || line.kind.as_deref() != Some("event_msg")
        {
            return Ok(false);
        }
        let Some(payload) = line
            .payload
            .filter(|payload| payload["type"] == "item_completed")
        else {
            return Ok(false);
        };
        let item = &payload["item"];
        Ok(item["type"] == "McpToolCall"
            && item["id"].as_str().is_some_and(|id| identifier(id, 255))
            && item["server"] == "agentscm"
            && item["tool"] == "agentscm_start"
            && item["status"] == "completed"
            && super::claude_code::start_arguments_match(&item["arguments"], request)
            && super::claude_code::start_result_matches(&item["result"], request, repository))
    })
}

/// Incrementally recovers visible source records for one explicitly authorized Work association.
/// The caller resolves current authorization and capture mode before each call. It may call again
/// while `complete` is false; an incomplete final line is left for the host to finish writing.
pub fn capture_rollout(
    path: &Path,
    request: &CaptureRequest,
    spool: &LocalSpool,
) -> Result<RolloutCaptureReport> {
    if request.harness != crate::change_record::AgentHarnessV1::Codex {
        return Err(CaptureError::Invalid(
            "Codex rollout requires a Codex capture request".into(),
        ));
    }
    let _source = spool.lock_visible_history()?;
    if super::claude_code::history_failed(request, spool)? {
        return Err(CaptureError::Conflict(
            "native history permanently uses hooks for this generation".into(),
        ));
    }
    super::claude_code::finish_visible_history(
        capture_rollout_page(path, request, spool),
        request,
        spool,
    )
}

fn capture_rollout_page(
    path: &Path,
    request: &CaptureRequest,
    spool: &LocalSpool,
) -> Result<RolloutCaptureReport> {
    if request.harness != crate::change_record::AgentHarnessV1::Codex {
        return Err(CaptureError::Invalid(
            "Codex rollout requires a Codex capture request".into(),
        ));
    }
    let mut opened = open_rollout(path, &request.observed_session_id)?;
    let mut previous = spool.visible_history_cursor(
        &request.canonical_session_id,
        &request.work_id,
        request.generation,
    )?;
    let path_digest = sha256(opened.path.as_os_str().as_encoded_bytes());
    if let Some(cursor) = &previous
        && (cursor.file.path_digest != path_digest
            || cursor.harness != request.harness
            || cursor.file.file_device != opened.metadata.dev()
            || cursor.file.file_inode != opened.metadata.ino()
            || cursor.header_digest != opened.header_digest
            || cursor.file.offset > opened.metadata.len())
    {
        record_provider_history_gap(request, spool, "codex-history-replaced-or-truncated")?;
        return Err(CaptureError::Conflict(
            "Codex history was replaced or truncated".into(),
        ));
    }
    let mut next = previous.clone().unwrap_or_else(|| VisibleHistoryCursorV1 {
        harness: request.harness,
        file: ProviderHistoryCursorV1 {
            path_digest,
            file_device: opened.metadata.dev(),
            file_inode: opened.metadata.ino(),
            work_id: request.work_id.clone(),
            generation: request.generation,
            canonical_session_id: request.canonical_session_id.clone(),
            offset: 0,
        },
        header_digest: opened.header_digest.clone(),
        line_number: 0,
        model: None,
        questions: QuestionSnapshot::default(),
        questions_complete: true,
    });
    if previous.is_none() {
        // Pin the validated source before a slow first page. A hook that times out waiting for
        // that page must not race it by recording the same tool with weaker hook-only facts.
        // Offset zero consumes no history; later progress still follows durable event batches.
        spool.advance_visible_history_cursor(&request.canonical_session_id, None, next.clone())?;
        previous = Some(next.clone());
    }
    let mut report = RolloutCaptureReport::default();
    let own_from = opened.own_from;
    let page = read_visible_page(
        &mut opened,
        next.file.offset,
        next.line_number,
        |number, raw| {
            let line = serde_json::from_slice::<RolloutLine>(raw).ok();
            let Some(line) = line else {
                record_provider_history_gap(
                    request,
                    spool,
                    &format!("codex-malformed-line-{number}"),
                )?;
                report.report.gaps += 1;
                next.questions_complete = false;
                return Ok(());
            };
            if !own_record(&line, own_from, &request.observed_session_id)? {
                return Ok(());
            }
            let (Some(kind), Some(payload)) = (line.kind.as_deref(), line.payload.as_ref()) else {
                return Ok(());
            };
            observe_questions(
                &line,
                &mut next.questions,
                &mut next.questions_complete,
                request.live_capture_mode != CaptureModeV1::MetadataOnly,
            );
            if kind == "turn_context" {
                next.model = payload["model"]
                    .as_str()
                    .filter(|model| identifier(model, 255))
                    .map(str::to_owned);
            } else if kind == "token_usage_record"
                && let Some(usage) = payload["turn_id"].as_str().and_then(|turn| {
                    usage_record(turn, next.model.as_deref(), &payload["usage"], request)
                })
            {
                match spool.append(&CaptureEnvelopeV1::Usage(usage)) {
                    Ok(AppendOutcome::Stored) => report.usage_observations += 1,
                    Ok(AppendOutcome::Duplicate) | Err(CaptureError::Conflict(_)) => {}
                    Err(error) => return Err(error),
                }
            }
            let Some(observed_at) = line.timestamp.as_deref().and_then(parse_rfc3339) else {
                return Ok(());
            };
            let place = line
                .ordinal
                .map_or_else(|| format!("line{number}"), |ordinal| ordinal.to_string());
            let (events, ignored) = select(kind, payload, observed_at, &place);
            let events: Vec<_> = events
                .into_iter()
                .map(|event| automatic_history_event(event, request))
                .collect();
            report.report.private_blocks_ignored += ignored;
            if events.is_empty() {
                return Ok(());
            }
            let is_tool = matches!(
                payload["item"]["type"].as_str(),
                Some("CommandExecution" | "FileChange" | "McpToolCall" | "WebSearch")
            );
            let tool_id = payload["item"]["id"]
                .as_str()
                .filter(|id| identifier(id, 255));
            if is_tool && let Some(tool_id) = tool_id {
                let _managed = spool.lock_managed_capture()?;
                for (event, phase) in events.into_iter().zip(["pre", "post"]) {
                    if native_tool_phase(spool, request, tool_id, phase)? {
                        continue;
                    }
                    let operation = tool_operation(
                        &request.work_id,
                        request.generation,
                        &request.canonical_session_id,
                        &request.observed_session_id,
                        tool_id,
                        phase,
                    );
                    spool.bind_observation(&operation, &sha256(raw))?;
                    add_report(
                        &mut report.report,
                        capture_history_record(vec![event], request, spool, &operation)?,
                    );
                }
            } else {
                let identity = format!(
                    "{}\0{}\0{}\0{}\0{place}",
                    request.work_id,
                    request.generation,
                    request.canonical_session_id,
                    request.observed_session_id
                );
                let operation = format!("visible:history:{}:record", sha256(identity.as_bytes()));
                spool.bind_observation(&operation, &sha256(raw))?;
                add_report(
                    &mut report.report,
                    capture_history_record(events, request, spool, &operation)?,
                );
            }
            Ok(())
        },
    );
    let page = match page {
        Ok(page) => page,
        Err(error) => {
            record_provider_history_gap(
                request,
                spool,
                &format!("codex-history-read-{}", next.file.offset),
            )?;
            return Err(error);
        }
    };
    next.file.offset = page.offset;
    next.line_number = page.line_number;
    next.questions.offset = page.offset;
    report.complete = page.complete;
    report.records = page.records;
    report.questions = next.questions.clone();
    report.questions_complete = next.questions_complete;
    spool.advance_visible_history_cursor(&request.canonical_session_id, previous.as_ref(), next)?;
    Ok(report)
}

/// A caller-held question scan position. It retains only bounded question metadata, never the
/// surrounding conversation. A new MCP process starts a new bounded backfill.
#[derive(Clone, Debug)]
pub struct QuestionCursor {
    path_digest: String,
    file_inode: u64,
    header_digest: String,
    line_number: u64,
    snapshot: QuestionSnapshot,
    reliable: bool,
}

#[derive(Clone, Debug)]
pub struct QuestionReport {
    pub snapshot: QuestionSnapshot,
    pub complete: bool,
    pub reliable: bool,
    pub records: usize,
}

/// Reads native async questions independently of conversation sharing. Only their IDs and
/// timestamps are retained here; the question's text and answer never enter this cursor.
pub fn poll_questions(
    path: &Path,
    native_session: &str,
    cursor: &mut Option<QuestionCursor>,
) -> Result<QuestionReport> {
    let mut opened = open_rollout(path, native_session)?;
    let path_digest = sha256(opened.path.as_os_str().as_encoded_bytes());
    if let Some(previous) = cursor.as_ref()
        && (previous.path_digest != path_digest
            || previous.file_inode != opened.metadata.ino()
            || previous.header_digest != opened.header_digest
            || previous.snapshot.offset > opened.metadata.len())
    {
        return Err(CaptureError::Conflict(
            "Codex question history was replaced or truncated".into(),
        ));
    }
    let mut next = cursor.clone().unwrap_or_else(|| QuestionCursor {
        path_digest,
        file_inode: opened.metadata.ino(),
        header_digest: opened.header_digest.clone(),
        line_number: 0,
        snapshot: QuestionSnapshot::default(),
        reliable: true,
    });
    let own_from = opened.own_from;
    let page = read_visible_page(
        &mut opened,
        next.snapshot.offset,
        next.line_number,
        |_, raw| {
            let line = serde_json::from_slice::<RolloutLine>(raw).ok();
            match line {
                Some(line) if own_record(&line, own_from, native_session)? => {
                    observe_questions(&line, &mut next.snapshot, &mut next.reliable, false);
                }
                Some(_) => {}
                None => next.reliable = false,
            }
            Ok(())
        },
    )?;
    next.line_number = page.line_number;
    next.snapshot.offset = page.offset;
    let report = QuestionReport {
        snapshot: next.snapshot.clone(),
        complete: page.complete,
        reliable: next.reliable,
        records: page.records,
    };
    *cursor = Some(next);
    Ok(report)
}

fn observe_questions(
    line: &RolloutLine,
    snapshot: &mut QuestionSnapshot,
    reliable: &mut bool,
    retain_excerpt: bool,
) {
    if line.kind.as_deref() != Some("event_msg") {
        return;
    }
    let Some(payload) = line
        .payload
        .as_ref()
        .filter(|payload| payload["type"] == "item_completed")
    else {
        return;
    };
    let item = &payload["item"];
    if item["type"] == "AgentMessage" && item.get("questions").is_some() {
        let Some(questions) = item["questions"].as_array() else {
            *reliable = false;
            return;
        };
        let Some(call) = item["id"].as_str().filter(|id| identifier(id, 256)) else {
            *reliable = false;
            return;
        };
        let Some(since) = line.timestamp.as_deref().and_then(parse_rfc3339) else {
            *reliable = false;
            return;
        };
        for (index, question) in questions.iter().enumerate() {
            let id = json!(["request_user_input_async", call, index]).to_string();
            if snapshot.pending.iter().any(|pending| pending.id == id) {
                continue;
            }
            if id.len() > 512 || snapshot.pending.len() >= 256 {
                *reliable = false;
                break;
            }
            let Some(title) = question["title"].as_str() else {
                *reliable = false;
                continue;
            };
            let asks = retain_excerpt
                .then(|| {
                    let mut text = json!(
                        title
                            .chars()
                            .filter(|ch| !ch.is_control())
                            .take(400)
                            .collect::<String>()
                    );
                    redact_json(&mut text);
                    text.as_str().unwrap_or_default().to_owned()
                })
                .filter(|text| !text.is_empty());
            snapshot.pending.push(PendingQuestion { id, asks, since });
        }
    } else if item["type"] == "UserMessage" {
        for text in item["content"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|block| block["text"].as_str())
        {
            let text = text.trim();
            const OPEN: &str = "<send_user_message_question_reply>";
            const CLOSE: &str = "</send_user_message_question_reply>";
            if !text.starts_with(OPEN) {
                continue;
            }
            let Some(answer) = text
                .strip_prefix(OPEN)
                .and_then(|text| text.strip_suffix(CLOSE))
            else {
                *reliable = false;
                continue;
            };
            let Ok(Value::Array(answers)) = serde_json::from_str::<Value>(answer) else {
                *reliable = false;
                continue;
            };
            for answer in answers {
                let Some(id) = answer["questionItemId"].as_str() else {
                    *reliable = false;
                    continue;
                };
                let Ok(Value::Array(parts)) = serde_json::from_str::<Value>(id) else {
                    *reliable = false;
                    continue;
                };
                if parts.len() != 3
                    || parts[0] != "request_user_input_async"
                    || parts[1].as_str().is_none_or(|call| !identifier(call, 256))
                    || parts[2].as_u64().is_none()
                    || !answer["answer"].is_string()
                {
                    *reliable = false;
                    continue;
                }
                let canonical = Value::Array(parts).to_string();
                snapshot.pending.retain(|pending| pending.id != canonical);
            }
        }
    }
}

/// What live hooks can record for Codex. Checked against Codex CLI 0.167.1: its hook input schemas,
/// a session run with these hooks, and the session files it and four earlier versions wrote.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Capabilities {
    /// `UserPromptSubmit` carries the prompt.
    pub user_prompts: bool,
    /// `Stop` carries the last assistant message of the turn.
    pub turn_final_assistant_messages: bool,
    /// No hook carries an assistant message written before the turn's last one.
    pub interim_assistant_messages: bool,
    pub tool_starts: bool,
    /// `PostToolUse` carries the tool's response. Codex runs it for a shell command whatever its
    /// exit code, and not for an MCP call that failed or a call a hook refused: those are recorded
    /// as started only.
    pub tool_finishes: bool,
    /// No hook says that a tool call failed: a shell command's response is its output, with no
    /// exit code, so a finished call is recorded as returned.
    pub tool_failures: bool,
    /// Hosted tools, such as web search, run no hook.
    pub hosted_tool_calls: bool,
    /// `PostModelRequest` carries the token counts of one model request.
    pub request_token_usage: bool,
    /// `SessionEnd` fires, with no reason other than `other`.
    pub session_end: bool,
    /// Reasoning is stored encrypted and is never read.
    pub reasoning: bool,
    /// `import-codex` reads a session file: prompts, every assistant message, commands, file
    /// changes, MCP calls and web searches with their results and whether each failed, and token
    /// usage.
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
/// prompts, assistant messages, commands, file changes, MCP calls and web searches with their
/// results, and each model request's token usage. Reasoning, images and the instructions Codex
/// adds around a prompt are not selected. A subagent's file starts with a copy of its parent's
/// history, which is left to the parent's own file. Returns the report and the number of usage
/// observations newly recorded.
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
        // A record's place in the file names its events.
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
/// Messages and tool calls come from Codex's typed records (`item_completed`): each names a
/// command, file change, MCP call or web search with the id its hooks use, and says whether it
/// failed. The model-level records beside them are not selected; in recent versions they are
/// scripts that wrap those calls.
fn select(
    kind: &str,
    payload: &Value,
    observed_at: i64,
    place: &str,
) -> (Vec<SelectedEvent>, usize) {
    if kind == "response_item" && payload["type"] == "reasoning" {
        return (Vec::new(), 1);
    }
    if kind != "event_msg" || payload["type"] != "item_completed" {
        return (Vec::new(), 0);
    }
    let item = &payload["item"];
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
    let messages = |kind| {
        let mut visible = texts(&item["content"]);
        if item["type"] == "AgentMessage" {
            for question in item["questions"].as_array().into_iter().flatten().take(256) {
                if let Some(title) = question["title"].as_str() {
                    let mut question_text = title.to_owned();
                    for option in question["options"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .filter_map(Value::as_str)
                    {
                        question_text.push_str("\n- ");
                        question_text.push_str(option);
                    }
                    visible.push(json!(question_text));
                }
            }
        } else if item["type"] == "UserMessage" {
            for text in &mut visible {
                if let Some(answer) = text.as_str().and_then(visible_question_answer) {
                    *text = json!(answer);
                }
            }
        }
        visible
            .into_iter()
            .filter(|text| text.as_str().is_some_and(|text| !text.is_empty()))
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
    // A tool record's kind, its name where the record gives one, its input and its result.
    let (tool, name, input, result) = match item["type"].as_str() {
        Some("Reasoning") => return (Vec::new(), 1),
        Some("UserMessage") => return (messages(CapturedEventKindV1::UserPrompt), 0),
        Some("AgentMessage") => return (messages(CapturedEventKindV1::AssistantMessage), 0),
        Some("CommandExecution") => (
            ToolKindV1::Shell,
            None,
            json!({"command": item["command"], "parsed_cmd": item["parsed_cmd"]}),
            json!({"output": item["aggregated_output"], "exit_code": item["exit_code"]}),
        ),
        Some("FileChange") => (
            ToolKindV1::Edit,
            None,
            json!({"changes": item["changes"]}),
            json!({"stdout": item["stdout"], "stderr": item["stderr"]}),
        ),
        Some("McpToolCall") => (
            ToolKindV1::Other,
            // As Codex's hooks name an MCP tool.
            item["server"]
                .as_str()
                .zip(item["tool"].as_str())
                .map(|(server, tool)| format!("mcp__{server}__{tool}")),
            item["arguments"].clone(),
            // Text only: a result's images are not selected.
            json!(texts(&item["result"]["content"])),
        ),
        Some("WebSearch") => (
            ToolKindV1::Search,
            None,
            json!({"query": item["query"]}),
            Value::Null,
        ),
        _ => return (Vec::new(), 0),
    };
    let failed = item["status"] == "failed" || item["result"]["isError"] == true;
    // A record with another status, or with none (a web search), does not say how it ended.
    let completion = if failed {
        Some(ToolCompletionV1::Failed)
    } else {
        (item["status"] == "completed").then_some(ToolCompletionV1::Returned)
    };
    let fact = |completion| ToolFactV1 {
        kind: tool,
        request_id: item["id"]
            .as_str()
            .filter(|id| identifier(id, 255))
            .map(str::to_owned),
        name: name.clone().filter(|name| identifier(name, 128)),
        completion,
    };
    // The record is written when the call ends and says when it began.
    let started_at = payload["started_at_ms"]
        .as_i64()
        .map(|milliseconds| milliseconds / 1000)
        .filter(|started| (1..=observed_at).contains(started))
        .unwrap_or(observed_at);
    let events = vec![
        SelectedEvent::history(
            event_id(0),
            started_at,
            CapturedEventKindV1::ToolStarted,
            Some(fact(None)),
            json!({"tool_name": name, "tool_input": input}),
        ),
        SelectedEvent::history(
            event_id(1),
            observed_at,
            CapturedEventKindV1::ToolFinished,
            Some(fact(completion)),
            json!({"tool_result": result, "is_error": failed}),
        ),
    ];
    (events, 0)
}

fn visible_question_answer(text: &str) -> Option<String> {
    let body = text
        .trim()
        .strip_prefix("<send_user_message_question_reply>")?
        .strip_suffix("</send_user_message_question_reply>")?;
    let answers: Vec<Value> = serde_json::from_str(body).ok()?;
    let mut visible = Vec::new();
    for answer in answers {
        let chosen = answer["answer"].as_str()?;
        visible.push(match answer["question"].as_str() {
            Some(question) => format!("{question}\n{chosen}"),
            None => chosen.to_owned(),
        });
    }
    Some(visible.join("\n\n"))
}
