//! Stdio MCP transport for the stateless Hub endpoint. The server starts in one checkout, whose
//! connection is its default; a tool call that names another connected checkout goes to that
//! repository's connection instead.

use std::collections::HashSet;
use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use reqwest::header::CONTENT_TYPE;
use serde_json::{Map, Value, json};

use agentscm::hub::forge_http::{self, send_full};
use agentscm::{Error, Result};

use crate::client_cli::read_token;
use crate::client_config::{
    AGENTS, Connection, canonical_root, connection_for_checkout, is_current_in, load, relative,
};
use crate::client_session::{self, Process, Recorded};
use crate::hub_cli::map_hub_error;

const REQUEST_BYTES: usize = 1024 * 1024;
const RESPONSE_BYTES: usize = 4 * 1024 * 1024;

fn invalid(message: &str) -> Error {
    Error::Invalid(message.to_owned())
}

fn selected(root: &Path, connections: &[Connection]) -> Result<Connection> {
    connections
        .iter()
        .find(|connection| connection.root == root)
        .cloned()
        .ok_or_else(|| invalid("this checkout has no saved client connection"))
}

fn connection(root: Option<&Path>) -> Result<(Connection, PathBuf, String)> {
    let cwd = std::env::current_dir()?;
    let checkout = canonical_root(root.unwrap_or(&cwd))?;
    let connections = load()?;
    let connection = connection_for_checkout(&connections, &checkout)
        .map(|(connection, _)| connection.clone())
        .ok_or_else(|| invalid("this checkout has no saved client connection"))?;
    if !is_current_in(&connection, &checkout) {
        return Err(invalid(
            "the saved client connection no longer matches this checkout",
        ));
    }
    let token = read_token(&connection.token_file)?;
    Ok((connection, checkout, token))
}

/// True when `saved` differs from `connection` only by a renewed managed token.
fn renewed_in_place(saved: &Connection, connection: &Connection) -> bool {
    saved.owned_token_id.is_some()
        && connection.owned_token_id.is_some()
        && Connection {
            token_file: connection.token_file.clone(),
            owned_token_id: connection.owned_token_id.clone(),
            owned_token_expires_at: connection.owned_token_expires_at,
            ..saved.clone()
        } == *connection
}

/// The token to use for the next request. The same binding keeps working when only its
/// managed token was renewed in place; any other change stops the server.
///
/// `verified` remembers the last saved connection whose checkout origin passed `is_current_in`
/// (which spawns a subprocess); it is trusted for `ORIGIN_RECHECK` while the saved connection is
/// unchanged. shortcut: an origin edited inside that window is noticed late, upgrade to a
/// config-file mtime key.
fn current_token(
    connection: &Connection,
    checkout: &Path,
    token: &str,
    verified: &mut Option<(Connection, Instant)>,
) -> Result<String> {
    let changed = || invalid("client connection changed; restart the AgentSCM MCP server");
    let connections = load()?;
    let saved = match selected(&connection.root, &connections) {
        Ok(saved) => saved,
        // The connection this server started with is gone (removed, or its token expired).
        // The checkout's current one keeps the server running when it reaches the same Hub and
        // repository, so a worktree session survives its main checkout reconnecting.
        Err(_) => {
            let (current, _) =
                connection_for_checkout(&connections, checkout).ok_or_else(changed)?;
            if current.hub != connection.hub
                || current.repo != connection.repo
                || !is_current_in(current, checkout)
            {
                return Err(changed());
            }
            return read_token(&current.token_file);
        }
    };
    let renewed = renewed_in_place(&saved, connection);
    let fresh = verified
        .as_ref()
        .is_some_and(|(seen, at)| *seen == saved && at.elapsed() < ORIGIN_RECHECK);
    if !(saved == *connection || renewed) || !(fresh || is_current_in(&saved, checkout)) {
        return Err(changed());
    }
    if !fresh {
        *verified = Some((saved.clone(), Instant::now()));
    }
    let current = read_token(&saved.token_file)?;
    // A supplied token file whose contents changed is another credential, not a renewal.
    if current != token && !renewed {
        return Err(changed());
    }
    Ok(current)
}

fn endpoint(connection: &Connection) -> Result<reqwest::Url> {
    let mut url = reqwest::Url::parse(&format!("{}/mcp", connection.hub))
        .map_err(|_| invalid("saved Hub URL is invalid"))?;
    url.query_pairs_mut().append_pair("repo", &connection.repo);
    Ok(url)
}

fn read_line(reader: &mut impl BufRead) -> Result<Option<Vec<u8>>> {
    let mut line = Vec::new();
    let mut bounded = std::io::Read::take(reader, (REQUEST_BYTES + 2) as u64);
    if bounded.read_until(b'\n', &mut line)? == 0 {
        return Ok(None);
    }
    if line.last() == Some(&b'\n') {
        line.pop();
    }
    if line.last() == Some(&b'\r') {
        line.pop();
    }
    if line.len() > REQUEST_BYTES {
        return Err(invalid("MCP request is too large"));
    }
    Ok(Some(line))
}

fn response_id(message: &Value) -> Option<Value> {
    let object = message.as_object()?;
    let id = object.get("id")?;
    if object.get("method").is_none()
        && (object.contains_key("result") || object.contains_key("error"))
    {
        return None;
    }
    Some(if id.is_string() || id.is_number() {
        id.clone()
    } else {
        Value::Null
    })
}

fn rpc_error(id: &Value, code: i32, message: &str) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}})
}

fn rpc_hub_error(id: &Value, envelope: &agentscm::hub::error_envelope::ErrorEnvelope) -> Value {
    json!({"jsonrpc":"2.0","id":id,"error":{"code":-32000,"message":envelope.render(),"data":envelope}})
}

/// A tool result the server gives itself, when a call cannot be sent to any Hub.
fn refused(reason: &str) -> Value {
    json!({"content": [{"type": "text", "text": reason}], "isError": true})
}

fn refuse(id: Option<&Value>, reason: &str, writer: &mut impl Write) -> Result<()> {
    match id {
        Some(id) => write_json(
            writer,
            &json!({"jsonrpc": "2.0", "id": id, "result": refused(reason)}),
        ),
        None => Ok(()),
    }
}

fn json_content_type(headers: &reqwest::header::HeaderMap) -> bool {
    headers
        .get(CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.split(';').next())
        .is_some_and(|value| value.trim().eq_ignore_ascii_case("application/json"))
}

fn valid_reply(reply: &Value, id: &Value) -> bool {
    let Some(object) = reply.as_object() else {
        return false;
    };
    object.get("jsonrpc").and_then(Value::as_str) == Some("2.0")
        && object.get("id") == Some(id)
        && (object.contains_key("result") ^ object.contains_key("error"))
}

async fn forward(
    client: &reqwest::Client,
    url: &reqwest::Url,
    token: &str,
    body: Vec<u8>,
    client_id: &str,
    report_completion: bool,
) -> Result<(u16, bool, Value)> {
    let request = client
        .post(url.clone())
        .bearer_auth(token)
        .header(agentscm::hub::onboarding::CLIENT_HEADER, client_id)
        .header(CONTENT_TYPE, "application/json")
        .body(body);
    let request = crate::client_config::with_completion_reporting(request, report_completion);
    let reply = send_full(request, RESPONSE_BYTES)
        .await
        .map_err(map_hub_error)?;
    Ok((reply.status, json_content_type(&reply.headers), reply.body))
}

fn write_json(writer: &mut impl Write, value: &Value) -> Result<()> {
    serde_json::to_writer(&mut *writer, value)?;
    writer.write_all(b"\n")?;
    writer.flush()?;
    Ok(())
}

const NOT_SIGNED_IN: &str = "AgentSCM is not signed in on this computer, so this checkout cannot connect yet. Run `agentscm hub login`, then try again.";
const STILL_CONNECTING: &str = "AgentSCM is still connecting this checkout to its Hub. Try again in a few seconds. If this keeps happening, the repository may not be on your Hub; ask a Hub admin to connect it.";
const POLL: Duration = Duration::from_secs(10);
const ORIGIN_RECHECK: Duration = Duration::from_secs(30);

/// Why this checkout has no connection yet, in words for the person or agent reading it. A
/// refusal the auto-connect hook recorded (repository not on the Hub, computer revoked or
/// signed out) wins; otherwise a computer with no sign-in cannot connect at all.
fn why_not_connected(root: Option<&Path>) -> String {
    let recorded = std::env::current_dir()
        .ok()
        .and_then(|cwd| canonical_root(root.unwrap_or(&cwd)).ok())
        .and_then(|checkout| crate::client_cli::attention(&checkout));
    match (recorded, crate::hub_cli::auto_connect_hubs()) {
        (Some(message), _) => format!("AgentSCM cannot connect this checkout: {message}."),
        (None, Ok(hubs)) if hubs.is_empty() => NOT_SIGNED_IN.to_owned(),
        _ => STILL_CONNECTING.to_owned(),
    }
}

