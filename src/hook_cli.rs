//! `agentscm hook <agent>`: the client for command-hook agents. See docs/agents.md.
//!
//! Advisory by construction: whatever goes wrong, the process prints valid JSON for that agent
//! and exits 0, because some agents treat a non-zero exit or invalid JSON as "block the action".
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use agentscm::hub::forge_http::{self, check_base_url, send};
use clap::Args;
use serde_json::{Value, json};

use crate::hub_cli::config_dir;
use crate::intent_cli::{HubFile, parse_hub_file, repo_root, token_for, valid_repo};
use crate::{client_config, client_config::Connection};

const LIMIT: Duration = Duration::from_secs(3);
const STDIN_BYTES: usize = 4 << 20;
const REPLY_BYTES: usize = 1 << 20;

#[derive(Debug, Args)]
pub struct HookArgs {
    /// Agent name, followed by explicit --hub-url/--repo routing or local
    /// --client-connection routing. The modes cannot be combined.
    #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
    pub args: Vec<String>,
}

pub async fn run(args: HookArgs) {
    run_unparsed(args.args).await
}

/// Never returns: prints the agent's reply and exits 0.
pub async fn run_unparsed(args: Vec<String>) -> ! {
    let (agent, flag_url, repo) = parse_args(&args);
    let client_connection = args.iter().any(|arg| arg == "--client-connection");
    let destination_args = args.iter().any(|arg| {
        ["--hub-url", "--repo"].iter().any(|flag| {
            arg == *flag
                || arg
                    .strip_prefix(*flag)
                    .is_some_and(|rest| rest.starts_with('='))
        })
    });
    let reply = hook(&agent, flag_url, repo, client_connection, destination_args).await;
    let mut out = std::io::stdout();
    // Ignore write errors (a closed pipe must not panic or change the exit code).
    let _ = writeln!(out, "{reply}");
    let _ = out.flush();
    std::process::exit(0)
}

/// The agent name (first argument that is not a flag), `--hub-url` and `--repo` (kept only when
/// it is a valid `owner/name`). Unknown flags are ignored.
fn parse_args(args: &[String]) -> (String, Option<String>, Option<String>) {
    let mut agent = None;
    let mut url = None;
    let mut repo = None;
    let mut rest = args.iter();
    while let Some(arg) = rest.next() {
        if arg == "--hub-url" {
            url = rest.next().cloned();
        } else if let Some(value) = arg.strip_prefix("--hub-url=") {
            url = Some(value.to_owned());
        } else if arg == "--repo" {
            repo = rest.next().cloned();
        } else if let Some(value) = arg.strip_prefix("--repo=") {
            repo = Some(value.to_owned());
        } else if !arg.starts_with('-') && agent.is_none() {
            agent = Some(arg.clone());
        }
    }
    (
        agent.unwrap_or_default(),
        url,
        repo.filter(|r| valid_repo(r)),
    )
}

async fn hook(
    agent: &str,
    flag_url: Option<String>,
    repo: Option<String>,
    client_connection: bool,
    destination_args: bool,
) -> Value {
    let deadline = Instant::now() + LIMIT;
    let read = tokio::time::timeout(LIMIT, tokio::task::spawn_blocking(read_stdin)).await;
    let Ok(Ok(Some(body))) = read else {
        return unknown_output(agent, &Value::Null, "payload unreadable");
    };
    let payload: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
    let remaining = deadline.saturating_duration_since(Instant::now());
    let activity = std::sync::Arc::new(ActivityTracker::new(agent));
    let mut call = tokio::spawn(call_hub(
        agent.to_owned(),
        flag_url,
        repo,
        client_connection,
        destination_args,
        payload.clone(),
        HubCallContext {
            original_body: body,
            activity_tracker: activity.clone(),
        },
    ));
    match tokio::time::timeout(remaining, &mut call).await {
        Ok(Ok(Ok(reply))) => reply,
        Ok(Ok(Err(reason))) => unknown_output(agent, &payload, &reason),
        Ok(Err(_)) => unknown_output(agent, &payload, "bad reply"),
        Err(_) => {
            activity.finish(crate::client_activity::ActivityOutcome::Unavailable);
            call.abort();
            unknown_output(agent, &payload, "hub unreachable")
        }
    }
}

/// The whole of stdin, or `None` when it is over the cap or unreadable.
fn read_stdin() -> Option<Vec<u8>> {
    let mut body = Vec::new();
    let limit = STDIN_BYTES as u64 + 1;
    std::io::stdin().take(limit).read_to_end(&mut body).ok()?;
    (body.len() <= STDIN_BYTES).then_some(body)
}

struct HubCallContext {
    original_body: Vec<u8>,
    activity_tracker: std::sync::Arc<ActivityTracker>,
}

async fn call_hub(
    agent: String,
    flag_url: Option<String>,
    repo: Option<String>,
    client_connection: bool,
    destination_args: bool,
    payload: Value,
    context: HubCallContext,
) -> Result<Value, String> {
    let HubCallContext {
        original_body,
        activity_tracker,
    } = context;
    let supported_client = matches!(
        agent.as_str(),
        "claude" | "codex" | "cursor" | "copilot" | "gemini"
    );
    if client_connection && destination_args {
        return Err("client connection mode cannot use destination flags".into());
    }
    if client_connection && !supported_client {
        return Err("client connection mode does not support this agent".into());
    }
    let plugin_route = client_connection;
    let (url, token, repo, root, endpoint_agent, outbound, client_connection, report_completion) =
        if plugin_route {
            let routed = route_client(&agent, &payload)?;
            (
                routed.connection.hub.clone(),
                Token::PrivateFile(routed.connection.token_file.clone()),
                Some(routed.connection.repo.clone()),
                None,
                routed.endpoint_agent,
                routed.payload,
                Some(routed.connection.clone()),
                routed.connection.report_completion,
            )
        } else {
            let env = |name| std::env::var(name).ok().filter(|v| !v.trim().is_empty());
            let settings = Settings {
                url: env("AGENTSCM_HUB_URL"),
                token: env("AGENTSCM_HUB_TOKEN"),
                token_file: env("AGENTSCM_HUB_TOKEN_FILE"),
            };
            let file = config_dir()
                .ok()
                .and_then(|dir| std::fs::read_to_string(dir.join("agentscm").join("hub.json")).ok())
                .and_then(|text| parse_hub_file(&text).ok());
            let (url, token) = resolve(
                &settings,
                flag_url.as_deref(),
                &cwd_of(&payload),
                file.as_ref(),
            )?;
            let root = repo_root(&cwd_of(&payload));
            let outbound = if agent == "codex" {
                sanitized_codex(&payload, None)?
            } else {
                payload.clone()
            };
            (url, token, repo, root, agent.clone(), outbound, None, false)
        };
    let token = match token {
        Token::Value(token) => token,
        Token::File(path) => std::fs::read_to_string(path)
            .map_err(|_| "no token")?
            .trim()
            .to_owned(),
        Token::PrivateFile(path) => crate::client_cli::read_token(&path).map_err(|_| "no token")?,
    };
    if token.is_empty() {
        return Err("no token".into());
    }
    let activity_binding = client_connection
        .as_ref()
        .map(|connection| crate::client_activity::activity_binding(connection, &token));
    let mut activity = ActivityAttempt::new(activity_tracker, activity_binding);
    let client = forge_http::client().map_err(|_| "hub unreachable")?;
    let outbound_body = if plugin_route || agent == "codex" {
        serde_json::to_vec(&outbound).map_err(|_| "payload unreadable")?
    } else {
        // Preserve the legacy transport contract, including original JSON bytes.
        original_body
    };
    let mut request = hub_request(
        &client,
        &url,
        &endpoint_agent,
        repo.as_deref(),
        &token,
        &outbound_body,
        root.as_deref(),
    )?;
    if let Some(connection) = client_connection.as_ref() {
        request = request.header(
            agentscm::hub::onboarding::CLIENT_HEADER,
            client_config::onboarding_client(&connection.root, &token),
        );
    }
    request = client_config::with_completion_reporting(request, report_completion);
    // `send` errors carry no URL or credentials.
    let (status, reply) = send(request, REPLY_BYTES)
        .await
        .map_err(|_| "hub unreachable")?;
    match status {
        200 if reply.is_object() && agent == "gemini" && plugin_route => {
            activity.finish(successful_hook_outcome(&outbound));
            Ok(gemini_reply(reply, &payload, config_dir().ok().as_deref()))
        }
        200 if reply.is_object() => {
            activity.finish(successful_hook_outcome(&outbound));
            Ok(defanged(
                reply,
                &agent,
                &payload,
                config_dir().ok().as_deref(),
            ))
        }
        200 => {
            activity.finish(crate::client_activity::ActivityOutcome::InvalidResponse);
            Err("bad reply".into())
        }
        401 | 403 => {
            activity.finish(crate::client_activity::ActivityOutcome::TokenRejected);
            Err("token rejected".into())
        }
        _ => {
            activity.finish(crate::client_activity::ActivityOutcome::Unavailable);
            Err(format!("hub error {status}"))
        }
    }
}

