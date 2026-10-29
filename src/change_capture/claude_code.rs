//! Claude Code capture adapter.
//!
//! The hook path consumes actual native hook JSON one event at a time. The import path reads the
//! user's own Claude Code JSONL and deliberately selects only visible user/assistant text and
//! tool events. Thinking blocks, attachments, request caches and unrelated record types are not
//! retained.

use std::fs::OpenOptions;
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::change_capture::redact::redact_json;
use crate::change_capture::spool::{
    LocalSpool, ManagedInvocationV1, NativeOccurrenceReceiptV1, NativeOccurrenceRouteV1,
    ProviderHistoryCursorV1,
};
use crate::change_capture::{
    AppendOutcome, CaptureError, DeferredHookV1, DeferredReasonV1, GitSourceRequest, OpenedGitRoot,
    Result, capture_git_source, capture_git_source_opened, now_seconds,
};
use crate::change_record::{
    ActionFinishV1, ActionKindV1, ActionOutcomeV1, ActionStartV1, AgentHarnessV1, ArtifactMediaV1,
    CaptureBatchV1, CaptureEnvelopeV1, CaptureModeV1, CapturedEventKindV1, CapturedEventV1,
    CheckResultV1, CoverageV1, DriftV1, GapFactV1, GapReasonV1, MAX_SHARED_BODY_BYTES,
    RecipeProvenanceV1, RetentionClassV1, SCHEMA_VERSION_V1, SessionIdentityV1, SetupComponentV1,
    SetupObservationV1, SubagentFactV1, ToolCompletionV1, ToolFactV1, ToolKindV1,
};
use crate::checks::sha256;
use crate::hub::github::parse_rfc3339;
use crate::hub::insights::{
    AttemptOutcome, AttemptPhase, UsageBasis, UsageMeasurementScope, UsageRecord,
};

pub const ADAPTER_VERSION: &str = "1.0.0";
const MAX_HOOK_BYTES: usize = 1024 * 1024;
const MAX_JSONL_LINE_BYTES: usize = 4 * 1024 * 1024;
const MAX_IMPORT_BYTES: u64 = 64 * 1024 * 1024;
const MAX_IMPORT_LINES: usize = 100_000;
const MAX_HISTORY_TAIL_BYTES: u64 = 4 * 1024 * 1024;
const MAX_HISTORY_TAIL_LINES: usize = 1_000;
const MAX_ACTION_RULES: usize = 32;

