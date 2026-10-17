use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::thread::JoinHandle;
use std::time::Duration;

use agentscm::model::FileChange;
use agentscm::store::Engine;
use rusqlite::{Connection, OpenFlags};
use serde_json::Value;
use tempfile::TempDir;

struct Server {
    child: Child,
    address: SocketAddr,
    stderr: Option<JoinHandle<()>>,
}

impl Server {
    fn start(data: &Path) -> Self {
        let mut child = Command::new(env!("CARGO_BIN_EXE_agentscm"))
            .arg("--data-dir")
            .arg(data)
            .args(["serve", "--bind", "127.0.0.1:0"])
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let stderr = child.stderr.take().unwrap();
        let (tx, rx) = std::sync::mpsc::sync_channel(1);
        let reader = std::thread::spawn(move || {
            for line in BufReader::new(stderr).lines() {
                let Ok(line) = line else { break };
                if line.starts_with("listening on ") {
                    let _ = tx.try_send(line);
                }
            }
        });
        let line = match rx.recv_timeout(Duration::from_secs(10)) {
            Ok(line) => line,
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                let _ = reader.join();
                panic!("server did not report its listening address: {error}");
            }
        };
        let address = line.strip_prefix("listening on ").unwrap().parse().unwrap();
        Self {
            child,
            address,
            stderr: Some(reader),
        }
    }

    fn request(&self, method: &str, path: &str) -> String {
        let mut stream = TcpStream::connect(self.address).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(10)))
            .unwrap();
        stream
            .set_write_timeout(Some(Duration::from_secs(10)))
            .unwrap();
        write!(stream, "{method} {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\nContent-Length: 0\r\n\r\n").unwrap();
        let mut output = String::new();
        stream
            .take(8 * 1024 * 1024)
            .read_to_string(&mut output)
            .unwrap();
        output
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        if let Some(reader) = self.stderr.take() {
            let _ = reader.join();
        }
    }
}

fn body(response: &str) -> &str {
    response.split_once("\r\n\r\n").unwrap().1
}

fn assert_private_response(response: &str) {
    let headers = response
        .split_once("\r\n\r\n")
        .unwrap()
        .0
        .to_ascii_lowercase();
    assert!(headers.contains("cache-control: no-store"));
    assert!(headers.contains("x-content-type-options: nosniff"));
    assert!(headers.contains("referrer-policy: no-referrer"));
    assert!(headers.contains("content-security-policy:"));
}