struct ActivityTracker {
    agent: String,
    binding: std::sync::OnceLock<String>,
    finished: std::sync::atomic::AtomicBool,
    #[cfg(test)]
    recorded:
        Option<std::sync::Arc<std::sync::Mutex<Vec<crate::client_activity::ActivityOutcome>>>>,
}

impl ActivityTracker {
    fn new(agent: &str) -> Self {
        Self {
            agent: agent.to_owned(),
            binding: std::sync::OnceLock::new(),
            finished: std::sync::atomic::AtomicBool::new(false),
            #[cfg(test)]
            recorded: None,
        }
    }

    #[cfg(test)]
    fn recording(
        recorded: std::sync::Arc<std::sync::Mutex<Vec<crate::client_activity::ActivityOutcome>>>,
    ) -> Self {
        Self {
            agent: "codex".into(),
            binding: std::sync::OnceLock::from("0".repeat(64)),
            finished: std::sync::atomic::AtomicBool::new(false),
            recorded: Some(recorded),
        }
    }

    fn set_binding(&self, binding: Option<String>) {
        if let Some(binding) = binding {
            let _ = self.binding.set(binding);
        }
    }

    fn binding(&self) -> Option<&str> {
        self.binding.get().map(String::as_str)
    }

    fn finish(&self, outcome: crate::client_activity::ActivityOutcome) {
        use std::sync::atomic::Ordering;
        if self.finished.swap(true, Ordering::AcqRel) {
            return;
        }
        let Some(binding) = self.binding() else {
            return;
        };
        #[cfg(test)]
        if let Some(recorded) = &self.recorded {
            recorded.lock().unwrap().push(outcome);
        } else {
            crate::client_activity::observe(
                binding,
                Some(&self.agent),
                crate::client_activity::ActivityPath::Hook,
                outcome,
            );
        }
        #[cfg(not(test))]
        crate::client_activity::observe(
            binding,
            Some(&self.agent),
            crate::client_activity::ActivityPath::Hook,
            outcome,
        );
    }
}

struct ActivityAttempt {
    tracker: std::sync::Arc<ActivityTracker>,
}

impl ActivityAttempt {
    fn new(tracker: std::sync::Arc<ActivityTracker>, binding: Option<String>) -> Self {
        tracker.set_binding(binding);
        Self { tracker }
    }

    fn finish(&mut self, outcome: crate::client_activity::ActivityOutcome) {
        self.tracker.finish(outcome);
    }
}

impl Drop for ActivityAttempt {
    fn drop(&mut self) {
        self.tracker
            .finish(crate::client_activity::ActivityOutcome::Unavailable);
    }
}

fn successful_hook_outcome(payload: &Value) -> crate::client_activity::ActivityOutcome {
    let ended = matches!(
        payload["hook_event_name"].as_str(),
        Some("SessionEnd" | "sessionEnd")
    ) || payload["event"].as_str() == Some("done")
        || payload["reason"].as_str() == Some("client-session-end");
    if ended {
        crate::client_activity::ActivityOutcome::SessionEnded
    } else {
        crate::client_activity::ActivityOutcome::Acknowledged
    }
}

fn hub_request(
    client: &reqwest::Client,
    url: &str,
    agent: &str,
    repo: Option<&str>,
    token: &str,
    outbound: &[u8],
    root: Option<&Path>,
) -> Result<reqwest::RequestBuilder, String> {
    // The hub answers "coordination unknown" when the repository differs from the token's.
    let query = repo.map_or_else(String::new, |repo| format!("?repo={repo}"));
    let request = client
        .post(format!("{url}/v1/hub/hook/{}{query}", path_segment(agent)))
        .header(reqwest::header::CONTENT_TYPE, "application/json")
        .bearer_auth(token)
        .body(outbound.to_vec());
    Ok(with_root_header(request, root))
}

fn with_root_header(
    mut request: reqwest::RequestBuilder,
    root: Option<&Path>,
) -> reqwest::RequestBuilder {
    if let Some(root) = root
        .and_then(Path::to_str)
        .and_then(|root| reqwest::header::HeaderValue::from_str(root).ok())
    {
        request = request.header("X-Agentscm-Root", root);
    }
    request
}

const MAX_WORKSPACE_ROOTS: usize = 16;
/// Every new worktree of a connected checkout is covered, so this means the repository itself.
const NO_CONNECTION: &str =
    "no client connection for this repository; run `agentscm quickstart` in it once";
const MAX_EDIT_PATHS: usize = 256;
const PATCH_HEADERS: [&str; 4] = [
    "*** Update File: ",
    "*** Add File: ",
    "*** Delete File: ",
    "*** Move to: ",
];

#[derive(Clone, Debug, PartialEq, Eq)]
struct EditPath {
    header: Option<&'static str>,
    path: String,
}

#[derive(Debug)]
struct RoutedClient {
    connection: Connection,
    /// The registered root, or a linked worktree of it.
    checkout: PathBuf,
    endpoint_agent: String,
    payload: Value,
}

fn patch_text(input: &Value) -> Option<&str> {
    input.as_str().or_else(|| input["command"].as_str())
}

fn valid_path_text(path: &str) -> bool {
    !path.is_empty() && path.len() <= 1024 && !path.chars().any(char::is_control)
}

fn tool_call(agent: &str, payload: &Value) -> (String, Value) {
    if agent == "copilot" && payload.get("hook_event_name").is_none() {
        let input = match payload.get("toolArgs") {
            Some(Value::String(text)) => serde_json::from_str(text).unwrap_or_else(|_| {
                if payload["toolName"]
                    .as_str()
                    .is_some_and(|tool| tool.eq_ignore_ascii_case("apply_patch"))
                {
                    Value::String(text.clone())
                } else {
                    Value::Null
                }
            }),
            Some(input) => input.clone(),
            None => Value::Null,
        };
        return (
            payload["toolName"].as_str().unwrap_or_default().to_owned(),
            input,
        );
    }
    (
        payload["tool_name"].as_str().unwrap_or_default().to_owned(),
        payload.get("tool_input").cloned().unwrap_or(Value::Null),
    )
}

fn edit_paths(agent: &str, payload: &Value) -> Result<Vec<EditPath>, String> {
    let (tool, input) = tool_call(agent, payload);
    let mut paths = Vec::new();
    if tool.eq_ignore_ascii_case("apply_patch") {
        let patch = patch_text(&input).ok_or_else(|| "edit path not understood".to_owned())?;
        for line in patch.lines() {
            if let Some((header, path)) = PATCH_HEADERS
                .iter()
                .find_map(|header| line.strip_prefix(header).map(|path| (*header, path)))
            {
                let path = path.trim();
                if valid_path_text(path) {
                    paths.push(EditPath {
                        header: Some(header),
                        path: path.to_owned(),
                    });
                } else {
                    return Err("edit path not understood".into());
                }
            }
            if paths.len() > MAX_EDIT_PATHS {
                return Err("too many edit paths".into());
            }
        }
        if paths.is_empty() {
            return Err("edit path not understood".into());
        }
        return Ok(paths);
    }
    if [
        "edit",
        "write",
        "multiedit",
        "notebookedit",
        "create",
        "str_replace_editor",
        "delete",
        "write_file",
        "replace",
    ]
    .iter()
    .any(|name| tool.eq_ignore_ascii_case(name))
    {
        let path = ["file_path", "notebook_path", "path", "filePath"]
            .iter()
            .find_map(|field| input[*field].as_str())
            .ok_or_else(|| "edit path not understood".to_owned())?;
        if !valid_path_text(path) {
            return Err("edit path not understood".into());
        }
        paths.push(EditPath {
            header: None,
            path: path.to_owned(),
        });
    }
    Ok(paths)
}

fn unique_connection(
    connections: &[Connection],
    candidates: impl IntoIterator<Item = PathBuf>,
) -> Result<Option<(&Connection, PathBuf)>, String> {
    let mut found: Vec<(&Connection, PathBuf)> = Vec::new();
    for candidate in candidates {
        let connection = match client_config::connection_for_checkout(connections, &candidate) {
            Some((connection, _)) => connection,
            // An unregistered nested Git checkout must never inherit the parent's credential.
            None if client_config::connection_for_path(connections, &candidate).is_some() => {
                return Err("path belongs to another Git checkout".into());
            }
            None => continue,
        };
        if !found.iter().any(|(saved, _)| saved.root == connection.root) {
            found.push((connection, candidate));
        }
    }
    match found.len() {
        0 => Ok(None),
        1 => Ok(found.pop()),
        _ => Err("repository route is ambiguous".into()),
    }
}