#[derive(Clone, Debug)]
pub struct CaptureRequest {
    pub operation_prefix: String,
    pub work_id: String,
    pub generation: i64,
    /// Native session identity observed in the hook payload.
    pub observed_session_id: String,
    /// Hub-authoritative session identity after alias resolution.
    pub canonical_session_id: String,
    /// Pinned with `capture_mode` per Work, generation and canonical session.
    pub parent_session_id: Option<String>,
    /// Session identity sent to the Hub. Pinned so a later binding change cannot alter it.
    pub capture_mode: CaptureModeV1,
    /// The binding's current mode. It alone decides whether bodies are retained.
    pub live_capture_mode: CaptureModeV1,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ActionRuleV1 {
    pub native_tool: String,
    pub kind: ActionKindV1,
    pub coverage: CoverageV1,
    pub result_source: NativeResultSourceV1,
    pub recipe_digest: Option<String>,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum NativeResultSourceV1 {
    /// Parse only an explicit integer `exit_code` or `exitCode` in the native tool response.
    ProcessExitCode,
    /// Record completion without treating tool transport success as verification success.
    Unverified,
}

#[derive(Clone, Debug)]
pub struct ActionCaptureConfig {
    pub source: GitSourceRequest,
    pub setup_digest: String,
    pub rules: Vec<ActionRuleV1>,
    /// Exact Bash command strings that invoke the registered recipe runner. These hooks retain
    /// lifecycle facts but do not create a second generic native action receipt.
    pub managed_invocations: Vec<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NativeActionOutcome {
    NotApplicable,
    Started(AppendOutcome),
    Finished(AppendOutcome),
    MissingStart,
    Interrupted,
}

#[derive(Clone, Debug)]
pub struct NativeHookReport {
    pub event: HookCaptureOutcome,
    pub action: NativeActionOutcome,
    pub warnings: Vec<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HookCaptureOutcome {
    Stored,
    Duplicate,
    Ignored,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CaptureReport {
    pub batches: usize,
    pub events: usize,
    pub gaps: usize,
    pub private_blocks_ignored: usize,
}

#[derive(Deserialize)]
struct TranscriptLine {
    #[serde(rename = "type")]
    kind: Option<String>,
    #[serde(rename = "sessionId", alias = "session_id")]
    session_id: Option<String>,
    timestamp: Option<String>,
    uuid: Option<String>,
    #[serde(alias = "requestId")]
    request_id: Option<String>,
    message: Option<TranscriptMessage>,
    usage: Option<Value>,
}

#[derive(Deserialize)]
struct TranscriptMessage {
    content: Option<Value>,
    model: Option<String>,
    usage: Option<Value>,
}

struct SelectedEvent {
    event_id: String,
    observed_at: i64,
    kind: CapturedEventKindV1,
    tool: Option<ToolFactV1>,
    local_body: Option<Value>,
    setup: Option<SetupObservationV1>,
    boundary: Option<String>,
    subagent: Option<SubagentFactV1>,
}

#[derive(Serialize)]
struct NativeRecipe<'a> {
    schema_version: u16,
    adapter_version: &'static str,
    native_tool: &'a str,
    tool_input: Option<&'a Value>,
}

enum ReadLine {
    End,
    Limit,
    Oversized(u64),
    Bytes(u64),
}

#[derive(Clone, Copy)]
struct HookIdentityOverride<'a> {
    event_id: &'a str,
    operation_id: &'a str,
}

pub fn capture_hook(
    input: &[u8],
    request: &CaptureRequest,
    spool: &LocalSpool,
) -> Result<HookCaptureOutcome> {
    let outcome = capture_hook_with_identity(input, request, spool, None);
    for warning in take_hook_warnings() {
        eprintln!("agentscm: {warning}");
    }
    outcome
}

thread_local! {
    // Non-fatal capture notes from the shared hook path, surfaced through `NativeHookReport`.
    static HOOK_WARNINGS: std::cell::RefCell<Vec<String>> = const { std::cell::RefCell::new(Vec::new()) };
}

fn take_hook_warnings() -> Vec<String> {
    HOOK_WARNINGS.with(|warnings| std::mem::take(&mut *warnings.borrow_mut()))
}

fn capture_hook_with_identity(
    input: &[u8],
    request: &CaptureRequest,
    spool: &LocalSpool,
    identity_override: Option<HookIdentityOverride<'_>>,
) -> Result<HookCaptureOutcome> {
    capture_hook_with_identity_mode(input, request, spool, identity_override, true)
}

fn capture_hook_with_identity_mode(
    input: &[u8],
    request: &CaptureRequest,
    spool: &LocalSpool,
    identity_override: Option<HookIdentityOverride<'_>>,
    update_current_association: bool,
) -> Result<HookCaptureOutcome> {
    if input.len() > MAX_HOOK_BYTES {
        return Err(CaptureError::Limit(format!(
            "Claude Code hook input exceeds {MAX_HOOK_BYTES} bytes"
        )));
    }
    let payload: Value = serde_json::from_slice(input)?;
    let payload = payload
        .as_object()
        .ok_or_else(|| CaptureError::Invalid("Claude Code hook input must be an object".into()))?;
    let event_name = payload
        .get("hook_event_name")
        .and_then(Value::as_str)
        .ok_or_else(|| CaptureError::Invalid("Claude Code hook event name is missing".into()))?;
    let session_id = payload
        .get("session_id")
        .and_then(Value::as_str)
        .ok_or_else(|| CaptureError::Invalid("Claude Code hook session id is missing".into()))?;
    if session_id != request.observed_session_id {
        return Err(CaptureError::Invalid(
            "Claude Code hook session differs from the configured capture session".into(),
        ));
    }
    if update_current_association {
        spool.remember_session_association(
            &request.observed_session_id,
            &request.canonical_session_id,
            &request.work_id,
            request.generation,
            request.parent_session_id.as_deref(),
            request.capture_mode,
        )?;
        ensure_deferred_gaps(&request.observed_session_id, spool)?;
    }
    let provider_timestamp = payload.get("timestamp").and_then(Value::as_str);
    let tool_name = payload.get("tool_name").and_then(Value::as_str);
    let provider_id = [
        "tool_use_id",
        "prompt_id",
        "message_id",
        "uuid",
        "request_id",
        "agent_id",
    ]
    .iter()
    .find_map(|key| payload.get(*key).and_then(Value::as_str));
    let (kind, tool, local_body) = match event_name {
        "SessionStart" => (CapturedEventKindV1::SessionStarted, None, None),
        "UserPromptSubmit" => (
            CapturedEventKindV1::UserPrompt,
            None,
            payload
                .get("prompt")
                .cloned()
                .map(|content| json!({"content": content})),
        ),
        "PreToolUse" => (
            check_kind(CapturedEventKindV1::ToolStarted, tool_name),
            Some(tool_fact(tool_name, provider_id, None)),
            Some(json!({
                "tool_name": tool_name,
                "tool_input": payload.get("tool_input"),
            })),
        ),
        "PostToolUse" | "PostToolUseFailure" => (
            check_kind(CapturedEventKindV1::ToolFinished, tool_name),
            Some(tool_fact(
                tool_name,
                provider_id,
                Some(if event_name == "PostToolUse" {
                    ToolCompletionV1::Returned
                } else {
                    ToolCompletionV1::Failed
                }),
            )),
            Some(json!({
                "tool_name": tool_name,
                "tool_response": payload.get("tool_response"),
                "error": payload.get("error"),
            })),
        ),
        "MessageDisplay" => (
            CapturedEventKindV1::AssistantMessage,
            None,
            payload.get("delta").cloned().map(|content| {
                json!({
                    "content": content,
                    "message_id": payload.get("message_id"),
                    "index": payload.get("index"),
                    "final": payload.get("final"),
                })
            }),
        ),
        "SessionEnd" => (CapturedEventKindV1::SessionEnded, None, None),
        "Stop" => (CapturedEventKindV1::TurnEnded, None, None),
        "PreCompact" => (CapturedEventKindV1::Compaction, None, None),
        "SubagentStart" => (CapturedEventKindV1::SubagentStarted, None, None),
        "SubagentStop" => (CapturedEventKindV1::SubagentStopped, None, None),
        _ => return Ok(HookCaptureOutcome::Ignored),
    };
    let boundary = hook_boundary(event_name, payload);
    let subagent = hook_subagent(event_name, payload);
    if subagent.is_none()
        && matches!(
            kind,
            CapturedEventKindV1::SubagentStarted | CapturedEventKindV1::SubagentStopped
        )
    {
        return Ok(HookCaptureOutcome::Ignored);
    }
    let starts_new_session = event_name == "SessionStart"
        && payload.get("source").and_then(Value::as_str) == Some("startup");
    if update_current_association
        && !starts_new_session
        && !spool.session_origin_observed(&request.canonical_session_id)?
    {
        // The marker is retried on every hook until the origin is seen, so a failure here must
        // not cost the hook's own event.
        if let Err(error) = ensure_adapter_started_late_gap(request, spool) {
            HOOK_WARNINGS.with(|warnings| {
                warnings
                    .borrow_mut()
                    .push(format!("adapter-started-late marker not retained: {error}"));
            });
        }
    }
    let identity_input = serde_json::to_vec(&json!({
        "session": session_id,
        "event": event_name,
        "provider_id": provider_id,
        "tool": tool_name,
        "provider_timestamp": provider_timestamp,
        "message_index": payload.get("index"),
        "message_final": payload.get("final"),
    }))?;
    let unstable_identity =
        identity_override.is_none() && provider_id.is_none() && provider_timestamp.is_none();
    // A replayed turn, compaction or subagent marker is informational and cannot hide work, so
    // it needs a unique occurrence id but not an identity-uncertainty gap on every turn.
    let identity_uncertain = unstable_identity
        && !matches!(
            kind,
            CapturedEventKindV1::TurnEnded
                | CapturedEventKindV1::Compaction
                | CapturedEventKindV1::SubagentStarted
                | CapturedEventKindV1::SubagentStopped
        );
    let event_id = match identity_override {
        Some(identity) => identity.event_id.to_owned(),
        None if unstable_identity => {
            spool.uncertain_event_id(&request.canonical_session_id, &sha256(&identity_input))?
        }
        None => format!("evt:{}", sha256(&identity_input)),
    };
    let operation_id = identity_override.map_or_else(
        || format!("cap:{}", sha256(event_id.as_bytes())),
        |identity| identity.operation_id.to_owned(),
    );
    let provider_time = provider_timestamp.and_then(parse_rfc3339);
    let observed_at = match provider_time {
        Some(time) => time,
        None => now_seconds()?,
    };
    let selected = SelectedEvent {
        event_id: event_id.clone(),
        observed_at,
        kind,
        tool,
        local_body,
        setup: native_setup_observation(payload, spool)?,
        boundary,
        subagent,
    };
    let Retained {
        event,
        retention_missing,
        redacted_gap,
    } = retain_selected(selected, request, spool)?;
    let mut events = vec![event];
    events.extend(redacted_gap);
    if identity_uncertain {
        let gap_id = format!("gap:{}", sha256(format!("{event_id}\0identity").as_bytes()));
        let sequence = spool.sequence_for(&request.canonical_session_id, &gap_id)?;
        events.push(CapturedEventV1 {
            event_id: gap_id,
            sequence,
            observed_at,
            kind: CapturedEventKindV1::Gap,
            tool: None,
            artifact: None,
            setup: None,
            gap: Some(GapFactV1 {
                first_missing: sequence,
                last_missing: sequence,
                reason: GapReasonV1::IdentityUncertain,
            }),
            boundary: None,
            subagent: None,
        });
    }
    if retention_missing {
        let gap_id = format!(
            "gap:{}",
            sha256(format!("{event_id}\0local-retention").as_bytes())
        );
        let sequence = spool.sequence_for(&request.canonical_session_id, &gap_id)?;
        events.push(CapturedEventV1 {
            event_id: gap_id,
            sequence,
            observed_at,
            kind: CapturedEventKindV1::Gap,
            tool: None,
            artifact: None,
            setup: None,
            gap: Some(GapFactV1 {
                first_missing: sequence,
                last_missing: sequence,
                reason: GapReasonV1::CaptureInterrupted,
            }),
            boundary: None,
            subagent: None,
        });
    }
    let first_missing = events.iter().map(|event| event.sequence).min().unwrap_or(1);
    let last_missing = events
        .iter()
        .map(|event| event.sequence)
        .max()
        .unwrap_or(first_missing);
    let envelope = CaptureEnvelopeV1::EventBatch(batch(request, operation_id.clone(), events));
    let append = |envelope: &CaptureEnvelopeV1| match provider_time {
        Some(_) => spool.append(envelope),
        None => spool.append_at_receipt(envelope),
    };
    let appended = match append(&envelope) {
        Ok(outcome) => outcome,
        Err(CaptureError::Limit(_)) => {
            let gap_id = format!(
                "gap:{}",
                sha256(format!("{operation_id}\0quota-paused").as_bytes())
            );
            let gap = CapturedEventV1 {
                event_id: gap_id.clone(),
                sequence: first_missing,
                observed_at,
                kind: CapturedEventKindV1::Gap,
                tool: None,
                artifact: None,
                setup: None,
                gap: Some(GapFactV1 {
                    first_missing,
                    last_missing,
                    reason: GapReasonV1::CaptureInterrupted,
                }),
                boundary: None,
                subagent: None,
            };
            let gap = CaptureEnvelopeV1::EventBatch(batch(
                request,
                format!("cap:{}", sha256(gap_id.as_bytes())),
                vec![gap],
            ));
            match provider_time {
                Some(_) => spool.append_priority(&gap)?,
                None => spool.append_priority_at_receipt(&gap)?,
            }
        }
        Err(error) => return Err(error),
    };
    let outcome = match appended {
        crate::change_capture::AppendOutcome::Stored => HookCaptureOutcome::Stored,
        crate::change_capture::AppendOutcome::Duplicate => HookCaptureOutcome::Duplicate,
    };
    if starts_new_session {
        spool.mark_session_origin_observed(&request.canonical_session_id)?;
    }
    Ok(outcome)
}

/// Retains a sanitized local fact when the Hub cannot resolve an exact authorized target for the
/// native session. This path never creates a Hub envelope and never retains prompt, tool input,
/// tool output, error or cwd fields. A PreToolUse record binds the source immediately when that
/// source can be captured.
pub fn deferred_native_hook(
    input: &[u8],
    reason: DeferredReasonV1,
    actions: &ActionCaptureConfig,
    spool: &LocalSpool,
) -> Result<AppendOutcome> {
    validate_action_config(actions)?;
    if input.len() > MAX_HOOK_BYTES {
        return Err(CaptureError::Limit(format!(
            "Claude Code hook input exceeds {MAX_HOOK_BYTES} bytes"
        )));
    }
    let payload: Value = serde_json::from_slice(input)?;
    let payload = payload
        .as_object()
        .ok_or_else(|| CaptureError::Invalid("Claude Code hook input must be an object".into()))?;
    let event_name = payload
        .get("hook_event_name")
        .and_then(Value::as_str)
        .ok_or_else(|| CaptureError::Invalid("Claude Code hook event name is missing".into()))?;
    let session_id = payload
        .get("session_id")
        .and_then(Value::as_str)
        .filter(|value| identifier(value, 255))
        .ok_or_else(|| CaptureError::Invalid("Claude Code hook session id is invalid".into()))?;
    let tool_name = payload.get("tool_name").and_then(Value::as_str);
    let event_kind = match event_name {
        "SessionStart" => Some(CapturedEventKindV1::SessionStarted),
        "UserPromptSubmit" => Some(CapturedEventKindV1::UserPrompt),
        "PreToolUse" => Some(check_kind(CapturedEventKindV1::ToolStarted, tool_name)),
        "PostToolUse" | "PostToolUseFailure" => {
            Some(check_kind(CapturedEventKindV1::ToolFinished, tool_name))
        }
        "MessageDisplay" => Some(CapturedEventKindV1::AssistantMessage),
        "SessionEnd" => Some(CapturedEventKindV1::SessionEnded),
        "Stop" => Some(CapturedEventKindV1::TurnEnded),
        "PreCompact" => Some(CapturedEventKindV1::Compaction),
        "SubagentStart" => Some(CapturedEventKindV1::SubagentStarted),
        "SubagentStop" => Some(CapturedEventKindV1::SubagentStopped),
        _ => None,
    };
    let tool_kind = matches!(
        event_kind,
        Some(
            CapturedEventKindV1::ToolStarted
                | CapturedEventKindV1::ToolFinished
                | CapturedEventKindV1::CheckStarted
                | CapturedEventKindV1::CheckFinished
        )
    )
    .then(|| tool_kind(tool_name));
    let provider_timestamp = payload.get("timestamp").and_then(Value::as_str);
    let provider_id = [
        "tool_use_id",
        "prompt_id",
        "message_id",
        "uuid",
        "request_id",
        "agent_id",
    ]
    .iter()
    .find_map(|key| payload.get(*key).and_then(Value::as_str));
    let identity = serde_json::to_vec(&json!({
        "session": session_id,
        "event": event_name,
        "provider_id": provider_id,
        "tool": tool_name,
        "provider_timestamp": provider_timestamp,
        "message_index": payload.get("index"),
        "message_final": payload.get("final"),
    }))?;
    let occurrence_id = if provider_id.is_none() && provider_timestamp.is_none() {
        spool.uncertain_event_id(session_id, &sha256(&identity))?
    } else {
        format!("evt:{}", sha256(&identity))
    };
    let reserved_sequence = spool.sequence_for(session_id, &occurrence_id)?;
    let input_digest = sha256(&serde_json::to_vec(payload)?);
    let (retained_source, recipe_digest, action_kind) = if event_name == "PreToolUse" {
        let native_tool = tool_name.filter(|value| identifier(value, 128));
        if native_tool
            .is_some_and(|tool| managed_invocation(tool, payload.get("tool_input"), actions))
        {
            (None, None, None)
        } else {
            let recipe_digest = native_tool
                .map(|tool| native_recipe_digest(tool, payload.get("tool_input")))
                .transpose()?;
            let action_kind = native_tool.and_then(|tool| {
                recipe_digest.as_deref().and_then(|digest| {
                    actions
                        .rules
                        .iter()
                        .find(|rule| {
                            rule.native_tool == tool
                                && rule.kind == ActionKindV1::Verification
                                && rule.recipe_digest.as_deref() == Some(digest)
                        })
                        .or_else(|| {
                            actions.rules.iter().find(|rule| {
                                rule.native_tool == tool
                                    && rule.kind == ActionKindV1::Execution
                                    && rule.recipe_digest.is_none()
                            })
                        })
                        .map(|rule| rule.kind)
                })
            });
            (
                capture_git_source(&actions.source, spool).ok(),
                recipe_digest,
                action_kind,
            )
        }
    } else {
        (None, None, None)
    };
    spool.retain_deferred(&DeferredHookV1 {
        occurrence_id,
        reserved_sequence: Some(reserved_sequence),
        session_id: session_id.to_owned(),
        hook_event_name: event_name.to_owned(),
        observed_at: match provider_timestamp.and_then(parse_rfc3339) {
            Some(time) => time,
            None => now_seconds()?,
        },
        reason,
        input_digest,
        event_kind,
        tool_kind,
        source: retained_source,
        recipe_digest,
        action_kind,
    })
}

fn ensure_deferred_gaps(observed_session_id: &str, spool: &LocalSpool) -> Result<()> {
    for deferred in spool.deferred_gaps(observed_session_id)? {
        // Without an alias, the reserved observed-session sequence is already in the canonical
        // stream. The next retained event exposes that exact hole; adding another explicit gap
        // would double-count the same missed occurrence.
        if deferred.target.observed_session_id == deferred.target.canonical_session_id {
            continue;
        }
        let event_id = format!(
            "gap:{}",
            sha256(
                format!(
                    "{}\0{}\0{}\0{}\0deferred",
                    deferred.target.canonical_session_id,
                    deferred.target.work_id,
                    deferred.target.generation,
                    deferred.occurrence_id
                )
                .as_bytes()
            )
        );
        let sequence = spool.sequence_for(&deferred.target.canonical_session_id, &event_id)?;
        let request = CaptureRequest {
            operation_prefix: format!("capture:deferred:{}", deferred.reserved_sequence),
            work_id: deferred.target.work_id,
            generation: deferred.target.generation,
            observed_session_id: deferred.target.observed_session_id,
            canonical_session_id: deferred.target.canonical_session_id,
            parent_session_id: deferred.target.parent_session_id,
            capture_mode: deferred.target.capture_mode,
            live_capture_mode: deferred.target.capture_mode,
        };
        let event = CapturedEventV1 {
            event_id: event_id.clone(),
            sequence,
            observed_at: deferred.observed_at,
            kind: CapturedEventKindV1::Gap,
            tool: None,
            artifact: None,
            setup: None,
            gap: Some(GapFactV1 {
                first_missing: sequence,
                last_missing: sequence,
                reason: GapReasonV1::CaptureInterrupted,
            }),
            boundary: None,
            subagent: None,
        };
        let operation_id = format!("cap:{}", sha256(event_id.as_bytes()));
        spool.append_priority(&CaptureEnvelopeV1::EventBatch(batch(
            &request,
            operation_id,
            vec![event],
        )))?;
    }
    Ok(())
}

fn ensure_adapter_started_late_gap(request: &CaptureRequest, spool: &LocalSpool) -> Result<()> {
    let event_id = format!(
        "gap:{}",
        sha256(
            format!(
                "{}\0{}\0{}\0adapter-started-late",
                request.work_id, request.generation, request.canonical_session_id
            )
            .as_bytes()
        )
    );
    let sequence = spool.sequence_for(&request.canonical_session_id, &event_id)?;
    let operation_id = format!("cap:{}", sha256(event_id.as_bytes()));
    let event = CapturedEventV1 {
        event_id: event_id.clone(),
        sequence,
        observed_at: now_seconds()?,
        kind: CapturedEventKindV1::Gap,
        tool: None,
        artifact: None,
        setup: None,
        gap: Some(GapFactV1 {
            first_missing: sequence,
            last_missing: sequence,
            reason: GapReasonV1::AdapterStartedLate,
        }),
        boundary: None,
        subagent: None,
    };
    spool.append_priority_at_receipt(&CaptureEnvelopeV1::EventBatch(batch(
        request,
        operation_id,
        vec![event],
    )))?;
    Ok(())
}

fn record_action_capture_pause(request: &CaptureRequest, spool: &LocalSpool) -> Result<()> {
    let event_id = format!(
        "gap:{}",
        sha256(
            format!(
                "{}\0{}\0{}\0action-capture-paused",
                request.work_id, request.generation, request.canonical_session_id
            )
            .as_bytes()
        )
    );
    let sequence = spool.sequence_for(&request.canonical_session_id, &event_id)?;
    let operation_id = format!("cap:{}", sha256(event_id.as_bytes()));
    let event = CapturedEventV1 {
        event_id: event_id.clone(),
        sequence,
        observed_at: now_seconds()?,
        kind: CapturedEventKindV1::Gap,
        tool: None,
        artifact: None,
        setup: None,
        gap: Some(GapFactV1 {
            first_missing: sequence,
            last_missing: sequence,
            reason: GapReasonV1::CaptureInterrupted,
        }),
        boundary: None,
        subagent: None,
    };
    spool.append_priority_at_receipt(&CaptureEnvelopeV1::EventBatch(batch(
        request,
        operation_id,
        vec![event],
    )))?;
    Ok(())
}

/// Captures one native hook event and, for configured tools, creates a source-bound action start
/// or finish. This observes a caller-controlled host and therefore emits only caller-asserted
/// assurance. It never decides whether an action is approved.
pub fn capture_native_hook(
    input: &[u8],
    request: &CaptureRequest,
    actions: &ActionCaptureConfig,
    spool: &LocalSpool,
) -> Result<NativeHookReport> {
    take_hook_warnings();
    let mut report = capture_native_hook_inner(input, request, actions, spool)?;
    report.warnings.extend(take_hook_warnings());
    Ok(report)
}

fn capture_native_hook_inner(
    input: &[u8],
    request: &CaptureRequest,
    actions: &ActionCaptureConfig,
    spool: &LocalSpool,
) -> Result<NativeHookReport> {
    validate_action_config(actions)?;
    let payload: Value = serde_json::from_slice(input)?;
    let payload = payload
        .as_object()
        .ok_or_else(|| CaptureError::Invalid("Claude Code hook input must be an object".into()))?;
    let event_name = payload
        .get("hook_event_name")
        .and_then(Value::as_str)
        .ok_or_else(|| CaptureError::Invalid("Claude Code hook event name is missing".into()))?;
    if !matches!(
        event_name,
        "PreToolUse" | "PostToolUse" | "PostToolUseFailure"
    ) {
        let event = capture_hook_with_identity(input, request, spool, None)?;
        return Ok(NativeHookReport {
            event,
            action: NativeActionOutcome::NotApplicable,
            warnings: Vec::new(),
        });
    }
    let native_tool = payload
        .get("tool_name")
        .and_then(Value::as_str)
        .ok_or_else(|| CaptureError::Invalid("Claude Code tool name is missing".into()))?;
    let tool_use_id = payload
        .get("tool_use_id")
        .and_then(Value::as_str)
        .filter(|value| identifier(value, 255))
        .ok_or_else(|| CaptureError::Invalid("Claude Code tool-use identity is missing".into()))?;
    let managed_command =
        managed_invocation_command(native_tool, payload.get("tool_input"), actions);
    let recipe_digest = native_recipe_digest(native_tool, payload.get("tool_input"))?;
    let candidate_rule = actions
        .rules
        .iter()
        .find(|rule| {
            rule.native_tool == native_tool
                && rule.kind == ActionKindV1::Verification
                && rule.recipe_digest.as_deref() == Some(recipe_digest.as_str())
        })
        .or_else(|| {
            actions.rules.iter().find(|rule| {
                rule.native_tool == native_tool
                    && rule.kind == ActionKindV1::Execution
                    && rule.recipe_digest.is_none()
            })
        });
    let candidate_route = if managed_command.is_some() {
        NativeOccurrenceRouteV1::Managed
    } else if let Some(rule) = candidate_rule {
        NativeOccurrenceRouteV1::OrdinaryAction {
            kind: rule.kind,
            recipe_provenance: match rule.kind {
                ActionKindV1::Execution => RecipeProvenanceV1::ObservedNativeInput,
                ActionKindV1::Verification => RecipeProvenanceV1::ConfiguredExactMatch,
            },
            coverage: rule.coverage,
            result_source: native_result_source_name(rule.result_source).into(),
        }
    } else {
        NativeOccurrenceRouteV1::OrdinaryEvent
    };
    let phase = if event_name == "PreToolUse" {
        "pre"
    } else {
        "post"
    };
    let phase_fingerprint = managed_phase_fingerprint(payload)?;
    let (mut claim, compacted, claim_created) = claim_native_occurrence(
        request,
        actions,
        tool_use_id,
        payload,
        &recipe_digest,
        candidate_route,
        phase,
        &phase_fingerprint,
        spool,
    )?;
    if let Some(receipt) = compacted {
        return replay_compacted_occurrence(
            input,
            payload,
            event_name,
            native_tool,
            tool_use_id,
            &phase_fingerprint,
            receipt,
            spool,
        );
    }
    let mut claim = claim.take().expect("new or retained v4 occurrence claim");
    // Only the Managed and OrdinaryAction routes consume the source; skip the git scan otherwise.
    if phase == "pre"
        && claim.source.is_none()
        && claim.route != Some(NativeOccurrenceRouteV1::OrdinaryEvent)
    {
        let root =
            Path::new(claim.root_path.as_deref().ok_or_else(|| {
                CaptureError::Conflict("native occurrence root is missing".into())
            })?);
        let original_source = GitSourceRequest {
            repository: claim.repository.clone(),
            worktree_id: claim.worktree_id.clone(),
            repository_path: root.to_owned(),
            base_ref: claim.base_ref.clone().ok_or_else(|| {
                CaptureError::Conflict("native occurrence base is missing".into())
            })?,
        };
        let opened = OpenedGitRoot::open(root)?;
        if opened.device != claim.root_device || opened.inode != claim.root_inode {
            return Err(CaptureError::Conflict(
                "native occurrence source root changed after claim".into(),
            ));
        }
        let source = capture_git_source_opened(&original_source, spool, &opened, true)?;
        let _managed = spool.lock_managed_capture()?;
        let retained = spool
            .managed_invocation_for_occurrence(&request.observed_session_id, tool_use_id)?
            .ok_or_else(|| CaptureError::Conflict("native occurrence claim disappeared".into()))?;
        if retained.version != 4
            || retained.marker_id != claim.marker_id
            || retained.route != claim.route
            || retained.pre_fingerprint.as_deref() != Some(phase_fingerprint.as_str())
        {
            return Err(CaptureError::Conflict(
                "native occurrence changed during bounded source capture".into(),
            ));
        }
        claim = spool.enrich_managed_invocation_source(&claim.marker_id, source)?;
    }
    let original_request = managed_marker_request(&claim);
    match claim.route.as_ref().expect("validated v4 occurrence route") {
        NativeOccurrenceRouteV1::Managed => capture_managed_invocation(
            input,
            payload,
            event_name,
            native_tool,
            tool_use_id,
            &original_request,
            claim_created,
            spool,
        ),
        NativeOccurrenceRouteV1::OrdinaryEvent => {
            let _managed = spool.lock_managed_capture()?;
            let event = capture_claimed_event(
                input,
                &original_request,
                &claim,
                phase,
                claim_created,
                spool,
            )?;
            if claim.pre_fingerprint.is_some() && claim.post_fingerprint.is_some() {
                spool.remove_managed_invocation(&claim.marker_id)?;
            }
            Ok(NativeHookReport {
                event,
                action: NativeActionOutcome::NotApplicable,
                warnings: Vec::new(),
            })
        }
        NativeOccurrenceRouteV1::OrdinaryAction {
            kind,
            recipe_provenance,
            coverage,
            result_source,
        } => capture_ordinary_claimed_action(
            input,
            payload,
            event_name,
            native_tool,
            &recipe_digest,
            &original_request,
            &claim,
            claim_created,
            *kind,
            *recipe_provenance,
            *coverage,
            parse_native_result_source(result_source)?,
            &actions.setup_digest,
            spool,
        ),
    }
}

#[allow(clippy::too_many_arguments)]
fn claim_native_occurrence(
    request: &CaptureRequest,
    actions: &ActionCaptureConfig,
    tool_use_id: &str,
    payload: &serde_json::Map<String, Value>,
    recipe_digest: &str,
    candidate_route: NativeOccurrenceRouteV1,
    phase: &str,
    phase_fingerprint: &str,
    spool: &LocalSpool,
) -> Result<(
    Option<ManagedInvocationV1>,
    Option<NativeOccurrenceReceiptV1>,
    bool,
)> {
    let command = payload
        .get("tool_input")
        .and_then(Value::as_object)
        .and_then(|input| input.get("command"))
        .and_then(Value::as_str)
        .unwrap_or_default();
    let command_digest = sha256(command.as_bytes());
    let _managed = spool.lock_managed_capture()?;
    if let Some(receipt) = spool.occurrence_receipt(&request.observed_session_id, tool_use_id)? {
        let retained = if phase == "pre" {
            receipt.pre_fingerprint.as_deref()
        } else {
            receipt.post_fingerprint.as_deref()
        };
        if retained != Some(phase_fingerprint) {
            return Err(CaptureError::Conflict(
                "compacted native occurrence phase was replayed with different native facts".into(),
            ));
        }
        return Ok((None, Some(receipt), false));
    }
    if let Some(marker) =
        spool.managed_invocation_for_occurrence(&request.observed_session_id, tool_use_id)?
    {
        if marker.version != 4 {
            return Err(CaptureError::Conflict(
                "legacy native occurrence claim requires inspect/export and an explicit new spool identity"
                    .into(),
            ));
        }
        if marker.command_digest != command_digest
            || marker.recipe_digest.as_deref() != Some(recipe_digest)
        {
            return Err(CaptureError::Conflict(
                "native occurrence opposite phase changed its canonical tool or input facts".into(),
            ));
        }
        let marker = spool.bind_managed_phase(&marker.marker_id, phase, phase_fingerprint)?;
        return Ok((Some(marker), None, false));
    }
    let root = actions.source.repository_path.canonicalize()?;
    if root != actions.source.repository_path {
        return Err(CaptureError::Invalid(
            "native occurrence root must remain canonical".into(),
        ));
    }
    let opened = OpenedGitRoot::open(&root)?;
    let root_path = root
        .to_str()
        .ok_or_else(|| CaptureError::Invalid("native occurrence root must be UTF-8".into()))?
        .to_owned();
    let marker_id = sha256(format!("{}\0{}", request.observed_session_id, tool_use_id).as_bytes());
    let marker = ManagedInvocationV1 {
        version: 4,
        marker_id,
        operation_prefix: Some(request.operation_prefix.clone()),
        observed_session_id: request.observed_session_id.clone(),
        canonical_session_id: request.canonical_session_id.clone(),
        work_id: request.work_id.clone(),
        generation: request.generation,
        tool_use_id: tool_use_id.to_owned(),
        command_digest,
        repository: actions.source.repository.clone(),
        worktree_id: actions.source.worktree_id.clone(),
        base_ref: Some(actions.source.base_ref.clone()),
        root_path: Some(root_path),
        root_device: opened.device,
        root_inode: opened.inode,
        source: None,
        setup_digest: Some(actions.setup_digest.clone()),
        recipe_digest: Some(recipe_digest.to_owned()),
        parent_session_id: request.parent_session_id.clone(),
        capture_mode: Some(request.capture_mode),
        observed_at: now_seconds()?,
        pre_fingerprint: (phase == "pre").then(|| phase_fingerprint.to_owned()),
        post_fingerprint: (phase == "post").then(|| phase_fingerprint.to_owned()),
        route: Some(candidate_route),
    };
    spool.retain_managed_invocation(&marker)?;
    Ok((Some(marker), None, true))
}

fn capture_claimed_event(
    input: &[u8],
    request: &CaptureRequest,
    claim: &ManagedInvocationV1,
    phase: &str,
    update_current_association: bool,
    spool: &LocalSpool,
) -> Result<HookCaptureOutcome> {
    let operation_id = format!("native:event:{}:{phase}", claim.marker_id);
    let event_id = format!(
        "evt:{}",
        sha256(format!("native\0{}\0{phase}", claim.marker_id).as_bytes())
    );
    capture_hook_with_identity_mode(
        input,
        request,
        spool,
        Some(HookIdentityOverride {
            event_id: &event_id,
            operation_id: &operation_id,
        }),
        update_current_association,
    )
}

#[allow(clippy::too_many_arguments)]
fn replay_compacted_occurrence(
    _input: &[u8],
    _payload: &serde_json::Map<String, Value>,
    event_name: &str,
    _native_tool: &str,
    _tool_use_id: &str,
    _phase_fingerprint: &str,
    receipt: NativeOccurrenceReceiptV1,
    spool: &LocalSpool,
) -> Result<NativeHookReport> {
    let phase = if event_name == "PreToolUse" {
        "pre"
    } else {
        "post"
    };
    let operation_id = match receipt.route {
        NativeOccurrenceRouteV1::Managed => {
            format!("managed:event:{}:{phase}", receipt.marker_id)
        }
        _ => format!("native:event:{}:{phase}", receipt.marker_id),
    };
    let event = if spool.replayable_event_batch(&operation_id)?.is_some() {
        HookCaptureOutcome::Duplicate
    } else {
        return Err(CaptureError::Conflict(
            "native occurrence compact receipt has no retained event identity".into(),
        ));
    };
    let action = match receipt.route {
        NativeOccurrenceRouteV1::OrdinaryEvent => NativeActionOutcome::NotApplicable,
        NativeOccurrenceRouteV1::Managed => {
            if spool
                .action_start(&format!("recipe:start:{}", receipt.marker_id))?
                .is_some()
            {
                NativeActionOutcome::NotApplicable
            } else if spool
                .action_finish(&format!("native:finish:{}", receipt.marker_id))?
                .is_some()
            {
                NativeActionOutcome::Finished(AppendOutcome::Duplicate)
            } else {
                NativeActionOutcome::Interrupted
            }
        }
        NativeOccurrenceRouteV1::OrdinaryAction { .. } => {
            if event_name == "PreToolUse" {
                if spool
                    .action_start(&format!("native:start:{}", receipt.marker_id))?
                    .is_some()
                {
                    NativeActionOutcome::Started(AppendOutcome::Duplicate)
                } else {
                    NativeActionOutcome::Interrupted
                }
            } else if spool
                .action_finish(&format!("native:finish:{}", receipt.marker_id))?
                .is_some()
            {
                NativeActionOutcome::Finished(AppendOutcome::Duplicate)
            } else {
                NativeActionOutcome::Interrupted
            }
        }
    };
    Ok(NativeHookReport {
        event,
        action,
        warnings: Vec::new(),
    })
}

fn native_result_source_name(source: NativeResultSourceV1) -> &'static str {
    match source {
        NativeResultSourceV1::ProcessExitCode => "process_exit_code",
        NativeResultSourceV1::Unverified => "unverified",
    }
}

fn parse_native_result_source(source: &str) -> Result<NativeResultSourceV1> {
    match source {
        "process_exit_code" => Ok(NativeResultSourceV1::ProcessExitCode),
        "unverified" => Ok(NativeResultSourceV1::Unverified),
        _ => Err(CaptureError::Conflict(
            "native occurrence retained an unknown result source".into(),
        )),
    }
}

#[allow(clippy::too_many_arguments)]
fn capture_ordinary_claimed_action(
    input: &[u8],
    payload: &serde_json::Map<String, Value>,
    event_name: &str,
    native_tool: &str,
    recipe_digest: &str,
    request: &CaptureRequest,
    claim: &ManagedInvocationV1,
    claim_created: bool,
    kind: ActionKindV1,
    recipe_provenance: RecipeProvenanceV1,
    coverage: CoverageV1,
    result_source: NativeResultSourceV1,
    observed_setup_digest: &str,
    spool: &LocalSpool,
) -> Result<NativeHookReport> {
    let _managed = spool.lock_managed_capture()?;
    let marker_id = &claim.marker_id;
    let action_id = format!("act:{marker_id}");
    let start_operation_id = format!("native:start:{marker_id}");
    let finish_operation_id = format!("native:finish:{marker_id}");
    let rule = ActionRuleV1 {
        native_tool: native_tool.to_owned(),
        kind,
        coverage,
        result_source,
        recipe_digest: (kind == ActionKindV1::Verification).then(|| recipe_digest.to_owned()),
    };

    if event_name == "PreToolUse" {
        let event = capture_claimed_event(input, request, claim, "pre", claim_created, spool)?;
        let action = (|| -> Result<AppendOutcome> {
            if let Some(retained) = spool.action_start(&start_operation_id)? {
                let candidate = ActionStartV1 {
                    schema_version: SCHEMA_VERSION_V1,
                    operation_id: start_operation_id.clone(),
                    work_id: request.work_id.clone(),
                    generation: request.generation,
                    session_id: request.canonical_session_id.clone(),
                    action_id: action_id.clone(),
                    source: retained.source,
                    kind,
                    recipe_provenance,
                    recipe_digest: recipe_digest.to_owned(),
                    setup_digest: claim.setup_digest.clone().expect("validated v4 setup"),
                    started_at: retained.started_at,
                    observed_order: retained.observed_order,
                    observation_scope_id: retained.observation_scope_id,
                };
                return spool.append(&CaptureEnvelopeV1::ActionStart(candidate));
            }
            let source = claim.source.clone().ok_or_else(|| {
                CaptureError::Conflict("native occurrence source capture is incomplete".into())
            })?;
            spool
                .append_action_start_ordered(ActionStartV1 {
                    schema_version: SCHEMA_VERSION_V1,
                    operation_id: start_operation_id.clone(),
                    work_id: request.work_id.clone(),
                    generation: request.generation,
                    session_id: request.canonical_session_id.clone(),
                    action_id: action_id.clone(),
                    source,
                    kind,
                    recipe_provenance,
                    recipe_digest: recipe_digest.to_owned(),
                    setup_digest: claim.setup_digest.clone().expect("validated v4 setup"),
                    started_at: now_seconds()?,
                    observed_order: 0,
                    observation_scope_id: None,
                })
                .map(|(outcome, _)| outcome)
        })();
        return match action {
            Ok(outcome) => Ok(NativeHookReport {
                event,
                action: NativeActionOutcome::Started(outcome),
                warnings: Vec::new(),
            }),
            Err(CaptureError::Limit(error)) => {
                let pause = record_action_capture_pause(request, spool);
                let warning = match pause {
                    Ok(()) => format!("native action capture paused: {error}"),
                    Err(pause_error) => format!(
                        "native action capture paused: {error}; interruption marker failed: {pause_error}"
                    ),
                };
                Ok(NativeHookReport {
                    event,
                    action: NativeActionOutcome::Interrupted,
                    warnings: vec![warning],
                })
            }
            Err(error) => Err(error),
        };
    }

    let Some(start) = spool.action_start(&start_operation_id)? else {
        let event = capture_claimed_event(input, request, claim, "post", false, spool)?;
        return Ok(NativeHookReport {
            event,
            action: NativeActionOutcome::MissingStart,
            warnings: vec!["native tool completion had no retained action start".into()],
        });
    };
    if start.work_id != request.work_id
        || start.generation != request.generation
        || start.action_id != action_id
    {
        return Err(CaptureError::Conflict(
            "native tool completion does not match its retained action start".into(),
        ));
    }
    if start.recipe_digest != recipe_digest
        || start.kind != rule.kind
        || start.recipe_provenance != recipe_provenance
    {
        return Err(CaptureError::Conflict(
            "native tool completion recipe differs from its retained action start".into(),
        ));
    }
    if let Some(retained) = spool.action_finish(&finish_operation_id)? {
        let (outcome, result) = native_result(event_name, payload.get("tool_response"), &rule);
        let candidate = ActionFinishV1 {
            schema_version: SCHEMA_VERSION_V1,
            operation_id: finish_operation_id,
            work_id: request.work_id.clone(),
            generation: request.generation,
            action_id,
            outcome,
            result,
            assurance: crate::change_record::AssuranceBasisV1::CallerAsserted,
            coverage,
            finished_at: retained.finished_at,
            drift: retained.drift,
            output_artifacts: retained.output_artifacts,
        };
        let outcome = spool.append(&CaptureEnvelopeV1::ActionFinish(candidate))?;
        let event = capture_claimed_event(input, request, claim, "post", false, spool)?;
        spool.remove_managed_invocation(marker_id)?;
        return Ok(NativeHookReport {
            event,
            action: NativeActionOutcome::Finished(outcome),
            warnings: Vec::new(),
        });
    }
    let result_fingerprint = sha256(&serde_json::to_vec(&json!({
        "schema_version": SCHEMA_VERSION_V1,
        "adapter_version": ADAPTER_VERSION,
        "event": event_name,
        "native_tool": native_tool,
        "setup_digest": observed_setup_digest,
        "tool_response": payload.get("tool_response"),
        "error": payload.get("error"),
    }))?);
    spool.bind_observation(&finish_operation_id, &result_fingerprint)?;
    let event = capture_claimed_event(input, request, claim, "post", false, spool)?;
    let mut warnings = Vec::new();
    let mut output_artifacts = Vec::new();
    if request.live_capture_mode != CaptureModeV1::MetadataOnly {
        let mut body = json!({
            "tool_name": native_tool,
            "tool_response": payload.get("tool_response"),
            "error": payload.get("error"),
        });
        // Action outputs are never shared, but a shared-conversation checkout still scrubs them.
        if request.live_capture_mode == CaptureModeV1::SharedConversation {
            redact_json(&mut body);
        }
        let output = serde_json::to_vec(&body)?;
        match spool.retain_artifact(&output, ArtifactMediaV1::Json, RetentionClassV1::WorkLocal) {
            Ok(reference) => output_artifacts.push(reference),
            Err(error) => {
                output_artifacts.push(spool.missing_artifact_reference(
                    &output,
                    ArtifactMediaV1::Json,
                    RetentionClassV1::WorkLocal,
                ));
                warnings.push(format!(
                    "native tool result was observed but local retention failed: {error}"
                ));
            }
        }
    }
    let drift = if start.setup_digest != observed_setup_digest {
        DriftV1::SetupChanged
    } else {
        match capture_git_source(&marker_source_request(claim)?, spool) {
            Ok(after) if after == start.source => DriftV1::NoneObserved,
            Ok(_) => DriftV1::SourceChanged,
            Err(_) => DriftV1::Unknown,
        }
    };
    let (outcome, result) = native_result(event_name, payload.get("tool_response"), &rule);
    let finish = ActionFinishV1 {
        schema_version: SCHEMA_VERSION_V1,
        operation_id: finish_operation_id,
        work_id: request.work_id.clone(),
        generation: request.generation,
        action_id,
        outcome,
        result,
        assurance: crate::change_record::AssuranceBasisV1::CallerAsserted,
        coverage,
        finished_at: now_seconds()?,
        drift,
        output_artifacts,
    };
    let outcome = spool.append(&CaptureEnvelopeV1::ActionFinish(finish))?;
    let report = NativeHookReport {
        event,
        action: NativeActionOutcome::Finished(outcome),
        warnings,
    };
    spool.remove_managed_invocation(marker_id)?;
    Ok(report)
}

#[allow(clippy::too_many_arguments)]
fn capture_managed_invocation(
    input: &[u8],
    payload: &serde_json::Map<String, Value>,
    event_name: &str,
    native_tool: &str,
    tool_use_id: &str,
    request: &CaptureRequest,
    marker_was_new: bool,
    spool: &LocalSpool,
) -> Result<NativeHookReport> {
    let _managed = spool.lock_managed_capture()?;
    let occurrence_id =
        sha256(format!("{}\0{}", request.observed_session_id, tool_use_id).as_bytes());
    let marker_id = occurrence_id;
    let runner_start_operation_id = format!("recipe:start:{marker_id}");
    let generic_start_operation_id = format!("native:start:{marker_id}");
    let generic_finish_operation_id = format!("native:finish:{marker_id}");
    let generic_action_id = format!("act:{marker_id}");
    let event_phase = if event_name == "PreToolUse" {
        "pre"
    } else {
        "post"
    };
    let phase_fingerprint = managed_phase_fingerprint(payload)?;
    let managed_event_operation_id = format!("managed:event:{marker_id}:{event_phase}");
    let managed_event_id = format!(
        "evt:{}",
        sha256(format!("managed\0{marker_id}\0{event_phase}").as_bytes())
    );

    let mut retained_marker =
        spool.managed_invocation_for_occurrence(&request.observed_session_id, tool_use_id)?;
    if retained_marker
        .as_ref()
        .is_some_and(|marker| marker.version != 4 || marker.marker_id != marker_id)
    {
        return Err(CaptureError::Conflict(
            "legacy target-bound managed invocation is inspect/export only".into(),
        ));
    }

    if let Some(batch) = spool.replayable_event_batch(&managed_event_operation_id)? {
        spool.verify_observation(&managed_event_operation_id, &phase_fingerprint)?;
        validate_managed_event_replay(&batch, event_phase, tool_use_id)?;
        if event_name == "PreToolUse" {
            return Ok(NativeHookReport {
                event: HookCaptureOutcome::Duplicate,
                action: NativeActionOutcome::NotApplicable,
                warnings: Vec::new(),
            });
        }
    }

    let marker = match retained_marker.take() {
        Some(marker) => {
            spool.bind_managed_phase(&marker.marker_id, event_phase, &phase_fingerprint)?
        }
        None => {
            return Err(CaptureError::Conflict(
                "native occurrence claim disappeared".into(),
            ));
        }
    };
    let original_request = managed_marker_request(&marker);

    spool.bind_observation(&managed_event_operation_id, &phase_fingerprint)?;
    let event = capture_hook_with_identity_mode(
        input,
        &original_request,
        spool,
        Some(HookIdentityOverride {
            event_id: &managed_event_id,
            operation_id: &managed_event_operation_id,
        }),
        marker_was_new,
    )?;

    if event_name == "PreToolUse" {
        return Ok(NativeHookReport {
            event,
            action: NativeActionOutcome::NotApplicable,
            warnings: Vec::new(),
        });
    }

    let pre_operation_id = format!("managed:event:{marker_id}:pre");
    if marker.pre_fingerprint.is_none()
        || spool.replayable_event_batch(&pre_operation_id)?.is_none()
    {
        return Ok(NativeHookReport {
            event,
            action: NativeActionOutcome::Interrupted,
            warnings: vec![
                "managed Post was retained before a durable admitted Pre; runner execution remains blocked"
                    .into(),
            ],
        });
    }

    if spool.recipe_attempt_references_marker(&marker_id)? {
        return Ok(NativeHookReport {
            event,
            action: NativeActionOutcome::Interrupted,
            warnings: vec![
                "managed runner attempt is retained; generic fallback remains blocked".into(),
            ],
        });
    }

    if let Some(start) = spool.action_start(&runner_start_operation_id)? {
        if start.work_id != original_request.work_id
            || start.generation != original_request.generation
            || start.session_id != original_request.canonical_session_id
            || start.recipe_provenance != RecipeProvenanceV1::ConfiguredExactMatch
        {
            return Err(CaptureError::Conflict(
                "managed runner start differs from its native invocation".into(),
            ));
        }
        spool.remove_managed_invocation(&marker_id)?;
        return Ok(NativeHookReport {
            event,
            action: NativeActionOutcome::NotApplicable,
            warnings: Vec::new(),
        });
    }

    let Some(source) = marker.source.clone() else {
        return Ok(NativeHookReport {
            event,
            action: NativeActionOutcome::Interrupted,
            warnings: vec![
                "managed runner produced no durable start and source capture is incomplete".into(),
            ],
        });
    };
    let fallback = (|| -> Result<AppendOutcome> {
        let start = match spool.action_start(&generic_start_operation_id)? {
            Some(start) => start,
            None => {
                let (_, start) = spool.append_action_start_ordered(ActionStartV1 {
                    schema_version: SCHEMA_VERSION_V1,
                    operation_id: generic_start_operation_id.clone(),
                    work_id: marker.work_id.clone(),
                    generation: marker.generation,
                    session_id: marker.canonical_session_id.clone(),
                    action_id: generic_action_id.clone(),
                    source,
                    kind: ActionKindV1::Execution,
                    recipe_provenance: RecipeProvenanceV1::ObservedNativeInput,
                    recipe_digest: marker
                        .recipe_digest
                        .clone()
                        .expect("validated retained recipe"),
                    setup_digest: marker
                        .setup_digest
                        .clone()
                        .expect("validated retained setup"),
                    started_at: marker.observed_at,
                    observed_order: 0,
                    observation_scope_id: None,
                })?;
                start
            }
        };
        if start.work_id != marker.work_id
            || start.generation != marker.generation
            || start.session_id != marker.canonical_session_id
            || start.action_id != generic_action_id
            || start.kind != ActionKindV1::Execution
            || Some(start.recipe_digest.as_str()) != marker.recipe_digest.as_deref()
        {
            return Err(CaptureError::Conflict(
                "managed fallback action differs from its native invocation".into(),
            ));
        }
        let rule = ActionRuleV1 {
            native_tool: native_tool.to_owned(),
            kind: ActionKindV1::Execution,
            coverage: CoverageV1::Partial,
            result_source: NativeResultSourceV1::ProcessExitCode,
            recipe_digest: None,
        };
        let (outcome, _) = native_result(event_name, payload.get("tool_response"), &rule);
        let result_fingerprint = sha256(&serde_json::to_vec(&json!({
            "schema_version": SCHEMA_VERSION_V1,
            "adapter_version": ADAPTER_VERSION,
            "event": event_name,
            "native_tool": native_tool,
            "setup_digest": marker.setup_digest.as_deref(),
            "managed_fallback": true,
            "tool_response": payload.get("tool_response"),
            "error": payload.get("error"),
        }))?);
        spool.bind_observation(&generic_finish_operation_id, &result_fingerprint)?;
        if let Some(retained) = spool.action_finish(&generic_finish_operation_id)? {
            let candidate = ActionFinishV1 {
                schema_version: SCHEMA_VERSION_V1,
                operation_id: generic_finish_operation_id,
                work_id: marker.work_id.clone(),
                generation: marker.generation,
                action_id: generic_action_id,
                outcome,
                result: CheckResultV1::Unknown,
                assurance: crate::change_record::AssuranceBasisV1::CallerAsserted,
                coverage: CoverageV1::Partial,
                finished_at: retained.finished_at,
                drift: DriftV1::Unknown,
                output_artifacts: retained.output_artifacts,
            };
            return spool.append(&CaptureEnvelopeV1::ActionFinish(candidate));
        }
        spool.append(&CaptureEnvelopeV1::ActionFinish(ActionFinishV1 {
            schema_version: SCHEMA_VERSION_V1,
            operation_id: generic_finish_operation_id,
            work_id: marker.work_id.clone(),
            generation: marker.generation,
            action_id: generic_action_id,
            outcome,
            result: CheckResultV1::Unknown,
            assurance: crate::change_record::AssuranceBasisV1::CallerAsserted,
            coverage: CoverageV1::Partial,
            finished_at: now_seconds()?,
            drift: DriftV1::Unknown,
            output_artifacts: Vec::new(),
        }))
    })();
    match fallback {
        Ok(outcome) => {
            spool.remove_managed_invocation(&marker_id)?;
            Ok(NativeHookReport {
                event,
                action: NativeActionOutcome::Finished(outcome),
                warnings: vec![
                    "managed runner produced no durable verification start; retained generic execution with unknown verification result"
                        .into(),
                ],
            })
        }
        Err(CaptureError::Conflict(error)) => Err(CaptureError::Conflict(error)),
        Err(error) => Ok(NativeHookReport {
            event,
            action: NativeActionOutcome::Interrupted,
            warnings: vec![format!("managed fallback capture paused: {error}")],
        }),
    }
}

fn managed_marker_request(marker: &ManagedInvocationV1) -> CaptureRequest {
    CaptureRequest {
        operation_prefix: marker
            .operation_prefix
            .clone()
            .unwrap_or_else(|| format!("capture:managed:{}", &marker.marker_id[..16])),
        work_id: marker.work_id.clone(),
        generation: marker.generation,
        observed_session_id: marker.observed_session_id.clone(),
        canonical_session_id: marker.canonical_session_id.clone(),
        parent_session_id: marker.parent_session_id.clone(),
        capture_mode: marker.capture_mode.unwrap_or(CaptureModeV1::MetadataOnly),
        live_capture_mode: marker.capture_mode.unwrap_or(CaptureModeV1::MetadataOnly),
    }
}

fn marker_source_request(marker: &ManagedInvocationV1) -> Result<GitSourceRequest> {
    Ok(GitSourceRequest {
        repository: marker.repository.clone(),
        worktree_id: marker.worktree_id.clone(),
        repository_path: PathBuf::from(
            marker.root_path.as_deref().ok_or_else(|| {
                CaptureError::Conflict("native occurrence root is missing".into())
            })?,
        ),
        base_ref: marker
            .base_ref
            .clone()
            .ok_or_else(|| CaptureError::Conflict("native occurrence base is missing".into()))?,
    })
}

fn managed_phase_fingerprint(payload: &serde_json::Map<String, Value>) -> Result<String> {
    let mut facts = Value::Object(payload.clone());
    if let Value::Object(object) = &mut facts {
        object.remove("timestamp");
    }
    canonicalize_json(&mut facts);
    Ok(sha256(&serde_json::to_vec(&facts)?))
}

fn canonicalize_json(value: &mut Value) {
    match value {
        Value::Array(values) => {
            for value in values {
                canonicalize_json(value);
            }
        }
        Value::Object(object) => {
            let previous = std::mem::take(object);
            let mut entries: Vec<_> = previous.into_iter().collect();
            entries.sort_by(|left, right| left.0.cmp(&right.0));
            for (key, mut value) in entries {
                canonicalize_json(&mut value);
                object.insert(key, value);
            }
        }
        _ => {}
    }
}

fn validate_managed_event_replay(
    batch: &CaptureBatchV1,
    phase: &str,
    tool_use_id: &str,
) -> Result<()> {
    let valid_kind = |kind| match phase {
        "pre" => matches!(
            kind,
            CapturedEventKindV1::ToolStarted | CapturedEventKindV1::CheckStarted
        ),
        "post" => matches!(
            kind,
            CapturedEventKindV1::ToolFinished | CapturedEventKindV1::CheckFinished
        ),
        _ => false,
    };
    let matching = batch
        .events
        .iter()
        .filter(|event| {
            valid_kind(event.kind)
                && event
                    .tool
                    .as_ref()
                    .and_then(|tool| tool.request_id.as_deref())
                    == Some(tool_use_id)
        })
        .count();
    if matching != 1 {
        return Err(CaptureError::Conflict(
            "managed event replay facts do not uniquely identify the provider occurrence".into(),
        ));
    }
    Ok(())
}

fn validate_action_config(config: &ActionCaptureConfig) -> Result<()> {
    if config.rules.len() > MAX_ACTION_RULES {
        return Err(CaptureError::Limit(format!(
            "native action mapping exceeds {MAX_ACTION_RULES} rules"
        )));
    }
    if config.setup_digest.len() != 64
        || !config
            .setup_digest
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err(CaptureError::Invalid(
            "native action setup digest must be lowercase SHA-256".into(),
        ));
    }
    if config.managed_invocations.len() > MAX_ACTION_RULES
        || config
            .managed_invocations
            .iter()
            .enumerate()
            .any(|(index, command)| {
                command.is_empty()
                    || command.len() > 4096
                    || command.contains('\0')
                    || config.managed_invocations[..index].contains(command)
            })
    {
        return Err(CaptureError::Invalid(
            "managed recipe invocations must be unique bounded commands".into(),
        ));
    }
    for (index, rule) in config.rules.iter().enumerate() {
        if !identifier(&rule.native_tool, 128)
            || config.rules[..index].iter().any(|prior| {
                prior.native_tool == rule.native_tool && prior.recipe_digest == rule.recipe_digest
            })
        {
            return Err(CaptureError::Invalid(
                "native action rules must have unique bounded tool and recipe matchers".into(),
            ));
        }
        match (rule.kind, rule.recipe_digest.as_deref()) {
            (ActionKindV1::Execution, None) => {}
            (ActionKindV1::Verification, Some(digest)) if valid_digest(digest) => {}
            _ => {
                return Err(CaptureError::Invalid(
                    "execution rules require no recipe digest and verification rules require one"
                        .into(),
                ));
            }
        }
        if rule.kind == ActionKindV1::Verification
            && rule.result_source != NativeResultSourceV1::ProcessExitCode
        {
            return Err(CaptureError::Invalid(
                "verification rules require an explicit supported result source".into(),
            ));
        }
        if rule.result_source == NativeResultSourceV1::ProcessExitCode
            && !matches!(rule.native_tool.as_str(), "Bash" | "PowerShell")
        {
            return Err(CaptureError::Invalid(
                "process-exit result mapping is supported only for Bash or PowerShell".into(),
            ));
        }
    }
    Ok(())
}

fn managed_invocation(
    native_tool: &str,
    tool_input: Option<&Value>,
    config: &ActionCaptureConfig,
) -> bool {
    managed_invocation_command(native_tool, tool_input, config).is_some()
}

fn managed_invocation_command<'a>(
    native_tool: &str,
    tool_input: Option<&Value>,
    config: &'a ActionCaptureConfig,
) -> Option<&'a str> {
    if native_tool != "Bash" {
        return None;
    }
    let command = tool_input
        .and_then(Value::as_object)
        .and_then(|input| input.get("command"))
        .and_then(Value::as_str)?;
    config
        .managed_invocations
        .iter()
        .find(|managed| managed.as_str() == command)
        .map(String::as_str)
}