fn cli(data: &Path, arguments: &[&str]) -> Value {
    let output = Command::new(env!("CARGO_BIN_EXE_agentscm"))
        .arg("--data-dir")
        .arg(data)
        .args(arguments)
        .env("PATH", "")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{arguments:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap()
}

fn fixture(root: &Path) -> (PathBuf, PathBuf) {
    let data = root.join("data");
    let work = root.join("workspace");
    cli(&data, &["init"]);
    let revision = Engine::open(&data)
        .unwrap()
        .checkpoint(
            None,
            &[
                FileChange {
                    path: "a.txt".into(),
                    content: Some(b"<script>candidate()</script>".to_vec()),
                },
                FileChange {
                    path: "b.bin".into(),
                    content: Some(vec![0, 255, 16]),
                },
            ],
        )
        .unwrap();
    cli(
        &data,
        &[
            "workflow",
            "start",
            "--change",
            "browser-review",
            "--workspace",
            work.to_str().unwrap(),
            "--revision",
            &revision,
            "--issue",
            "https://example.invalid/<img onerror=issue()>",
            "--intent",
            "Inspect <script>intent()</script>",
            "--criterion",
            "Preserve exact bytes",
            "--scope",
            "a.txt",
            "--recipe",
            &"a".repeat(64),
            "--toolchain",
            &"b".repeat(64),
            "--environment",
            &"c".repeat(64),
        ],
    );
    cli(
        &data,
        &[
            "workflow",
            "note",
            "--command",
            "<script>note()</script>",
            "--environment",
            "unverified",
            "--result",
            "claimed pass",
        ],
    );
    cli(&data, &["workflow", "prepare", "prepare-browser"]);
    (data, work)
}

fn session(data: &Path) -> Vec<u8> {
    Connection::open_with_flags(
        data.join("workflow.sqlite"),
        OpenFlags::SQLITE_OPEN_READ_ONLY,
    )
    .unwrap()
    .query_row("SELECT state FROM session WHERE id=1", [], |row| row.get(0))
    .unwrap()
}

#[test]
fn missing_workflow_is_private_and_get_does_not_create_a_journal() {
    let root = TempDir::new().unwrap();
    let data = root.path().join("data");
    let server = Server::start(&data);
    assert!(!data.join("workflow.sqlite").exists());
    assert!(!data.join("workflow.lock").exists());
    for path in ["/v1/workflow", "/v1/workflow/inspection"] {
        let response = server.request("GET", path);
        assert!(response.starts_with("HTTP/1.1 404"), "{response}");
        assert_private_response(&response);
    }
    assert!(!data.join("workflow.sqlite").exists());
    assert!(!data.join("workflow.lock").exists());
}

#[test]
fn real_workflow_inspection_is_bounded_token_free_and_does_not_change_the_session() {
    let root = TempDir::new().unwrap();
    let (data, _) = fixture(root.path());
    let server = Server::start(&data);
    let before = session(&data);

    let limited = server.request("GET", "/v1/workflow/inspection?limit=1");
    assert!(limited.starts_with("HTTP/1.1 200"), "{limited}");
    assert_private_response(&limited);
    let limited: Value = serde_json::from_str(body(&limited)).unwrap();
    assert_eq!(limited["diff"]["complete"], false);

    let full = server.request("GET", "/v1/workflow/inspection?limit=4096");
    assert!(full.starts_with("HTTP/1.1 200"), "{full}");
    assert_private_response(&full);
    let value: Value = serde_json::from_str(body(&full)).unwrap();
    assert_eq!(value["diff"]["complete"], true);
    assert_eq!(value["diff"]["entries"].as_array().unwrap().len(), 2);
    assert!(
        value["diff"]["entries"]
            .as_array()
            .unwrap()
            .iter()
            .any(|entry| entry["after_bytes"] == serde_json::json!([0, 255, 16]))
    );
    assert_eq!(value["workspace_observation"], "not_observed");
    assert_eq!(value["admission"], "not_assessed");
    assert!(value.get("inspection").is_none());
    assert!(value.get("approval_token").is_none());
    assert!(value.get("approved").is_none());

    assert_eq!(session(&data), before);
}

#[test]
fn query_cannot_select_filesystem_roots_or_issue_mutations() {
    let root = TempDir::new().unwrap();
    let (data, _) = fixture(root.path());
    let server = Server::start(&data);
    let before = session(&data);
    for path in [
        "/v1/workflow?data_dir=elsewhere",
        "/v1/workflow/inspection?path=elsewhere",
        "/v1/workflow/inspection?limit=1&limit=2",
        "/v1/workflow/inspection?limit=0",
        "/v1/workflow/inspection?limit=4097",
        "/v1/workflow/inspection?limit=%FF",
    ] {
        let response = server.request("GET", path);
        assert!(response.starts_with("HTTP/1.1 400"), "{path}: {response}");
        assert_private_response(&response);
    }
    for path in ["/v1/workflow", "/v1/workflow/inspection"] {
        let response = server.request("POST", path);
        assert!(response.starts_with("HTTP/1.1 405"), "{path}: {response}");
    }
    assert_eq!(session(&data), before);
    assert!(!root.path().join("elsewhere").exists());
}

#[test]
fn status_does_not_open_or_attest_to_the_recorded_workspace() {
    let root = TempDir::new().unwrap();
    let (data, workspace) = fixture(root.path());
    fs::rename(&workspace, root.path().join("moved-workspace")).unwrap();
    let server = Server::start(&data);
    let before = session(&data);
    let response = server.request("GET", "/v1/workflow");
    assert!(response.starts_with("HTTP/1.1 200"), "{response}");
    let view: Value = serde_json::from_str(body(&response)).unwrap();
    assert_eq!(view["workspace_observation"], "not_observed");
    assert_eq!(view["admission"], "not_assessed");
    assert!(!body(&response).contains(workspace.to_str().unwrap()));
    assert!(!workspace.exists());
    assert_eq!(session(&data), before);
}

#[test]
fn corrupt_retained_session_returns_sanitized_private_errors() {
    let root = TempDir::new().unwrap();
    let (data, _) = fixture(root.path());
    Connection::open(data.join("workflow.sqlite"))
        .unwrap()
        .execute(
            "UPDATE session SET state=?1 WHERE id=1",
            [b"{secret-retained-state".as_slice()],
        )
        .unwrap();
    let server = Server::start(&data);
    let before = session(&data);
    let response = server.request("GET", "/v1/workflow");
    assert!(response.starts_with("HTTP/1.1 500"), "{response}");
    assert_private_response(&response);
    assert!(!body(&response).contains("secret-retained-state"));
    assert!(!body(&response).contains(data.to_str().unwrap()));
    assert_eq!(session(&data), before);
}

#[test]
fn native_server_restart_preserves_the_workflow_and_does_not_create_approval() {
    let root = TempDir::new().unwrap();
    let (data, _) = fixture(root.path());
    let before = session(&data);
    let first = Server::start(&data);
    let initial = first.request("GET", "/v1/workflow/inspection");
    assert!(initial.starts_with("HTTP/1.1 200"), "{initial}");
    let initial: Value = serde_json::from_str(body(&initial)).unwrap();
    drop(first);
    let second = Server::start(&data);
    let resumed = second.request("GET", "/v1/workflow/inspection");
    assert!(resumed.starts_with("HTTP/1.1 200"), "{resumed}");
    let resumed: Value = serde_json::from_str(body(&resumed)).unwrap();
    assert_eq!(resumed, initial);
    assert_eq!(resumed["local_approval_recorded"], false);
    assert_eq!(resumed["admission"], "not_assessed");
    assert_eq!(session(&data), before);
}

#[test]
fn source_diff_keeps_crlf_and_lone_cr_distinct_from_lf() {
    let root = TempDir::new().unwrap();
    let (data, _) = fixture(root.path());
    let engine = Engine::open(&data).unwrap();
    let left = engine
        .checkpoint(
            None,
            &[
                FileChange {
                    path: "a.txt".into(),
                    content: Some(b"a\r\nb".to_vec()),
                },
                FileChange {
                    path: "b.bin".into(),
                    content: Some(b"a\rb".to_vec()),
                },
            ],
        )
        .unwrap();
    let right = engine
        .checkpoint(
            Some(&left),
            &[
                FileChange {
                    path: "a.txt".into(),
                    content: Some(b"a\nb".to_vec()),
                },
                FileChange {
                    path: "b.bin".into(),
                    content: Some(b"a\nb".to_vec()),
                },
            ],
        )
        .unwrap();
    let diff = agentscm::guided_service::source_diff(&engine, &left, &right, 4096).unwrap();
    assert_eq!(diff["complete"], true);
    assert_eq!(diff["entries"].as_array().unwrap().len(), 2);
    assert!(
        diff["entries"]
            .as_array()
            .unwrap()
            .iter()
            .any(
                |entry| entry["before_bytes"] == serde_json::json!([97, 13, 10, 98])
                    && entry["after_bytes"] == serde_json::json!([97, 10, 98])
            )
    );
    assert!(
        diff["entries"]
            .as_array()
            .unwrap()
            .iter()
            .any(
                |entry| entry["before_bytes"] == serde_json::json!([97, 13, 98])
                    && entry["after_bytes"] == serde_json::json!([97, 10, 98])
            )
    );
}
