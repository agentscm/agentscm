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
        for index in 0..3 {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = [0_u8; 8192];
            let read = stream.read(&mut request).unwrap();
            let request = String::from_utf8_lossy(&request[..read]);
            let body = if index == 1 {
                assert!(request.starts_with("GET /v1/hub/health "));
                serde_json::json!({"version":"1.2.3"})
            } else if index == 0 {
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
    // 0: the Hub keeps no bodies for this repository.
    let none = hub_check_with(serde_json::json!({"shared_conversation_retention_days": 0}));
    assert_eq!(none["shared_conversation_retention_days"], 0);
    // An older Hub, or a value no Hub would configure, is unknown rather than a guessed number.
    for extra in [
        serde_json::json!({}),
        serde_json::json!({"shared_conversation_retention_days": -1}),
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

/// A fake Hub that answers `/v1/hub/whoami` with `whoami_status` and `/v1/hub/health` with a
/// version and commit, until `stop` is set.
fn fake_hub(
    repo: &str,
    whoami_status: u16,
    stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
) -> (std::net::SocketAddr, std::thread::JoinHandle<()>) {
    use std::io::{Read, Write};
    use std::sync::atomic::Ordering;
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let address = listener.local_addr().unwrap();
    let repo = repo.to_owned();
    let handle = std::thread::spawn(move || {
        while !stop.load(Ordering::Relaxed) {
            let Ok((mut stream, _)) = listener.accept() else {
                std::thread::sleep(std::time::Duration::from_millis(5));
                continue;
            };
            stream.set_nonblocking(false).unwrap();
            let mut request = [0_u8; 8192];
            let read = stream.read(&mut request).unwrap();
            let request = String::from_utf8_lossy(&request[..read]);
            let expiry = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_secs() as i64
                + 300;
            let (status, body) = if request.starts_with("GET /v1/hub/health ") {
                (
                    200,
                    serde_json::json!({"version":"1.2.3","commit":"0123456789abcdef0123456789abcdef01234567"}),
                )
            } else if whoami_status == 200 {
                (
                    200,
                    serde_json::json!({
                        "token":{"repository":repo,"read_only":false,"expires_at":expiry},
                        "owner":{"provider":"github","host":"github.com","login":"synthetic-user"}
                    }),
                )
            } else {
                (whoami_status, serde_json::json!({}))
            };
            let body = body.to_string();
            write!(stream, "HTTP/1.1 {status} Synthetic\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).unwrap();
        }
    });
    (address, handle)
}

#[test]
fn eight_saved_connections_with_mixed_validity_each_report_their_own_health() {
    use std::sync::{Arc, atomic::AtomicBool, atomic::Ordering};
    let temp = tempfile::tempdir().unwrap();
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64;
    let stop = Arc::new(AtomicBool::new(false));
    let closed = {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.local_addr().unwrap()
    };
    // (name, whoami status or None for a closed port, owned token expiry, token file present,
    //  expected health, whether the Hub build is expected)
    #[allow(clippy::type_complexity)]
    let cases: [(&str, Option<u16>, Option<i64>, bool, &str, bool); 8] = [
        ("valid", Some(200), None, true, "authenticated", true),
        (
            "due",
            Some(200),
            Some(now + 60),
            true,
            "renewal_pending",
            true,
        ),
        ("expired", Some(401), Some(1), true, "expired", true),
        ("unauthorized", Some(401), None, true, "revoked", true),
        ("forbidden", Some(403), None, true, "revoked", true),
        ("broken", Some(500), None, true, "unreachable", true),
        ("down", None, None, true, "unreachable", false),
        ("notoken", None, None, false, "no_usable_token", false),
    ];
    let mut servers = Vec::new();
    let mut tokens = Vec::new();
    let mut saved = Vec::new();
    for (index, (name, status, owned, has_token, _, _)) in cases.iter().enumerate() {
        let root = repo(temp.path(), name);
        let secret = format!("synthetic-secret-{index}-{name}");
        tokens.push(secret.clone());
        let address = match status {
            Some(status) => {
                let (address, handle) =
                    fake_hub(&format!("acme/{name}"), *status, Arc::clone(&stop));
                servers.push(handle);
                address
            }
            None => closed,
        };
        let hub = format!("http://{address}");
        let repository = format!("acme/{name}");
        let mut token = temp.path().join(format!("token-{name}"));
        let mut connection = serde_json::json!({
            "root":root,"origin_host":"github.com","hub":hub,"repo":repository
        });
        if let Some(expiry) = owned {
            // A managed token must sit at the path derived from its connection.
            let id = format!("tok_synthetic_{name}");
            use sha2::Digest;
            let mut digest = sha2::Sha256::new();
            for part in [&hub, root.to_str().unwrap(), "github.com", &repository, &id] {
                digest.update(part.as_bytes());
                if part != id {
                    digest.update([0]);
                }
            }
            let suffix: String = digest.finalize()[..16]
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect();
            token = temp
                .path()
                .join(format!("agentscm/tokens/client-{suffix}.token"));
            std::fs::create_dir_all(token.parent().unwrap()).unwrap();
            connection["owned_token_id"] = serde_json::json!(id);
            connection["owned_token_expires_at"] = serde_json::json!(expiry);
        }
        if *has_token {
            std::fs::write(&token, &secret).unwrap();
            std::fs::set_permissions(&token, std::fs::Permissions::from_mode(0o600)).unwrap();
        }
        connection["token_file"] = serde_json::json!(token);
        saved.push(connection);
    }
    save(temp.path(), serde_json::Value::Array(saved));

    let output = run(temp.path(), None, true);
    stop.store(true, Ordering::Relaxed);
    for server in servers {
        server.join().unwrap();
    }
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let text = String::from_utf8_lossy(&output.stdout).into_owned()
        + &String::from_utf8_lossy(&output.stderr);
    for token in &tokens {
        assert!(!text.contains(token.as_str()));
    }
    let value: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    let connections = value["connections"].as_array().unwrap();
    assert_eq!(connections.len(), 8);
    for (name, _, _, _, health, build) in cases {
        let connection = connections
            .iter()
            .find(|c| c["repository"] == format!("acme/{name}"))
            .unwrap();
        assert_eq!(connection["health"]["state"], health, "{name}");
        let reported = &connection["hub_check"]["hub_build"];
        if build {
            assert_eq!(reported["version"], "1.2.3", "{name}");
            assert_eq!(
                reported["commit"], "0123456789abcdef0123456789abcdef01234567",
                "{name}"
            );
        } else {
            assert!(reported.is_null(), "{name}");
        }
    }
}