fn native_result(
    event_name: &str,
    response: Option<&Value>,
    rule: &ActionRuleV1,
) -> (ActionOutcomeV1, CheckResultV1) {
    if event_name == "PostToolUseFailure" {
        return (
            ActionOutcomeV1::Failed,
            if rule.kind == ActionKindV1::Verification {
                CheckResultV1::Incomplete
            } else {
                CheckResultV1::Unknown
            },
        );
    }
    if rule.result_source == NativeResultSourceV1::Unverified {
        return (ActionOutcomeV1::Succeeded, CheckResultV1::Unknown);
    }
    let Some(response) = response.and_then(Value::as_object) else {
        return (ActionOutcomeV1::Unknown, CheckResultV1::Unknown);
    };
    if response
        .get("interrupted")
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        return (
            ActionOutcomeV1::Incomplete,
            if rule.kind == ActionKindV1::Verification {
                CheckResultV1::Incomplete
            } else {
                CheckResultV1::Unknown
            },
        );
    }
    let Some(exit_code) = response
        .get("exit_code")
        .or_else(|| response.get("exitCode"))
        .and_then(Value::as_i64)
    else {
        return (ActionOutcomeV1::Unknown, CheckResultV1::Unknown);
    };
    if exit_code == 0 {
        (
            ActionOutcomeV1::Succeeded,
            if rule.kind == ActionKindV1::Verification {
                CheckResultV1::Passed
            } else {
                CheckResultV1::Unknown
            },
        )
    } else {
        (
            ActionOutcomeV1::Failed,
            if rule.kind == ActionKindV1::Verification {
                CheckResultV1::Failed
            } else {
                CheckResultV1::Unknown
            },
        )
    }
}

