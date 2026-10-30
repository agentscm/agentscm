use std::fs;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use agentscm::model::{EntryChange, EntryKind, FileChange};
use agentscm::store::Engine;
use serde_json::Value;
use tempfile::TempDir;

struct Server {
    child: Child,
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn run(data_dir: &TempDir, args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_agentscm"))
        .arg("--data-dir")
        .arg(data_dir.path())
        .args(args)
        .output()
        .unwrap()
}

fn start_server(data_dir: &TempDir) -> (Server, SocketAddr) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    drop(listener);
    let mut child = Command::new(env!("CARGO_BIN_EXE_agentscm"))
        .arg("--data-dir")
        .arg(data_dir.path())
        .args([
            "serve",
            "--bind",
            &address.to_string(),
            "--max-concurrency",
            "1",
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        if TcpStream::connect(address).is_ok() {
            return (Server { child }, address);
        }
        thread::sleep(Duration::from_millis(20));
    }
    let _ = child.kill();
    let _ = child.wait();
    panic!("server did not listen within five seconds")
}

#[test]
fn serve_requires_public_for_non_loopback_bind() {
    let data_dir = TempDir::new().unwrap();
    let output = run(&data_dir, &["serve", "--bind", "0.0.0.0:0"]);
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("requires --public"));
}

fn get(address: SocketAddr, target: &str) -> Vec<u8> {
    let mut stream = send_get(address, target);
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    let mut response = Vec::new();
    stream.read_to_end(&mut response).unwrap();
    response
}

