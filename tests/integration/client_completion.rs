#![cfg(unix)]

use std::io::{Read, Write};
use std::net::TcpListener;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::Duration;

fn git(root: &Path, args: &[&str]) {
    let output = Command::new("git")
        .current_dir(root)
        .args(args)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn listener() -> (String, mpsc::Receiver<String>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let (sender, receiver) = mpsc::channel();
    std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(3)))
            .unwrap();
        let mut request = Vec::new();
        let mut chunk = [0_u8; 4096];
        loop {
            let read = stream.read(&mut chunk).unwrap();
            assert!(read > 0, "hook closed before sending its full request");
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
        stream
            .write_all(
                b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}",
            )
            .unwrap();
    });
    (format!("http://{address}"), receiver)
}

#[test]
fn native_client_connection_sends_completion_header_only_when_enabled() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("repo");
    std::fs::create_dir(&root).unwrap();
    git(&root, &["init", "--quiet"]);
    git(
        &root,
        &[
            "remote",
            "add",
            "origin",
            "https://github.com/acme/widgets.git",
        ],
    );
    let root = root.canonicalize().unwrap();
    let config = temp.path().join("config");
    let agentscm = config.join("agentscm");
    std::fs::create_dir_all(&agentscm).unwrap();
    let token = temp.path().join("client.token");
    std::fs::write(&token, "synthetic-private-token\n").unwrap();
    std::fs::set_permissions(&token, std::fs::Permissions::from_mode(0o600)).unwrap();

    for enabled in [false, true] {
        let (hub, request) = listener();
        let reporting = if enabled {
            ",\"report_completion\":true"
        } else {
            ""
        };
        let saved = format!(
            "{{\"version\":1,\"connections\":[{{\"root\":{},\"origin_host\":\"github.com\",\"hub\":{},\"repo\":\"acme/widgets\",\"token_file\":{}{reporting}}}]}}",
            serde_json::to_string(&root).unwrap(),
            serde_json::to_string(&hub).unwrap(),
            serde_json::to_string(&token).unwrap(),
        );
        std::fs::write(agentscm.join("connections.json"), saved).unwrap();

        let mut child = Command::new(env!("CARGO_BIN_EXE_agentscm"))
            .args(["hook", "claude", "--client-connection"])
            .env_clear()
            .env("XDG_CONFIG_HOME", &config)
            .env("HOME", temp.path())
            .env("PATH", std::env::var_os("PATH").unwrap_or_default())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        write!(
            child.stdin.take().unwrap(),
            "{{\"hook_event_name\":\"SessionEnd\",\"session_id\":\"session\",\"cwd\":{}}}",
            serde_json::to_string(&root).unwrap()
        )
        .unwrap();
        let output = child.wait_with_output().unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let request = request.recv_timeout(Duration::from_secs(3)).unwrap();
        assert_eq!(
            request
                .to_ascii_lowercase()
                .contains("x-agentscm-completion-reporting: true"),
            enabled
        );
    }
}