fn valid_digest(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

/// Tails usage facts only from one explicitly configured Claude history root and exact native
/// session file. Conversation bodies are neither retained here nor sent to the Hub.
pub fn capture_provider_usage(
    transcript_path: &Path,
    history_root: &Path,
    request: &CaptureRequest,
    history_from_start: bool,
    spool: &LocalSpool,
) -> Result<usize> {
    let root_metadata = std::fs::symlink_metadata(history_root)?;
    if root_metadata.file_type().is_symlink() || !root_metadata.is_dir() {
        return Err(CaptureError::Invalid(
            "configured Claude history root is not a regular directory".into(),
        ));
    }
    let root = history_root.canonicalize()?;
    let path_metadata = std::fs::symlink_metadata(transcript_path)?;
    if path_metadata.file_type().is_symlink() || !path_metadata.is_file() {
        return Err(CaptureError::Invalid(
            "Claude transcript path is not a regular non-symlink file".into(),
        ));
    }
    let path = transcript_path.canonicalize()?;
    let expected_name = format!("{}.jsonl", request.observed_session_id);
    if !path.starts_with(&root)
        || path.file_name().and_then(|name| name.to_str()) != Some(expected_name.as_str())
    {
        return Err(CaptureError::Invalid(
            "Claude transcript is outside the configured history root or names another session"
                .into(),
        ));
    }
    let mut file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(&path)?;
    let opened = file.metadata()?;
    if !opened.is_file()
        || opened.dev() != path_metadata.dev()
        || opened.ino() != path_metadata.ino()
    {
        return Err(CaptureError::Conflict(
            "Claude transcript changed during validation".into(),
        ));
    }
    let path_digest = sha256(path.as_os_str().as_encoded_bytes());
    let previous = spool.provider_history_cursor(&request.canonical_session_id)?;
    let cursor_at = |offset| ProviderHistoryCursorV1 {
        path_digest: path_digest.clone(),
        file_device: opened.dev(),
        file_inode: opened.ino(),
        work_id: request.work_id.clone(),
        generation: request.generation,
        canonical_session_id: request.canonical_session_id.clone(),
        offset,
    };
    if let Some(cursor) = previous.as_ref() {
        if cursor.path_digest != path_digest
            || cursor.file_device != opened.dev()
            || cursor.file_inode != opened.ino()
            || cursor.canonical_session_id != request.canonical_session_id
        {
            record_provider_history_gap(
                request,
                spool,
                &format!(
                    "file-replaced-{}-{}-{}-{}",
                    cursor.file_device,
                    cursor.file_inode,
                    opened.dev(),
                    opened.ino()
                ),
            )?;
            return Err(CaptureError::Conflict(
                "Claude history file identity changed".into(),
            ));
        }
        if opened.len() < cursor.offset {
            record_provider_history_gap(
                request,
                spool,
                &format!("file-truncated-{}-{}", cursor.offset, opened.len()),
            )?;
            return Err(CaptureError::Conflict(
                "Claude history was truncated".into(),
            ));
        }
        if cursor.work_id != request.work_id || cursor.generation != request.generation {
            let snapshot_end = opened.len();
            record_provider_history_gap(
                request,
                spool,
                &format!(
                    "work-rebound-{}-{}-{}-{}-{}",
                    cursor.work_id,
                    cursor.generation,
                    request.work_id,
                    request.generation,
                    snapshot_end
                ),
            )?;
            spool.rebind_provider_history_cursor(
                &request.canonical_session_id,
                cursor,
                cursor_at(snapshot_end),
            )?;
            return Ok(0);
        }
    }
    let offset = previous.as_ref().map_or(0, |cursor| cursor.offset);
    if previous.is_none() && !history_from_start {
        spool.advance_provider_history_cursor(
            &request.canonical_session_id,
            None,
            cursor_at(opened.len()),
        )?;
        return Ok(0);
    }
    file.seek(SeekFrom::Start(offset))?;
    let snapshot_end = opened.len();
    let snapshot_remaining = snapshot_end.saturating_sub(offset);
    let read_limit = snapshot_remaining.min(MAX_HISTORY_TAIL_BYTES + 1);
    let mut reader = BufReader::new(file.take(read_limit));
    let mut consumed = 0_u64;
    let mut complete_offset = offset;
    let mut records = 0_usize;
    let mut malformed = 0_usize;
    while records < MAX_HISTORY_TAIL_LINES && consumed <= MAX_HISTORY_TAIL_BYTES {
        let mut line = Vec::new();
        let read = reader.read_until(b'\n', &mut line)?;
        if read == 0 {
            break;
        }
        consumed = consumed.saturating_add(read as u64);
        if consumed > MAX_HISTORY_TAIL_BYTES {
            record_provider_history_gap(
                request,
                spool,
                &format!("byte-window-skipped-{complete_offset}-{snapshot_end}"),
            )?;
            spool.advance_provider_history_cursor(
                &request.canonical_session_id,
                previous.as_ref(),
                cursor_at(snapshot_end),
            )?;
            return Ok(records);
        }
        if line.last() != Some(&b'\n') {
            break;
        }
        complete_offset = complete_offset.saturating_add(read as u64);
        records += 1;
        let parsed: TranscriptLine = match serde_json::from_slice(&line) {
            Ok(parsed) => parsed,
            Err(_) => {
                malformed += 1;
                continue;
            }
        };
        if parsed
            .session_id
            .as_deref()
            .is_some_and(|session| session != request.observed_session_id)
        {
            record_provider_history_gap(request, spool, "foreign-session-record")?;
            return Err(CaptureError::Invalid(
                "Claude history contains another session".into(),
            ));
        }
        if let Some(usage) = provider_usage_record(parsed, request) {
            let observation_id = usage.observation_id.clone();
            match spool.append(&CaptureEnvelopeV1::Usage(usage)) {
                Ok(_) => {}
                Err(CaptureError::Conflict(_)) => record_provider_history_gap(
                    request,
                    spool,
                    &format!("usage-conflict-{observation_id}-{complete_offset}"),
                )?,
                Err(error) => return Err(error),
            }
        }
    }
    if malformed > 0 {
        record_provider_history_gap(
            request,
            spool,
            &format!("malformed-records-{offset}-{complete_offset}-{malformed}"),
        )?;
    }
    let next = cursor_at(complete_offset);
    spool.advance_provider_history_cursor(
        &request.canonical_session_id,
        previous.as_ref(),
        next,
    )?;
    Ok(records)
}

fn provider_usage_record(line: TranscriptLine, request: &CaptureRequest) -> Option<UsageRecord> {
    let scope = match line.kind.as_deref()? {
        "assistant" => UsageMeasurementScope::Request,
        "result" => UsageMeasurementScope::SessionCumulative,
        _ => return None,
    };
    let message = line.message.as_ref();
    let usage = match scope {
        UsageMeasurementScope::Request => message?.usage.as_ref()?,
        UsageMeasurementScope::SessionCumulative => line.usage.as_ref()?,
    };
    let input_tokens = usage_count(usage, "input_tokens");
    let output_tokens = usage_count(usage, "output_tokens");
    let cache_read_input_tokens = usage_count(usage, "cache_read_input_tokens");
    let cache_creation_input_tokens = usage_count(usage, "cache_creation_input_tokens");
    if input_tokens.is_none()
        && output_tokens.is_none()
        && cache_read_input_tokens.is_none()
        && cache_creation_input_tokens.is_none()
    {
        return None;
    }
    let event_id = line.uuid.filter(|value| identifier(value, 255));
    let request_id = line.request_id.filter(|value| identifier(value, 255));
    let (identity_kind, identity) = request_id
        .as_deref()
        .map(|identity| ("request", identity))
        .or_else(|| event_id.as_deref().map(|identity| ("event", identity)))?;
    let observation_id = format!(
        "claude-usage:{}",
        sha256(
            format!(
                "{}\0{}\0{}\0{:?}",
                request.observed_session_id, identity_kind, identity, scope
            )
            .as_bytes()
        )
    );
    // A provider request may be written as more than one assistant JSONL entry, each with a
    // different event UUID and the same cumulative request usage. Keep request observations
    // canonically identical so the spool's observation-id replay suppresses those duplicates.
    let provider_event_id = request_id.is_none().then_some(event_id).flatten();
    Some(UsageRecord {
        work_id: request.work_id.clone(),
        observation_id,
        generation: request.generation,
        session: request.canonical_session_id.clone(),
        phase: AttemptPhase::Other,
        outcome: AttemptOutcome::Unknown,
        agent: "claude".into(),
        provider: "unknown".into(),
        model: message
            .and_then(|message| message.model.clone())
            .unwrap_or_else(|| "unknown".into()),
        usage_basis: UsageBasis::ProviderResult,
        source_commit: None,
        input_tokens,
        output_tokens,
        cache_read_input_tokens,
        cache_creation_input_tokens,
        action_id: None,
        attempt_id: None,
        provider_request_id: request_id,
        provider_event_id,
        measurement_scope: Some(scope),
        cost: None,
    })
}

fn usage_count(usage: &Value, key: &str) -> Option<i64> {
    usage
        .get(key)
        .and_then(Value::as_i64)
        .filter(|value| *value >= 0)
}

fn record_provider_history_gap(
    request: &CaptureRequest,
    spool: &LocalSpool,
    detail: &str,
) -> Result<()> {
    let event_id = format!(
        "gap:{}",
        sha256(
            format!(
                "{}\0{}\0{}\0provider-history\0{detail}",
                request.work_id, request.generation, request.canonical_session_id
            )
            .as_bytes()
        )
    );
    let operation_id = format!("provider:history:{}:gap", sha256(event_id.as_bytes()));
    let sequence = spool.sequence_for(&request.canonical_session_id, &event_id)?;
    let gap_batch = |observed_at| {
        batch(
            request,
            operation_id.clone(),
            vec![CapturedEventV1 {
                event_id: event_id.clone(),
                sequence,
                observed_at,
                kind: CapturedEventKindV1::Gap,
                tool: None,
                artifact: None,
                setup: None,
                gap: Some(GapFactV1 {
                    first_missing: sequence,
                    last_missing: sequence,
                    reason: GapReasonV1::CaptureInterrupted,
                }),
                boundary: None,
                subagent: None,
            }],
        )
    };
    let validate_retained = |retained: CaptureBatchV1| {
        let exact_gap = matches!(retained.events.as_slice(), [event]
            if event.event_id == event_id
                && event.sequence == sequence
                && event.kind == CapturedEventKindV1::Gap
                && event.tool.is_none()
                && event.artifact.is_none()
                && event.setup.is_none()
                && event.gap.as_ref().is_some_and(|gap|
                    gap.first_missing == sequence
                        && gap.last_missing == sequence
                        && gap.reason == GapReasonV1::CaptureInterrupted));
        if retained.operation_id == operation_id
            && retained.work_id == request.work_id
            && retained.generation == request.generation
            && retained.session.harness == AgentHarnessV1::ClaudeCode
            && retained.session.session_id == request.canonical_session_id
            && exact_gap
        {
            Ok(())
        } else {
            Err(CaptureError::Conflict(
                "provider history gap identity has different retained facts".into(),
            ))
        }
    };
    if let Some(retained) = spool.replayable_event_batch(&operation_id)? {
        return validate_retained(retained);
    }
    let envelope = CaptureEnvelopeV1::EventBatch(gap_batch(now_seconds()?));
    match spool.append_priority(&envelope) {
        Ok(_) => Ok(()),
        Err(CaptureError::Conflict(error)) => {
            let retained = spool
                .replayable_event_batch(&operation_id)?
                .ok_or_else(|| {
                    CaptureError::Conflict(format!(
                        "provider history gap append conflicted without retained facts: {error}"
                    ))
                })?;
            validate_retained(retained)
        }
        Err(error) => Err(error),
    }
}

fn native_recipe_digest(native_tool: &str, tool_input: Option<&Value>) -> Result<String> {
    Ok(sha256(&serde_json::to_vec(&NativeRecipe {
        schema_version: SCHEMA_VERSION_V1,
        adapter_version: ADAPTER_VERSION,
        native_tool,
        tool_input,
    })?))
}

/// Imports a bounded visible-history stream. The initial gap states that this is retrospective
/// history, not proof that live capture observed the session from its beginning.
pub fn import_jsonl(
    mut input: impl BufRead,
    request: &CaptureRequest,
    spool: &LocalSpool,
) -> Result<CaptureReport> {
    spool.remember_session_association(
        &request.observed_session_id,
        &request.canonical_session_id,
        &request.work_id,
        request.generation,
        request.parent_session_id.as_deref(),
        request.capture_mode,
    )?;
    let initial_id = format!(
        "gap:{}",
        sha256(format!("{}\0late", request.canonical_session_id).as_bytes())
    );
    let initial_sequence = spool.sequence_for(&request.canonical_session_id, &initial_id)?;
    let mut selected = vec![CapturedEventV1 {
        event_id: initial_id,
        sequence: initial_sequence,
        observed_at: 0,
        kind: CapturedEventKindV1::Gap,
        tool: None,
        artifact: None,
        setup: None,
        gap: Some(GapFactV1 {
            first_missing: initial_sequence,
            last_missing: initial_sequence,
            reason: GapReasonV1::AdapterStartedLate,
        }),
        boundary: None,
        subagent: None,
    }];
    let mut report = CaptureReport {
        gaps: 1,
        ..CaptureReport::default()
    };
    let (mut line, mut total, mut lines, mut missing) = (Vec::new(), 0u64, 0usize, 0u64);
    loop {
        let read = next_line(
            &mut input,
            &mut line,
            MAX_IMPORT_BYTES.saturating_sub(total),
        )?;
        let (bytes, oversized) = match read {
            ReadLine::End => break,
            ReadLine::Limit => {
                return Err(CaptureError::Limit(
                    "Claude Code history import exceeds its line or byte bound".into(),
                ));
            }
            ReadLine::Oversized(bytes) => (bytes, true),
            ReadLine::Bytes(bytes) => (bytes, false),
        };
        total = total.saturating_add(bytes);
        lines = lines.saturating_add(1);
        if total > MAX_IMPORT_BYTES || lines > MAX_IMPORT_LINES {
            return Err(CaptureError::Limit(
                "Claude Code history import exceeds its line or byte bound".into(),
            ));
        }
        if oversized {
            missing = missing.saturating_add(1);
            continue;
        }
        let parsed: TranscriptLine = match serde_json::from_slice(&line) {
            Ok(parsed) => parsed,
            Err(_) => {
                missing = missing.saturating_add(1);
                continue;
            }
        };
        if parsed
            .session_id
            .as_deref()
            .is_some_and(|session| session != request.observed_session_id)
        {
            return Err(CaptureError::Invalid(
                "Claude Code history contains another session".into(),
            ));
        }
        let (events, ignored) = select_line(parsed, request, lines)?;
        report.private_blocks_ignored = report.private_blocks_ignored.saturating_add(ignored);
        for event in events {
            let retained = retain_selected(event, request, spool)?;
            selected.push(retained.event);
            selected.extend(retained.redacted_gap);
            missing = missing.saturating_add(u64::from(retained.retention_missing));
        }
    }
    let history_anchor = selected
        .iter()
        .skip(1)
        .map(|event| event.observed_at)
        .filter(|value| *value > 0)
        .min()
        .unwrap_or(0);
    selected[0].observed_at = history_anchor;
    if missing > 0 {
        let event_id = format!(
            "gap:{}",
            sha256(format!("{}\0interrupted\0{missing}", request.canonical_session_id).as_bytes())
        );
        let sequence = spool.sequence_for(&request.canonical_session_id, &event_id)?;
        selected.push(CapturedEventV1 {
            event_id,
            sequence,
            observed_at: history_anchor,
            kind: CapturedEventKindV1::Gap,
            tool: None,
            artifact: None,
            setup: None,
            gap: Some(GapFactV1 {
                first_missing: sequence,
                last_missing: sequence.saturating_add(missing.saturating_sub(1)),
                reason: GapReasonV1::CaptureInterrupted,
            }),
            boundary: None,
            subagent: None,
        });
        report.gaps += 1;
    }
    selected.sort_by_key(|event| event.sequence);
    report.events = selected.len();
    for (index, events) in batch_chunks(&selected).into_iter().enumerate() {
        let operation_id = format!("{}:{index}", request.operation_prefix);
        let envelope = CaptureEnvelopeV1::EventBatch(batch(request, operation_id, events.to_vec()));
        spool.append(&envelope)?;
        report.batches += 1;
    }
    Ok(report)
}

/// Splits sequence-ordered events into runs that respect both the per-batch event and gap limits.
fn batch_chunks(events: &[CapturedEventV1]) -> Vec<&[CapturedEventV1]> {
    use crate::change_record::{MAX_EVENTS_PER_BATCH, MAX_GAPS_PER_SESSION};
    let mut chunks = Vec::new();
    let (mut start, mut gaps) = (0, 0);
    for (index, event) in events.iter().enumerate() {
        let is_gap = usize::from(event.gap.is_some());
        if index - start == MAX_EVENTS_PER_BATCH || gaps + is_gap > MAX_GAPS_PER_SESSION {
            chunks.push(&events[start..index]);
            (start, gaps) = (index, 0);
        }
        gaps += is_gap;
    }
    if start < events.len() {
        chunks.push(&events[start..]);
    }
    chunks
}

fn select_line(
    line: TranscriptLine,
    request: &CaptureRequest,
    line_ordinal: usize,
) -> Result<(Vec<SelectedEvent>, usize)> {
    let Some(kind @ ("user" | "assistant")) = line.kind.as_deref() else {
        return Ok((Vec::new(), 0));
    };
    let Some(observed_at) = line.timestamp.as_deref().and_then(parse_rfc3339) else {
        return Ok((Vec::new(), 0));
    };
    let Some(content) = line.message.and_then(|message| message.content) else {
        return Ok((Vec::new(), 0));
    };
    let blocks: Vec<Value> = match content {
        Value::String(text) => vec![json!({"type":"text", "text": text})],
        Value::Array(blocks) => blocks,
        _ => return Ok((Vec::new(), 0)),
    };
    let mut selected = Vec::new();
    let mut private = 0usize;
    for (index, block) in blocks.into_iter().enumerate() {
        let Some(block_kind) = block.get("type").and_then(Value::as_str) else {
            continue;
        };
        let base_id = line
            .uuid
            .as_deref()
            .filter(|value| identifier(value, 200))
            .map(str::to_owned)
            .unwrap_or_else(|| {
                sha256(
                    &serde_json::to_vec(&(
                        request.observed_session_id.as_str(),
                        kind,
                        observed_at,
                        line_ordinal,
                        &block,
                    ))
                    .unwrap_or_default(),
                )
            });
        let event_id = format!("evt:{base_id}:{index}");
        match (kind, block_kind) {
            ("user", "text") => selected.push(SelectedEvent {
                event_id,
                observed_at,
                kind: CapturedEventKindV1::UserPrompt,
                tool: None,
                local_body: Some(json!({"content": block.get("text")})),
                setup: None,
                boundary: None,
                subagent: None,
            }),
            ("assistant", "text") => selected.push(SelectedEvent {
                event_id,
                observed_at,
                kind: CapturedEventKindV1::AssistantMessage,
                tool: None,
                local_body: Some(json!({"content": block.get("text")})),
                setup: None,
                boundary: None,
                subagent: None,
            }),
            ("assistant", "tool_use") => {
                let tool_name = block.get("name").and_then(Value::as_str);
                let request_id = block.get("id").and_then(Value::as_str);
                selected.push(SelectedEvent {
                    event_id,
                    observed_at,
                    kind: check_kind(CapturedEventKindV1::ToolStarted, tool_name),
                    tool: Some(tool_fact(tool_name, request_id, None)),
                    local_body: Some(json!({
                        "tool_name": tool_name,
                        "tool_input": block.get("input"),
                    })),
                    setup: None,
                    boundary: None,
                    subagent: None,
                });
            }
            ("user", "tool_result") => {
                let request_id = block.get("tool_use_id").and_then(Value::as_str);
                selected.push(SelectedEvent {
                    event_id,
                    observed_at,
                    kind: CapturedEventKindV1::ToolFinished,
                    tool: Some(tool_fact(
                        None,
                        request_id,
                        Some(
                            if block.get("is_error").and_then(Value::as_bool) == Some(true) {
                                ToolCompletionV1::Failed
                            } else {
                                ToolCompletionV1::Returned
                            },
                        ),
                    )),
                    local_body: Some(json!({
                        "tool_result": block.get("content"),
                        "is_error": block.get("is_error"),
                    })),
                    setup: None,
                    boundary: None,
                    subagent: None,
                });
            }
            (_, "thinking" | "redacted_thinking") => private = private.saturating_add(1),
            _ => {}
        }
    }
    if selected.iter().any(|event| event.event_id.len() > 255) {
        for event in &mut selected {
            if event.event_id.len() > 255 {
                event.event_id = format!("evt:{}", sha256(event.event_id.as_bytes()));
            }
        }
    }
    Ok((selected, private))
}

struct Retained {
    event: CapturedEventV1,
    retention_missing: bool,
    /// Present when redaction changed the retained body.
    redacted_gap: Option<CapturedEventV1>,
}

fn retain_selected(
    event: SelectedEvent,
    request: &CaptureRequest,
    spool: &LocalSpool,
) -> Result<Retained> {
    let sequence = spool.sequence_for(&request.canonical_session_id, &event.event_id)?;
    let mut redactions = 0;
    let body = match (request.live_capture_mode, event.local_body) {
        (CaptureModeV1::LocalConversation, body) => body,
        (CaptureModeV1::SharedConversation, Some(mut body)) => {
            redactions = redact_json(&mut body);
            if redactions > 0
                && let Value::Object(map) = &mut body
            {
                map.insert("redactions".to_owned(), json!(redactions));
            }
            Some(body)
        }
        _ => None,
    };
    let (artifact, retention_missing) = match body {
        Some(body) => {
            let bytes = serde_json::to_vec(&body)?;
            // Validation uses the session's pinned mode, so a mid-session switch to sharing
            // keeps bodies local until the next session. Oversized bodies also stay local.
            let shared = request.live_capture_mode == CaptureModeV1::SharedConversation
                && request.capture_mode == CaptureModeV1::SharedConversation
                && u64::try_from(bytes.len()).is_ok_and(|size| size <= MAX_SHARED_BODY_BYTES);
            let retention = if shared {
                RetentionClassV1::SharedApproved
            } else {
                RetentionClassV1::WorkLocal
            };
            match spool.retain_artifact(&bytes, ArtifactMediaV1::Json, retention) {
                Ok(reference) => (Some(reference), false),
                Err(_) => (
                    Some(spool.missing_artifact_reference(
                        &bytes,
                        ArtifactMediaV1::Json,
                        RetentionClassV1::WorkLocal,
                    )),
                    true,
                ),
            }
        }
        None => (None, false),
    };
    let redacted_gap = if redactions > 0 {
        let gap_id = format!(
            "gap:{}",
            sha256(format!("{}\0redacted", event.event_id).as_bytes())
        );
        let gap_sequence = spool.sequence_for(&request.canonical_session_id, &gap_id)?;
        Some(CapturedEventV1 {
            event_id: gap_id,
            sequence: gap_sequence,
            observed_at: event.observed_at,
            kind: CapturedEventKindV1::Gap,
            tool: None,
            artifact: None,
            setup: None,
            gap: Some(GapFactV1 {
                // The altered event keeps its sequence; the range marks what was changed.
                first_missing: sequence,
                last_missing: sequence,
                reason: GapReasonV1::Redacted,
            }),
            boundary: None,
            subagent: None,
        })
    } else {
        None
    };
    Ok(Retained {
        event: CapturedEventV1 {
            event_id: event.event_id,
            sequence,
            observed_at: event.observed_at,
            kind: event.kind,
            tool: event.tool,
            artifact,
            setup: event.setup,
            gap: None,
            boundary: event.boundary,
            subagent: event.subagent,
        },
        retention_missing,
        redacted_gap,
    })
}

fn native_setup_observation(
    payload: &serde_json::Map<String, Value>,
    spool: &LocalSpool,
) -> Result<Option<SetupObservationV1>> {
    let model = payload
        .get("model")
        .and_then(Value::as_str)
        .filter(|value| identifier(value, 255));
    let permission_mode = payload
        .get("permission_mode")
        .and_then(Value::as_str)
        .filter(|value| identifier(value, 255));
    let effort = payload
        .get("effort")
        .and_then(|effort| {
            effort.as_str().or_else(|| {
                effort
                    .as_object()
                    .and_then(|object| object.get("level"))
                    .and_then(Value::as_str)
            })
        })
        .filter(|value| identifier(value, 255));
    if model.is_none() && permission_mode.is_none() && effort.is_none() {
        return Ok(None);
    }
    let bytes = serde_json::to_vec(&json!({
        "schema_version": SCHEMA_VERSION_V1,
        "harness": "claude_code",
        "adapter_version": ADAPTER_VERSION,
        "provider": Value::Null,
        "model": model,
        "permission_mode": permission_mode,
        "effort": effort,
    }))?;
    let artifact = spool
        .retain_artifact(&bytes, ArtifactMediaV1::Json, RetentionClassV1::WorkLocal)
        .unwrap_or_else(|_| {
            spool.missing_artifact_reference(
                &bytes,
                ArtifactMediaV1::Json,
                RetentionClassV1::WorkLocal,
            )
        });
    let mut unknown_components = vec![
        SetupComponentV1::Instructions,
        SetupComponentV1::Skills,
        SetupComponentV1::Tools,
    ];
    if model.is_none() {
        unknown_components.push(SetupComponentV1::Model);
    }
    if permission_mode.is_none() {
        unknown_components.push(SetupComponentV1::Permissions);
    }
    unknown_components.sort();
    Ok(Some(SetupObservationV1 {
        provider: None,
        model: model.map(str::to_owned),
        permission_mode: permission_mode.map(str::to_owned),
        effort: effort.map(str::to_owned),
        artifact,
        unknown_components,
    }))
}

fn batch(
    request: &CaptureRequest,
    operation_id: String,
    events: Vec<CapturedEventV1>,
) -> CaptureBatchV1 {
    CaptureBatchV1 {
        schema_version: SCHEMA_VERSION_V1,
        operation_id,
        work_id: request.work_id.clone(),
        generation: request.generation,
        session: SessionIdentityV1 {
            harness: AgentHarnessV1::ClaudeCode,
            adapter_version: ADAPTER_VERSION.into(),
            session_id: request.canonical_session_id.clone(),
            parent_session_id: request.parent_session_id.clone(),
            capture_mode: request.capture_mode,
        },
        events,
    }
}

fn tool_fact(
    name: Option<&str>,
    request_id: Option<&str>,
    completion: Option<ToolCompletionV1>,
) -> ToolFactV1 {
    ToolFactV1 {
        kind: tool_kind(name),
        request_id: request_id
            .filter(|value| identifier(value, 255))
            .map(str::to_owned),
        name: name
            .filter(|value| identifier(value, 128))
            .map(str::to_owned),
        completion,
    }
}

/// SessionStart `source` or SessionEnd `reason`, kept only when allowlisted.
fn hook_boundary(event_name: &str, payload: &serde_json::Map<String, Value>) -> Option<String> {
    let (field, allowed): (_, &[&str]) = match event_name {
        "SessionStart" => ("source", &["startup", "resume", "clear", "compact"]),
        "SessionEnd" => ("reason", &["clear", "logout", "prompt_input_exit"]),
        _ => return None,
    };
    let value = payload.get(field).and_then(Value::as_str);
    match (event_name, value) {
        (_, Some(value)) if allowed.contains(&value) => Some(value.to_owned()),
        ("SessionEnd", Some(_)) => Some("other".to_owned()),
        _ => None,
    }
}

fn hook_subagent(
    event_name: &str,
    payload: &serde_json::Map<String, Value>,
) -> Option<SubagentFactV1> {
    if !matches!(event_name, "SubagentStart" | "SubagentStop") {
        return None;
    }
    let field = |key: &str, max| {
        payload
            .get(key)
            .and_then(Value::as_str)
            .filter(|value| identifier(value, max))
            .map(str::to_owned)
    };
    Some(SubagentFactV1 {
        agent_id: field("agent_id", 255)?,
        agent_type: field("agent_type", 128),
    })
}

fn check_kind(default: CapturedEventKindV1, tool_name: Option<&str>) -> CapturedEventKindV1 {
    match tool_name {
        Some(name) if is_check(name) && default == CapturedEventKindV1::ToolStarted => {
            CapturedEventKindV1::CheckStarted
        }
        Some(name) if is_check(name) && default == CapturedEventKindV1::ToolFinished => {
            CapturedEventKindV1::CheckFinished
        }
        _ => default,
    }
}

fn is_check(name: &str) -> bool {
    matches!(name.to_ascii_lowercase().as_str(), "test" | "tests")
}

fn tool_kind(name: Option<&str>) -> ToolKindV1 {
    match name.unwrap_or_default().to_ascii_lowercase().as_str() {
        "read" | "view" => ToolKindV1::Read,
        "edit" | "apply_patch" | "multiedit" => ToolKindV1::Edit,
        "write" => ToolKindV1::Write,
        "search" | "grep" | "glob" => ToolKindV1::Search,
        "bash" | "shell" | "terminal" => ToolKindV1::Shell,
        "test" | "tests" => ToolKindV1::Test,
        "task" | "agent" => ToolKindV1::Model,
        _ => ToolKindV1::Other,
    }
}

fn identifier(value: &str, max: usize) -> bool {
    !value.is_empty()
        && value.len() <= max
        && value.bytes().all(|byte| {
            byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-' | b':' | b'@')
        })
}

fn next_line(
    input: &mut impl BufRead,
    buffer: &mut Vec<u8>,
    remaining: u64,
) -> std::io::Result<ReadLine> {
    buffer.clear();
    let (mut seen, mut oversized, mut bytes) = (false, false, 0u64);
    loop {
        let chunk = input.fill_buf()?;
        if chunk.is_empty() {
            break;
        }
        seen = true;
        let newline = chunk.iter().position(|byte| *byte == b'\n');
        let available = usize::try_from(remaining.saturating_sub(bytes).saturating_add(1))
            .unwrap_or(usize::MAX);
        let take = newline
            .map_or(chunk.len(), |index| index + 1)
            .min(available);
        bytes = bytes.saturating_add(take as u64);
        if bytes > remaining {
            input.consume(take);
            return Ok(ReadLine::Limit);
        }
        if buffer.len().saturating_add(take) > MAX_JSONL_LINE_BYTES {
            oversized = true;
            buffer.clear();
        }
        if !oversized {
            buffer.extend_from_slice(&chunk[..take]);
        }
        input.consume(take);
        if newline.is_some() {
            break;
        }
    }
    Ok(match (seen, oversized) {
        (false, _) => ReadLine::End,
        (true, true) => ReadLine::Oversized(bytes),
        (true, false) => ReadLine::Bytes(bytes),
    })
}
