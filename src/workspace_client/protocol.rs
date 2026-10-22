use std::io::{BufRead, Write};
use std::time::Duration;

use serde::Deserialize;
use serde_json::{Map, Value, json};

use agentscm::workspace::SnapshotTrigger;
use agentscm::workspace_timeline_service::WorkspaceTimelineService;

use super::{
    PinnedWorkspace, checked_child_basename, checked_cursor, checked_id, checked_limit, invalid,
};
use crate::Result;

pub const MAX_MCP_REQUEST_BYTES: usize = 256 * 1024;
pub const MAX_MCP_RESULT_BYTES: usize = 1024 * 1024;
const SUPPORTED_VERSIONS: [&str; 3] = ["2025-11-25", "2025-06-18", "2025-03-26"];

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CaptureArgs {}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct TimelineArgs {
    before: Option<u64>,
    limit: Option<u16>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct EntryArgs {
    id: u64,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct DiffArgs {
    left: u64,
    right: u64,
    after: Option<String>,
    limit: Option<u16>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ForkArgs {
    id: u64,
    child_basename: String,
    change_id: String,
}

fn rpc_error(id: Value, code: i32, message: &str) -> Value {
    json!({"jsonrpc":"2.0","id":id,"error":{"code":code,"message":message}})
}

fn tool_result(value: Value) -> Result<Value> {
    let text = serde_json::to_string(&value)?;
    let result = json!({
        "content":[{"type":"text","text":text}],
        "structuredContent":value,
        "isError":false
    });
    if serde_json::to_vec(&result)?.len() > MAX_MCP_RESULT_BYTES {
        return Ok(tool_failure(
            "workspace operation result exceeds the local MCP output limit",
        ));
    }
    Ok(result)
}

fn tool_failure(message: &str) -> Value {
    json!({
        "content":[{"type":"text","text":message}],
        "isError":true
    })
}

fn id_schema() -> Value {
    json!({"type":"string","minLength":1,"maxLength":128,"pattern":"^[A-Za-z0-9._:/-]+$"})
}

fn tools(pinned: &PinnedWorkspace) -> Value {
    let annotations = |read_only: bool, idempotent: bool| {
        json!({
            "readOnlyHint":read_only,
            "destructiveHint":false,
            "idempotentHint":idempotent,
            "openWorldHint":false
        })
    };
    let schema = |properties: Map<String, Value>, required: Vec<&str>| json!({"type":"object","properties":properties,"required":required,"additionalProperties":false});
    let capture = Map::new();
    let mut timeline = Map::new();
    timeline.insert("before".into(), json!({"type":"integer","minimum":1}));
    timeline.insert(
        "limit".into(),
        json!({"type":"integer","minimum":1,"maximum":100}),
    );
    let mut entry = Map::new();
    entry.insert("id".into(), json!({"type":"integer","minimum":1}));
    let mut diff = Map::new();
    diff.insert("left".into(), json!({"type":"integer","minimum":1}));
    diff.insert("right".into(), json!({"type":"integer","minimum":1}));
    diff.insert(
        "after".into(),
        json!({"type":"string","minLength":1,"maxLength":4096}),
    );
    diff.insert(
        "limit".into(),
        json!({"type":"integer","minimum":1,"maximum":100}),
    );
    let mut fork = entry.clone();
    fork.insert(
        "child_basename".into(),
        json!({"type":"string","minLength":1,"maxLength":128,"pattern":"^[^/\\\\]+$"}),
    );
    fork.insert("change_id".into(), id_schema());
    let mut tools = vec![
        json!({"name":"agentscm_snapshot_capture","description":"Capture the pinned workspace bytes as a retained snapshot. This does not approve, land, publish, or invoke a model.","inputSchema":schema(capture,vec![]),"annotations":annotations(false,false)}),
        json!({"name":"agentscm_snapshot_timeline","description":"List retained snapshots for the pinned workspace.","inputSchema":schema(timeline,vec![]),"annotations":annotations(true,true)}),
        json!({"name":"agentscm_snapshot_entry","description":"Read one retained snapshot and its exact-source check associations.","inputSchema":schema(entry.clone(),vec!["id"]),"annotations":annotations(true,true)}),
        json!({"name":"agentscm_snapshot_diff","description":"Read a bounded diff between retained snapshots.","inputSchema":schema(diff,vec!["left","right"]),"annotations":annotations(true,true)}),
    ];
    if pinned.fork_root.is_some() {
        tools.push(json!({"name":"agentscm_snapshot_fork","description":"Materialize a snapshot beneath the operator-pinned fork root into a different existing Change. This does not approve, land, publish, or merge.","inputSchema":schema(fork,vec!["id","child_basename","change_id"]),"annotations":annotations(false,false)}));
    }
    Value::Array(tools)
}

fn exact_object<'a>(value: &'a Value, allowed: &[&str]) -> Option<&'a Map<String, Value>> {
    let object = value.as_object()?;
    object
        .keys()
        .all(|key| allowed.contains(&key.as_str()))
        .then_some(object)
}

fn call(
    pinned: &PinnedWorkspace,
    service: &WorkspaceTimelineService,
    params: Option<&Value>,
) -> std::result::Result<Value, i32> {
    let params = params
        .and_then(|value| exact_object(value, &["name", "arguments", "_meta"]))
        .ok_or(-32602)?;
    let name = params.get("name").and_then(Value::as_str).ok_or(-32602)?;
    let arguments = params
        .get("arguments")
        .cloned()
        .unwrap_or_else(|| json!({}));
    let result = match name {
        "agentscm_snapshot_capture" => {
            let _: CaptureArgs = serde_json::from_value(arguments).map_err(|_| -32602)?;
            service.capture_with_deadline(SnapshotTrigger::McpExplicit, Duration::from_secs(25))
        }
        "agentscm_snapshot_timeline" => {
            let args: TimelineArgs = serde_json::from_value(arguments).map_err(|_| -32602)?;
            let limit = checked_limit(args.limit).map_err(|_| -32602)?;
            service.timeline(limit.into(), args.before)
        }
        "agentscm_snapshot_entry" => {
            let args: EntryArgs = serde_json::from_value(arguments).map_err(|_| -32602)?;
            if args.id == 0 {
                return Err(-32602);
            }
            service.entry(args.id)
        }
        "agentscm_snapshot_diff" => {
            let args: DiffArgs = serde_json::from_value(arguments).map_err(|_| -32602)?;
            if args.left == 0 || args.right == 0 {
                return Err(-32602);
            }
            let after = checked_cursor(args.after).map_err(|_| -32602)?;
            let limit = checked_limit(args.limit).map_err(|_| -32602)?;
            service.diff(args.left, args.right, limit.into(), after)
        }
        "agentscm_snapshot_fork" => {
            if pinned.fork_root.is_none() {
                return Ok(tool_failure(
                    "forking is not enabled for this local MCP server",
                ));
            }
            let args: ForkArgs = serde_json::from_value(arguments).map_err(|_| -32602)?;
            if args.id == 0 {
                return Err(-32602);
            }
            let child = checked_child_basename(&args.child_basename).map_err(|_| -32602)?;
            let change = checked_id(&args.change_id, "change_id").map_err(|_| -32602)?;
            service.fork(args.id, &child, &change)
        }
        _ => return Err(-32602),
    };
    match result {
        Ok(value) => tool_result(value).map_err(|_| -32603),
        Err(_) => Ok(tool_failure(
            "workspace operation failed; inspect the local AgentSCM diagnostics",
        )),
    }
}

fn handle(
    pinned: &PinnedWorkspace,
    service: &WorkspaceTimelineService,
    message: Value,
) -> Option<Value> {
    let Some(object) = exact_object(&message, &["jsonrpc", "id", "method", "params"]) else {
        return Some(rpc_error(Value::Null, -32600, "invalid request"));
    };
    if object.get("jsonrpc").and_then(Value::as_str) != Some("2.0") {
        return Some(rpc_error(Value::Null, -32600, "invalid request"));
    }
    let id = object.get("id").cloned();
    if id
        .as_ref()
        .is_some_and(|id| !id.is_string() && !id.is_number())
    {
        return Some(rpc_error(Value::Null, -32600, "invalid request"));
    }
    let method = object.get("method").and_then(Value::as_str);
    let id = id?;
    let result = match method {
        Some("initialize") => {
            let asked = object
                .get("params")
                .and_then(|params| params.get("protocolVersion"))
                .and_then(Value::as_str);
            let version = asked
                .filter(|version| SUPPORTED_VERSIONS.contains(version))
                .unwrap_or(SUPPORTED_VERSIONS[1]);
            Ok(json!({
                "protocolVersion":version,
                "capabilities":{"tools":{"listChanged":false}},
                "serverInfo":{"name":"agentscm-workspace","version":env!("CARGO_PKG_VERSION")},
                "instructions":"Operate only on the workspace pinned when this local server started. Capture and fork do not approve, land, publish, or merge changes."
            }))
        }
        Some("ping") => Ok(json!({})),
        Some("tools/list") => Ok(json!({"tools":tools(pinned)})),
        Some("tools/call") => call(pinned, service, object.get("params")),
        _ => Err(-32601),
    };
    Some(match result {
        Ok(result) => json!({"jsonrpc":"2.0","id":id,"result":result}),
        Err(-32602) => rpc_error(id, -32602, "invalid params"),
        Err(-32603) => rpc_error(id, -32603, "result unavailable"),
        Err(_) => rpc_error(id, -32601, "method not found"),
    })
}

fn encoded_reply(reply: &Value) -> Result<Vec<u8>> {
    let mut bytes = serde_json::to_vec(reply)?;
    if bytes.len() > MAX_MCP_RESULT_BYTES {
        let id = reply.get("id").cloned().unwrap_or(Value::Null);
        bytes = serde_json::to_vec(&rpc_error(id, -32603, "result unavailable"))?;
    }
    Ok(bytes)
}

fn write_reply(writer: &mut impl Write, reply: &Value) -> Result<()> {
    let bytes = encoded_reply(reply)?;
    writer.write_all(&bytes)?;
    writer.write_all(b"\n")?;
    writer.flush()?;
    Ok(())
}

/// Serve newline-delimited JSON-RPC until EOF.
pub fn run_mcp(
    pinned: &PinnedWorkspace,
    service: &WorkspaceTimelineService,
    reader: &mut impl BufRead,
    writer: &mut impl Write,
) -> Result<()> {
    loop {
        let mut bytes = Vec::new();
        let mut limited = std::io::Read::take(&mut *reader, (MAX_MCP_REQUEST_BYTES + 2) as u64);
        let read = limited.read_until(b'\n', &mut bytes)?;
        if read == 0 {
            return Ok(());
        }
        if bytes.last() == Some(&b'\n') {
            bytes.pop();
        }
        if bytes.last() == Some(&b'\r') {
            bytes.pop();
        }
        if bytes.len() > MAX_MCP_REQUEST_BYTES {
            write_reply(writer, &rpc_error(Value::Null, -32700, "request too large"))?;
            return Err(invalid("workspace MCP request exceeds 256 KiB"));
        }
        let message = match serde_json::from_slice(&bytes) {
            Ok(message) => message,
            Err(_) => {
                write_reply(writer, &rpc_error(Value::Null, -32700, "parse error"))?;
                continue;
            }
        };
        if let Some(reply) = handle(pinned, service, message) {
            write_reply(writer, &reply)?;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn oversized_tool_content_and_rpc_envelopes_have_bounded_fallbacks() {
        let tool = tool_result(json!({"value":"x".repeat(MAX_MCP_RESULT_BYTES)})).unwrap();
        assert_eq!(tool["isError"], true);
        assert!(serde_json::to_vec(&tool).unwrap().len() < MAX_MCP_RESULT_BYTES);

        let reply = json!({
            "jsonrpc":"2.0",
            "id":"request-7",
            "result":{"value":"x".repeat(MAX_MCP_RESULT_BYTES)}
        });
        let encoded = encoded_reply(&reply).unwrap();
        assert!(encoded.len() < MAX_MCP_RESULT_BYTES);
        let fallback: Value = serde_json::from_slice(&encoded).unwrap();
        assert_eq!(fallback["id"], "request-7");
        assert_eq!(fallback["error"]["code"], -32603);
    }
}