/// The server's own argument, on every tool: the checkout a call is about. It chooses the
/// connection here and is taken out before the call is forwarded, so a Hub never sees it.
const ROOT: &str = "root";
const ROOT_HELP: &str = "Absolute path of another checkout connected on this computer, when the call is about that repository and not the one this session started in. Absolute paths choose it without `root`.";
const NOT_SENT: &str = "The call was not sent.";
const CONNECT: &str = "Run `agentscm client connect` in that checkout, then call again.";

/// Lists `root` on every tool. Only the two calls made before editing explain it; the rest
/// name it, so the tool list stays short.
fn with_root(tools: &mut Value) {
    for tool in tools.as_array_mut().into_iter().flatten() {
        let explained = matches!(
            tool["name"].as_str(),
            Some("agentscm_start" | "agentscm_check")
        );
        if let Some(properties) = tool
            .pointer_mut("/inputSchema/properties")
            .and_then(Value::as_object_mut)
        {
            let mut root = json!({"type": "string"});
            if explained {
                root["description"] = json!(ROOT_HELP);
            }
            properties.insert(ROOT.into(), root);
        }
    }
}

/// The tools that take `session`.
fn session_tools(tools: &Value) -> HashSet<String> {
    tools
        .as_array()
        .into_iter()
        .flatten()
        .filter(|tool| tool.pointer("/inputSchema/properties/session").is_some())
        .filter_map(|tool| tool["name"].as_str().map(str::to_owned))
        .collect()
}

fn same_repository(one: &Connection, other: &Connection) -> bool {
    one.hub == other.hub && one.repo.eq_ignore_ascii_case(&other.repo)
}

/// The connected checkout a call's arguments name: `root`, else a `worktree` that is a
/// checkout's absolute path, else absolute `paths`, the way an edit hook routes by the file.
/// `None` means the default: the checkout the server started in, `start`, which relative paths
/// are read against. Absolute paths are rewritten repo-relative, as every Hub expects them.
/// `Err` is why the call cannot be sent anywhere, in words for the agent.
fn route(
    arguments: &mut Map<String, Value>,
    start: &Path,
    default: Option<&Connection>,
) -> std::result::Result<Option<(Connection, PathBuf)>, String> {
    let root = match arguments.remove(ROOT) {
        None | Some(Value::Null) => None,
        Some(Value::String(root)) if !root.is_empty() => Some(start.join(root)),
        Some(_) => {
            return Err(format!(
                "AgentSCM: `root` must be a checkout's path. {NOT_SENT}"
            ));
        }
    };
    let absolute = |value: &Value| {
        value
            .as_str()
            .is_some_and(|path| Path::new(path).is_absolute())
    };
    let worktree = arguments
        .get("worktree")
        .filter(|worktree| absolute(worktree))
        .and_then(Value::as_str)
        .map(PathBuf::from);
    let listed = |arguments: &Map<String, Value>| {
        arguments.get("path").is_some_and(absolute)
            || arguments
                .get("paths")
                .and_then(Value::as_array)
                .is_some_and(|paths| paths.iter().any(absolute))
    };
    if root.is_none() && worktree.is_none() && !listed(arguments) {
        return Ok(None);
    }
    let connections =
        load().map_err(|_| format!("AgentSCM: client connections are unavailable. {NOT_SENT}"))?;
    let find = |path: &Path| {
        connection_for_checkout(&connections, path)
            .map(|(connection, checkout)| (connection.clone(), checkout))
    };
    let unconnected = |path: &Path| {
        format!(
            "AgentSCM: {} is not in a repository connected on this computer. {NOT_SENT} {CONNECT}",
            path.display()
        )
    };
    let two = |one: &Connection, other: &Connection| {
        format!(
            "AgentSCM: this call names files in two repositories ({} and {}). {NOT_SENT} Call once per repository.",
            one.repo, other.repo
        )
    };
    let mut target = match (&root, &worktree) {
        (Some(root), _) => Some(find(root).ok_or_else(|| unconnected(root))?),
        // A checkout with no connection is refused; any other text is only the Work's label.
        (None, Some(worktree)) => match find(worktree) {
            None if crate::intent_cli::repo_root(worktree).is_some() => {
                return Err(unconnected(worktree));
            }
            found => found,
        },
        (None, None) => None,
    };
    let anchored = target.is_some();
    let mut relative_paths = false;
    let mut place = |value: &mut Value| -> std::result::Result<(), String> {
        let Some(path) = value.as_str().map(PathBuf::from) else {
            return Ok(());
        };
        if !path.is_absolute() {
            relative_paths = true;
            return Ok(());
        }
        let (connection, checkout) = find(&path).ok_or_else(|| unconnected(&path))?;
        if let Some((chosen, _)) = &target
            && !same_repository(chosen, &connection)
        {
            return Err(two(chosen, &connection));
        }
        *value = json!(relative(&checkout, &path).ok_or_else(|| unconnected(&path))?);
        if target.is_none() {
            target = Some((connection, checkout));
        }
        Ok(())
    };
    if let Some(path) = arguments.get_mut("path") {
        place(path)?;
    }
    for path in arguments
        .get_mut("paths")
        .and_then(Value::as_array_mut)
        .into_iter()
        .flatten()
    {
        place(path)?;
    }
    let Some((connection, checkout)) = target else {
        return Ok(None);
    };
    let here = default.is_some_and(|default| same_repository(default, &connection));
    if relative_paths && !anchored && !here {
        return Err(format!(
            "AgentSCM: this call mixes absolute paths in {} with relative paths, which are read against the checkout this session started in ({}). {NOT_SENT} Call once per repository, or pass `root` so relative paths are read against that checkout.",
            connection.repo,
            start.display()
        ));
    }
    // Another connection of the default's own repository, such as a linked worktree's, is still
    // another agent: the call goes through it, as the edit hooks do.
    let same_connection = default.is_some_and(|default| default.root == connection.root);
    Ok((!(here && same_connection)).then_some((connection, checkout)))
}

/// The other connection of the default's repository that `session`'s hooks route to, such as a
/// linked worktree's own. The record is only a hint: the connection must still be saved, in the
/// same Hub and repository, and current for its checkout, or the default serves the call.
fn session_connection(default: &Connection, session: &str) -> Option<(Connection, PathBuf)> {
    let root = client_session::route_of(
        &crate::hub_cli::config_dir().ok()?,
        session,
        &default.hub,
        &default.repo,
    )?;
    if root == default.root {
        return None;
    }
    let connection = selected(&root, &load().ok()?).ok()?;
    (same_repository(default, &connection) && is_current_in(&connection, &root))
        .then_some((connection, root))
}

/// The token of another repository's connection, after the checks the default connection gets.
fn open(connection: &Connection, checkout: &Path) -> std::result::Result<String, String> {
    let stale = || {
        format!(
            "AgentSCM: the saved connection for {} cannot be used from {}. {NOT_SENT} {CONNECT}",
            connection.repo,
            checkout.display()
        )
    };
    if !is_current_in(connection, checkout) {
        return Err(stale());
    }
    read_token(&connection.token_file).map_err(|_| stale())
}

/// Whether an MCP client's name is one of the agents that run AgentSCM hooks. A client that
/// never said its name counts too: only a client known to have no hooks gets a made-up session.
fn hooked(client: Option<&str>) -> bool {
    client.is_none_or(|name| {
        let name = name.to_ascii_lowercase();
        AGENTS.iter().any(|agent| name.contains(agent))
            || name.contains("visual studio code")
            || name.contains("vscode")
    })
}

/// A session name for one client process without hooks: its own name and eight random digits.
fn own_session(client: &str) -> String {
    let mut name: String = client
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .take(32)
        .collect();
    if name.trim_matches('-').is_empty() {
        name = "mcp".into();
    }
    let mut random = [0_u8; 4];
    // Without a random source the process id still tells two clients on this computer apart.
    if getrandom::fill(&mut random).is_err() {
        random = std::process::id().to_be_bytes();
    }
    format!("{name}-{:08x}", u32::from_be_bytes(random))
}

/// Where one request goes.
struct Sent<'a> {
    connection: &'a Connection,
    token: &'a str,
    activity_binding: &'a str,
}

/// What the server knows about the agent it serves, apart from its default connection.
struct Client {
    http: reqwest::Client,
    /// Where the server started. Relative paths are read against it.
    start: PathBuf,
    /// `clientInfo.name` from `initialize`.
    name: Option<String>,
    /// This process's ancestors, read once, by the first call that names no session.
    ancestors: Option<Vec<Process>>,
    /// The session of a client without hooks, made once so it is the same for every call.
    own_session: Option<String>,
    /// The tools that take `session`, from the last `tools/list`.
    session_tools: HashSet<String>,
}