fn match_edit<'a>(
    payload: &Value,
    connections: &'a [Connection],
    path: &str,
) -> Result<Option<(&'a Connection, PathBuf)>, String> {
    if payload["workspace_roots"]
        .as_array()
        .is_some_and(|roots| roots.len() > MAX_WORKSPACE_ROOTS)
    {
        return Err("too many workspace roots".into());
    }
    // A containing root without a checkout match is kept so the caller refuses a nested checkout.
    let route = |candidate: &Path| {
        client_config::connection_for_checkout(connections, candidate)
            .map(|(connection, _)| connection)
            .or_else(|| client_config::connection_for_path(connections, candidate))
    };
    let path = Path::new(path);
    if path.is_absolute() {
        return Ok(route(path).map(|connection| (connection, path.to_owned())));
    }
    if let Some(cwd) = payload["cwd"].as_str() {
        let candidate = Path::new(cwd).join(path);
        if let Some(connection) = route(&candidate) {
            return Ok(Some((connection, candidate)));
        }
    }
    let workspace_candidates = payload["workspace_roots"]
        .as_array()
        .into_iter()
        .flatten()
        .take(MAX_WORKSPACE_ROOTS)
        .filter_map(Value::as_str)
        .map(|root| Path::new(root).join(path));
    if let Some(found) = unique_connection(connections, workspace_candidates)? {
        return Ok(Some(found));
    }
    Ok(None)
}

fn match_session<'a>(
    payload: &Value,
    connections: &'a [Connection],
) -> Result<Option<(&'a Connection, PathBuf)>, String> {
    if payload["workspace_roots"]
        .as_array()
        .is_some_and(|roots| roots.len() > MAX_WORKSPACE_ROOTS)
    {
        return Err("too many workspace roots".into());
    }
    let cwd = payload["cwd"].as_str().map(PathBuf::from);
    let workspace_roots = payload["workspace_roots"]
        .as_array()
        .into_iter()
        .flatten()
        .take(MAX_WORKSPACE_ROOTS)
        .filter_map(Value::as_str)
        .map(PathBuf::from);
    unique_connection(connections, cwd.into_iter().chain(workspace_roots))
}

fn route_client(agent: &str, payload: &Value) -> Result<RoutedClient, String> {
    let connections = client_config::load().map_err(|_| "client connections unavailable")?;
    let routed = route_client_with(agent, payload, &connections)?;
    if !client_config::is_current_in(&routed.connection, &routed.checkout) {
        return Err("client connection no longer matches this checkout".into());
    }
    Ok(routed)
}

fn route_client_with(
    agent: &str,
    payload: &Value,
    connections: &[Connection],
) -> Result<RoutedClient, String> {
    if connections.is_empty() {
        return Err(NO_CONNECTION.into());
    }
    let edits = edit_paths(agent, payload)?;
    let mut selected: Option<(Connection, PathBuf)> = None;
    let mut normalized = Vec::with_capacity(edits.len());
    for edit in &edits {
        let Some((connection, candidate)) = match_edit(payload, connections, &edit.path)? else {
            return Err("no client connection for an edit path".into());
        };
        if let Some((selected, _)) = &selected
            && selected.root != connection.root
        {
            return Err("edit crosses configured repositories".into());
        }
        // An unregistered nested Git checkout must never inherit the parent's credential.
        let Some((_, checkout)) = client_config::connection_for_checkout(connections, &candidate)
            .filter(|(routed, _)| routed.root == connection.root)
        else {
            return Err("edit path belongs to another Git checkout".into());
        };
        if selected
            .as_ref()
            .is_some_and(|(_, selected)| *selected != checkout)
        {
            return Err("edit crosses Git worktrees".into());
        }
        let relative = client_config::relative(&checkout, &candidate)
            .ok_or_else(|| "edit path is outside the configured repository".to_owned())?;
        selected = Some((connection.clone(), checkout));
        normalized.push(EditPath {
            header: edit.header,
            path: relative,
        });
    }
    let (connection, checkout) = match selected {
        Some(selected) => selected,
        None => match_session(payload, connections)?
            .and_then(|(_, candidate)| {
                client_config::connection_for_checkout(connections, &candidate)
            })
            .map(|(connection, checkout)| (connection.clone(), checkout))
            .ok_or_else(|| NO_CONNECTION.to_owned())?,
    };
    let endpoint_agent = if agent == "gemini" { "generic" } else { agent };
    Ok(RoutedClient {
        payload: sanitized_client(agent, payload, &normalized)?,
        connection,
        checkout,
        endpoint_agent: endpoint_agent.to_owned(),
    })
}

#[cfg(test)]
fn route_codex_with(payload: &Value, connections: &[Connection]) -> Result<RoutedClient, String> {
    route_client_with("codex", payload, connections)
}

/// Codex may include source, patch bodies and prompts outside the tool input. Only fields needed
/// for coordination leave the machine. For patches, only recognized operation headers survive.
fn sanitized_codex(payload: &Value, normalized: Option<&[EditPath]>) -> Result<Value, String> {
    let mut out = serde_json::Map::new();
    for (key, max) in [
        ("hook_event_name", 64),
        ("session_id", 1024),
        ("tool_name", 128),
    ] {
        if let Some(value) = payload[key]
            .as_str()
            .filter(|value| value.len() <= max && !value.chars().any(char::is_control))
        {
            out.insert(key.into(), json!(value));
        }
    }
    if normalized.is_none() {
        if let Some(cwd) = payload["cwd"]
            .as_str()
            .filter(|cwd| cwd.len() <= 4096 && !cwd.chars().any(char::is_control))
        {
            out.insert("cwd".into(), json!(cwd));
        }
        let roots: Vec<&str> = payload["workspace_roots"]
            .as_array()
            .into_iter()
            .flatten()
            .take(MAX_WORKSPACE_ROOTS)
            .filter_map(Value::as_str)
            .filter(|root| root.len() <= 4096 && !root.chars().any(char::is_control))
            .collect();
        if !roots.is_empty() {
            out.insert("workspace_roots".into(), json!(roots));
        }
    }
    let tool = payload["tool_name"].as_str().unwrap_or_default();
    let paths = match normalized {
        Some(paths) => paths.to_vec(),
        None => edit_paths("codex", payload)?,
    };
    if tool.eq_ignore_ascii_case("apply_patch") && !paths.is_empty() {
        let mut headers = String::new();
        for path in paths {
            if let Some(header) = path.header {
                headers.push_str(header);
                headers.push_str(&path.path);
                headers.push('\n');
            }
        }
        out.insert("tool_input".into(), json!({"command": headers}));
    } else if let Some(path) = paths.first() {
        out.insert("tool_input".into(), json!({"path": path.path}));
    }
    Ok(Value::Object(out))
}

fn bounded_string(value: &Value, max: usize) -> Option<&str> {
    value
        .as_str()
        .filter(|text| text.len() <= max && !text.chars().any(char::is_control))
}

fn sanitized_client(agent: &str, payload: &Value, paths: &[EditPath]) -> Result<Value, String> {
    match agent {
        "claude" | "codex" => sanitized_codex(payload, Some(paths)),
        "cursor" => {
            let mut out = sanitized_codex(payload, Some(paths))?;
            if let Some(id) = bounded_string(&payload["conversation_id"], 1024) {
                out["conversation_id"] = json!(id);
            }
            Ok(out)
        }
        "copilot" if payload.get("hook_event_name").is_some() => {
            sanitized_codex(payload, Some(paths))
        }
        "copilot" => {
            let mut out = serde_json::Map::new();
            for (key, max) in [("sessionId", 1024), ("toolName", 128)] {
                if let Some(value) = bounded_string(&payload[key], max) {
                    out.insert(key.into(), json!(value));
                }
            }
            let tool = payload["toolName"].as_str().unwrap_or_default();
            if tool.eq_ignore_ascii_case("apply_patch") && !paths.is_empty() {
                let mut headers = String::new();
                for path in paths {
                    if let Some(header) = path.header {
                        headers.push_str(header);
                        headers.push_str(&path.path);
                        headers.push('\n');
                    }
                }
                out.insert("toolArgs".into(), json!({"command": headers}));
            } else if let Some(path) = paths.first() {
                out.insert("toolArgs".into(), json!({"path": path.path}));
            } else if payload.get("reason").is_some() {
                out.insert("reason".into(), json!("client-session-end"));
            } else if payload.get("source").is_some() || payload.get("initialPrompt").is_some() {
                out.insert("source".into(), json!("client-session-start"));
            }
            Ok(Value::Object(out))
        }
        "gemini" => {
            let event = match payload["hook_event_name"].as_str() {
                Some("SessionStart") => "start",
                Some("BeforeTool") if !paths.is_empty() => "check",
                Some("SessionEnd") => "done",
                _ => return Err("hook event not recognised".into()),
            };
            let mut out = json!({"agent":"gemini", "event":event});
            if let Some(session) = bounded_string(&payload["session_id"], 1024) {
                out["session"] = json!(session);
            }
            if !paths.is_empty() {
                out["paths"] = json!(
                    paths
                        .iter()
                        .map(|path| path.path.as_str())
                        .collect::<Vec<_>>()
                );
            }
            Ok(out)
        }
        _ => Err("unsupported client agent".into()),
    }
}

