#![cfg(unix)]

use std::net::TcpListener;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::Command;

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

fn repo(parent: &Path, name: &str) -> std::path::PathBuf {
    let root = parent.join(name);
    std::fs::create_dir(&root).unwrap();
    git(&root, &["init", "--quiet"]);
    git(
        &root,
        &[
            "remote",
            "add",
            "origin",
            &format!("https://github.com/acme/{name}.git"),
        ],
    );
    root.canonicalize().unwrap()
}

fn run(config: &Path, root: Option<&Path>, check: bool) -> std::process::Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_agentscm"));
    command.args(["client", "status", "--json"]);
    if check {
        command.arg("--check");
    }
    if let Some(root) = root {
        command.arg("--root").arg(root);
    }
    command
        .env_clear()
        .env("XDG_CONFIG_HOME", config)
        .env("HOME", config)
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .output()
        .unwrap()
}

fn save(config: &Path, connections: serde_json::Value) {
    let directory = config.join("agentscm");
    std::fs::create_dir_all(&directory).unwrap();
    std::fs::write(
        directory.join("connections.json"),
        serde_json::to_vec(&serde_json::json!({"version":1,"connections":connections})).unwrap(),
    )
    .unwrap();
}

#[test]
fn local_json_is_versioned_and_does_not_leak_token_or_token_path() {
    let temp = tempfile::tempdir().unwrap();
    let root = repo(temp.path(), "one");
    let token = temp.path().join("secret-token-file");
    std::fs::write(&token, "synthetic-secret-value\n").unwrap();
    std::fs::set_permissions(&token, std::fs::Permissions::from_mode(0o600)).unwrap();
    save(
        temp.path(),
        serde_json::json!([{
            "root":root,"origin_host":"github.com","hub":"http://127.0.0.1:9",
            "repo":"acme/one","token_file":token,"agents":["codex"]
        }]),
    );
    let output = run(temp.path(), Some(&root), false);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let text = String::from_utf8(output.stdout).unwrap();
    assert!(!text.contains("synthetic-secret-value"));
    assert!(!text.contains("secret-token-file"));
    let value: serde_json::Value = serde_json::from_str(&text).unwrap();
    assert_eq!(value["version"], 1);
    assert_eq!(value["connections"][0]["hub_check"]["state"], "skipped");
    assert_eq!(value["connections"][0]["credential"]["state"], "available");
    assert!(value["connections"][0]["credential"]["expires_at"].is_null());
}

#[test]
fn local_snapshot_keeps_two_hubs_and_binding_generations_isolated() {
    let temp = tempfile::tempdir().unwrap();
    let first = repo(temp.path(), "first");
    let second = repo(temp.path(), "second");
    let token_a = temp.path().join("a");
    let token_b = temp.path().join("b");
    std::fs::write(&token_a, "token-a").unwrap();
    std::fs::write(&token_b, "token-b").unwrap();
    std::fs::set_permissions(&token_a, std::fs::Permissions::from_mode(0o600)).unwrap();
    std::fs::set_permissions(&token_b, std::fs::Permissions::from_mode(0o600)).unwrap();
    save(
        temp.path(),
        serde_json::json!([
            {"root":first,"origin_host":"github.com","hub":"http://127.0.0.1:8","repo":"acme/first","token_file":token_a},
            {"root":second,"origin_host":"github.com","hub":"http://127.0.0.1:9","repo":"acme/second","token_file":token_b}
        ]),
    );
    let output = run(temp.path(), None, false);
    assert!(output.status.success());
    let value: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(value["connections"].as_array().unwrap().len(), 2);
    assert_ne!(
        value["connections"][0]["binding"]["generation"],
        value["connections"][1]["binding"]["generation"]
    );
    assert_ne!(
        value["connections"][0]["binding"]["activity_generation"],
        value["connections"][1]["binding"]["activity_generation"]
    );
}

#[test]
fn rejected_token_is_typed_without_creating_onboarding_proof() {
    let temp = tempfile::tempdir().unwrap();
    let root = repo(temp.path(), "rejected");
    let token = temp.path().join("token");
    std::fs::write(&token, "rejected-token").unwrap();
    std::fs::set_permissions(&token, std::fs::Permissions::from_mode(0o600)).unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let server = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        use std::io::{Read, Write};
        let mut request = [0_u8; 4096];
        let read = stream.read(&mut request).unwrap();
        let request = String::from_utf8_lossy(&request[..read]);
        assert!(request.starts_with("GET /v1/hub/whoami "));
        stream.write_all(b"HTTP/1.1 401 Unauthorized\r\nContent-Type: application/json\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}").unwrap();
    });
    save(
        temp.path(),
        serde_json::json!([{
            "root":root,"origin_host":"github.com","hub":format!("http://{address}"),
            "repo":"acme/rejected","token_file":token,"agents":["codex"]
        }]),
    );
    let output = run(temp.path(), Some(&root), true);
    server.join().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let value: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(value["connections"][0]["hub_check"]["state"], "rejected");
    assert_eq!(value["connections"][0]["credential"]["state"], "available");
    assert_eq!(
        value["connections"][0]["recent_participation"],
        serde_json::json!([])
    );
}

