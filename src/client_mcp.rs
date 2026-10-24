//! Checkout-bound stdio MCP transport for the stateless Hub endpoint.

use std::io::{BufRead, Write};
use std::path::Path;

use reqwest::header::CONTENT_TYPE;
use serde_json::{Value, json};

use agentscm::hub::forge_http::{self, send_full};
use agentscm::{Error, Result};

use crate::client_cli::read_token;
use crate::client_config::{
    Connection, canonical_root, connection_for_checkout, is_current, is_current_in, load,
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

fn connection(root: Option<&Path>) -> Result<(Connection, String)> {
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
    Ok((connection, token))
}

fn unchanged(connection: &Connection, token: &str) -> Result<()> {
    let saved = selected(&connection.root, &load()?)?;
    if saved != *connection
        || !is_current(connection)
        || read_token(&connection.token_file)? != token
    {
        return Err(invalid(
            "client connection changed; restart the AgentSCM MCP server",
        ));
    }
    Ok(())
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

async fn serve(
    connection: &Connection,
    token: &str,
    reader: &mut impl BufRead,
    writer: &mut impl Write,
    mut check_connection: impl FnMut() -> Result<()>,
) -> Result<()> {
    let client = forge_http::client().map_err(map_hub_error)?;
    let url = endpoint(connection)?;
    let activity_binding = crate::client_activity::activity_binding(connection, token);
    loop {
        let line = match read_line(reader) {
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
        if check_connection().is_err() {
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
        let client_id = crate::client_config::onboarding_client(&connection.root, token);
        let forwarded = forward(
            &client,
            &url,
            token,
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
    let (connection, token) = connection(root)?;
    let stdin = std::io::stdin();
    let stdout = std::io::stdout();
    serve(
        &connection,
        &token,
        &mut stdin.lock(),
        &mut stdout.lock(),
        || unchanged(&connection, &token),
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Cursor, Read};
    use std::net::TcpListener;
    use std::path::PathBuf;
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
        serve(connection, token, &mut reader, &mut output, || Ok(()))
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
}
