//! Checkout-bound stdio MCP transport for the stateless Hub endpoint.

use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::Duration;

use reqwest::header::CONTENT_TYPE;
use serde_json::{Value, json};

use agentscm::hub::forge_http::{self, send_full};
use agentscm::{Error, Result};

use crate::client_cli::read_token;
use crate::client_config::{
    Connection, canonical_root, connection_for_checkout, is_current_in, load,
};
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
fn current_token(connection: &Connection, checkout: &Path, token: &str) -> Result<String> {
    let changed = || invalid("client connection changed; restart the AgentSCM MCP server");
    let saved = selected(&connection.root, &load()?)?;
    let renewed = renewed_in_place(&saved, connection);
    if !(saved == *connection || renewed) || !is_current_in(&saved, checkout) {
        return Err(changed());
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
const POLL: Duration = Duration::from_secs(2);

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

/// Answers locally until `find` sees a connection. `tools/list` already names every tool, so an
/// agent's first session can see them; a call explains why it cannot run yet. When a connection
/// appears, from a request or from the `poll` timer, announces the tools changed and returns it
/// with the request to forward. A user-scope MCP entry starts in every project, and most are not
/// connected; an agent's start-up hook may connect this one while it runs.
fn unconnected<T>(
    reader: &mut (impl BufRead + Send),
    writer: &mut impl Write,
    mut find: impl FnMut() -> std::result::Result<T, String>,
    poll: Duration,
) -> Result<Option<(T, Vec<u8>)>> {
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
                            Ok(Ok(Some(line))) => Ok(Some((found, line))),
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
                    return Ok(Some((found, line)));
                }
                Err(why) => why,
            };
            let _ = want.send(());
            let Ok(message) = serde_json::from_slice::<Value>(&line) else {
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
                    "instructions": "This checkout is not connected to an AgentSCM Hub yet. The tools are listed, and work once it connects; until then a call says why it cannot run.",
                }),
                Some("ping") => json!({}),
                Some("tools/list") => json!({"tools": agentscm::hub::mcp::agent_tools()}),
                Some("tools/call") => {
                    json!({"content": [{"type": "text", "text": reason}], "isError": true})
                }
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