#[test]
fn in_flight_token_rotation_clears_retired_binding_proof() {
    let temp = tempfile::tempdir().unwrap();
    let root = repo(temp.path(), "rotation");
    let token = temp.path().join("rotation-token");
    std::fs::write(&token, "old-token").unwrap();
    std::fs::set_permissions(&token, std::fs::Permissions::from_mode(0o600)).unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let (accepted_tx, accepted_rx) = std::sync::mpsc::channel();
    let (reply_tx, reply_rx) = std::sync::mpsc::channel();
    let server = std::thread::spawn(move || {
        use std::io::{Read, Write};
        let (mut stream, _) = listener.accept().unwrap();
        let mut request = [0_u8; 4096];
        let read = stream.read(&mut request).unwrap();
        assert!(String::from_utf8_lossy(&request[..read]).starts_with("GET /v1/hub/whoami "));
        accepted_tx.send(()).unwrap();
        reply_rx.recv().unwrap();
        let expiry = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64
            + 300;
        let body = serde_json::json!({
            "token":{"repository":"acme/rotation","read_only":false,"expires_at":expiry},
            "owner":{"provider":"github","host":"github.com","login":"synthetic-user"}
        })
        .to_string();
        write!(stream, "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}", body.len(), body).unwrap();
    });
    save(
        temp.path(),
        serde_json::json!([{
            "root":root,"origin_host":"github.com","hub":format!("http://{address}"),
            "repo":"acme/rotation","token_file":token
        }]),
    );
    let config = temp.path().to_owned();
    let selected = root.clone();
    let command = std::thread::spawn(move || run(&config, Some(&selected), true));
    accepted_rx.recv().unwrap();
    std::fs::write(&token, "new-token").unwrap();
    reply_tx.send(()).unwrap();
    let output = command.join().unwrap();
    server.join().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let value: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    let status = &value["connections"][0];
    assert_eq!(status["binding"]["state"], "unknown");
    assert!(status["binding"].get("generation").is_none());
    assert!(status["binding"].get("activity_generation").is_none());
    assert_eq!(status["credential"]["state"], "unknown");
    assert_eq!(status["hub_check"]["state"], "unavailable");
    assert_eq!(status["recent_participation"], serde_json::json!([]));
}

#[test]
fn mismatched_checkout_does_not_contact_hub() {
    let temp = tempfile::tempdir().unwrap();
    let root = repo(temp.path(), "mismatch");
    let token = temp.path().join("token");
    std::fs::write(&token, "token").unwrap();
    std::fs::set_permissions(&token, std::fs::Permissions::from_mode(0o600)).unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    save(
        temp.path(),
        serde_json::json!([{
            "root":root,"origin_host":"github.com","hub":format!("http://{}", listener.local_addr().unwrap()),
            "repo":"acme/mismatch","token_file":token
        }]),
    );
    git(
        &root,
        &[
            "remote",
            "set-url",
            "origin",
            "https://gitlab.com/acme/mismatch.git",
        ],
    );
    let output = run(temp.path(), Some(&root), true);
    assert!(output.status.success());
    assert!(
        listener.accept().is_err(),
        "mismatched binding contacted the Hub"
    );
    let value: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(value["connections"][0]["binding"]["state"], "mismatch");
    assert_eq!(value["connections"][0]["hub_check"]["state"], "skipped");
}