impl Client {
    fn new(start: PathBuf) -> Result<Self> {
        Ok(Self {
            http: forge_http::client().map_err(map_hub_error)?,
            start,
            name: None,
            ancestors: None,
            own_session: None,
            session_tools: session_tools(&agentscm::hub::mcp::agent_tools()),
        })
    }

    /// The session for a call that names none: the native one the agent's start hook recorded
    /// for this process tree; one name for the whole process when the client has no hooks; and
    /// nothing when a hooked agent's session cannot be told, so the Hub decides as before.
    fn session(&mut self) -> Option<String> {
        let config = crate::hub_cli::config_dir().ok()?;
        let ancestors = self.ancestors.get_or_insert_with(client_session::ancestors);
        match client_session::recorded(&config, ancestors) {
            Recorded::One(session) => Some(session),
            Recorded::Several => None,
            Recorded::None if hooked(self.name.as_deref()) => None,
            Recorded::None => {
                let name = self.name.as_deref().unwrap_or_default();
                Some(
                    self.own_session
                        .get_or_insert_with(|| own_session(name))
                        .clone(),
                )
            }
        }
    }

    /// Completes a tool call: routes it, and names its session when it names none. Returns the
    /// other repository the call is for, or `None` for the default connection.
    fn prepare(
        &mut self,
        message: &mut Value,
        default: Option<&Connection>,
    ) -> std::result::Result<Option<(Connection, PathBuf)>, String> {
        let tool = message["params"]["name"]
            .as_str()
            .unwrap_or_default()
            .to_owned();
        let mut arguments = match message.pointer("/params/arguments") {
            Some(Value::Object(arguments)) => arguments.clone(),
            None | Some(Value::Null) => Map::new(),
            // Anything else is for the Hub to refuse.
            Some(_) => return Ok(None),
        };
        let named = arguments.contains_key("session");
        let session = if !self.session_tools.contains(&tool) {
            None
        } else if let Some(Value::String(session)) = arguments.get("session") {
            Some(session.clone())
        } else if named {
            None
        } else {
            self.session()
        };
        // A call that names a checkout of its own is sent there, whatever the session recorded.
        let explicit = arguments.get(ROOT).is_some_and(|root| !root.is_null())
            || arguments
                .get("worktree")
                .and_then(Value::as_str)
                .is_some_and(|worktree| Path::new(worktree).is_absolute());
        let mut target = route(&mut arguments, &self.start, default)?;
        if target.is_none()
            && !explicit
            && let (Some(default), Some(session)) = (default, &session)
        {
            target = session_connection(default, session);
        }
        if !named && let Some(session) = session {
            arguments.insert("session".into(), json!(session));
        }
        if let Some(params) = message.get_mut("params").and_then(Value::as_object_mut)
            && (!arguments.is_empty() || params.contains_key("arguments"))
        {
            params.insert("arguments".into(), Value::Object(arguments));
        }
        Ok(target)
    }

    /// Reads a request that arrives while the start checkout has no connection: remembers the
    /// client's name, and says whether a tool call names another checkout that has one.
    fn look(&mut self, message: &Value) -> std::result::Result<bool, String> {
        match message["method"].as_str() {
            Some("initialize") => {
                self.name = message["params"]["clientInfo"]["name"]
                    .as_str()
                    .map(str::to_owned);
                Ok(false)
            }
            Some("tools/call") => {
                let mut arguments = message
                    .pointer("/params/arguments")
                    .and_then(Value::as_object)
                    .cloned()
                    .unwrap_or_default();
                route(&mut arguments, &self.start, None).map(|target| target.is_some())
            }
            _ => Ok(false),
        }
    }

    /// Sends one request to the repository it is for and writes the client the reply.
    async fn send(
        &mut self,
        mut message: Value,
        mut body: Vec<u8>,
        default: Option<&Sent<'_>>,
        writer: &mut impl Write,
    ) -> Result<()> {
        let id = response_id(&message);
        let method = message["method"].as_str().unwrap_or_default().to_owned();
        if method == "initialize" {
            self.name = message["params"]["clientInfo"]["name"]
                .as_str()
                .map(str::to_owned);
        }
        let mut elsewhere = None;
        if method == "tools/call" {
            match self.prepare(&mut message, default.map(|sent| sent.connection)) {
                Ok(target) => {
                    elsewhere = target;
                    body = serde_json::to_vec(&message)?;
                }
                Err(reason) => return refuse(id.as_ref(), &reason, writer),
            }
        }
        let reply = match (&elsewhere, default) {
            (Some((connection, checkout)), _) => {
                let token = match open(connection, checkout) {
                    Ok(token) => token,
                    Err(reason) => return refuse(id.as_ref(), &reason, writer),
                };
                let activity_binding = crate::client_activity::activity_binding(connection, &token);
                let sent = Sent {
                    connection,
                    token: &token,
                    activity_binding: &activity_binding,
                };
                exchange(&self.http, &sent, body, id.clone(), writer).await?
            }
            (None, Some(default)) => {
                exchange(&self.http, default, body, id.clone(), writer).await?
            }
            (None, None) => return refuse(id.as_ref(), STILL_CONNECTING, writer),
        };
        let Some(mut reply) = reply else {
            return Ok(());
        };
        if method == "tools/list"
            && let Some(tools) = reply.pointer_mut("/result/tools")
        {
            self.session_tools = session_tools(tools);
            with_root(tools);
        }
        // A call that left the default repository says so: a later call about the same Work
        // names no path, and needs `root` to follow it.
        if let Some((connection, checkout)) = &elsewhere
            && default.is_none_or(|sent| !same_repository(sent.connection, connection))
            && let Some(content) = reply
                .pointer_mut("/result/content")
                .and_then(Value::as_array_mut)
        {
            content.push(json!({"type": "text", "text": format!(
                "AgentSCM: this call went to {}, the repository of the checkout at {}, not the one this session started in. Pass `root` set to that path on later calls about this Work.",
                connection.repo,
                checkout.display()
            )}));
        }
        write_json(writer, &reply)
    }
}

/// Answers locally until `find` sees a connection. `tools/list` already names every tool, so an
/// agent's first session can see them; a call explains why it cannot run yet. When a connection
/// appears, from a request or from the `poll` timer, announces the tools changed and returns it
/// with the request to forward. A user-scope MCP entry starts in every project, and most are not
/// connected; an agent's start-up hook may connect this one while it runs.
///
/// `look` sees every request first. `Ok(true)` hands a tool call back to the caller, which sends
/// it to the other connected checkout it names; `Err` is why that call cannot be sent.
fn unconnected<T>(
    reader: &mut (impl BufRead + Send),
    writer: &mut impl Write,
    mut find: impl FnMut() -> std::result::Result<T, String>,
    poll: Duration,
    mut look: impl FnMut(&Value) -> std::result::Result<bool, String>,
) -> Result<Option<Waited<T>>> {
    let changed = json!({"jsonrpc": "2.0", "method": "notifications/tools/list_changed"});
    std::thread::scope(|scope| {
        // The reader thread reads one line per request, so none is pending once a connection is found
        // from a request, and the reader is free for `serve` when the scope ends.
        let (want, wanted) = mpsc::channel::<()>();
        let (send, lines) = mpsc::channel();
        let reader = &mut *reader;
        scope.spawn(
            move || {
                while wanted.recv().is_ok() && send.send(read_line(reader)).is_ok() {}
            },
        );
        let _ = want.send(());
        loop {
            let line = match lines.recv_timeout(poll) {
                Ok(Ok(Some(line))) => line,
                Ok(Ok(None)) | Err(mpsc::RecvTimeoutError::Disconnected) => return Ok(None),
                Ok(Err(error)) => {
                    write_json(
                        writer,
                        &rpc_error(&Value::Null, -32700, "request too large"),
                    )?;
                    return Err(error);
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    if let Ok(found) = find() {
                        write_json(writer, &changed)?;
                        // The read already under way is the first request to forward.
                        return match lines.recv() {
                            Ok(Ok(Some(line))) => Ok(Some(Waited::Connected(found, line))),
                            Ok(Err(error)) => Err(error),
                            Ok(Ok(None)) | Err(_) => Ok(None),
                        };
                    }
                    continue;
                }
            };
            let reason = match find() {
                Ok(found) => {
                    write_json(writer, &changed)?;
                    return Ok(Some(Waited::Connected(found, line)));
                }
                Err(why) => why,
            };
            let message = serde_json::from_slice::<Value>(&line).ok();
            let looked = message.as_ref().map_or(Ok(false), &mut look);
            if looked == Ok(true) {
                // No read is under way, so the reader is free when the scope ends.
                return Ok(Some(Waited::Elsewhere(line)));
            }
            let _ = want.send(());
            let Some(message) = message else {
                write_json(writer, &rpc_error(&Value::Null, -32700, "parse error"))?;
                continue;
            };
            let Some(id) = response_id(&message) else {
                continue;
            };
            let result = match message["method"].as_str() {
                Some("initialize") => json!({
                    "protocolVersion": message["params"]["protocolVersion"].as_str().unwrap_or("2025-06-18"),
                    "capabilities": {"tools": {"listChanged": true}},
                    "serverInfo": {"name": "agentscm", "version": env!("CARGO_PKG_VERSION")},
                    // Initialize happens once, so a session that connects later keeps these.
                    "instructions": format!(
                        "This checkout is not connected to an AgentSCM Hub yet. The tools are listed, and work once it connects; until then a call says why it cannot run. {}",
                        agentscm::hub::agent_rules::prose()
                    ),
                }),
                Some("ping") => json!({}),
                Some("tools/list") => {
                    let mut tools = agentscm::hub::mcp::agent_tools();
                    with_root(&mut tools);
                    json!({"tools": tools})
                }
                Some("tools/call") => refused(looked.as_ref().err().unwrap_or(&reason)),
                _ => {
                    write_json(
                        writer,
                        &rpc_error(&id, -32601, "this checkout is not connected to AgentSCM"),
                    )?;
                    continue;
                }
            };
            write_json(
                writer,
                &json!({"jsonrpc": "2.0", "id": id, "result": result}),
            )?;
        }
    })
}