async fn serve(
    connection: &Connection,
    token: &str,
    mut first: Option<Vec<u8>>,
    reader: &mut impl BufRead,
    writer: &mut impl Write,
    mut current_token: impl FnMut() -> Result<String>,
) -> Result<()> {
    let client = forge_http::client().map_err(map_hub_error)?;
    let url = endpoint(connection)?;
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
        let id = response_id(&message);
        drop(message);
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
            if let Some(id) = id {
                write_json(
                    writer,
                    &rpc_error(&id, -32000, "AgentSCM connection changed; restart MCP"),
                )?;
            }
            return Err(invalid(
                "client connection changed; restart the AgentSCM MCP server",
            ));
        }
        let client_id = crate::client_config::onboarding_client(&connection.root, &token);
        let forwarded = forward(
            &client,
            &url,
            &token,
            line,
            &client_id,
            connection.report_completion,
        )
        .await;
        match (id, forwarded) {
            (None, Ok((202 | 204, _, _))) => report_activity(
                &activity_binding,
                crate::client_activity::ActivityOutcome::Acknowledged,
            ),
            (None, Ok((401 | 403, _, _))) => {
                report_activity(
                    &activity_binding,
                    crate::client_activity::ActivityOutcome::TokenRejected,
                );
                eprintln!("AgentSCM MCP: Hub notification failed");
            }
            (None, Ok(_)) => {
                report_activity(
                    &activity_binding,
                    crate::client_activity::ActivityOutcome::InvalidResponse,
                );
                eprintln!("AgentSCM MCP: Hub notification failed");
            }
            (None, Err(_)) => {
                report_activity(
                    &activity_binding,
                    crate::client_activity::ActivityOutcome::Unavailable,
                );
                eprintln!("AgentSCM MCP: Hub notification failed");
            }
            (Some(id), Ok((200, true, reply))) if valid_reply(&reply, &id) => {
                report_activity(
                    &activity_binding,
                    crate::client_activity::ActivityOutcome::Acknowledged,
                );
                write_json(writer, &reply)?;
            }
            (Some(id), Ok((401 | 403, _, _))) => {
                report_activity(
                    &activity_binding,
                    crate::client_activity::ActivityOutcome::TokenRejected,
                );
                eprintln!("AgentSCM MCP: Hub request failed");
                write_json(
                    writer,
                    &rpc_error(&id, -32000, "AgentSCM Hub request failed"),
                )?;
            }
            (Some(id), Ok(_)) => {
                report_activity(
                    &activity_binding,
                    crate::client_activity::ActivityOutcome::InvalidResponse,
                );
                eprintln!("AgentSCM MCP: Hub request failed");
                write_json(
                    writer,
                    &rpc_error(&id, -32000, "AgentSCM Hub request failed"),
                )?;
            }
            (Some(id), Err(_)) => {
                report_activity(
                    &activity_binding,
                    crate::client_activity::ActivityOutcome::Unavailable,
                );
                eprintln!("AgentSCM MCP: Hub request failed");
                write_json(
                    writer,
                    &rpc_error(&id, -32000, "AgentSCM Hub request failed"),
                )?;
            }
        }
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
    let ((connection, checkout, token), first) = match connection(root) {
        Ok(found) => (found, None),
        Err(why) => {
            eprintln!("AgentSCM MCP: {why}; tool calls fail until this checkout is connected");
            let find = || connection(root).map_err(|_| why_not_connected(root));
            match unconnected(&mut reader, &mut writer, find, POLL)? {
                Some((found, line)) => (found, Some(line)),
                None => return Ok(()),
            }
        }
    };
    serve(&connection, &token, first, &mut reader, &mut writer, || {
        current_token(&connection, &checkout, &token)
    })
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

    async fn proxied(input: &str, connection: &Connection, token: &str) -> String {
        let mut reader = Cursor::new(input.as_bytes());
        let mut output = Vec::new();
        serve(connection, token, None, &mut reader, &mut output, || {
            Ok(token.to_owned())
        })
        .await
        .unwrap();
        String::from_utf8(output).unwrap()
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
                    || Err(invalid("changed"))
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
            "{\"jsonrpc\":\"2.0\",\"id\":4,\"method\":\"tools/call\"}\n",
        );
        let mut reader = Cursor::new(input.as_bytes());
        let mut output = Vec::new();
        let mut calls = 0;
        let found = unconnected(
            &mut reader,
            &mut output,
            || {
                calls += 1;
                if calls == 6 {
                    Ok("connection")
                } else {
                    Err("still connecting".to_owned())
                }
            },
            Duration::from_secs(60),
        )
        .unwrap();
        let (connection, line) = found.unwrap();
        assert_eq!(connection, "connection");
        assert!(String::from_utf8(line).unwrap().contains("\"id\":4"));
        let replies = replies(output);
        assert_eq!(replies.len(), 5);
        assert_eq!(replies[0]["result"]["protocolVersion"], "2025-03-26");
        assert_eq!(
            replies[0]["result"]["capabilities"]["tools"]["listChanged"],
            true
        );
        assert_eq!(
            replies[1]["result"]["tools"],
            agentscm::hub::mcp::agent_tools()
        );
        assert_eq!(replies[2]["id"], 3);
        assert_eq!(replies[2]["result"]["isError"], true);
        assert_eq!(
            replies[2]["result"]["content"][0]["text"],
            "still connecting"
        );
        assert_eq!(replies[3]["error"]["code"], -32601);
        assert_eq!(replies[4]["method"], "notifications/tools/list_changed");
        let mut ended = Cursor::new(&b""[..]);
        assert!(
            unconnected(
                &mut ended,
                &mut Vec::new(),
                || Err::<(), _>(String::new()),
                Duration::from_secs(60)
            )
            .unwrap()
            .is_none()
        );
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
                )
            });
            // The announcement is written while the client is still silent.
            std::thread::sleep(Duration::from_millis(300));
            release
                .send(b"{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"tools/list\"}\n".to_vec())
                .unwrap();
            handle.join().unwrap()
        });
        let (value, line) = found.unwrap().unwrap();
        assert_eq!(value, 7);
        assert!(String::from_utf8(line).unwrap().contains("tools/list"));
        let replies = replies(output);
        assert_eq!(replies.len(), 1);
        assert_eq!(replies[0]["method"], "notifications/tools/list_changed");
    }
}