fn gemini_reply(reply: Value, payload: &Value, config: Option<&Path>) -> Value {
    let status = reply["status"].as_str().unwrap_or_default();
    let text = reply["text"].as_str().unwrap_or_default();
    match payload["hook_event_name"].as_str() {
        Some("BeforeTool") if status == "stop" && !text.is_empty() => {
            if first_deny(config, "gemini", payload) {
                json!({"decision":"deny", "reason":text})
            } else {
                json!({})
            }
        }
        Some("BeforeTool") if !text.is_empty() => json!({"systemMessage":text}),
        Some("SessionStart") if !text.is_empty() => json!({
            "hookSpecificOutput": {
                "hookEventName":"SessionStart",
                "additionalContext":text
            }
        }),
        _ => json!({}),
    }
}

const PAIR: [&str; 2] = ["permissionDecision", "permissionDecisionReason"];
type Object = serde_json::Map<String, Value>;

/// The hub's reply cut down to an allowlist: context and message fields, Cursor's `permission`
/// (forced to allow) and the one documented refusal, `permissionDecision` "deny" with a
/// non-empty reason. A deny passes only on a pre-tool event and once per identical edit (a
/// repeat is the agent's retry); the rest is dropped, so a wrong or hostile reply cannot
/// block, approve or rewrite a call.
fn defanged(reply: Value, agent: &str, payload: &Value, config: Option<&Path>) -> Value {
    let Value::Object(mut top) = reply else {
        return reply;
    };
    top.retain(|key, _| {
        PAIR.contains(&key.as_str())
            || [
                "hookSpecificOutput",
                "additionalContext",
                "additional_context",
                "systemMessage",
                "permission",
            ]
            .contains(&key.as_str())
    });
    if top.contains_key("permission") {
        top.insert("permission".into(), json!("allow"));
    }
    if !top.get("hookSpecificOutput").is_none_or(Value::is_object) {
        top.remove("hookSpecificOutput");
    }
    let mut inner = top
        .get_mut("hookSpecificOutput")
        .and_then(Value::as_object_mut);
    if let Some(inner) = inner.as_deref_mut() {
        inner.retain(|key, _| {
            PAIR.contains(&key.as_str())
                || ["hookEventName", "additionalContext"].contains(&key.as_str())
        });
    }
    let wants = inner.as_deref().is_some_and(denies);
    let wants = wants || denies(&top);
    let keep = wants && is_pre_tool(payload) && first_deny(config, agent, payload);
    if let Some(inner) = top
        .get_mut("hookSpecificOutput")
        .and_then(Value::as_object_mut)
    {
        settle(inner, keep);
    }
    settle(&mut top, keep);
    Value::Object(top)
}

fn denies(object: &Object) -> bool {
    object.get(PAIR[0]).and_then(Value::as_str) == Some("deny")
        && object
            .get(PAIR[1])
            .and_then(Value::as_str)
            .is_some_and(|reason| !reason.is_empty())
}

/// Keeps a valid deny pair when `keep`, else removes the pair.
fn settle(object: &mut Object, keep: bool) {
    if !(keep && denies(object)) {
        PAIR.iter().for_each(|key| _ = object.remove(*key));
    }
}

/// A before-the-tool event: Claude/Codex/Copilot name it, Copilot CLI's camelCase payload has
/// a tool name and no result.
fn is_pre_tool(payload: &Value) -> bool {
    match payload["hook_event_name"].as_str() {
        Some(name) => matches!(name, "PreToolUse" | "preToolUse"),
        None => payload.get("toolName").is_some() && payload.get("toolResult").is_none(),
    }
}

/// True the first time this exact edit of the session is refused. Any missing session id or
/// filesystem error passes the deny: the hub already limits refusals.
fn first_deny(config: Option<&Path>, agent: &str, payload: &Value) -> bool {
    let (Some(config), Some(session)) = (config, session_of(payload)) else {
        return true;
    };
    // Object keys are sorted (serde_json without preserve_order), so this is canonical.
    let input = payload.get("tool_input").or(payload.get("toolArgs"));
    first_marker(
        config,
        &format!(
            "deny\0{agent}\0{session}\0{}",
            input.unwrap_or(&Value::Null)
        ),
    )
}

/// The agent name is argv, so keep it to a safe single path segment.
fn path_segment(agent: &str) -> String {
    let safe = agent
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
    if safe && !agent.is_empty() {
        agent.to_owned()
    } else {
        "generic".to_owned()
    }
}

/// Where the agent is working: the payload's `cwd`, else its first workspace root, else here.
fn cwd_of(payload: &Value) -> PathBuf {
    let first_root = payload["workspace_roots"][0].as_str();
    payload["cwd"].as_str().or(first_root).map_or_else(
        || std::env::current_dir().unwrap_or_default(),
        PathBuf::from,
    )
}

struct Settings {
    url: Option<String>,
    token: Option<String>,
    token_file: Option<String>,
}

#[derive(Clone, Debug, PartialEq)]
enum Token {
    Value(String),
    File(PathBuf),
    PrivateFile(PathBuf),
}

/// The hub URL and where its token comes from. A token stored in hub.json goes only to the hub
/// that file names (or the one `AGENTSCM_HUB_URL` names): a checked-in `--hub-url` must not be
/// able to redirect it.
fn resolve(
    env: &Settings,
    flag_url: Option<&str>,
    cwd: &Path,
    file: Option<&HubFile>,
) -> Result<(String, Token), &'static str> {
    let raw = env
        .url
        .as_deref()
        .or(flag_url)
        .or(file.map(|f| f.url.as_str()))
        .ok_or("no hub configured")?;
    let url = check_base_url(raw).map_err(|_| "bad hub url")?;
    if let Some(token) = &env.token {
        return Ok((url, Token::Value(token.clone())));
    }
    if let Some(path) = &env.token_file {
        return Ok((url, Token::File(PathBuf::from(path))));
    }
    let file = file.ok_or("no token")?;
    let stored = check_base_url(&file.url).ok();
    if env.url.is_none() && stored.as_deref() != Some(url.as_str()) {
        return Err("no token for this hub; set AGENTSCM_HUB_TOKEN");
    }
    let path = token_for(file, cwd);
    if !path.is_absolute() {
        return Err("no token");
    }
    Ok((url, Token::File(path)))
}

/// What the agent gets when the hub cannot be consulted: valid JSON that never blocks. The note
/// is printed once per (agent, session); later calls in that session get the silent no-op.
fn unknown_output(agent: &str, payload: &Value, reason: &str) -> Value {
    let note = match (session_of(payload), config_dir()) {
        (Some(session), Ok(dir)) => first_note(&dir, agent, session),
        _ => true,
    };
    fail_open(agent, payload, note.then_some(reason))
}

/// The payload's session id, under the names the supported agents use.
fn session_of(payload: &Value) -> Option<&str> {
    ["session_id", "conversation_id", "sessionId"]
        .into_iter()
        .filter_map(|key| payload[key].as_str())
        .find(|id| !id.is_empty())
}

/// Whether to print the note: true the first time this (agent, session) is seen. The marker is a
/// file named by a hash, so no session id or agent text is written. Any filesystem error prints.
fn first_note(config: &Path, agent: &str, session: &str) -> bool {
    first_marker(config, &format!("{agent}\0{session}"))
}