#[test]
fn successful_check_sanitizes_onboarding_session_and_token() {
    let temp = tempfile::tempdir().unwrap();
    let root = repo(temp.path(), "success");
    let token = temp.path().join("token");
    std::fs::write(&token, "private-success-token").unwrap();
    std::fs::set_permissions(&token, std::fs::Permissions::from_mode(0o600)).unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let server = std::thread::spawn(move || {
        use std::io::{Read, Write};
        let expiry = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64
            + 300;
        for index in 0..2 {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = [0_u8; 8192];
            let read = stream.read(&mut request).unwrap();
            let request = String::from_utf8_lossy(&request[..read]);
            let body = if index == 0 {
                assert!(request.starts_with("GET /v1/hub/whoami "));
                serde_json::json!({
                    "token":{"repository":"acme/success","read_only":false,"expires_at":expiry},
                    "owner":{"provider":"github","host":"github.com","login":"synthetic-user"},
                    "shared_conversation_retention_days": 12
                })
            } else {
                assert!(request.starts_with("GET /v1/hub/onboarding?agent=codex "));
                serde_json::json!([{
                    "agent":"codex","state":"ready","blocker":null,"mcp_verified":true,
                    "native_verified":true,"expires_at":expiry,"next_action":"untrusted repair",
                    "session":"raw-private-session","token":"raw-private-token"
                }])
            };
            let body = body.to_string();
            write!(stream, "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}", body.len(), body).unwrap();
        }
    });
    save(
        temp.path(),
        serde_json::json!([{
            "root":root,"origin_host":"github.com","hub":format!("http://{address}"),
            "repo":"acme/success","token_file":token,"agents":["codex"]
        }]),
    );
    let output = run(temp.path(), Some(&root), true);
    server.join().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let text = String::from_utf8(output.stdout).unwrap();
    assert!(!text.contains("private-success-token"));
    assert!(!text.contains("raw-private-session"));
    assert!(!text.contains("raw-private-token"));
    assert!(!text.contains("untrusted repair"));
    let value: serde_json::Value = serde_json::from_str(&text).unwrap();
    assert_eq!(value["connections"][0]["hub_check"]["state"], "valid");
    assert_eq!(
        value["connections"][0]["hub_check"]["shared_conversation_retention_days"],
        12
    );
    assert_eq!(
        value["connections"][0]["recent_participation"][0]["state"],
        "ready"
    );
}

/// Runs `client status --check` against a Hub whose `whoami` also carries `extra`.
fn hub_check_with(extra: serde_json::Value) -> serde_json::Value {
    let temp = tempfile::tempdir().unwrap();
    let root = repo(temp.path(), "retention");
    let token = temp.path().join("token");
    std::fs::write(&token, "private-retention-token").unwrap();
    std::fs::set_permissions(&token, std::fs::Permissions::from_mode(0o600)).unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let server = std::thread::spawn(move || {
        use std::io::{Read, Write};
        let expiry = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64
            + 300;
        let (mut stream, _) = listener.accept().unwrap();
        let mut request = [0_u8; 8192];
        let _ = stream.read(&mut request).unwrap();
        let mut body = serde_json::json!({
            "token":{"repository":"acme/retention","read_only":false,"expires_at":expiry},
            "owner":{"provider":"github","host":"github.com","login":"synthetic-user"}
        });
        body.as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        let body = body.to_string();
        write!(stream, "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}", body.len(), body).unwrap();
    });
    save(
        temp.path(),
        serde_json::json!([{
            "root":root,"origin_host":"github.com","hub":format!("http://{address}"),
            "repo":"acme/retention","token_file":token
        }]),
    );
    let output = run(temp.path(), Some(&root), true);
    server.join().unwrap();
    let value: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    value["connections"][0]["hub_check"].clone()
}

#[test]
fn hub_retention_is_reported_only_when_the_hub_says_a_valid_number() {
    let reported = hub_check_with(serde_json::json!({"shared_conversation_retention_days": 3650}));
    assert_eq!(reported["state"], "valid");
    assert_eq!(reported["shared_conversation_retention_days"], 3650);
    // An older Hub, or a value no Hub would configure, is unknown rather than a guessed number.
    for extra in [
        serde_json::json!({}),
        serde_json::json!({"shared_conversation_retention_days": 0}),
        serde_json::json!({"shared_conversation_retention_days": 3651}),
        serde_json::json!({"shared_conversation_retention_days": "30"}),
    ] {
        let check = hub_check_with(extra.clone());
        assert_eq!(check["state"], "valid", "{extra}");
        assert!(
            check.get("shared_conversation_retention_days").is_none(),
            "{extra}"
        );
    }
}

#[test]
fn slow_hub_is_bounded_and_reported_unavailable() {
    let temp = tempfile::tempdir().unwrap();
    let root = repo(temp.path(), "slow");
    let token = temp.path().join("token");
    std::fs::write(&token, "slow-token").unwrap();
    std::fs::set_permissions(&token, std::fs::Permissions::from_mode(0o600)).unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let server = std::thread::spawn(move || {
        let (_stream, _) = listener.accept().unwrap();
        std::thread::sleep(std::time::Duration::from_secs(6));
    });
    save(
        temp.path(),
        serde_json::json!([{
            "root":root,"origin_host":"github.com","hub":format!("http://{address}"),
            "repo":"acme/slow","token_file":token
        }]),
    );
    let started = std::time::Instant::now();
    let output = run(temp.path(), Some(&root), true);
    assert!(started.elapsed() < std::time::Duration::from_secs(6));
    server.join().unwrap();
    assert!(output.status.success());
    let value: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(value["connections"][0]["hub_check"]["state"], "unavailable");
    assert_eq!(
        value["connections"][0]["recent_participation"],
        serde_json::json!([])
    );
}
