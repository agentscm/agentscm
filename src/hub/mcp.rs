//! `POST /mcp`: a stateless MCP (streamable HTTP) endpoint. The legacy coordination tools share
//! session code with the hooks; durable work tools share the authenticated store. Hand-rolled
//! JSON-RPC 2.0: one message per request, no
//! `Mcp-Session-Id`, no SSE, no batches.
use axum::Router;
use axum::body::Bytes;
use axum::extract::rejection::QueryRejection;
use axum::extract::{Json, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use super::advice::{answered, asked, unknown};
use super::http::{ApiError, Reader, blocking};
use super::session::{SessionCall, announce_for, done_for, others_for, status_for};
use super::skill_workflow::ReadError as SkillWorkflowReadError;
use super::store::{Principal, unix_now};
use super::work::{
    Briefing, BriefingAck, BriefingQuery, CheckpointKind, ForgeWorkRef, WorkCheckpoint,
    WorkConflictKind, WorkRole, WorkStart, WorkStartResult, WorkState,
};
use super::{HubError, HubState, Result};

const VERSIONS: [&str; 3] = ["2025-11-25", "2025-06-18", "2025-03-26"];
const MAX_CHECK_PATHS: usize = super::session::MAX_SESSION_PATHS;
const AGENT_INSTRUCTIONS: &str = "AgentSCM coordinates durable shared work and tells you when another agent is working on the same files. \
    Read agentscm_work_briefing before planning meaningful work, then call agentscm_start with the known user intent before editing. \
    Reuse work_id, a canonical forge source, or a stable request_key across sessions; a legacy task-only start is retry-safe only in its resolved session. \
    Claim work before taking a coordinated file path. Call agentscm_check before editing a file you have not \
    edited yet in this session, and agentscm_done when finished. If a reply says another agent is \
    already working on a file, do other work first or ask the user. When an edit is refused \
    because another agent holds the file, agentscm_ask asks that agent to hand it over, and \
    agentscm_handoff answers such a request. agentscm_workflow_status reads a retained native \
    workflow run; its historical state and present applicability are separate and are not an \
    admission decision. Record meaningful progress, decisions, blockers, pauses, finishes and handovers with agentscm_work_checkpoint. \
    Acknowledge a complete briefing only after it was deliberately delivered; agentscm_done releases the file intent but does not accept, land or finish the larger work.";
const READ_ONLY_INSTRUCTIONS: &str = "Use agentscm_work_briefing to inspect unfinished shared work and its acknowledged delta, and agentscm_workflow_status to read a retained native \
    workflow run mapped to this authenticated repository. Historical run state and present \
    applicability are separate, and neither is an admission decision. Read-only briefing calls never create or acknowledge a delivery.";
/// Overlaps are reported once per session, so a repeat check of a reported file is quiet.
const NO_OVERLAPS: &str =
    "AgentSCM: nothing new about these files since this session's last report.";
const HOOKED: &str = "Edits are checked automatically by hooks in this session; call \
    agentscm_check only for files you edit outside them.";

const PARSE_ERROR: (i32, &str) = (-32700, "parse error");
const INVALID_REQUEST: (i32, &str) = (-32600, "invalid request");
const NO_METHOD: (i32, &str) = (-32601, "method not found");
const INVALID_PARAMS: (i32, &str) = (-32602, "invalid params");

pub fn routes() -> Router<HubState> {
    Router::new().route("/mcp", post(mcp))
}

#[derive(Deserialize)]
struct McpQuery {
    repo: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct StartArgs {
    task: String,
    #[serde(default)]
    paths: Vec<String>,
    session: Option<String>,
    work_id: Option<String>,
    source: Option<ForgeWorkRef>,
    request_key: Option<String>,
    #[serde(default)]
    role: WorkRole,
    branch: Option<String>,
    worktree: Option<String>,
    next_action: Option<String>,
    #[serde(default)]
    depends_on: Vec<String>,
    #[serde(default)]
    related_work: Vec<String>,
    takeover_generation: Option<i64>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CheckArgs {
    paths: Vec<String>,
    session: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct AskArgs {
    path: String,
    note: Option<String>,
    session: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct HandoffArgs {
    path: String,
    accept: bool,
    reason: Option<String>,
    session: Option<String>,
}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct SessionArgs {
    session: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WorkflowStatusArgs {
    run_id: String,
}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct BriefingArgs {
    delivery: Option<String>,
    after: Option<i64>,
    work_after: Option<String>,
    #[serde(default)]
    work_done: bool,
    through: Option<i64>,
    #[serde(default = "default_briefing_limit")]
    limit: usize,
}

fn default_briefing_limit() -> usize {
    20
}

async fn mcp(
    State(state): State<HubState>,
    Reader(principal): Reader,
    query: std::result::Result<Query<McpQuery>, QueryRejection>,
    body: Bytes,
) -> std::result::Result<Response, ApiError> {
    let Query(query) = query.map_err(|_| ApiError::invalid())?;
    let Ok(message) = serde_json::from_slice::<Value>(&body) else {
        return Ok(failure(&Value::Null, PARSE_ERROR));
    };
    let Some(message) = message.as_object() else {
        return Ok(failure(&Value::Null, INVALID_REQUEST));
    };
    let Some(id) = message.get("id") else {
        // A notification.
        return Ok(StatusCode::ACCEPTED.into_response());
    };
    let method = message.get("method").and_then(Value::as_str);
    if method.is_none() && (message.contains_key("result") || message.contains_key("error")) {
        // A response to a request this server never sends.
        return Ok(StatusCode::ACCEPTED.into_response());
    }
    if !(id.is_string() || id.is_number()) {
        return Ok(failure(&Value::Null, INVALID_REQUEST));
    }
    let Some(method) = method else {
        return Ok(failure(id, INVALID_REQUEST));
    };
    let params = message.get("params");
    if principal.read_only && !read_only_call(method, params) {
        return Err(ApiError::read_only());
    }
    let outcome = match method {
        "initialize" => Ok(initialize(params, principal.read_only)),
        "ping" => Ok(json!({})),
        "tools/list" => Ok(json!({"tools": tools(principal.read_only)})),
        "tools/call" => {
            let foreign = query
                .repo
                .as_deref()
                .is_some_and(|repo| repo != principal.repository);
            call(&state, &principal, foreign, params).await
        }
        _ => Err(NO_METHOD),
    };
    Ok(match outcome {
        Ok(result) => Json(json!({"jsonrpc": "2.0", "id": id, "result": result})).into_response(),
        Err(error) => failure(id, error),
    })
}

fn read_only_call(method: &str, params: Option<&Value>) -> bool {
    matches!(method, "initialize" | "ping" | "tools/list")
        || (method == "tools/call"
            && matches!(
                params.and_then(|params| params["name"].as_str()),
                Some("agentscm_work_briefing" | "agentscm_workflow_status")
            ))
}

fn failure(id: &Value, (code, message): (i32, &str)) -> Response {
    Json(json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}}))
        .into_response()
}

fn initialize(params: Option<&Value>, read_only: bool) -> Value {
    let version = params
        .and_then(|params| params["protocolVersion"].as_str())
        .filter(|asked| VERSIONS.contains(asked))
        .unwrap_or(VERSIONS[1]);
    json!({
        "protocolVersion": version,
        "capabilities": {"tools": {"listChanged": false}},
        "serverInfo": {"name": "agentscm", "version": env!("CARGO_PKG_VERSION")},
        "instructions": if read_only { READ_ONLY_INSTRUCTIONS } else { AGENT_INSTRUCTIONS },
    })
}

fn tools(read_only: bool) -> Value {
    let session = json!({"type": "string", "description": "Session id; omit to use this agent's current session."});
    let paths = |description: &str| json!({"type": "array", "items": {"type": "string"}, "description": description});
    let schema = |properties: Value, required: &[&str]| {
        json!({"type": "object", "properties": properties, "required": required,
            "additionalProperties": false})
    };
    let workflow = json!(
        {"name": "agentscm_workflow_status",
         "description": "Reads one retained native skill workflow run mapped to this authenticated repository. Historical state and present applicability are separate; this is not an admission decision.",
         "annotations": {"readOnlyHint": true},
         "inputSchema": schema(json!({
             "run_id": {"type": "string", "minLength": 1, "maxLength": 64,
                 "description": "Retained native workflow run ID."}}), &["run_id"])}
    );
    let briefing = json!(
        {"name": "agentscm_work_briefing",
         "description": "Reads a bounded snapshot of unfinished shared work plus changes after the acknowledged cursor. Writer tokens receive a durable delivery to page and deliberately acknowledge; read-only tokens perform a pure read.",
         "annotations": {"readOnlyHint": true},
         "inputSchema": schema(json!({
             "delivery": {"type": "string", "description": "Durable delivery id returned by the first writer page; repeat the same cursors to retry the exact page."},
             "after": {"type": "integer", "minimum": 0, "description": "Event cursor; use next_after from the previous page."},
             "work_after": {"type": "string", "description": "Work snapshot cursor; use next_work_after from the previous page."},
             "work_done": {"type": "boolean", "description": "Keep true after next_work_done becomes true while paging remaining events."},
             "through": {"type": "integer", "minimum": 0, "description": "Fixed upper event sequence returned by the first page."},
             "limit": {"type": "integer", "minimum": 1, "maximum": 100, "default": 20}
         }), &[])}
    );
    if read_only {
        return json!([briefing, workflow]);
    }
    json!([
        {"name": "agentscm_start",
         "description": "Call once at the start of a task. Records the task and returns who else is working in this repository.",
         "inputSchema": schema(json!({
             "task": {"type": "string", "description": "What you are about to do, in one line."},
             "paths": paths("Repo-relative files you expect to edit."),
             "session": session,
             "work_id": {"type": "string", "description": "Existing durable work to resume or join."},
             "source": {"type": "object", "additionalProperties": false,
                 "properties": {
                     "provider": {"type": "string"}, "host": {"type": "string"},
                     "repository": {"type": "string"}, "issue": {"type": "string"}},
                 "required": ["provider", "host", "repository", "issue"]},
             "request_key": {"type": "string", "description": "Stable caller identity for one native work goal; reuse it across retries and sessions."},
             "role": {"type": "string", "enum": ["lead", "helper", "reviewer"], "default": "lead"},
             "branch": {"type": "string"}, "worktree": {"type": "string"},
             "next_action": {"type": "string"},
             "depends_on": {"type": "array", "items": {"type": "string"}},
             "related_work": {"type": "array", "items": {"type": "string"}},
             "takeover_generation": {"type": "integer", "minimum": 1}
         }), &["task"])},
        {"name": "agentscm_check",
         "description": "Call before editing a file you have not edited yet in this session; not needed in Claude Code, Codex or Copilot with AgentSCM hooks, which check each edit. Says when another agent is already working on those files.",
         "inputSchema": schema(json!({
             "paths": {"type": "array", "items": {"type": "string"}, "minItems": 1,
                 "maxItems": MAX_CHECK_PATHS,
                 "description": "Repo-relative files you are about to edit."},
             "session": session}), &["paths"])},
        {"name": "agentscm_status",
         "description": "Other agents and open pull requests on this repository.",
         "inputSchema": schema(json!({"session": session}), &[])},
        {"name": "agentscm_done",
         "description": "Call when the task is finished; releases your files.",
         "inputSchema": schema(json!({"session": session}), &[])},
        {"name": "agentscm_ask",
         "description": "Asks the agent that holds a file to hand it over to this session. One request per file; the holder answers with agentscm_handoff.",
         "inputSchema": schema(json!({
             "path": {"type": "string", "description": "The repo-relative file that was refused."},
             "note": {"type": "string", "maxLength": 200, "description": "Why you need it, in one line."},
             "session": session}), &["path"])},
        {"name": "agentscm_handoff",
         "description": "Answers another agent's request for a file this session holds: accept true hands it over, false keeps it.",
         "inputSchema": schema(json!({
             "path": {"type": "string", "description": "The repo-relative file that was asked for."},
             "accept": {"type": "boolean"},
             "reason": {"type": "string", "maxLength": 200, "description": "With accept false: why, in one line."},
             "session": session}), &["path", "accept"])},
        briefing,
        {"name": "agentscm_work_checkpoint",
         "description": "Records one idempotent progress, decision, blocker, pause, finish or handover checkpoint against the exact current work generation. Returns a receipt for the committed operation and the current work item; the item may reflect later activity when an exact operation retry resolves an uncertain outcome.",
         "inputSchema": schema(json!({
             "work_id": {"type": "string"},
             "operation_id": {"type": "string", "description": "Stable idempotency key for this exact checkpoint."},
             "generation": {"type": "integer", "minimum": 1},
             "expected_updated_seq": {"type": "integer", "minimum": 1, "description": "Required compare-and-set fence from the last observed work item. An exact retained request returns its stored receipt before this fence is compared."},
             "session": {"type": "string"},
             "kind": {"type": "string", "enum": ["progress", "decision", "blocker", "pause", "finish", "handover"]},
             "summary": {"type": "string"}, "next_action": {"type": "string"},
             "scope": {"type": "array", "items": {"type": "string"}},
             "branch": {"type": "string"}, "worktree": {"type": "string"},
             "resolve_blocker": {"type": "boolean", "default": false},
             "source_refs": {"type": "array", "items": {
                 "oneOf": [
                     {"type": "object", "additionalProperties": false,
                         "properties": {"kind": {"const": "repository_path"}, "path": {"type": "string"}, "line": {"type": "integer", "minimum": 1}},
                         "required": ["kind", "path"]},
                     {"type": "object", "additionalProperties": false,
                         "properties": {"kind": {"const": "commit"}, "oid": {"type": "string"}, "path": {"type": "string"}},
                         "required": ["kind", "oid"]},
                     {"type": "object", "additionalProperties": false,
                         "properties": {"kind": {"const": "work"}, "work_id": {"type": "string"}},
                         "required": ["kind", "work_id"]}
                 ]}},
             "depends_on": {"type": "array", "items": {"type": "string"}},
             "related_work": {"type": "array", "items": {"type": "string"}}
         }), &["work_id", "operation_id", "generation", "expected_updated_seq", "session", "kind", "summary"])},
        {"name": "agentscm_work_ack",
         "description": "Acknowledges one complete durable briefing after it was deliberately delivered. Never call this for a partial or unseen briefing.",
         "inputSchema": schema(json!({"delivery": {"type": "string"}}), &["delivery"])},
        workflow,
    ])
}

/// Parsed arguments; `None` for anything the tool does not accept.
fn args<T: DeserializeOwned>(arguments: Option<&Value>) -> Option<T> {
    let arguments = match arguments {
        None | Some(Value::Null) => json!({}),
        Some(other) => other.clone(),
    };
    serde_json::from_value(arguments).ok()
}

fn session_call(session: Option<String>, task: Option<String>, paths: Vec<String>) -> SessionCall {
    SessionCall {
        session,
        via: "mcp".into(),
        task,
        paths,
        deny: false,
        report: true,
    }
}

/// A tool result; reasons are fixed hub strings, never client input.
fn result(text: &str, is_error: bool) -> Value {
    json!({"content": [{"type": "text", "text": text}], "isError": is_error})
}

fn refused(reason: &str) -> Value {
    result(&unknown(reason), true)
}

fn hub_failure(error: &HubError) -> Value {
    refused(match error {
        HubError::Busy => "hub busy",
        HubError::WorkCapacity => "work storage capacity reached",
        HubError::Unavailable(_) => "hub unavailable",
        HubError::Invalid(_) => "invalid request",
        HubError::Unauthorized => "token not accepted",
    })
}

/// Argument problems are tool errors, not protocol errors; only a malformed call is -32602.
async fn call(
    state: &HubState,
    principal: &Principal,
    foreign: bool,
    params: Option<&Value>,
) -> std::result::Result<Value, (i32, &'static str)> {
    let name = params
        .and_then(|params| params["name"].as_str())
        .ok_or(INVALID_PARAMS)?;
    if !matches!(
        name,
        "agentscm_start"
            | "agentscm_check"
            | "agentscm_status"
            | "agentscm_done"
            | "agentscm_ask"
            | "agentscm_handoff"
            | "agentscm_work_briefing"
            | "agentscm_work_checkpoint"
            | "agentscm_work_ack"
            | "agentscm_workflow_status"
    ) {
        return Err(INVALID_PARAMS);
    }
    if foreign {
        return Ok(refused("this token is for another repository"));
    }
    let arguments = params.and_then(|params| params.get("arguments"));
    if name == "agentscm_workflow_status" {
        let Some(WorkflowStatusArgs { run_id }) = args(arguments) else {
            return Ok(refused("invalid arguments"));
        };
        return Ok(workflow_status(state, principal, &run_id).await);
    }
    if name == "agentscm_work_briefing" {
        let Some(arguments) = args::<BriefingArgs>(arguments) else {
            return Ok(refused("invalid arguments"));
        };
        return Ok(work_briefing(state, principal, arguments).await);
    }
    if name == "agentscm_work_checkpoint" {
        let Some(checkpoint) = args::<WorkCheckpoint>(arguments) else {
            return Ok(refused("invalid arguments"));
        };
        return Ok(work_checkpoint(state, principal, checkpoint).await);
    }
    if name == "agentscm_work_ack" {
        let Some(ack) = args::<BriefingAck>(arguments) else {
            return Ok(refused("invalid arguments"));
        };
        return Ok(work_ack(state, principal, ack).await);
    }
    if name == "agentscm_start" {
        let Some(arguments) = args::<StartArgs>(arguments) else {
            return Ok(refused("invalid arguments"));
        };
        return Ok(match start(state, principal, arguments).await {
            Ok(value) => value,
            Err(error) => hub_failure(&error),
        });
    }
    let Some(outcome) = run(state, principal, name, arguments).await else {
        return Ok(refused("invalid arguments"));
    };
    Ok(match outcome {
        Ok(text) => result(&text, false),
        Err(error) => hub_failure(&error),
    })
}

async fn workflow_status(state: &HubState, principal: &Principal, run_id: &str) -> Value {
    let registry = state.skill_workflows.clone();
    let repository = principal.repository.clone();
    let run_id = run_id.to_owned();
    match blocking(state, move || Ok(registry.read(&repository, &run_id))).await {
        Ok(Ok(envelope)) => match serde_json::to_string(&envelope) {
            Ok(text) => result(&text, false),
            Err(_) => refused("hub unavailable"),
        },
        Ok(Err(SkillWorkflowReadError::InvalidRunId)) => refused("invalid arguments"),
        Ok(Err(SkillWorkflowReadError::NotFound)) => refused("workflow run not found"),
        Ok(Err(SkillWorkflowReadError::Internal(detail))) => {
            eprintln!("hub workflow error: {detail}");
            refused("hub unavailable")
        }
        Err(error) => hub_failure(&error),
    }
}

async fn work_briefing(state: &HubState, principal: &Principal, arguments: BriefingArgs) -> Value {
    let query = BriefingQuery {
        delivery: arguments.delivery,
        after: arguments.after,
        work_after: arguments.work_after,
        work_done: arguments.work_done,
        through: arguments.through,
        limit: arguments.limit,
    };
    let store = state.store.clone();
    let token = principal.token_id.clone();
    match blocking(state, move || {
        store.work_briefing(&token, &query, unix_now())
    })
    .await
    {
        Ok(briefing) => structured(&briefing),
        Err(error) => hub_failure(&error),
    }
}

async fn work_checkpoint(
    state: &HubState,
    principal: &Principal,
    checkpoint: WorkCheckpoint,
) -> Value {
    let store = state.store.clone();
    let token = principal.token_id.clone();
    match blocking(state, move || {
        store.work_checkpoint(&token, &checkpoint, unix_now())
    })
    .await
    {
        Ok(item) => structured(&item),
        Err(error) => work_failure(&error),
    }
}

async fn work_ack(state: &HubState, principal: &Principal, ack: BriefingAck) -> Value {
    let store = state.store.clone();
    let token = principal.token_id.clone();
    match blocking(state, move || store.work_ack(&token, &ack, unix_now())).await {
        Ok(()) => structured(&json!({"acknowledged": true})),
        Err(error) => work_failure(&error),
    }
}

fn structured<T: Serialize>(value: &T) -> Value {
    match serde_json::to_value(value) {
        Ok(value) => match serde_json::to_string(&value) {
            Ok(text) => json!({
                "content": [{"type": "text", "text": text}],
                "structuredContent": value,
                "isError": false
            }),
            Err(_) => refused("hub unavailable"),
        },
        Err(_) => refused("hub unavailable"),
    }
}

fn work_failure(error: &HubError) -> Value {
    let (kind, message, alternatives): (&str, &str, &[&str]) = match error {
        HubError::Invalid(message) => (
            "invalid_or_changed",
            message,
            &[
                "Refresh agentscm_work_briefing and inspect the current generation.",
                "Retry with the same stable identity only when the request is unchanged.",
            ],
        ),
        HubError::Busy => (
            "busy",
            "hub busy",
            &["Retry the same idempotent request later."],
        ),
        HubError::WorkCapacity => (
            "work_storage_capacity",
            "work storage capacity reached",
            &[
                "Request that the hub operator run agentscm work compact for this repository; do not access the hub database automatically.",
            ],
        ),
        HubError::Unauthorized => (
            "unauthorized",
            "token not accepted",
            &["Use an active agent token for this repository."],
        ),
        HubError::Unavailable(_) => (
            "unavailable",
            "hub unavailable",
            &["Preserve the same idempotency key and retry after the hub recovers."],
        ),
    };
    let value = json!({"error": {"kind": kind, "message": message, "alternatives": alternatives}});
    let text = serde_json::to_string(&value).unwrap_or_else(|_| "hub unavailable".to_owned());
    json!({
        "content": [{"type": "text", "text": text}],
        "structuredContent": value,
        "isError": true
    })
}

/// `None`: the arguments do not fit the tool.
async fn run(
    state: &HubState,
    principal: &Principal,
    name: &str,
    arguments: Option<&Value>,
) -> Option<Result<String>> {
    let token = principal.token_id.clone();
    Some(match name {
        "agentscm_check" => {
            let CheckArgs { paths, session } = args(arguments)?;
            if !(1..=MAX_CHECK_PATHS).contains(&paths.len()) {
                return None;
            }
            check(state, principal, session_call(session, None, paths)).await
        }
        "agentscm_status" => {
            let SessionArgs { session } = args(arguments)?;
            status_for(state, principal, session_call(session, None, Vec::new())).await
        }
        "agentscm_ask" => {
            let AskArgs {
                path,
                note,
                session,
            } = args(arguments)?;
            let store = state.store.clone();
            blocking(state, move || {
                store
                    .ask_handoff(
                        &token,
                        session.as_deref(),
                        &path,
                        note.as_deref(),
                        unix_now(),
                    )
                    .map(|outcome| asked(&outcome, &path))
            })
            .await
        }
        "agentscm_handoff" => {
            let HandoffArgs {
                path,
                accept,
                reason,
                session,
            } = args(arguments)?;
            let store = state.store.clone();
            blocking(state, move || {
                store
                    .answer_handoff(
                        &token,
                        session.as_deref(),
                        &path,
                        accept,
                        reason.as_deref(),
                        unix_now(),
                    )
                    .map(|outcome| answered(&outcome, &path))
            })
            .await
        }
        "agentscm_done" => {
            let SessionArgs { session } = args(arguments)?;
            done(state, principal, session).await
        }
        _ => return None,
    })
}

async fn done(state: &HubState, principal: &Principal, session: Option<String>) -> Result<String> {
    let store = state.store.clone();
    let token = principal.token_id.clone();
    let requested = session.clone();
    let resolved = blocking(state, move || {
        store.session_name_for(&token, requested.as_deref(), unix_now())
    })
    .await?;
    let store = state.store.clone();
    let token = principal.token_id.clone();
    let checkpoint_session = resolved.clone();
    let item = blocking(state, move || {
        store.work_for_session(&token, &checkpoint_session, unix_now())
    })
    .await;
    let ended = done_for(state, principal, Some(resolved)).await?;
    if !ended {
        return Ok("AgentSCM: nothing to release.".into());
    }
    let item = match item {
        Ok(Some(item)) => item,
        Ok(None) => return Ok("AgentSCM: released.".into()),
        Err(error) => {
            eprintln!("hub: work lookup at agentscm_done: {error}");
            return Ok("AgentSCM: released. Larger work checkpoint status is unknown.".into());
        }
    };
    match item.state {
        WorkState::Paused => {
            return Ok("AgentSCM: released; unfinished durable work remains paused for resumption or deliberate handover.".into());
        }
        WorkState::Blocked => {
            return Ok("AgentSCM: released; durable work remains blocked and its blocker was not resolved.".into());
        }
        WorkState::Handover => {
            return Ok(
                "AgentSCM: released; durable work remains ready for deliberate handover.".into(),
            );
        }
        WorkState::Active => {}
        WorkState::Finished => return Ok("AgentSCM: released.".into()),
    }
    let checkpoint = WorkCheckpoint {
        work_id: item.id,
        operation_id: format!(
            "mcp-done-{}-{}-{}",
            item.lead_session, item.generation, item.updated_seq
        ),
        generation: item.generation,
        expected_updated_seq: Some(item.updated_seq),
        session: item.lead_session,
        kind: CheckpointKind::Pause,
        summary: "Session ended without a reported final work outcome.".into(),
        next_action: item
            .next_action
            .or_else(|| Some("Resume or deliberately hand over this unfinished work.".into())),
        scope: None,
        branch: None,
        worktree: None,
        resolve_blocker: false,
        source_refs: Vec::new(),
        depends_on: item.depends_on,
        related_work: item.related_work,
    };
    let store = state.store.clone();
    let token = principal.token_id.clone();
    match blocking(state, move || {
        store.work_checkpoint(&token, &checkpoint, unix_now())
    })
    .await
    {
        Ok(_) => Ok("AgentSCM: released; unfinished durable work was paused for resumption or deliberate handover.".into()),
        Err(error) => {
            eprintln!("hub: pause checkpoint at agentscm_done: {error}");
            Ok("AgentSCM: released. Larger work checkpoint status is unknown.".into())
        }
    }
}

async fn start(state: &HubState, principal: &Principal, arguments: StartArgs) -> Result<Value> {
    let StartArgs {
        task,
        paths,
        session,
        work_id,
        source,
        request_key,
        role,
        branch,
        worktree,
        next_action,
        depends_on,
        related_work,
        takeover_generation,
    } = arguments;

    // Validate every legacy session/path field before a briefing receipt or durable claim can be
    // written. The actual touch still happens only after the claim succeeds.
    super::session::check(&session_call(
        session.clone(),
        Some(task.clone()),
        paths.clone(),
    ))?;

    // Preview current shared state before a new durable goal can be created. The preview is pure;
    // explicit agentscm_work_briefing calls create the only deliveries that can be acknowledged.
    let store = state.store.clone();
    let token = principal.token_id.clone();
    let briefing = blocking(state, move || {
        store.work_briefing_preview(&token, 20, unix_now())
    })
    .await?;

    // Resolve the same session rules as hooks without taking any coordinated path. The durable
    // claim below must succeed before this call announces the requested paths.
    let resolved = announce_for(
        state,
        principal,
        SessionCall {
            session,
            via: "mcp".into(),
            task: None,
            paths: Vec::new(),
            deny: false,
            report: false,
        },
    )
    .await?
    .touched
    .session;

    let explicit = work_id.is_some() || source.is_some() || request_key.is_some();
    let store = state.store.clone();
    let token = principal.token_id.clone();
    let title = task.clone();
    let work = blocking(state, move || {
        let start = WorkStart {
            work_id,
            source,
            request_key,
            title,
            session: resolved.clone(),
            role,
            scope: paths.clone(),
            branch,
            worktree,
            next_action,
            depends_on,
            related_work,
            takeover_generation,
        };
        if explicit {
            store.work_start(&token, &start, unix_now())
        } else {
            store.work_start_auto(&token, &start, unix_now())
        }
        .map(|work| (work, resolved, paths))
    })
    .await?;
    let (work, resolved, paths) = work;
    if work.conflict.is_some() {
        return Ok(start_result(String::new(), briefing, work));
    }

    let advice = announce_for(
        state,
        principal,
        session_call(Some(resolved), Some(task), paths),
    )
    .await?;
    let own = advice
        .touched
        .intent
        .as_ref()
        .map(|intent| intent.id.as_str());
    let mut parts = Vec::new();
    if !advice.text.is_empty() {
        parts.push(advice.text.clone());
    }
    // The touch is committed; a failed listing must not turn the start into an error.
    parts.push(match others_for(state, principal, own).await {
        Ok(others) => others,
        Err(error) => {
            eprintln!("hub: other agents for a start: {error}");
            unknown("other agents could not be listed")
        }
    });
    if advice
        .touched
        .via
        .iter()
        .any(|via| via.starts_with("hook:"))
    {
        parts.push(HOOKED.to_owned());
    }
    Ok(start_result(parts.join("\n"), briefing, work))
}

fn start_result(coordination: String, briefing: Briefing, work: WorkStartResult) -> Value {
    let alternatives: &[&str] = match work.conflict.as_ref().map(|conflict| conflict.kind) {
        Some(WorkConflictKind::ClaimHeld) => &[
            "Choose distinct work or join this work as helper or reviewer.",
            "Ask the current lead for a deliberate handover.",
        ],
        Some(WorkConflictKind::StaleTakeoverRequired) => &[
            "Refresh the briefing and inspect the stale claim.",
            "Retry the explicit work id with takeover_generation set to current_generation only if takeover is intended.",
        ],
        Some(WorkConflictKind::GenerationChanged) => &[
            "Refresh the briefing and inspect the new generation.",
            "Retry against current_generation only if the same takeover is still intended.",
        ],
        Some(WorkConflictKind::SessionGoalChanged) => &[
            "Keep the current session bound to its existing unfinished goal.",
            "Use an explicit stable work identity in another session for distinct work.",
        ],
        Some(WorkConflictKind::MetadataChanged) => &[
            "Retry the same start identity with the original metadata.",
            "Record deliberate scope, branch, worktree or next-action changes with agentscm_work_checkpoint.",
        ],
        None => &[],
    };
    let structured = json!({
        "coordination": &coordination,
        "briefing": &briefing,
        "briefing_kind": "preview",
        "acknowledgement_required": false,
        "work": &work,
        "alternatives": alternatives,
        "acknowledged": false,
    });
    let mut text = coordination;
    if !text.is_empty() {
        text.push('\n');
    }
    text.push_str(&briefing_summary(&briefing));
    text.push('\n');
    text.push_str(&format!(
        "AgentSCM durable work {} at generation {}{}.",
        work.item.id,
        work.item.generation,
        if work.conflict.is_some() {
            " has a claim conflict; inspect structuredContent for bounded alternatives"
        } else if work.created {
            " was created and claimed before coordinated paths were announced"
        } else {
            " was reused or joined before coordinated paths were announced"
        }
    ));
    json!({
        "content": [{"type": "text", "text": text}],
        "structuredContent": structured,
        "isError": work.conflict.is_some()
    })
}

fn briefing_summary(briefing: &Briefing) -> String {
    let follow_up = briefing.delivery.as_deref().map_or_else(
        || {
            " This is a pure preview; call agentscm_work_briefing to begin durable pagination. No acknowledgement was created."
                .to_owned()
        },
        |delivery| {
            format!(
                " Delivery {delivery} remains unacknowledged; continue it with the returned cursors before acknowledging the complete delivery."
            )
        },
    );
    format!(
        "AgentSCM briefing: {} unfinished item(s), {} change event(s), complete={}.{} Read structuredContent before planning.",
        briefing.items.len(),
        briefing.events.len(),
        briefing.complete,
        follow_up
    )
}

async fn check(state: &HubState, principal: &Principal, call: SessionCall) -> Result<String> {
    let advice = announce_for(state, principal, call).await?;
    Ok(if advice.text.is_empty() {
        NO_OVERLAPS.to_owned()
    } else {
        advice.text
    })
}