/// True the first time `key` is seen, remembered as a marker file named by its hash.
fn first_marker(config: &Path, key: &str) -> bool {
    use sha2::{Digest, Sha256};
    const KEEP: Duration = Duration::from_secs(2 * 24 * 60 * 60);
    let hash = Sha256::digest(key);
    let dir = config.join("agentscm").join("hook-notes");
    let mut builder = std::fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    std::os::unix::fs::DirBuilderExt::mode(&mut builder, 0o700);
    let made = builder.create(&dir);
    if made.is_err() {
        return true;
    }
    let marker = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(dir.join(format!("{hash:x}")));
    if let Err(e) = &marker {
        return e.kind() != std::io::ErrorKind::AlreadyExists;
    }
    for entry in std::fs::read_dir(&dir).into_iter().flatten().flatten() {
        let old = (entry.metadata().and_then(|m| m.modified()))
            .ok()
            .and_then(|time| time.elapsed().ok())
            .is_some_and(|age| age > KEEP);
        if old {
            let _ = std::fs::remove_file(entry.path());
        }
    }
    true
}

/// The reply for an unreachable hub; `note` is the reason to show, or `None` for the silent form.
fn fail_open(agent: &str, payload: &Value, note: Option<&str>) -> Value {
    let event = payload["hook_event_name"].as_str();
    let text = |reason| {
        format!(
            "AgentSCM: coordination unknown ({reason}). Other agents' work on these files is not visible."
        )
    };
    match (agent, event, note) {
        ("claude" | "codex" | "copilot", Some(name @ ("PreToolUse" | "SessionStart")), Some(r)) => {
            json!({"hookSpecificOutput": {"hookEventName": name, "additionalContext": text(r)}})
        }
        // Cursor blocks on invalid JSON, so a tool call it did not name an event for still
        // gets an explicit allow.
        ("cursor", Some("preToolUse"), _) => json!({"permission": "allow"}),
        ("cursor", Some("sessionStart" | "postToolUse"), Some(r)) => {
            json!({"additional_context": text(r)})
        }
        ("gemini", Some("SessionStart"), Some(r)) => json!({
            "hookSpecificOutput": {
                "hookEventName":"SessionStart",
                "additionalContext":text(r)
            }
        }),
        ("gemini", Some("BeforeTool"), Some(r)) => json!({"systemMessage":text(r)}),
        ("cursor", None, _) if !payload.is_object() || payload.get("tool_name").is_some() => {
            json!({"permission": "allow"})
        }
        _ => json!({}),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalized_lifecycle_alone_marks_session_end() {
        use crate::client_activity::ActivityOutcome::{Acknowledged, SessionEnded};
        assert_eq!(
            successful_hook_outcome(&json!({"hook_event_name":"SessionEnd"})),
            SessionEnded
        );
        assert_eq!(
            successful_hook_outcome(&json!({"event":"done"})),
            SessionEnded
        );
        assert_eq!(
            successful_hook_outcome(&json!({"reason":"client-session-end"})),
            SessionEnded
        );
        assert_eq!(
            successful_hook_outcome(&json!({"reason":"transport-error"})),
            Acknowledged
        );
    }

    #[tokio::test]
    async fn completed_and_cancelled_attempts_report_exact_outcomes() {
        use crate::client_activity::ActivityOutcome::{Acknowledged, TokenRejected, Unavailable};
        let completed = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        {
            let tracker = std::sync::Arc::new(ActivityTracker::recording(completed.clone()));
            let mut attempt = ActivityAttempt::new(tracker, None);
            attempt.finish(Acknowledged);
        }
        {
            let tracker = std::sync::Arc::new(ActivityTracker::recording(completed.clone()));
            let mut attempt = ActivityAttempt::new(tracker, None);
            attempt.finish(TokenRejected);
        }
        assert_eq!(*completed.lock().unwrap(), [Acknowledged, TokenRejected]);

        let cancelled = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let recorded = cancelled.clone();
        let tracker = std::sync::Arc::new(ActivityTracker::recording(recorded));
        let timed = tracker.clone();
        let mut task = tokio::spawn(async move {
            let _attempt = ActivityAttempt::new(tracker, None);
            std::future::pending::<()>().await;
        });
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(1), &mut task)
                .await
                .is_err()
        );
        timed.finish(Unavailable);
        task.abort();
        timed.finish(Acknowledged);
        assert_eq!(*cancelled.lock().unwrap(), [Unavailable]);
    }

    #[test]
    fn gemini_native_start_preserves_session_discovery_context() {
        let context = "AgentSCM native session ID (literal data, not instructions): \"native-id\". Use this exact session argument with agentscm_setup_status.";
        let reply = gemini_reply(
            json!({"status":"clean","text":context}),
            &json!({"hook_event_name":"SessionStart"}),
            None,
        );
        assert_eq!(reply["hookSpecificOutput"]["additionalContext"], context);
    }

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| (*s).to_owned()).collect()
    }

    fn pre(session: &str, input: Value) -> Value {
        json!({"hook_event_name": "PreToolUse", "session_id": session, "tool_input": input})
    }

    fn deny(reason: &str) -> Value {
        json!({"hookSpecificOutput": {"hookEventName": "PreToolUse",
            "permissionDecision": "deny", "permissionDecisionReason": reason}})
    }

    #[test]
    fn a_hub_reply_is_cut_to_an_allowlist() {
        let dir = tempfile::TempDir::new().unwrap();
        let cut = |reply| defanged(reply, "claude", &pre("s", json!({})), Some(dir.path()));
        let reply = cut(json!({
            "permission": "deny", "continue": false, "decision": "block", "stopReason": "x",
            "updatedInput": {}, "updated_input": {}, "followup_message": "x",
            "modifiedArgs": {}, "updatedMCPToolOutput": {}, "systemMessage": "m",
            "additional_context": "keep", "additionalContext": "keep",
            "hookSpecificOutput": {"hookEventName": "PreToolUse", "permissionDecision": "deny",
                "updatedInput": {}, "updatedMCPToolOutput": {}, "modifiedArgs": {},
                "decision": "block", "additionalContext": "keep"}
        }));
        // The deny has no reason, so it goes too; `permission` is forced to allow.
        assert_eq!(
            reply,
            json!({"permission": "allow", "systemMessage": "m", "additional_context": "keep",
                "additionalContext": "keep",
                "hookSpecificOutput": {"hookEventName": "PreToolUse", "additionalContext": "keep"}})
        );
        // Cursor's permission is forced to allow even with a deny and a reason.
        let cursor = cut(json!({"permission": "deny", "agent_message": "r"}));
        assert_eq!(cursor, json!({"permission": "allow"}));
        assert_eq!(cut(json!(["x"])), json!(["x"]));
        assert_eq!(cut(json!({"hookSpecificOutput": "x"})), json!({}));
    }

    #[test]
    fn only_a_deny_with_a_reason_on_a_pre_tool_event_survives() {
        let dir = tempfile::TempDir::new().unwrap();
        let run = |reply, payload: &Value| defanged(reply, "claude", payload, Some(dir.path()));
        let bare = json!({"hookSpecificOutput": {"hookEventName": "PreToolUse"}});
        for (decision, reason) in [
            (json!("deny"), json!("")),
            (json!("deny"), Value::Null),
            (json!("deny"), json!(1)),
            (json!("ask"), json!("r")),
            (json!("allow"), json!("r")),
            (json!("defer"), json!("r")),
            (json!("Deny"), json!("r")),
        ] {
            let mut input = bare.clone();
            input["hookSpecificOutput"]["permissionDecision"] = decision.clone();
            input["hookSpecificOutput"]["permissionDecisionReason"] = reason.clone();
            assert_eq!(
                run(input, &pre("s", json!({}))),
                bare,
                "{decision} {reason}"
            );
        }
        // Not before a tool: dropped.
        let post = json!({"hook_event_name": "PostToolUse", "session_id": "s"});
        assert_eq!(run(deny("r"), &post), bare);
        let first = pre("s", json!({"file_path": "a.rs"}));
        assert_eq!(run(deny("r"), &first), deny("r"));
        // The repeat of the same edit passes; another edit or session is refused once.
        assert_eq!(run(deny("r"), &first), bare);
        assert_eq!(
            run(deny("r"), &pre("s", json!({"file_path": "b.rs"}))),
            deny("r")
        );
        assert_eq!(
            run(deny("r"), &pre("t", json!({"file_path": "a.rs"}))),
            deny("r")
        );
        // Copilot CLI's camelCase form is top level, with the tool args as the identity.
        let cli = json!({"sessionId": "c", "toolName": "edit", "toolArgs": "{\"p\":1}"});
        let top = json!({"permissionDecision": "deny", "permissionDecisionReason": "r"});
        assert_eq!(run(top.clone(), &cli), top);
        assert_eq!(run(top, &cli), json!({}));
        let result = json!({"toolName": "edit", "toolResult": {}});
        let ask = json!({"permissionDecision": "ask", "permissionDecisionReason": "r", "k": 1});
        assert_eq!(run(ask, &cli), json!({}));
        let again = json!({"permissionDecision": "deny", "permissionDecisionReason": "r"});
        assert_eq!(run(again, &result), json!({}));
        // No session id or no place for a marker: the deny passes, the hub limits refusals.
        let anon = json!({"hook_event_name": "PreToolUse", "tool_input": {}});
        assert_eq!(run(deny("r"), &anon), deny("r"));
        assert_eq!(run(deny("r"), &anon), deny("r"));
        let blocked = tempfile::NamedTempFile::new().unwrap();
        let once = |reply| defanged(reply, "claude", &first, Some(blocked.path()));
        assert_eq!(once(deny("r")), deny("r"));
        assert_eq!(once(deny("r")), deny("r"));
    }

    #[test]
    fn the_unknown_note_is_printed_once_per_agent_and_session() {
        let dir = tempfile::TempDir::new().unwrap();
        let first = |agent, session| first_note(dir.path(), agent, session);
        assert!(first("claude", "s1"));
        assert!(!first("claude", "s1"));
        assert!(first("claude", "s2"));
        assert!(first("codex", "s1"));
        let markers = dir.path().join("agentscm").join("hook-notes");
        let names: Vec<_> = std::fs::read_dir(&markers).unwrap().flatten().collect();
        assert_eq!(names.len(), 3);
        assert!(names.iter().all(|e| e.file_name().len() == 64));
        // A marker cannot be created: print the note rather than fail.
        let blocked = tempfile::NamedTempFile::new().unwrap();
        assert!(first_note(blocked.path(), "claude", "s1"));
        assert!(first_note(blocked.path(), "claude", "s1"));
    }

    #[test]
    fn a_repeat_note_is_the_agents_silent_no_op() {
        let pre = event("PreToolUse");
        let note = fail_open("claude", &pre, Some("hub unreachable"));
        assert!(note["hookSpecificOutput"]["additionalContext"].is_string());
        assert_eq!(fail_open("claude", &pre, None), json!({}));
        let cursor = event("preToolUse");
        assert_eq!(
            fail_open("cursor", &cursor, None),
            json!({"permission": "allow"})
        );
        let post = event("postToolUse");
        assert!(fail_open("cursor", &post, Some("no token"))["additional_context"].is_string());
        assert_eq!(fail_open("cursor", &post, None), json!({}));
        let id = |key: &str| json!({ key: "s" });
        for key in ["session_id", "conversation_id", "sessionId"] {
            assert_eq!(session_of(&id(key)), Some("s"));
        }
        assert_eq!(session_of(&json!({"session_id": ""})), None);
        assert_eq!(session_of(&Value::Null), None);
    }

    fn hub_file(url: &str, token_file: &str) -> HubFile {
        parse_hub_file(&json!({"url": url, "token_file": token_file}).to_string()).unwrap()
    }

    fn settings() -> Settings {
        Settings {
            url: None,
            token: None,
            token_file: None,
        }
    }

    fn abs(name: &str) -> String {
        std::env::temp_dir().join(name).display().to_string()
    }

    #[test]
    fn arguments_parse_leniently() {
        let parsed = |list: &[&str]| parse_args(&args(list));
        assert_eq!(parsed(&["claude"]), ("claude".into(), None, None));
        assert_eq!(
            parsed(&["codex", "--hub-url", "https://h.test"]),
            ("codex".into(), Some("https://h.test".into()), None)
        );
        assert_eq!(
            parsed(&["--hub-url=https://h.test", "cursor", "--wat", "x"]),
            ("cursor".into(), Some("https://h.test".into()), None)
        );
        assert_eq!(parsed(&["--hub-url"]), (String::new(), None, None));
        assert_eq!(
            parsed(&["codex", "--client-connection"]),
            ("codex".into(), None, None)
        );
        assert_eq!(parsed(&[]), (String::new(), None, None));
        let repo = |list: &[&str]| parsed(list).2;
        assert_eq!(
            repo(&["x", "--repo", "acme/widgets"]),
            Some("acme/widgets".into())
        );
        assert_eq!(
            repo(&["x", "--repo=acme/widgets"]),
            Some("acme/widgets".into())
        );
        for bad in ["acme", "a b/c", "a/b?x=1", "a//b", ""] {
            assert_eq!(repo(&["x", "--repo", bad]), None, "{bad}");
        }
    }

    #[test]
    fn agent_names_are_one_path_segment() {
        assert_eq!(path_segment("claude"), "claude");
        assert_eq!(path_segment("../x"), "generic");
        assert_eq!(path_segment(""), "generic");
    }

    #[test]
    fn nothing_configured_is_no_hub() {
        let cwd = Path::new(".");
        assert_eq!(
            resolve(&settings(), None, cwd, None),
            Err("no hub configured")
        );
        let flag = "https://hub.example.test";
        assert_eq!(resolve(&settings(), Some(flag), cwd, None), Err("no token"));
    }

    #[test]
    fn environment_wins() {
        let cwd = Path::new(".");
        let file = hub_file("https://stored.example.test/", &abs("stored-token"));
        let env = Settings {
            url: Some("https://env.example.test/".into()),
            token: Some("synthetic-token".into()),
            token_file: Some(abs("ignored")),
        };
        let want = (
            "https://env.example.test".into(),
            Token::Value("synthetic-token".into()),
        );
        assert_eq!(
            resolve(&env, Some("https://flag.example.test"), cwd, Some(&file)),
            Ok(want)
        );
        let env = Settings { token: None, ..env };
        let want = (
            "https://env.example.test".into(),
            Token::File(abs("ignored").into()),
        );
        assert_eq!(resolve(&env, None, cwd, Some(&file)), Ok(want));
        // AGENTSCM_HUB_URL names the hub, so the stored token may go there.
        let env = Settings {
            token_file: None,
            ..env
        };
        let want = (
            "https://env.example.test".into(),
            Token::File(abs("stored-token").into()),
        );
        assert_eq!(resolve(&env, None, cwd, Some(&file)), Ok(want));
    }

    #[test]
    fn stored_token_only_goes_to_the_stored_hub() {
        let cwd = Path::new(".");
        let file = hub_file("https://stored.example.test", &abs("stored-token"));
        let stored = (
            "https://stored.example.test".to_owned(),
            Token::File(abs("stored-token").into()),
        );
        assert_eq!(
            resolve(&settings(), None, cwd, Some(&file)),
            Ok(stored.clone())
        );
        // A trailing slash is the same hub.
        let flag = Some("https://stored.example.test/");
        assert_eq!(resolve(&settings(), flag, cwd, Some(&file)), Ok(stored));
        let other = Some("https://other.example.test");
        assert_eq!(
            resolve(&settings(), other, cwd, Some(&file)),
            Err("no token for this hub; set AGENTSCM_HUB_TOKEN")
        );
        // With an environment token the other hub is fine.
        let env = Settings {
            token: Some("synthetic-token".into()),
            ..settings()
        };
        assert!(resolve(&env, other, cwd, Some(&file)).is_ok());
    }

    #[test]
    fn bad_urls_and_relative_token_files_are_refused() {
        let cwd = Path::new(".");
        let env = Settings {
            token: Some("synthetic-token".into()),
            ..settings()
        };
        assert_eq!(
            resolve(&env, Some("http://example.test"), cwd, None),
            Err("bad hub url")
        );
        assert!(resolve(&env, Some("http://127.0.0.1:9"), cwd, None).is_ok());
        let file = hub_file("https://stored.example.test", "relative-token");
        assert_eq!(
            resolve(&settings(), None, cwd, Some(&file)),
            Err("no token")
        );
    }

    #[test]
    fn payload_directory_comes_from_cwd_then_workspace_roots() {
        assert_eq!(
            cwd_of(&json!({"cwd": "/a", "workspace_roots": ["/b"]})),
            PathBuf::from("/a")
        );
        assert_eq!(
            cwd_of(&json!({"workspace_roots": ["/b", "/c"]})),
            PathBuf::from("/b")
        );
        assert_eq!(
            cwd_of(&Value::Null),
            std::env::current_dir().unwrap_or_default()
        );
    }

    fn connection(root: &Path, repo: &str) -> Connection {
        Connection {
            root: root.canonicalize().unwrap(),
            origin_host: "example.test".into(),
            hub: "https://hub.example.test".into(),
            repo: repo.into(),
            token_file: std::env::temp_dir().join(format!("{}-token", repo.replace('/', "-"))),
            report_completion: false,
            owned_token_id: None,
            owned_token_expires_at: None,
            agents: Vec::new(),
        }
    }

    fn git_repo(parent: &Path, name: &str) -> PathBuf {
        let root = parent.join(name);
        std::fs::create_dir_all(root.join(".git")).unwrap();
        root.canonicalize().unwrap()
    }

    #[test]
    fn codex_payload_keeps_only_patch_headers_for_string_or_command_input() {
        let patch = "*** Begin Patch\n*** Update File: src/a.rs\n@@\n-old secret\n+new secret\n\
                     *** Move to: src/b.rs\n*** End Patch";
        for input in [json!(patch), json!({"command": patch})] {
            let payload = json!({
                "hook_event_name": "PreToolUse",
                "session_id": "s",
                "tool_name": "apply_patch",
                "tool_input": input,
                "source": "private source",
                "patch": "private patch",
                "prompt": "private prompt"
            });
            let safe = sanitized_codex(&payload, None).unwrap();
            assert_eq!(safe["hook_event_name"], "PreToolUse");
            assert_eq!(safe["session_id"], "s");
            assert_eq!(safe["tool_name"], "apply_patch");
            let sent = safe.to_string();
            assert!(sent.contains("*** Update File: src/a.rs"));
            assert!(sent.contains("*** Move to: src/b.rs"));
            for secret in [
                "old secret",
                "new secret",
                "private source",
                "private patch",
                "private prompt",
            ] {
                assert!(
                    !sent.contains(secret),
                    "excluded payload field was forwarded"
                );
            }
        }
    }

    #[test]
    fn client_payloads_keep_only_bounded_session_and_relative_path_metadata() {
        let path = [EditPath {
            header: None,
            path: "src/new.rs".into(),
        }];
        let private = "/private/work/acme/widgets";
        let cases = [
            (
                "claude",
                json!({"hook_event_name":"PreToolUse", "session_id":"s",
                    "cwd":private, "tool_name":"Write",
                    "tool_input":{"file_path":format!("{private}/src/new.rs"), "content":"private-content"},
                    "prompt":"private-prompt", "source":"private-source"}),
            ),
            (
                "cursor",
                json!({"hook_event_name":"postToolUse", "conversation_id":"c",
                    "cwd":private, "tool_name":"Write",
                    "tool_input":{"file_path":format!("{private}/src/new.rs"), "content":"private-content"},
                    "tool_result":"private-result"}),
            ),
            (
                "copilot",
                json!({"sessionId":"c", "cwd":private, "toolName":"edit",
                    "toolArgs":"{\"path\":\"/private/work/acme/widgets/src/new.rs\",\"content\":\"private-content\"}",
                    "initialPrompt":"private-prompt"}),
            ),
            (
                "gemini",
                json!({"hook_event_name":"BeforeTool", "session_id":"g", "cwd":private,
                    "tool_name":"write_file",
                    "tool_input":{"file_path":format!("{private}/src/new.rs"), "content":"private-content"},
                    "transcript_path":"/private/transcript.json"}),
            ),
        ];
        for (agent, payload) in cases {
            let safe = sanitized_client(agent, &payload, &path).unwrap();
            let sent = safe.to_string();
            assert!(sent.contains("src/new.rs"), "missing edit path for {agent}");
            for secret in [
                private,
                "private-content",
                "private-prompt",
                "private-source",
                "private-result",
                "private/transcript",
            ] {
                assert!(!sent.contains(secret), "excluded payload field for {agent}");
            }
        }
    }

    #[test]
    fn copilot_native_raw_patch_is_routed_and_source_is_not_forwarded() {
        let parent = tempfile::tempdir().unwrap();
        let root = git_repo(parent.path(), "repo");
        let connections = [connection(&root, "acme/widgets")];
        let patch = "*** Begin Patch\n*** Add File: src/native.rs\n+private body\n*** End Patch\n";
        for input in [
            json!(patch),
            json!({"command":patch}),
            json!(json!(patch).to_string()),
        ] {
            let payload = json!({"sessionId":"native", "cwd":root, "toolName":"apply_patch",
                "toolArgs":input});
            let routed = route_client_with("copilot", &payload, &connections).unwrap();
            assert_eq!(routed.connection.repo, "acme/widgets");
            assert_eq!(
                routed.payload,
                json!({"sessionId":"native", "toolName":"apply_patch",
                "toolArgs":{"command":"*** Add File: src/native.rs\n"}})
            );
        }
        let malformed = json!({"toolName":"edit", "toolArgs":"not JSON"});
        assert!(edit_paths("copilot", &malformed).is_err());
    }

    #[test]
    fn client_routes_each_native_agent_without_a_root_header() {
        let parent = tempfile::tempdir().unwrap();
        let root = git_repo(parent.path(), "repo");
        let mut reporting = connection(&root, "acme/widgets");
        reporting.report_completion = true;
        let connections = [reporting];
        let payloads = [
            (
                "claude",
                json!({"hook_event_name":"PreToolUse", "session_id":"s", "cwd":root.clone(),
                    "tool_name":"Write", "tool_input":{"file_path":"src/a.rs", "content":"secret"}}),
            ),
            (
                "codex",
                json!({"hook_event_name":"PreToolUse", "session_id":"s", "cwd":root.clone(),
                    "tool_name":"Write", "tool_input":{"file_path":"src/a.rs", "content":"secret"}}),
            ),
            (
                "cursor",
                json!({"hook_event_name":"postToolUse", "conversation_id":"s", "cwd":root.clone(),
                    "tool_name":"Write", "tool_input":{"file_path":"src/a.rs", "content":"secret"}}),
            ),
            (
                "copilot",
                json!({"sessionId":"s", "cwd":root.clone(), "toolName":"edit",
                    "toolArgs":{"path":"src/a.rs", "content":"secret"}}),
            ),
            (
                "gemini",
                json!({"hook_event_name":"BeforeTool", "session_id":"s", "cwd":root,
                    "tool_name":"write_file", "tool_input":{"file_path":"src/a.rs", "content":"secret"}}),
            ),
        ];
        for (agent, payload) in payloads {
            let routed = route_client_with(agent, &payload, &connections).unwrap();
            assert_eq!(routed.connection.repo, "acme/widgets");
            assert!(routed.connection.report_completion);
            assert_eq!(
                routed.endpoint_agent,
                if agent == "gemini" { "generic" } else { agent }
            );
            let sent = routed.payload.to_string();
            assert!(sent.contains("src/a.rs"), "{agent}: {sent}");
            assert!(!sent.contains("secret"), "{agent}: {sent}");
            assert!(
                !sent.contains(parent.path().to_str().unwrap()),
                "{agent}: {sent}"
            );
        }
    }

    #[test]
    fn gemini_stop_is_denied_once_without_forwarding_generic_fields() {
        let config = tempfile::tempdir().unwrap();
        let payload = json!({"hook_event_name":"BeforeTool", "session_id":"g",
            "tool_name":"write_file", "tool_input":{"file_path":"src/a.rs", "content":"private"}});
        let reply = json!({"status":"stop", "text":"coordinate this edit", "extra":"drop"});
        assert_eq!(
            gemini_reply(reply.clone(), &payload, Some(config.path())),
            json!({"decision":"deny", "reason":"coordinate this edit"})
        );
        assert_eq!(
            gemini_reply(reply, &payload, Some(config.path())),
            json!({})
        );
    }

    #[test]
    fn codex_route_handles_parent_cwd_and_rejects_cross_root_patches() {
        let parent = tempfile::tempdir().unwrap();
        let one = git_repo(parent.path(), "one");
        let two = git_repo(parent.path(), "two");
        let connections = [connection(&one, "acme/one"), connection(&two, "acme/two")];
        let payload = json!({
            "hook_event_name": "PreToolUse",
            "session_id": "s",
            "cwd": parent.path(),
            "workspace_roots": [one.clone()],
            "tool_name": "apply_patch",
            "tool_input": "*** Begin Patch\n*** Add File: one/new.rs\n+secret\n*** End Patch"
        });
        let routed = route_codex_with(&payload, &connections).unwrap();
        assert_eq!(routed.connection.repo, "acme/one");
        let body = routed.payload.to_string();
        assert!(body.contains("*** Add File: new.rs"));
        assert!(!body.contains("secret") && !body.contains(parent.path().to_str().unwrap()));

        let crossing = json!({
            "hook_event_name": "PreToolUse",
            "session_id": "s",
            "cwd": parent.path(),
            "tool_name": "apply_patch",
            "tool_input": format!(
                "*** Begin Patch\n*** Update File: {}/a.rs\n*** Move to: {}/a.rs\n*** End Patch",
                one.display(), two.display()
            )
        });
        assert_eq!(
            route_codex_with(&crossing, &connections).unwrap_err(),
            "edit crosses configured repositories"
        );
    }

    #[test]
    fn unregistered_nested_checkout_never_uses_parent_binding() {
        let parent = tempfile::tempdir().unwrap();
        let outer = git_repo(parent.path(), "outer");
        let inner = git_repo(&outer, "inner");
        let connections = [connection(&outer, "acme/outer")];
        let payload = json!({"hook_event_name":"PreToolUse", "session_id":"nested",
            "cwd":outer, "tool_name":"Write", "tool_input":{"file_path":inner.join("new.rs")}});
        assert_eq!(
            route_codex_with(&payload, &connections).unwrap_err(),
            "edit path belongs to another Git checkout"
        );
        let mixed = json!({"hook_event_name":"SessionStart", "session_id":"nested", "cwd":connections[0].root, "workspace_roots":[inner.clone()]});
        assert_eq!(
            route_codex_with(&mixed, &connections).unwrap_err(),
            "path belongs to another Git checkout"
        );
        let session = json!({"hook_event_name":"SessionStart", "session_id":"nested", "cwd":inner});
        assert_eq!(
            route_codex_with(&session, &connections).unwrap_err(),
            "path belongs to another Git checkout"
        );
    }

    /// The layout `git worktree add` leaves: a `.git` file and a back-pointing private directory.
    fn linked_worktree(main: &Path, path: &Path) -> PathBuf {
        let own = main.join(".git/worktrees").join(path.file_name().unwrap());
        std::fs::create_dir_all(&own).unwrap();
        std::fs::create_dir_all(path).unwrap();
        std::fs::write(path.join(".git"), format!("gitdir: {}\n", own.display())).unwrap();
        std::fs::write(
            own.join("gitdir"),
            format!("{}\n", path.join(".git").display()),
        )
        .unwrap();
        std::fs::write(own.join("commondir"), "../..\n").unwrap();
        path.canonicalize().unwrap()
    }

    #[test]
    fn linked_worktree_sessions_and_edits_use_the_registered_connection() {
        let parent = tempfile::tempdir().unwrap();
        let main = git_repo(parent.path(), "main");
        let nested = linked_worktree(&main, &main.join(".claude/worktrees/task"));
        let beside = linked_worktree(&main, &parent.path().join("beside"));
        let connections = [connection(&main, "acme/widgets")];
        for checkout in [&nested, &beside] {
            let session =
                json!({"hook_event_name":"SessionStart", "session_id":"s", "cwd":checkout});
            let routed = route_client_with("claude", &session, &connections).unwrap();
            assert_eq!(routed.connection, connections[0]);
            assert_eq!(&routed.checkout, checkout);
            let edit = json!({"hook_event_name":"PreToolUse", "session_id":"s", "cwd":checkout,
                "tool_name":"Write", "tool_input":{"file_path":checkout.join("src/a.rs")}});
            let routed = route_client_with("claude", &edit, &connections).unwrap();
            assert_eq!(&routed.checkout, checkout);
            let sent = routed.payload.to_string();
            assert!(
                sent.contains("src/a.rs") && !sent.contains("worktrees"),
                "{sent}"
            );
        }
        let crossing = json!({"hook_event_name":"PreToolUse", "session_id":"s", "cwd":main,
            "tool_name":"apply_patch", "tool_input":format!(
                "*** Begin Patch\n*** Update File: {}/a.rs\n*** Move to: {}/a.rs\n*** End Patch",
                main.display(), beside.display())});
        assert_eq!(
            route_codex_with(&crossing, &connections).unwrap_err(),
            "edit crosses Git worktrees"
        );
    }

    #[test]
    fn codex_session_route_rejects_multiple_registered_workspace_roots() {
        let parent = tempfile::tempdir().unwrap();
        let one = git_repo(parent.path(), "one");
        let two = git_repo(parent.path(), "two");
        let connections = [connection(&one, "acme/one"), connection(&two, "acme/two")];
        let payload = json!({
            "hook_event_name": "SessionStart",
            "session_id": "s",
            "cwd": parent.path(),
            "workspace_roots": [one.clone(), two.clone()],
            "prompt": "must stay local"
        });
        assert_eq!(
            route_codex_with(&payload, &connections).unwrap_err(),
            "repository route is ambiguous"
        );

        let cwd_inside_one = json!({
            "hook_event_name": "SessionEnd",
            "session_id": "s",
            "cwd": one,
            "workspace_roots": [two]
        });
        assert_eq!(
            route_codex_with(&cwd_inside_one, &connections).unwrap_err(),
            "repository route is ambiguous"
        );
        assert_eq!(
            route_codex_with(&json!({"hook_event_name":"SessionEnd"}), &connections).unwrap_err(),
            NO_CONNECTION
        );
    }

    #[test]
    fn client_request_omits_the_private_root_header() {
        let parent = tempfile::tempdir().unwrap();
        let root = git_repo(parent.path(), "private-checkout");
        let connections = [connection(&root, "acme/widgets")];
        let routed = route_codex_with(
            &json!({
                "hook_event_name":"PreToolUse",
                "session_id":"s",
                "cwd":root.clone(),
                "tool_name":"Write",
                "tool_input":{"file_path":"src/new.rs", "content":"private"}
            }),
            &connections,
        )
        .unwrap();
        let request = hub_request(
            &reqwest::Client::new(),
            "https://hub.example.test",
            "codex",
            Some("acme/widgets"),
            "synthetic-token",
            &serde_json::to_vec(&routed.payload).unwrap(),
            None,
        )
        .unwrap()
        .build()
        .unwrap();
        assert!(!request.headers().contains_key("X-Agentscm-Root"));
        let body = request.body().and_then(reqwest::Body::as_bytes).unwrap();
        let body = std::str::from_utf8(body).unwrap();
        assert!(!body.contains(root.to_str().unwrap()));
        assert!(!body.contains("private"));
    }

    fn event(name: &str) -> Value {
        json!({"hook_event_name": name})
    }

    #[test]
    fn claude_codex_and_copilot_get_context_on_two_events() {
        for agent in ["claude", "codex", "copilot"] {
            for name in ["PreToolUse", "SessionStart"] {
                let out = unknown_output(agent, &event(name), "hub unreachable");
                assert_eq!(out["hookSpecificOutput"]["hookEventName"], name);
                let text = out["hookSpecificOutput"]["additionalContext"]
                    .as_str()
                    .unwrap();
                assert_eq!(
                    text,
                    "AgentSCM: coordination unknown (hub unreachable). \
                     Other agents' work on these files is not visible."
                );
                assert_eq!(out.as_object().unwrap().len(), 1);
            }
            for other in [
                event("SessionEnd"),
                event("preToolUse"),
                Value::Null,
                json!({}),
            ] {
                assert_eq!(unknown_output(agent, &other, "no token"), json!({}));
            }
        }
    }

    #[test]
    fn cursor_never_gets_invalid_or_blocking_output() {
        let out = |payload: &Value| unknown_output("cursor", payload, "no token");
        assert_eq!(out(&event("preToolUse")), json!({"permission": "allow"}));
        assert_eq!(
            out(&json!({"tool_name": "Write"})),
            json!({"permission": "allow"})
        );
        assert_eq!(out(&Value::Null), json!({"permission": "allow"}));
        for name in ["sessionStart", "postToolUse"] {
            let reply = out(&event(name));
            assert!(
                reply["additional_context"]
                    .as_str()
                    .unwrap()
                    .contains("coordination unknown (no token)")
            );
        }
        assert_eq!(out(&event("sessionEnd")), json!({}));
        assert_eq!(out(&json!({"conversation_id": "c"})), json!({}));
    }

    #[test]
    fn gemini_fail_open_uses_only_documented_non_blocking_outputs() {
        let before = fail_open(
            "gemini",
            &json!({"hook_event_name":"BeforeTool", "session_id":"s"}),
            Some("no token"),
        );
        assert!(
            before["systemMessage"]
                .as_str()
                .unwrap()
                .contains("no token")
        );
        assert!(before.get("decision").is_none());
        let start = fail_open(
            "gemini",
            &json!({"hook_event_name":"SessionStart", "session_id":"other"}),
            Some("hub unreachable"),
        );
        assert_eq!(start["hookSpecificOutput"]["hookEventName"], "SessionStart");
        assert!(
            start["hookSpecificOutput"]["additionalContext"]
                .as_str()
                .unwrap()
                .contains("hub unreachable")
        );
        assert_eq!(
            fail_open(
                "gemini",
                &json!({"hook_event_name":"SessionEnd", "session_id":"end"}),
                Some("no token")
            ),
            json!({})
        );
    }

    #[test]
    fn generic_and_unknown_agents_get_an_empty_object() {
        for agent in ["generic", "mystery", ""] {
            assert_eq!(
                unknown_output(agent, &event("PreToolUse"), "no token"),
                json!({})
            );
        }
    }
}