/// How the wait for the start checkout's connection ended.
#[derive(Debug, PartialEq)]
enum Waited<T> {
    /// It connected; the request is the first to forward.
    Connected(T, Vec<u8>),
    /// Still unconnected, but this tool call names another checkout that has a connection.
    Elsewhere(Vec<u8>),
}

/// Forwards one request. A well-formed reply is returned for the caller to finish and write;
/// every other outcome is reported to the client here.
async fn exchange(
    http: &reqwest::Client,
    sent: &Sent<'_>,
    body: Vec<u8>,
    id: Option<Value>,
    writer: &mut impl Write,
) -> Result<Option<Value>> {
    let &Sent {
        connection,
        token,
        activity_binding,
    } = sent;
    let client_id = crate::client_config::onboarding_client(&connection.root, token);
    let forwarded = forward(
        http,
        &endpoint(connection)?,
        token,
        body,
        &client_id,
        connection.report_completion,
    )
    .await;
    match (id, forwarded) {
        (None, Ok((202 | 204, _, _))) => report_activity(
            activity_binding,
            crate::client_activity::ActivityOutcome::Acknowledged,
        ),
        (None, Ok((401 | 403, _, _))) => {
            report_activity(
                activity_binding,
                crate::client_activity::ActivityOutcome::TokenRejected,
            );
            eprintln!("AgentSCM MCP: Hub notification failed");
        }
        (None, Ok(_)) => {
            report_activity(
                activity_binding,
                crate::client_activity::ActivityOutcome::InvalidResponse,
            );
            eprintln!("AgentSCM MCP: Hub notification failed");
        }
        (None, Err(_)) => {
            report_activity(
                activity_binding,
                crate::client_activity::ActivityOutcome::Unavailable,
            );
            eprintln!("AgentSCM MCP: Hub notification failed");
        }
        (Some(id), Ok((200, true, reply))) if valid_reply(&reply, &id) => {
            report_activity(
                activity_binding,
                crate::client_activity::ActivityOutcome::Acknowledged,
            );
            let mut reply = reply;
            // Only an older Hub's bare error is upgraded. A current Hub's envelope, and a refused
            // start's conflict, coordination and recovery payload, reach the agent unchanged.
            let bare_error = &reply["result"]["structuredContent"]["error"];
            if reply["result"]["isError"] == true
                && bare_error.is_object()
                && bare_error["recovery"].is_null()
            {
                reply["result"] =
                    agentscm::hub::error_envelope::decode(200, &reply["result"]).mcp();
            } else if reply["error"].is_object() {
                let error = agentscm::hub::error_envelope::decode(400, &reply);
                let code = reply["error"]["code"].as_i64().unwrap_or(-32000);
                reply["error"] = json!({"code":code,"message":error.render(),"data":error});
            }
            return Ok(Some(reply));
        }
        (Some(id), Ok((status @ (401 | 403), _, reply))) => {
            report_activity(
                activity_binding,
                crate::client_activity::ActivityOutcome::TokenRejected,
            );
            let error = agentscm::hub::error_envelope::decode(status, &reply);
            eprintln!("AgentSCM MCP: {}", error.render());
            write_json(writer, &rpc_hub_error(&id, &error))?;
        }
        (Some(id), Ok((status, _, reply))) => {
            report_activity(
                activity_binding,
                crate::client_activity::ActivityOutcome::InvalidResponse,
            );
            let error = if status >= 400 {
                agentscm::hub::error_envelope::decode(status, &reply)
            } else {
                agentscm::hub::error_envelope::ErrorEnvelope::local(
                    agentscm::hub::error_envelope::ErrorCode::InvalidResponse,
                )
            };
            eprintln!("AgentSCM MCP: {}", error.render());
            write_json(writer, &rpc_hub_error(&id, &error))?;
        }
        (Some(id), Err(_)) => {
            report_activity(
                activity_binding,
                crate::client_activity::ActivityOutcome::Unavailable,
            );
            let error = agentscm::hub::error_envelope::ErrorEnvelope::local(
                agentscm::hub::error_envelope::ErrorCode::HubUnreachable,
            );
            eprintln!("AgentSCM MCP: {}", error.render());
            write_json(writer, &rpc_hub_error(&id, &error))?;
        }
    }
    Ok(None)
}

async fn serve(
    connection: &Connection,
    token: &str,
    mut first: Option<Vec<u8>>,
    reader: &mut impl BufRead,
    writer: &mut impl Write,
    mut current_token: impl FnMut() -> Result<String>,
    client: &mut Client,
) -> Result<()> {
    let _relay = crate::hub_cli::config_dir()
        .ok()
        .and_then(|dir| agentscm::hook_relay::Address::new(&dir, &connection.hub).ok())
        .map(agentscm::hook_relay::Relay::spawn);
    let mut token = token.to_owned();
    let mut activity_binding = crate::client_activity::activity_binding(connection, &token);
    loop {
        let line = match first
            .take()
            .map_or_else(|| read_line(reader), |line| Ok(Some(line)))
        {
            Ok(Some(line)) => line,
            Ok(None) => return Ok(()),
            Err(error) => {
                write_json(
                    writer,
                    &rpc_error(&Value::Null, -32700, "request too large"),
                )?;
                return Err(error);
            }
        };
        let message: Value = match serde_json::from_slice(&line) {
            Ok(message) => message,
            Err(_) => {
                write_json(writer, &rpc_error(&Value::Null, -32700, "parse error"))?;
                continue;
            }
        };
        if !message.is_object() {
            write_json(writer, &rpc_error(&Value::Null, -32600, "invalid request"))?;
            continue;
        }
        let checked = current_token();
        if let Ok(current) = &checked
            && *current != token
        {
            token.clone_from(current);
            activity_binding = crate::client_activity::activity_binding(connection, &token);
        }
        if checked.is_err() {
            report_activity(
                &activity_binding,
                crate::client_activity::ActivityOutcome::BindingChanged,
            );
            if let Some(id) = response_id(&message) {
                write_json(
                    writer,
                    &rpc_error(&id, -32000, "AgentSCM connection changed; restart MCP"),
                )?;
            }
            return Err(invalid(
                "client connection changed; restart the AgentSCM MCP server",
            ));
        }
        let default = Sent {
            connection,
            token: &token,
            activity_binding: &activity_binding,
        };
        client.send(message, line, Some(&default), writer).await?;
    }
}

fn report_activity(binding: &str, outcome: crate::client_activity::ActivityOutcome) {
    crate::client_activity::observe(
        binding,
        None,
        crate::client_activity::ActivityPath::Mcp,
        outcome,
    );
}

