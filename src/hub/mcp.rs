//! `POST /mcp`: a stateless MCP (streamable HTTP) endpoint with seven tools. Six share the
//! session code with the hooks. Hand-rolled JSON-RPC 2.0: one message per request, no
//! `Mcp-Session-Id`, no SSE, no batches.
use axum::Router;
use axum::body::Bytes;
use axum::extract::rejection::QueryRejection;
use axum::extract::{Json, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};

use super::advice::{answered, asked, unknown};
use super::http::{ApiError, Reader, blocking};
use super::session::{SessionCall, announce_for, done_for, others_for, status_for};
use super::skill_workflow::ReadError as SkillWorkflowReadError;
use super::store::{Principal, unix_now};
use super::{HubError, HubState, Result};

const VERSIONS: [&str; 3] = ["2025-11-25", "2025-06-18", "2025-03-26"];
const MAX_CHECK_PATHS: usize = super::session::MAX_SESSION_PATHS;
const AGENT_INSTRUCTIONS: &str = "AgentSCM tells you when another agent is working on the same files. \
    Call agentscm_start at the start of a task, agentscm_check before editing a file you have not \
    edited yet in this session, and agentscm_done when finished. If a reply says another agent is \
    already working on a file, do other work first or ask the user. When an edit is refused \
    because another agent holds the file, agentscm_ask asks that agent to hand it over, and \
    agentscm_handoff answers such a request. agentscm_workflow_status reads a retained native \
    workflow run; its historical state and present applicability are separate and are not an \
    admission decision.";
const READ_ONLY_INSTRUCTIONS: &str = "Use agentscm_workflow_status to read a retained native \
    workflow run mapped to this authenticated repository. Historical run state and present \
    applicability are separate, and neither is an admission decision.";
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
            && params.and_then(|params| params["name"].as_str())
                == Some("agentscm_workflow_status"))
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
    if read_only {
        return json!([workflow]);
    }
    json!([
        {"name": "agentscm_start",
         "description": "Call once at the start of a task. Records the task and returns who else is working in this repository.",
         "inputSchema": schema(json!({
             "task": {"type": "string", "description": "What you are about to do, in one line."},
             "paths": paths("Repo-relative files you expect to edit."),
             "session": session}), &["task"])},
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

/// `None`: the arguments do not fit the tool.
async fn run(
    state: &HubState,
    principal: &Principal,
    name: &str,
    arguments: Option<&Value>,
) -> Option<Result<String>> {
    let token = principal.token_id.clone();
    Some(match name {
        "agentscm_start" => {
            let StartArgs {
                task,
                paths,
                session,
            } = args(arguments)?;
            start(state, principal, session_call(session, Some(task), paths)).await
        }
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
        _ => {
            let SessionArgs { session } = args(arguments)?;
            done_for(state, principal, session).await.map(|ended| {
                if ended {
                    "AgentSCM: released."
                } else {
                    "AgentSCM: nothing to release."
                }
                .to_owned()
            })
        }
    })
}

async fn start(state: &HubState, principal: &Principal, call: SessionCall) -> Result<String> {
    let advice = announce_for(state, principal, call).await?;
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
    Ok(parts.join("\n"))
}

async fn check(state: &HubState, principal: &Principal, call: SessionCall) -> Result<String> {
    let advice = announce_for(state, principal, call).await?;
    Ok(if advice.text.is_empty() {
        NO_OVERLAPS.to_owned()
    } else {
        advice.text
    })
}