fn send_get(address: SocketAddr, target: &str) -> TcpStream {
    let mut stream = TcpStream::connect(address).unwrap();
    stream
        .write_all(
            format!("GET {target} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
                .as_bytes(),
        )
        .unwrap();
    stream
}

fn read_headers(stream: &mut TcpStream) -> Vec<u8> {
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    let mut headers = Vec::new();
    while headers.len() < 16 * 1024 {
        let mut byte = [0];
        stream.read_exact(&mut byte).unwrap();
        headers.push(byte[0]);
        if headers.ends_with(b"\r\n\r\n") {
            return headers;
        }
    }
    panic!("response headers exceeded 16 KiB")
}

fn body(response: &[u8]) -> &[u8] {
    let boundary = response
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .expect("HTTP header boundary");
    &response[boundary + 4..]
}

#[test]
fn read_only_http_serves_health_revision_file_and_diff() {
    let data_dir = TempDir::new().unwrap();
    let request = data_dir.path().join("checkpoint.json");
    fs::write(
        &request,
        r#"{"parent":null,"changes":[{"path":"hello.txt","content":[104,105]}]}"#,
    )
    .unwrap();
    let checkpoint = run(
        &data_dir,
        &[
            "checkpoint",
            request.to_str().expect("UTF-8 temporary path"),
        ],
    );
    assert!(
        checkpoint.status.success(),
        "{}",
        String::from_utf8_lossy(&checkpoint.stderr)
    );
    let result: Value = serde_json::from_slice(&checkpoint.stdout).unwrap();
    let revision = result["revision"].as_str().unwrap();

    let (_server, address) = start_server(&data_dir);
    let health = get(address, "/health");
    assert!(health.starts_with(b"HTTP/1.1 200"));
    assert_eq!(
        serde_json::from_slice::<Value>(body(&health)).unwrap()["status"],
        "ok"
    );

    let metadata = get(address, &format!("/v1/revisions/{revision}"));
    assert!(metadata.starts_with(b"HTTP/1.1 200"));
    assert_eq!(
        serde_json::from_slice::<Value>(body(&metadata)).unwrap()["id"],
        revision
    );

    let file = get(
        address,
        &format!("/v1/revisions/{revision}/file?path=hello%2Etxt"),
    );
    assert!(file.starts_with(b"HTTP/1.1 200"));
    assert_eq!(body(&file), b"hi");

    let diff = get(
        address,
        &format!("/v1/diff?left={revision}&right={revision}&limit=10"),
    );
    assert!(diff.starts_with(b"HTTP/1.1 200"));
    let diff_body: Value = serde_json::from_slice(body(&diff)).unwrap();
    assert_eq!(diff_body["entries"], serde_json::json!([]));
    assert_eq!(diff_body["truncated"], false);
}

#[test]
fn http_has_no_mutating_routes_and_rejects_unbounded_diff() {
    let data_dir = TempDir::new().unwrap();
    let (_server, address) = start_server(&data_dir);
    let missing = get(address, "/v1/approve");
    assert!(missing.starts_with(b"HTTP/1.1 404"));

    let unbounded = get(address, "/v1/diff?left=x&right=y&limit=10001");
    assert!(unbounded.starts_with(b"HTTP/1.1 400"));

    let malformed = get(address, "/v1/diff?left=%ZZ&right=y");
    assert!(malformed.starts_with(b"HTTP/1.1 400"));

    let duplicate = get(address, "/v1/diff?left=x&right=y&limit=1&limit=2");
    assert!(duplicate.starts_with(b"HTTP/1.1 400"));
}

#[test]
fn slow_reader_holds_the_work_slot_until_its_response_is_dropped() {
    let data_dir = TempDir::new().unwrap();
    let engine = Engine::open(data_dir.path()).unwrap();
    let revision = engine
        .checkpoint(
            None,
            &[FileChange {
                path: "large.bin".to_owned(),
                content: Some(vec![b'x'; 8 * 1024 * 1024]),
            }],
        )
        .unwrap();
    let (_server, address) = start_server(&data_dir);
    let mut slow_reader = send_get(
        address,
        &format!("/v1/revisions/{revision}/file?path=large.bin"),
    );
    let first_headers = read_headers(&mut slow_reader);
    assert!(first_headers.starts_with(b"HTTP/1.1 200"));

    let competing = get(address, &format!("/v1/revisions/{revision}"));
    assert!(
        competing.starts_with(b"HTTP/1.1 503"),
        "slow response released the work slot after storage work completed"
    );

    drop(slow_reader);
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let after_close = get(address, &format!("/v1/revisions/{revision}"));
        if after_close.starts_with(b"HTTP/1.1 200") {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "work slot was not released after the slow connection closed"
        );
        thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn http_reports_entry_kind_header_and_kind_changes_in_diff() {
    let data_dir = TempDir::new().unwrap();
    let engine = Engine::open(data_dir.path()).unwrap();
    let entry = |path: &str, content: &[u8], kind| EntryChange {
        path: path.into(),
        content: Some(content.to_vec()),
        kind,
    };
    let left = engine
        .checkpoint_entries(
            None,
            &[
                entry("tool", b"#!/bin/sh\n", EntryKind::Regular),
                entry("link", b"target", EntryKind::Symlink),
            ],
        )
        .unwrap();
    let right = engine
        .checkpoint_entries(
            Some(&left),
            &[entry("tool", b"#!/bin/sh\n", EntryKind::Executable)],
        )
        .unwrap();
    drop(engine);
    let (_server, address) = start_server(&data_dir);
    let kind_header = |revision: &str, path: &str| {
        let response = get(
            address,
            &format!("/v1/revisions/{revision}/file?path={path}"),
        );
        assert!(response.starts_with(b"HTTP/1.1 200"));
        let head = String::from_utf8_lossy(&response[..response.len() - body(&response).len()])
            .to_ascii_lowercase();
        let line = head
            .lines()
            .find(|line| line.starts_with("x-agentscm-entry-kind:"))
            .expect("entry kind header")
            .to_owned();
        (line, body(&response).to_vec())
    };
    assert_eq!(
        kind_header(&left, "tool").0,
        "x-agentscm-entry-kind: regular"
    );
    assert_eq!(
        kind_header(&right, "tool").0,
        "x-agentscm-entry-kind: executable"
    );
    let (line, bytes) = kind_header(&right, "link");
    assert_eq!(line, "x-agentscm-entry-kind: symlink");
    assert_eq!(bytes, b"target");

    let diff = get(
        address,
        &format!("/v1/diff?left={left}&right={right}&limit=10"),
    );
    let diff: Value = serde_json::from_slice(body(&diff)).unwrap();
    let entries = diff["entries"].as_array().unwrap();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0]["path"], "tool");
    assert_eq!(entries[0]["after_kind"], "executable");
    assert!(entries[0].get("before_kind").is_none());
}