pub(crate) async fn run(root: Option<&Path>) -> Result<()> {
    // Not `stdin().lock()`: the unconnected loop reads from a second thread.
    let mut reader = std::io::BufReader::new(std::io::stdin());
    let stdout = std::io::stdout();
    let mut writer = stdout.lock();
    let cwd = std::env::current_dir()?;
    let mut client = Client::new(root.map_or_else(|| cwd.clone(), |root| cwd.join(root)))?;
    let ((connection, checkout, token), first) = match connection(root) {
        Ok(found) => (found, None),
        Err(why) => {
            eprintln!(
                "AgentSCM MCP: {why}; tool calls for this checkout fail until it is connected"
            );
            loop {
                let find = || connection(root).map_err(|_| why_not_connected(root));
                let look = |message: &Value| client.look(message);
                match unconnected(&mut reader, &mut writer, find, POLL, look)? {
                    Some(Waited::Connected(found, line)) => break (found, Some(line)),
                    Some(Waited::Elsewhere(line)) => {
                        let message = serde_json::from_slice(&line)?;
                        client.send(message, line, None, &mut writer).await?;
                    }
                    None => return Ok(()),
                }
            }
        }
    };
    client.start.clone_from(&checkout);
    let mut verified = None;
    serve(
        &connection,
        &token,
        first,
        &mut reader,
        &mut writer,
        || current_token(&connection, &checkout, &token, &mut verified),
        &mut client,
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Cursor, Read};
    use std::net::TcpListener;
    use std::sync::mpsc;

    fn saved(hub: String, root: &str, repo: &str) -> Connection {
        Connection {
            root: PathBuf::from(root),
            origin_host: "github.com".into(),
            hub,
            repo: repo.into(),
            token_file: PathBuf::from("/unused-token"),
            report_completion: false,
            owned_token_id: None,
            owned_token_expires_at: None,
            agents: Vec::new(),
        }
    }

    fn server(status: u16, content_type: &str, body: &str) -> (String, mpsc::Receiver<String>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let (sender, receiver) = mpsc::channel();
        let content_type = content_type.to_owned();
        let body = body.to_owned();
        std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(std::time::Duration::from_secs(2)))
                .unwrap();
            let mut request = Vec::new();
            let mut chunk = [0_u8; 4096];
            loop {
                let read = stream.read(&mut chunk).unwrap();
                request.extend_from_slice(&chunk[..read]);
                let Some(head) = request.windows(4).position(|part| part == b"\r\n\r\n") else {
                    continue;
                };
                let headers = String::from_utf8_lossy(&request[..head]);
                let length = headers
                    .lines()
                    .find_map(|line| {
                        let (name, value) = line.split_once(':')?;
                        name.eq_ignore_ascii_case("content-length")
                            .then(|| value.trim().parse::<usize>().ok())
                            .flatten()
                    })
                    .unwrap_or(0);
                if request.len() >= head + 4 + length {
                    break;
                }
            }
            sender.send(String::from_utf8(request).unwrap()).unwrap();
            write!(
                stream,
                "HTTP/1.1 {status} OK\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            )
            .unwrap();
        });
        (format!("http://{address}"), receiver)
    }

    fn client(start: &Path) -> Client {
        Client::new(start.to_owned()).unwrap()
    }

    async fn proxied_as(
        input: &str,
        connection: &Connection,
        token: &str,
        client: &mut Client,
    ) -> String {
        let mut reader = Cursor::new(input.as_bytes());
        let mut output = Vec::new();
        let current = || Ok(token.to_owned());
        serve(
            connection,
            token,
            None,
            &mut reader,
            &mut output,
            current,
            client,
        )
        .await
        .unwrap();
        String::from_utf8(output).unwrap()
    }

    async fn proxied(input: &str, connection: &Connection, token: &str) -> String {
        let mut client = client(Path::new("/checkout"));
        proxied_as(input, connection, token, &mut client).await
    }

    #[test]
    fn input_lines_are_bounded_without_allocating_the_remainder() {
        let input = vec![b'x'; REQUEST_BYTES + 128];
        let mut reader = Cursor::new(input);
        let error = read_line(&mut reader).unwrap_err().to_string();
        assert!(error.contains("too large"));
        assert_eq!(reader.position(), (REQUEST_BYTES + 2) as u64);
    }

    #[test]
    fn only_a_renewed_managed_token_keeps_the_server_running() {
        let mut managed = saved("https://one.example".into(), "/one", "owner/one");
        managed.owned_token_id = Some("tok_old".into());
        managed.owned_token_expires_at = Some(10);
        let mut renewed = managed.clone();
        renewed.owned_token_id = Some("tok_new".into());
        renewed.owned_token_expires_at = Some(20);
        renewed.token_file = PathBuf::from("/new-token");
        assert!(renewed_in_place(&renewed, &managed));
        let mut moved = renewed.clone();
        moved.hub = "https://two.example".into();
        assert!(!renewed_in_place(&moved, &managed));
        let mut other_repo = renewed.clone();
        other_repo.repo = "owner/two".into();
        assert!(!renewed_in_place(&other_repo, &managed));
        // A supplied token is managed elsewhere and never counts as renewed.
        let supplied = saved("https://one.example".into(), "/one", "owner/one");
        assert!(!renewed_in_place(&supplied, &supplied));
    }

    #[cfg(unix)]
    #[test]
    fn a_session_keeps_working_when_its_connection_moves_to_the_main_checkout() {
        crate::client_config::in_isolated_config_home(|home| {
            use std::os::unix::fs::PermissionsExt;
            let git = |dir: &Path, args: &[&str]| {
                let status = std::process::Command::new("git")
                    .args(args)
                    .current_dir(dir)
                    .output()
                    .unwrap();
                assert!(status.status.success(), "git {args:?}");
            };
            let dir = tempfile::tempdir().unwrap();
            let main = dir.path().join("main");
            std::fs::create_dir(&main).unwrap();
            git(&main, &["init", "--quiet"]);
            git(
                &main,
                &[
                    "remote",
                    "add",
                    "origin",
                    "https://github.com/acme/widgets.git",
                ],
            );
            git(
                &main,
                &[
                    "-c",
                    "user.name=t",
                    "-c",
                    "user.email=t@t",
                    "commit",
                    "--quiet",
                    "--allow-empty",
                    "-m",
                    "t",
                ],
            );
            let linked = dir.path().join("linked");
            git(
                &main,
                &[
                    "worktree",
                    "add",
                    "--quiet",
                    "--detach",
                    linked.to_str().unwrap(),
                ],
            );
            let (main, linked) = (main.canonicalize().unwrap(), linked.canonicalize().unwrap());
            let token = |name: &str, value: &str| {
                let path = home.join(name);
                std::fs::write(&path, value).unwrap();
                std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
                path
            };
            let bound = |root: PathBuf, repo: &str, file: PathBuf| Connection {
                origin_host: "github.com".into(),
                token_file: file,
                ..saved("https://hub.example".into(), root.to_str().unwrap(), repo)
            };
            let own = bound(linked.clone(), "acme/widgets", token("own", "own-token"));
            let write = |connections: &[Connection]| {
                let dir = home.join("agentscm");
                std::fs::create_dir_all(&dir).unwrap();
                std::fs::write(
                    dir.join("connections.json"),
                    serde_json::to_string(&json!({"version": 1, "connections": connections}))
                        .unwrap(),
                )
                .unwrap();
            };
            // The worktree's own connection was removed; the main checkout's serves the same
            // Hub and repository, so the server switches to its token instead of stopping.
            write(&[bound(
                main.clone(),
                "acme/widgets",
                token("main", "main-token"),
            )]);
            let mut verified = None;
            assert_eq!(
                current_token(&own, &linked, "own-token", &mut verified).unwrap(),
                "main-token"
            );
            // Another repository's connection is never taken over.
            write(&[bound(
                main.clone(),
                "acme/other",
                token("other", "other-token"),
            )]);
            assert!(current_token(&own, &linked, "own-token", &mut verified).is_err());
        });
    }

    /// A main checkout and a linked worktree of it, both connected to one Hub and repository
    /// with their own tokens, plus a main checkout of another repository, connected.
    #[cfg(unix)]
    fn worktrees(config: &Path) -> (tempfile::TempDir, [PathBuf; 3]) {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let git = |cwd: &Path, args: &[&str]| {
            let out = std::process::Command::new("git")
                .args(args)
                .current_dir(cwd)
                .output()
                .unwrap();
            assert!(out.status.success(), "git {args:?}");
        };
        let make = |name: &str| {
            let root = dir.path().join(name);
            std::fs::create_dir(&root).unwrap();
            let origin = format!("https://github.com/acme/{name}.git");
            git(&root, &["init", "--quiet"]);
            git(&root, &["remote", "add", "origin", &origin]);
            git(
                &root,
                &[
                    "-c",
                    "user.name=t",
                    "-c",
                    "user.email=t@t",
                    "commit",
                    "--quiet",
                    "--allow-empty",
                    "-m",
                    "t",
                ],
            );
            root
        };
        let (main, other) = (make("widgets"), make("gadgets"));
        let linked = dir.path().join("linked");
        git(
            &main,
            &[
                "worktree",
                "add",
                "--quiet",
                "--detach",
                linked.to_str().unwrap(),
            ],
        );
        let roots = [main, linked, other].map(|root| root.canonicalize().unwrap());
        let connections: Vec<Connection> = [
            ("main", "acme/widgets", &roots[0]),
            ("linked", "acme/widgets", &roots[1]),
            ("other", "acme/gadgets", &roots[2]),
        ]
        .into_iter()
        .map(|(name, repo, root)| {
            let token_file = config.join(format!("token-{name}"));
            std::fs::write(&token_file, format!("token-{name}")).unwrap();
            std::fs::set_permissions(&token_file, std::fs::Permissions::from_mode(0o600)).unwrap();
            Connection {
                token_file,
                ..saved("https://hub.example".into(), root.to_str().unwrap(), repo)
            }
        })
        .collect();
        std::fs::create_dir_all(config.join("agentscm")).unwrap();
        std::fs::write(
            config.join("agentscm").join("connections.json"),
            json!({"version": 1, "connections": connections}).to_string(),
        )
        .unwrap();
        (dir, roots)
    }

    #[cfg(unix)]
    #[test]
    fn a_worktree_with_its_own_connection_is_the_agent_its_hooks_name() {
        crate::client_config::in_isolated_config_home(|config| {
            let (_dir, [main, linked, other]) = worktrees(config);
            let default = selected(&main, &load().unwrap()).unwrap();
            let mut client = client(&main);
            let mut call = |arguments: Value| {
                let mut message = json!({"method": "tools/call",
                    "params": {"name": "agentscm_start", "arguments": arguments}});
                let target = client.prepare(&mut message, Some(&default))?;
                Ok::<_, String>((
                    target.map(|(connection, root)| (connection.root, root)),
                    message["params"]["arguments"].take(),
                ))
            };
            let here = Some((linked.clone(), linked.clone()));

            // No record: the default serves a call that names no path.
            assert_eq!(
                call(json!({"task": "t", "session": "native-1"})).unwrap().0,
                None
            );
            // The hook's record sends the session's calls through the worktree's connection,
            // and only that session's.
            let record = |root: &Path, repo: &str| {
                client_session::record_route(config, "native-1", "https://hub.example", repo, root)
                    .unwrap()
            };
            record(&linked, "acme/widgets");
            let (target, forwarded) = call(json!({"task": "t", "session": "native-1"})).unwrap();
            assert_eq!(target, here);
            assert_eq!(forwarded, json!({"task": "t", "session": "native-1"}));
            assert_eq!(
                call(json!({"task": "t", "session": "native-2"})).unwrap().0,
                None
            );
            // A path in the main checkout is still the session's own agent.
            assert_eq!(
                call(json!({"session": "native-1", "paths": [main.join("a.rs")]}))
                    .unwrap()
                    .0,
                here
            );
            // An explicit root or worktree is honoured: the session's record never overrides it.
            record(&linked, "acme/widgets");
            for key in ["root", "worktree"] {
                let (target, forwarded) =
                    call(json!({"task": "t", "session": "native-1", key: main.to_str().unwrap()}))
                        .unwrap();
                assert_eq!(target, None, "{key}");
                assert_eq!(forwarded["session"], "native-1");
            }
            // A route recorded for another repository is not this repository's.
            record(&linked, "acme/gadgets");
            record(&main, "acme/widgets");
            assert_eq!(
                call(json!({"task": "t", "session": "native-1"})).unwrap().0,
                None
            );
            record(&linked, "acme/widgets");
            assert_eq!(
                call(json!({"task": "t", "session": "native-1"})).unwrap().0,
                here
            );
            // A record naming the default's own root, another repository, or a connection that
            // is not saved is ignored.
            for root in [main.clone(), other.clone(), main.join("missing")] {
                record(&root, "acme/widgets");
                assert_eq!(
                    call(json!({"task": "t", "session": "native-1"})).unwrap().0,
                    None,
                    "{root:?}"
                );
            }
            // A record that is not a path is ignored too.
            let file = config.join("agentscm/session-routes");
            for entry in std::fs::read_dir(&file).unwrap().flatten() {
                std::fs::write(entry.path(), "not json").unwrap();
            }
            assert_eq!(
                call(json!({"task": "t", "session": "native-1"})).unwrap().0,
                None
            );

            // An absolute path in the worktree goes to its connection even with no record,
            // made repo-relative; one in the main checkout stays with the default.
            let (target, forwarded) = call(json!({"paths": [linked.join("src/a.rs")]})).unwrap();
            assert_eq!(target, here);
            assert_eq!(forwarded, json!({"paths": ["src/a.rs"]}));
            assert_eq!(
                call(json!({"paths": [main.join("src/a.rs")]})).unwrap(),
                (None, json!({"paths": ["src/a.rs"]}))
            );
        });
    }

    /// Three checkouts under one directory: `widgets` and `gadgets` connected to the given
    /// Hubs with tokens `token-widgets` and `token-gadgets`, and `loose` with no connection.
    #[cfg(unix)]
    fn checkouts(config: &Path, hubs: [&str; 2]) -> (tempfile::TempDir, [PathBuf; 3]) {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let roots = ["widgets", "gadgets", "loose"].map(|name| {
            let root = dir.path().join(name);
            std::fs::create_dir(&root).unwrap();
            let origin = format!("https://github.com/acme/{name}.git");
            for args in [
                vec!["init", "--quiet"],
                vec!["remote", "add", "origin", &origin],
            ] {
                let git = std::process::Command::new("git")
                    .args(&args)
                    .current_dir(&root)
                    .output()
                    .unwrap();
                assert!(git.status.success(), "git {args:?}");
            }
            root.canonicalize().unwrap()
        });
        let connections: Vec<Connection> = (0..2)
            .map(|index| {
                let name = ["widgets", "gadgets"][index];
                let token_file = config.join(format!("token-{name}"));
                std::fs::write(&token_file, format!("token-{name}")).unwrap();
                std::fs::set_permissions(&token_file, std::fs::Permissions::from_mode(0o600))
                    .unwrap();
                Connection {
                    token_file,
                    ..saved(
                        hubs[index].to_owned(),
                        roots[index].to_str().unwrap(),
                        &format!("acme/{name}"),
                    )
                }
            })
            .collect();
        std::fs::create_dir_all(config.join("agentscm")).unwrap();
        std::fs::write(
            config.join("agentscm").join("connections.json"),
            json!({"version": 1, "connections": connections}).to_string(),
        )
        .unwrap();
        (dir, roots)
    }

    #[cfg(unix)]
    #[test]
    fn a_call_goes_to_the_repository_its_paths_or_root_name_under_that_token() {
        crate::client_config::in_isolated_config_home(|config| {
            let answer =
                r#"{"jsonrpc":"2.0","id":1,"result":{"content":[{"type":"text","text":"ok"}]}}"#;
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            // A start with a file of the other repository, then a read that names it by root.
            for (tool, arguments, forwarded) in [
                (
                    "agentscm_start",
                    json!({"task": "t", "session": "s", "paths": ["{gadgets}/src/lib.rs"]}),
                    json!({"task": "t", "session": "s", "paths": ["src/lib.rs"]}),
                ),
                (
                    "agentscm_work_status",
                    json!({"root": "{gadgets}"}),
                    json!({}),
                ),
            ] {
                let (widgets_hub, widgets_seen) = server(200, "application/json", answer);
                let (gadgets_hub, gadgets_seen) = server(200, "application/json", answer);
                let (_dir, [widgets, gadgets, _]) = checkouts(config, [&widgets_hub, &gadgets_hub]);
                let default = selected(&widgets, &load().unwrap()).unwrap();
                let call = json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call",
                    "params": {"name": tool, "arguments": arguments}})
                .to_string()
                .replace("{gadgets}", gadgets.to_str().unwrap());
                let output = runtime.block_on(proxied_as(
                    &format!("{call}\n"),
                    &default,
                    "token-widgets",
                    &mut client(&widgets),
                ));
                let request = gadgets_seen.recv().unwrap();
                assert!(
                    request.starts_with("POST /mcp?repo=acme%2Fgadgets HTTP/1.1"),
                    "{request}"
                );
                assert!(
                    request
                        .to_ascii_lowercase()
                        .contains("authorization: bearer token-gadgets")
                );
                let body: Value =
                    serde_json::from_str(request.split("\r\n\r\n").nth(1).unwrap()).unwrap();
                assert_eq!(body["params"]["arguments"], forwarded);
                // The reply says where the call went, so the next call can follow it.
                let reply: Value = serde_json::from_str(&output).unwrap();
                let note = reply["result"]["content"][1]["text"].as_str().unwrap();
                assert!(
                    note.contains("acme/gadgets") && note.contains(gadgets.to_str().unwrap()),
                    "{note}"
                );
                assert!(widgets_seen.try_recv().is_err());
            }
        });
    }

    #[cfg(unix)]
    #[test]
    fn a_call_across_two_repositories_or_into_an_unconnected_one_is_refused() {
        crate::client_config::in_isolated_config_home(|config| {
            let hub = "https://hub.example";
            let (_dir, [widgets, gadgets, loose]) = checkouts(config, [hub, hub]);
            let default = selected(&widgets, &load().unwrap()).unwrap();
            let mut client = client(&widgets);
            // The repository a call is routed to (none: the default) and what is forwarded.
            let mut call = |arguments: Value| {
                let mut message = json!({"method": "tools/call",
                    "params": {"name": "agentscm_work_status", "arguments": arguments}});
                let target = client.prepare(&mut message, Some(&default))?;
                Ok::<_, String>((
                    target.map(|(connection, _)| connection.repo),
                    message["params"]["arguments"].take(),
                ))
            };
            let both = call(json!({"paths": [widgets.join("a.rs"), gadgets.join("b.rs")]}));
            let both = both.unwrap_err();
            assert!(
                both.contains("acme/widgets and acme/gadgets")
                    && both.contains("Call once per repository"),
                "{both}"
            );
            // A relative path is the start checkout's, so it cannot ride with another's file.
            let mixed = call(json!({"paths": ["a.rs", gadgets.join("b.rs")]})).unwrap_err();
            assert!(mixed.contains("Call once per repository"), "{mixed}");
            for arguments in [
                json!({"paths": [loose.join("c.rs")]}),
                json!({"root": loose}),
                json!({"worktree": loose}),
            ] {
                let refusal = call(arguments).unwrap_err();
                assert!(refusal.contains("`agentscm client connect`"), "{refusal}");
            }
            // The default repository's own absolute paths stay there, made repo-relative, and
            // `root` is never forwarded.
            assert_eq!(
                call(json!({"root": widgets, "paths": [widgets.join("src/a.rs")]})).unwrap(),
                (None, json!({"paths": ["src/a.rs"]}))
            );
            // With `root`, relative paths are that checkout's. A worktree that is only a label
            // routes nothing; one that is a connected checkout's path does.
            assert_eq!(
                call(json!({"root": gadgets, "path": "src/b.rs", "worktree": "feature"})).unwrap(),
                (
                    Some("acme/gadgets".into()),
                    json!({"path": "src/b.rs", "worktree": "feature"})
                )
            );
            let routed = call(json!({"worktree": gadgets})).unwrap();
            assert_eq!(routed.0.as_deref(), Some("acme/gadgets"));
        });
    }

    #[cfg(unix)]
    #[test]
    fn an_omitted_session_is_the_hooks_native_id_or_one_name_for_a_client_without_hooks() {
        crate::client_config::in_isolated_config_home(|config| {
            let host = Process {
                pid: 300,
                started: "Mon Nov  9 09:00:01 2026".into(),
                name: "agent".into(),
            };
            let records = client_session::directory(config);
            let named = |name: &str, ancestors: &[Process]| {
                let mut client = client(Path::new("/checkout"));
                client.name = Some(name.to_owned());
                client.ancestors = Some(ancestors.to_vec());
                client
            };
            // The session a start is forwarded with.
            let session = |client: &mut Client, arguments: Value| {
                let mut message = json!({"method": "tools/call",
                    "params": {"name": "agentscm_start", "arguments": arguments}});
                client.prepare(&mut message, None).unwrap();
                message["params"]["arguments"]["session"].take()
            };

            let mut hooked = named("claude-code", std::slice::from_ref(&host));
            // Before its start hook ran, a hooked agent's call is left for the Hub to place.
            assert_eq!(session(&mut hooked, json!({"task": "t"})), Value::Null);
            client_session::record_under(&records, &host, "native-1").unwrap();
            assert_eq!(session(&mut hooked, json!({"task": "t"})), "native-1");
            // A session the agent names itself stands, and a tool that takes none gets none.
            assert_eq!(
                session(&mut hooked, json!({"task": "t", "session": "mine"})),
                "mine"
            );
            let mut status =
                json!({"method": "tools/call", "params": {"name": "agentscm_work_status"}});
            hooked.prepare(&mut status, None).unwrap();
            assert_eq!(status["params"].get("arguments"), None);
            // A host with two conversations open cannot say whose call this is.
            client_session::record_under(&records, &host, "native-2").unwrap();
            assert_eq!(session(&mut hooked, json!({"task": "t"})), Value::Null);

            // A client that runs no hooks keeps one name of its own for the whole process.
            let mut plain = named("Plain Editor", &[]);
            let first = session(&mut plain, json!({"task": "t"}));
            assert!(
                first.as_str().unwrap().starts_with("plain-editor-"),
                "{first}"
            );
            assert_eq!(session(&mut plain, json!({"task": "t"})), first);
            assert_ne!(
                session(&mut named("Plain Editor", &[]), json!({"task": "t"})),
                first
            );
            // A client that never named itself may be a hooked agent, so it gets no name.
            let mut unnamed = client(Path::new("/checkout"));
            unnamed.ancestors = Some(Vec::new());
            assert_eq!(session(&mut unnamed, json!({"task": "t"})), Value::Null);
        });
    }

    #[test]
    fn root_selection_is_exact() {
        let connections = vec![
            saved("https://one.example".into(), "/one", "owner/one"),
            saved("https://two.example".into(), "/two", "owner/two"),
        ];
        assert_eq!(
            selected(Path::new("/two"), &connections).unwrap().repo,
            "owner/two"
        );
        assert!(selected(Path::new("/three"), &connections).is_err());
    }

    #[tokio::test]
    async fn changed_binding_stops_requests_and_notifications_before_transport() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let connection = saved(
            format!("http://{}", listener.local_addr().unwrap()),
            "/checkout",
            "owner/repo",
        );
        for input in [
            "{\"jsonrpc\":\"2.0\",\"id\":7,\"method\":\"tools/list\"}\n",
            "{\"jsonrpc\":\"2.0\",\"method\":\"notifications/initialized\"}\n",
        ] {
            let mut reader = Cursor::new(input.as_bytes());
            let mut output = Vec::new();
            assert!(
                serve(
                    &connection,
                    "private-token",
                    None,
                    &mut reader,
                    &mut output,
                    || Err(invalid("changed")),
                    &mut client(Path::new("/checkout"))
                )
                .await
                .is_err()
            );
            if input.contains("\"id\"") {
                let reply: Value = serde_json::from_slice(&output).unwrap();
                assert_eq!(reply["id"], 7);
                assert_eq!(reply["error"]["code"], -32000);
            } else {
                assert!(output.is_empty());
            }
            assert!(listener.accept().is_err());
        }
    }

    #[tokio::test]
    async fn forwards_exact_destination_credential_id_and_completion_setting() {
        for enabled in [false, true] {
            let (hub, request) = server(
                200,
                "application/json; charset=utf-8",
                r#"{"jsonrpc":"2.0","id":"local-7","result":{"tools":[]}}"#,
            );
            let mut connection = saved(hub, "/checkout", "Owner/project with space");
            connection.report_completion = enabled;
            let output = proxied(
                "{\"jsonrpc\":\"2.0\",\"id\":\"local-7\",\"method\":\"tools/list\"}\n",
                &connection,
                "private-token",
            )
            .await;
            assert_eq!(
                serde_json::from_str::<Value>(&output).unwrap()["id"],
                "local-7"
            );
            let request = request.recv().unwrap();
            assert!(request.starts_with("POST /mcp?repo=Owner%2Fproject+with+space HTTP/1.1"));
            let request = request.to_ascii_lowercase();
            assert!(request.contains("authorization: bearer private-token"));
            assert_eq!(
                request.contains("x-agentscm-completion-reporting: true"),
                enabled
            );
        }
    }

    #[tokio::test]
    async fn notifications_emit_no_stdout() {
        let (hub, request) = server(202, "application/json", "");
        let connection = saved(hub, "/checkout", "owner/repo");
        let output = proxied(
            "{\"jsonrpc\":\"2.0\",\"method\":\"notifications/initialized\"}\n",
            &connection,
            "private-token",
        )
        .await;
        assert!(output.is_empty());
        assert!(
            request
                .recv()
                .unwrap()
                .starts_with("POST /mcp?repo=owner%2Frepo")
        );
    }

    #[tokio::test]
    async fn connection_outage_service_failure_and_rejection_keep_distinct_envelopes() {
        let input = "{\"jsonrpc\":\"2.0\",\"id\":7,\"method\":\"ping\"}\n";
        for (status, code, action) in [
            (401, "unauthorized", "sign_in"),
            (403, "forbidden", "ask_owner"),
            (503, "unavailable", "retry"),
        ] {
            let body = json!({"error":code}).to_string();
            let (hub, request) = server(status, "application/json", &body);
            let output = proxied(
                input,
                &saved(hub, "/checkout", "owner/repo"),
                "synthetic-token",
            )
            .await;
            let reply: Value = serde_json::from_str(&output).unwrap();
            assert_eq!(reply["error"]["data"]["error"], code);
            assert_eq!(reply["error"]["data"]["recovery"]["action"], action);
            assert!(reply["error"]["data"].get("correlation_id").is_none());
            request.recv().unwrap();
        }
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let hub = format!("http://{}", listener.local_addr().unwrap());
        drop(listener);
        let output = proxied(
            input,
            &saved(hub, "/checkout", "owner/repo"),
            "synthetic-token",
        )
        .await;
        let reply: Value = serde_json::from_str(&output).unwrap();
        assert_eq!(reply["error"]["data"]["error"], "hub_unreachable");
        assert_eq!(reply["error"]["data"]["recovery"]["action"], "reconnect");
        assert!(reply["error"]["data"].get("correlation_id").is_none());
    }

    #[tokio::test]
    async fn redirects_are_not_followed_and_never_become_success() {
        let destination = TcpListener::bind("127.0.0.1:0").unwrap();
        destination.set_nonblocking(true).unwrap();
        let location = format!("http://{}/mcp", destination.local_addr().unwrap());
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = [0_u8; 4096];
            let _ = stream.read(&mut request).unwrap();
            write!(
                stream,
                "HTTP/1.1 307 Temporary Redirect\r\nLocation: {location}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
            )
            .unwrap();
        });
        let connection = saved(format!("http://{address}"), "/checkout", "owner/repo");
        let output = proxied(
            "{\"jsonrpc\":\"2.0\",\"id\":9,\"method\":\"ping\"}\n",
            &connection,
            "private-token",
        )
        .await;
        let reply: Value = serde_json::from_str(&output).unwrap();
        assert_eq!(reply["id"], 9);
        assert_eq!(reply["error"]["code"], -32000);
        assert!(destination.accept().is_err());
    }

    #[tokio::test]
    async fn rejects_non_json_or_wrong_id_replies_without_echoing_them() {
        for (content_type, body) in [
            ("text/plain", r#"{"raw":"private-token"}"#),
            (
                "application/json",
                r#"{"jsonrpc":"2.0","id":"remote","result":{}}"#,
            ),
        ] {
            let (hub, _) = server(200, content_type, body);
            let connection = saved(hub, "/checkout", "owner/repo");
            let output = proxied(
                "{\"jsonrpc\":\"2.0\",\"id\":\"local\",\"method\":\"ping\"}\n",
                &connection,
                "private-token",
            )
            .await;
            let reply: Value = serde_json::from_str(&output).unwrap();
            assert_eq!(reply["id"], "local");
            assert_eq!(reply["error"]["code"], -32000);
            assert!(!output.contains("private-token") && !output.contains("remote"));
        }
    }

    fn replies(output: Vec<u8>) -> Vec<Value> {
        String::from_utf8(output)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect()
    }

    #[test]
    fn an_unconnected_checkout_lists_its_tools_and_explains_a_call_until_connected() {
        let input = concat!(
            "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"initialize\",\"params\":{\"protocolVersion\":\"2025-03-26\"}}\n",
            "{\"jsonrpc\":\"2.0\",\"method\":\"notifications/initialized\"}\n",
            "{\"jsonrpc\":\"2.0\",\"id\":2,\"method\":\"tools/list\"}\n",
            "{\"jsonrpc\":\"2.0\",\"id\":3,\"method\":\"tools/call\",\"params\":{\"name\":\"agentscm_setup_status\"}}\n",
            "{\"jsonrpc\":\"2.0\",\"id\":5,\"method\":\"resources/list\"}\n",
            "{\"jsonrpc\":\"2.0\",\"id\":6,\"method\":\"tools/call\",\"params\":{\"name\":\"refused\"}}\n",
            "{\"jsonrpc\":\"2.0\",\"id\":7,\"method\":\"tools/call\",\"params\":{\"name\":\"elsewhere\"}}\n",
            "{\"jsonrpc\":\"2.0\",\"id\":4,\"method\":\"tools/call\"}\n",
        );
        let mut reader = Cursor::new(input.as_bytes());
        let mut output = Vec::new();
        let mut calls = 0;
        let mut find = || {
            calls += 1;
            if calls == 8 {
                Ok("connection")
            } else {
                Err("still connecting".to_owned())
            }
        };
        // What `Client::look` decides from a call's arguments, here from its tool name.
        let look = |message: &Value| match message["params"]["name"].as_str() {
            Some("refused") => Err("names an unconnected checkout".to_owned()),
            Some("elsewhere") => Ok(true),
            _ => Ok(false),
        };
        let wait = Duration::from_secs(60);
        // A call that names another connected checkout is handed back to be sent there.
        let found = unconnected(&mut reader, &mut output, &mut find, wait, look).unwrap();
        let Some(Waited::Elsewhere(line)) = found else {
            panic!("{found:?}");
        };
        assert!(String::from_utf8(line).unwrap().contains("\"id\":7"));
        // The wait goes on with the next request, until the start checkout connects.
        let found = unconnected(&mut reader, &mut output, &mut find, wait, look).unwrap();
        let Some(Waited::Connected(connection, line)) = found else {
            panic!("{found:?}");
        };
        assert_eq!(connection, "connection");
        assert!(String::from_utf8(line).unwrap().contains("\"id\":4"));
        let replies = replies(output);
        assert_eq!(replies.len(), 6);
        assert_eq!(replies[0]["result"]["protocolVersion"], "2025-03-26");
        assert_eq!(
            replies[0]["result"]["capabilities"]["tools"]["listChanged"],
            true
        );
        let mut tools = agentscm::hub::mcp::agent_tools();
        with_root(&mut tools);
        assert_eq!(replies[1]["result"]["tools"], tools);
        assert_eq!(replies[2]["id"], 3);
        assert_eq!(replies[2]["result"]["isError"], true);
        assert_eq!(
            replies[2]["result"]["content"][0]["text"],
            "still connecting"
        );
        assert_eq!(replies[3]["error"]["code"], -32601);
        // A call that names a checkout with no connection says so, not why this one waits.
        assert_eq!(
            replies[4]["result"]["content"][0]["text"],
            "names an unconnected checkout"
        );
        assert_eq!(replies[5]["method"], "notifications/tools/list_changed");
        let mut ended = Cursor::new(&b""[..]);
        assert!(
            unconnected(
                &mut ended,
                &mut Vec::new(),
                || Err::<(), _>(String::new()),
                Duration::from_secs(60),
                |_| Ok(false)
            )
            .unwrap()
            .is_none()
        );
    }

    #[test]
    fn every_listed_tool_takes_a_root_and_only_the_first_calls_explain_it() {
        let mut tools = agentscm::hub::mcp::agent_tools();
        let sessions = session_tools(&tools);
        assert!(sessions.contains("agentscm_start") && !sessions.contains("agentscm_work_status"));
        with_root(&mut tools);
        for tool in tools.as_array().unwrap() {
            let name = tool["name"].as_str().unwrap();
            let root = &tool["inputSchema"]["properties"][ROOT];
            assert_eq!(root["type"], "string", "{name}");
            let explained = ["agentscm_start", "agentscm_check"].contains(&name);
            assert_eq!(root["description"].is_string(), explained, "{name}");
        }
    }

    /// A reader that stays silent until released, like a client waiting for list_changed.
    struct Silent(mpsc::Receiver<Vec<u8>>, Cursor<Vec<u8>>);

    impl Read for Silent {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            if self.1.position() == self.1.get_ref().len() as u64
                && let Ok(more) = self.0.recv()
            {
                self.1 = Cursor::new(more);
            }
            self.1.read(buf)
        }
    }

    #[test]
    fn a_connection_that_appears_is_announced_without_waiting_for_a_request() {
        let (release, released) = mpsc::channel();
        let mut reader = std::io::BufReader::new(Silent(released, Cursor::new(Vec::new())));
        let mut output = Vec::new();
        let mut polls = 0;
        let found = std::thread::scope(|scope| {
            let handle = scope.spawn(|| {
                unconnected(
                    &mut reader,
                    &mut output,
                    || {
                        polls += 1;
                        if polls >= 2 {
                            Ok(7)
                        } else {
                            Err("connecting".into())
                        }
                    },
                    Duration::from_millis(20),
                    |_| Ok(false),
                )
            });
            // The announcement is written while the client is still silent.
            std::thread::sleep(Duration::from_millis(300));
            release
                .send(b"{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"tools/list\"}\n".to_vec())
                .unwrap();
            handle.join().unwrap()
        });
        let Some(Waited::Connected(value, line)) = found.unwrap() else {
            panic!("not connected");
        };
        assert_eq!(value, 7);
        assert!(String::from_utf8(line).unwrap().contains("tools/list"));
        let replies = replies(output);
        assert_eq!(replies.len(), 1);
        assert_eq!(replies[0]["method"], "notifications/tools/list_changed");
    }
}
