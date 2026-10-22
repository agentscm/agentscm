use std::io::Write;
use std::path::Path;
use std::time::Duration;

use serde_json::{Value, json};

use agentscm::workspace::SnapshotTrigger;
use agentscm::workspace_timeline_service::WorkspaceTimelineService;

use super::{ClientAgent, PinnedWorkspace};
use crate::Error;

pub(super) const MAX_HOOK_INPUT_BYTES: usize = 64 * 1024;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CaptureHookOutcome {
    Captured,
    Unchanged,
    Ignored,
    Failed,
}

fn completion_event(agent: ClientAgent, payload: &Value) -> bool {
    match agent {
        ClientAgent::Claude
            if payload.get("hook_event_name").and_then(Value::as_str) == Some("Stop") =>
        {
            true
        }
        ClientAgent::Codex
            if payload.get("hook_event_name").and_then(Value::as_str) == Some("Stop") =>
        {
            true
        }
        ClientAgent::Cursor
            if payload.get("hook_event_name").and_then(Value::as_str) == Some("stop")
                && matches!(
                    payload.get("status").and_then(Value::as_str),
                    Some("completed" | "aborted" | "error")
                )
                && payload.get("loop_count").and_then(Value::as_u64).is_some() =>
        {
            true
        }
        ClientAgent::Copilot
            if (payload.get("stopReason").and_then(Value::as_str) == Some("end_turn")
                && payload.get("sessionId").and_then(Value::as_str).is_some()
                && payload
                    .get("stop_hook_active")
                    .and_then(Value::as_bool)
                    .is_some())
                || (payload.get("hook_event_name").and_then(Value::as_str) == Some("Stop")
                    && payload.get("stop_reason").and_then(Value::as_str) == Some("end_turn")
                    && payload.get("session_id").and_then(Value::as_str).is_some()
                    && payload
                        .get("stop_hook_active")
                        .and_then(Value::as_bool)
                        .is_some()) =>
        {
            true
        }
        ClientAgent::Gemini
            if payload.get("hook_event_name").and_then(Value::as_str) == Some("AfterAgent")
                && payload
                    .get("stop_hook_active")
                    .and_then(Value::as_bool)
                    .is_some() =>
        {
            true
        }
        _ => false,
    }
}

fn event_root(agent: ClientAgent, payload: &Value) -> std::result::Result<&Path, ()> {
    if agent == ClientAgent::Cursor {
        let roots = payload
            .get("workspace_roots")
            .and_then(Value::as_array)
            .ok_or(())?;
        if roots.len() != 1 {
            return Err(());
        }
        return roots[0].as_str().map(Path::new).ok_or(());
    }
    if payload.get("workspace_roots").is_some() {
        return Err(());
    }
    payload
        .get("cwd")
        .and_then(Value::as_str)
        .map(Path::new)
        .ok_or(())
}

fn native_noop(_agent: ClientAgent) -> Value {
    json!({})
}

/// Best-effort completion capture. It always returns the client's nonblocking output shape.
/// Diagnostics are written to stderr; payload text and session identifiers are not retained.
pub fn run_capture_hook(
    agent: ClientAgent,
    pinned: &PinnedWorkspace,
    input: &[u8],
    service: &WorkspaceTimelineService,
    stderr: &mut impl Write,
) -> (CaptureHookOutcome, Value) {
    if input.len() > MAX_HOOK_INPUT_BYTES {
        let _ = writeln!(
            stderr,
            "AgentSCM capture skipped: hook input exceeds 64 KiB"
        );
        return (CaptureHookOutcome::Failed, native_noop(agent));
    }
    let payload: Value = match serde_json::from_slice(input) {
        Ok(Value::Object(payload)) => Value::Object(payload),
        _ => {
            let _ = writeln!(stderr, "AgentSCM capture skipped: unreadable hook input");
            return (CaptureHookOutcome::Failed, native_noop(agent));
        }
    };
    if !completion_event(agent, &payload) {
        return (CaptureHookOutcome::Ignored, native_noop(agent));
    }
    let root = match event_root(agent, &payload) {
        Ok(root) => root,
        Err(()) => {
            let _ = writeln!(stderr, "AgentSCM capture skipped: ambiguous workspace root");
            return (CaptureHookOutcome::Failed, native_noop(agent));
        }
    };
    if !pinned.contains_cwd(root) {
        return (CaptureHookOutcome::Ignored, native_noop(agent));
    }
    match service.capture_with_deadline(SnapshotTrigger::TurnEnd, Duration::from_secs(25)) {
        Ok(result) => match result
            .get("outcome")
            .and_then(|outcome| outcome.get("outcome"))
            .and_then(Value::as_str)
        {
            Some("captured") => (CaptureHookOutcome::Captured, native_noop(agent)),
            Some("unchanged") => (CaptureHookOutcome::Unchanged, native_noop(agent)),
            Some("paused") => {
                let _ = writeln!(
                    stderr,
                    "AgentSCM capture paused: the workspace timeline retention limit was reached"
                );
                (CaptureHookOutcome::Failed, native_noop(agent))
            }
            Some("skipped") => {
                let _ = writeln!(
                    stderr,
                    "AgentSCM capture skipped: retry capture explicitly from the pinned workspace"
                );
                (CaptureHookOutcome::Failed, native_noop(agent))
            }
            _ => {
                let _ = writeln!(stderr, "AgentSCM capture failed: invalid local result");
                (CaptureHookOutcome::Failed, native_noop(agent))
            }
        },
        Err(error) => {
            let _ = writeln!(stderr, "AgentSCM capture failed: {}", safe_error(&error));
            (CaptureHookOutcome::Failed, native_noop(agent))
        }
    }
}

fn safe_error(error: &Error) -> &'static str {
    match error {
        Error::Conflict(_) => "workspace state changed; retry capture explicitly",
        Error::Limit(_) => "a local capture limit was reached",
        _ => "inspect the local AgentSCM workspace and retry explicitly",
    }
}
