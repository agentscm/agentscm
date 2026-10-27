#![cfg(unix)]
//! `agentscm serve --front-door` against a synthetic checkout, a synthetic saved Hub session and a
//! fake Hub: one browser connection reads this computer's files and the Hub, the Hub sees only a
//! read-only token the service minted, and nothing outside the page's own origin gets in.

use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::os::unix::fs::PermissionsExt as _;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde_json::{Value, json};
use tempfile::TempDir;

const REPO: &str = "acme/widgets";
const HUMAN_SESSION: &str = "synthetic-human-session";
const MINTED_ID: &str = "tok_0123456789abcdef0123456789abcdef";
const MINTED: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

fn git(dir: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .args(args)
        .current_dir(dir)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap().trim().into()
}

fn checkout(root: &Path) -> String {
    checkout_from(root, "https://github.com/acme/widgets.git")
}

fn checkout_from(root: &Path, origin: &str) -> String {
    fs::create_dir_all(root).unwrap();
    git(root, &["init", "-q", "-b", "main"]);
    git(root, &["remote", "add", "origin", origin]);
    fs::write(root.join("README.md"), "synthetic\n").unwrap();
    git(root, &["add", "README.md"]);
    git(
        root,
        &[
            "-c",
            "user.name=Synthetic",
            "-c",
            "user.email=synthetic@example.invalid",
            "commit",
            "-qm",
            "init",
        ],
    );
    git(root, &["rev-parse", "HEAD"])
}

fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64
}

/// A fake Hub: mints and confirms one read-only token, answers one repository read, records requests.
fn fake_hub() -> (String, Arc<Mutex<Vec<String>>>) {
    fake_hub_for("github", "github.com", "github")
}

/// A fake Hub whose owner is `provider` on `host`; `minted_provider` is what it reports for the
/// minted token (a different value models a token for another forge).
fn fake_hub_for(
    provider: &'static str,
    host: &'static str,
    minted_provider: &'static str,
) -> (String, Arc<Mutex<Vec<String>>>) {
    fake_hub_reporting(provider, host, minted_provider, None)
}

/// Like [`fake_hub_for`]; `retention_days` is what the Hub's `whoami` says it keeps a shared
/// conversation (`None` models an older Hub that does not say).
fn fake_hub_reporting(
    provider: &'static str,
    host: &'static str,
    minted_provider: &'static str,
    retention_days: Option<i64>,
) -> (String, Arc<Mutex<Vec<String>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let seen = Arc::new(Mutex::new(Vec::new()));
    let log = seen.clone();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let request = read_request(&mut stream);
            log.lock().unwrap().push(request.clone());
            let line = request.lines().next().unwrap_or_default().to_owned();
            let expiry = now() + 30 * 60;
            let (status, body) = if line.starts_with("POST /v1/hub/tokens ") {
                (
                    201,
                    json!({"id": MINTED_ID, "token": MINTED, "name": "agentscm-tray", "repository": REPO, "repository_id": 7, "expires_at": expiry, "read_only": true}),
                )
            } else if line.starts_with("GET /v1/hub/whoami ")
                && request.contains("authorization: Bearer synthetic-agent-token")
            {
                // The checkout's own agent token, as the tray's Hub check reads it.
                let mut body = json!({"token": {"id": "tok_agent", "name": "agent", "repository": REPO, "repository_id": 7, "expires_at": expiry, "read_only": false}, "owner": {"provider": provider, "host": host, "user_id": 1, "login": "synthetic"}});
                if let Some(days) = retention_days {
                    body["shared_conversation_retention_days"] = json!(days);
                }
                (200, body)
            } else if line.starts_with("GET /v1/hub/whoami ") {
                (
                    200,
                    json!({"token": {"id": MINTED_ID, "name": "agentscm-tray", "repository": REPO, "repository_id": 7, "expires_at": expiry, "read_only": true}, "owner": {"provider": minted_provider, "host": host, "user_id": 1, "login": "synthetic"}}),
                )
            } else if line.starts_with("GET /v1/hub/repos/acme%2Fwidgets/refused") {
                (401, json!({"error": "unauthorized"}))
            } else if line.starts_with("GET /v1/hub/repos/acme%2Fwidgets/work") {
                (200, json!({"synthetic": "work page"}))
            } else if line.starts_with("DELETE /v1/hub/token ")
                || line.starts_with("DELETE /v1/hub/devices/dev_")
            {
                (204, Value::Null)
            } else if line.starts_with("GET /v1/hub/devices ") {
                (
                    200,
                    json!({"devices": [{"id": DEVICE_ID, "name": "laptop", "current": true}]}),
                )
            } else {
                (404, json!({"error": "not_found"}))
            };
            let body = if status == 204 {
                String::new()
            } else {
                body.to_string()
            };
            let _ = write!(
                stream,
                "HTTP/1.1 {status} Test\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
        }
    });
    (format!("http://{address}"), seen)
}

fn read_request(stream: &mut TcpStream) -> String {
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    let mut request = Vec::new();
    loop {
        let mut chunk = [0_u8; 4096];
        let count = stream.read(&mut chunk).unwrap_or(0);
        if count == 0 {
            break;
        }
        request.extend_from_slice(&chunk[..count]);
        if let Some(end) = request.windows(4).position(|part| part == b"\r\n\r\n") {
            let headers = String::from_utf8_lossy(&request[..end]).to_ascii_lowercase();
            let length = headers
                .lines()
                .find_map(|line| {
                    line.strip_prefix("content-length:")
                        .and_then(|v| v.trim().parse::<usize>().ok())
                })
                .unwrap_or(0);
            if request.len() >= end + 4 + length {
                break;
            }
        }
    }
    String::from_utf8_lossy(&request).into_owned()
}

fn private_file(path: &Path, contents: &str) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, contents).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o600)).unwrap();
}

struct FrontDoor {
    child: Child,
    address: SocketAddr,
    config: std::path::PathBuf,
}

impl FrontDoor {
    fn start(temp: &TempDir, hub: &str, root: &Path) -> Self {
        Self::start_on(temp, hub, root, "github.com")
    }

    fn start_on(temp: &TempDir, hub: &str, root: &Path, origin_host: &str) -> Self {
        let config = temp.path().join("config");
        let token_file = config.join("agentscm/tokens/widgets");
        private_file(&token_file, "synthetic-agent-token\n");
        private_file(
            &config.join("agentscm/connections.json"),
            &json!({"version": 1, "connections": [{
                "root": root, "origin_host": origin_host, "hub": hub, "repo": REPO, "token_file": token_file
            }]})
            .to_string(),
        );
        private_file(
            &config.join("agentscm/hub-sessions.json"),
            &json!({"version": 2, "default_hub": hub, "sessions": [{"hub": hub, "session": HUMAN_SESSION}]}).to_string(),
        );
        let mut child = Command::new(env!("CARGO_BIN_EXE_agentscm"))
            .arg("--no-telemetry")
            .arg("--data-dir")
            .arg(temp.path().join("store"))
            .args(["serve", "--front-door", "--bind", "127.0.0.1:0"])
            .env("XDG_CONFIG_HOME", &config)
            .env_remove("AGENTSCM_SERVE_READ_TOKEN")
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let stderr = child.stderr.take().unwrap();
        let mut lines = BufReader::new(stderr).lines();
        let line = lines.next().unwrap().unwrap();
        std::thread::spawn(move || {
            lines.for_each(|line| eprintln!("serve: {}", line.unwrap_or_default()))
        });
        let address = line
            .strip_prefix("listening on ")
            .unwrap_or_else(|| panic!("{line}"))
            .parse()
            .unwrap();
        Self {
            child,
            address,
            config,
        }
    }

    fn origin(&self) -> String {
        format!("http://{}", self.address)
    }

    fn request(&self, method: &str, path: &str, headers: &[(&str, &str)]) -> (u16, String, String) {
        self.request_with_body(method, path, headers, "")
    }

    fn request_with_body(
        &self,
        method: &str,
        path: &str,
        headers: &[(&str, &str)],
        body: &str,
    ) -> (u16, String, String) {
        let mut stream = TcpStream::connect(self.address).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(15)))
            .unwrap();
        let mut request = format!(
            "{method} {path} HTTP/1.1\r\nHost: {}\r\nConnection: close\r\n",
            self.address
        );
        for (name, value) in headers {
            request.push_str(&format!("{name}: {value}\r\n"));
        }
        request.push_str(&format!("Content-Length: {}\r\n\r\n{body}", body.len()));
        stream.write_all(request.as_bytes()).unwrap();
        let mut response = String::new();
        stream
            .take(16 * 1024 * 1024)
            .read_to_string(&mut response)
            .unwrap();
        let (head, body) = response.split_once("\r\n\r\n").unwrap();
        (
            head[9..12].parse().unwrap(),
            head.to_ascii_lowercase(),
            body.to_owned(),
        )
    }
}

impl Drop for FrontDoor {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[test]
fn one_connection_reads_this_computer_and_the_hub_without_handing_out_a_hub_token() {
    let temp = TempDir::new().unwrap();
    let root = fs::canonicalize(temp.path()).unwrap().join("widgets");
    let commit = checkout(&root);
    let (hub, seen) = fake_hub();
    let door = FrontDoor::start(&temp, &hub, &root);
    let origin = door.origin();

    // The discovery record is owner-only; anonymously the service names only its instance.
    let record_path = door.config.join("agentscm/front-door/service.json");
    assert_eq!(
        fs::metadata(&record_path).unwrap().permissions().mode() & 0o077,
        0
    );
    let record: Value = serde_json::from_str(&fs::read_to_string(&record_path).unwrap()).unwrap();
    assert_eq!(record["base"], origin);
    let (status, _, anonymous) = door.request("GET", "/v1/local/identity", &[]);
    assert_eq!(status, 200);
    let anonymous: Value = serde_json::from_str(&anonymous).unwrap();
    assert_eq!(
        anonymous,
        json!({"version": 1, "instance": record["instance"]})
    );

    // The page itself is served here.
    let (status, head, _) = door.request("GET", "/ui/", &[]);
    assert_eq!(status, 200);
    assert!(head.contains("content-security-policy"));

    // Only the admin secret, from outside a browser, mints a single-use code.
    let admin = format!("Bearer {}", record["admin"].as_str().unwrap());
    assert_eq!(door.request("POST", "/v1/local/pairings", &[]).0, 401);
    assert_eq!(
        door.request(
            "POST",
            "/v1/local/pairings",
            &[("Authorization", &admin), ("Origin", &origin)]
        )
        .0,
        401
    );
    let (status, _, minted) =
        door.request("POST", "/v1/local/pairings", &[("Authorization", &admin)]);
    assert_eq!(status, 201);
    let code = serde_json::from_str::<Value>(&minted).unwrap()["code"]
        .as_str()
        .unwrap()
        .to_owned();
    let pairing = format!("Bearer {code}");

    // Another site, or no page at all, cannot redeem it; the page's own origin can, once.
    assert_eq!(
        door.request(
            "POST",
            "/v1/local/pairings/redeem",
            &[
                ("Authorization", &pairing),
                ("Origin", "https://evil.example")
            ]
        )
        .0,
        403
    );
    assert_eq!(
        door.request(
            "POST",
            "/v1/local/pairings/redeem",
            &[("Authorization", &pairing)]
        )
        .0,
        401
    );
    let (status, head, redeemed) = door.request(
        "POST",
        "/v1/local/pairings/redeem",
        &[("Authorization", &pairing), ("Origin", &origin)],
    );
    assert_eq!(status, 200, "{redeemed}");
    assert!(head.contains("cache-control: no-store"));
    assert!(!head.contains("access-control-allow-origin"));
    let grant = serde_json::from_str::<Value>(&redeemed).unwrap()["grant"]
        .as_str()
        .unwrap()
        .to_owned();
    assert_eq!(
        door.request(
            "POST",
            "/v1/local/pairings/redeem",
            &[("Authorization", &pairing), ("Origin", &origin)]
        )
        .0,
        401
    );

    let credential = format!("Bearer local:{grant}:{REPO}");
    let auth = [("Authorization", credential.as_str())];

    // This computer's files.
    let (status, _, summary) = door.request("GET", "/v1/local/explorer/summary", &auth);
    assert_eq!(status, 200, "{summary}");
    assert_eq!(
        serde_json::from_str::<Value>(&summary).unwrap()["commit"],
        commit
    );
    let (status, _, repositories) = door.request("GET", "/v1/local/repositories", &auth);
    assert_eq!(status, 200);
    assert_eq!(
        serde_json::from_str::<Value>(&repositories).unwrap()["repositories"][0]["repository"],
        REPO
    );

    // The Hub, through a token the service minted from the saved session.
    let (status, _, work) = door.request("GET", "/v1/hub/repos/acme%2Fwidgets/work?limit=5", &auth);
    assert_eq!(status, 200, "{work}");
    assert_eq!(
        serde_json::from_str::<Value>(&work).unwrap()["synthetic"],
        "work page"
    );
    {
        let seen = seen.lock().unwrap();
        let mint = seen
            .iter()
            .find(|r| r.starts_with("POST /v1/hub/tokens "))
            .unwrap();
        assert!(mint.contains(&format!("authorization: Bearer {HUMAN_SESSION}")));
        assert!(mint.contains("\"read_only\":true"));
        assert!(mint.contains("\"name\":\"agentscm-local-page\""));
        let read = seen
            .iter()
            .find(|r| r.starts_with("GET /v1/hub/repos/acme%2Fwidgets/work?limit=5 "))
            .unwrap();
        assert!(read.contains(&format!("authorization: Bearer {MINTED}")));
        assert!(!read.contains(&grant) && !read.contains(HUMAN_SESSION));
    }
    // A second read reuses the cached token.
    assert_eq!(
        door.request("GET", "/v1/hub/repos/acme%2Fwidgets/work", &auth)
            .0,
        200
    );
    assert_eq!(
        seen.lock()
            .unwrap()
            .iter()
            .filter(|r| r.starts_with("POST /v1/hub/tokens "))
            .count(),
        1
    );

    // A Hub refusal of the computer's token mints once more, then reports a Hub problem rather
    // than ending the browser's connection.
    let mints = |seen: &Arc<Mutex<Vec<String>>>| {
        seen.lock()
            .unwrap()
            .iter()
            .filter(|r| r.starts_with("POST /v1/hub/tokens "))
            .count()
    };
    let before = mints(&seen);
    assert_eq!(
        door.request("GET", "/v1/hub/repos/acme%2Fwidgets/refused", &auth)
            .0,
        502
    );
    assert_eq!(mints(&seen), before + 1);

    // Naming the Hub selects the same checkout; naming another Hub selects nothing.
    let on_hub = format!("Bearer local:{grant}:{REPO}@{hub}");
    assert_eq!(
        door.request(
            "GET",
            "/v1/local/explorer/summary",
            &[("Authorization", &on_hub)]
        )
        .0,
        200
    );
    let elsewhere = format!("Bearer local:{grant}:{REPO}@https://other.example");
    assert_eq!(
        door.request(
            "GET",
            "/v1/local/explorer/summary",
            &[("Authorization", &elsewhere)]
        )
        .0,
        409
    );

    // Refusals: no credential, another site, an unconnected repository, writes, credential areas.
    assert_eq!(
        door.request("GET", "/v1/local/explorer/summary", &[]).0,
        401
    );
    assert_eq!(
        door.request(
            "GET",
            "/v1/hub/repos/acme%2Fwidgets/work",
            &[
                ("Authorization", &credential),
                ("Origin", "https://evil.example")
            ]
        )
        .0,
        403
    );
    let other = format!("Bearer local:{grant}:acme/other");
    assert_eq!(
        door.request(
            "GET",
            "/v1/local/explorer/summary",
            &[("Authorization", &other)]
        )
        .0,
        409,
        "an unconnected repository keeps the connection"
    );
    let (status, _, gone) = door.request("GET", "/v1/local/identity", &[("Authorization", &other)]);
    assert_eq!(status, 200);
    assert!(serde_json::from_str::<Value>(&gone).unwrap()["repository"].is_null());
    assert_eq!(
        door.request(
            "GET",
            "/v1/local/repositories",
            &[("Authorization", &other)]
        )
        .0,
        200
    );
    assert_eq!(
        door.request("POST", "/v1/hub/repos/acme%2Fwidgets/work", &auth)
            .0,
        405
    );
    assert_eq!(door.request("GET", "/v1/hub/workspace/me", &auth).0, 404);
    assert_eq!(door.request("GET", "/v1/hub/tokens", &auth).0, 404);
    for path in [
        "/v1/hub/repos/acme/%2e%2e/%2e%2e/tokens",
        "/v1/hub/repos/acme/widgets/../../tokens",
        "/v1/hub/session",
    ] {
        assert_eq!(door.request("GET", path, &auth).0, 404, "{path}");
    }
    assert!(
        !seen
            .lock()
            .unwrap()
            .iter()
            .any(|r| r.starts_with("GET /v1/hub/tokens") || r.starts_with("GET /v1/hub/session")),
        "no refused path reached the Hub"
    );
    let rebound = {
        let mut stream = TcpStream::connect(door.address).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        write!(stream, "GET /v1/local/explorer/summary HTTP/1.1\r\nHost: attacker.example\r\nAuthorization: {credential}\r\nConnection: close\r\n\r\n").unwrap();
        let mut response = String::new();
        stream.read_to_string(&mut response).unwrap();
        response
    };
    assert!(rebound.starts_with("HTTP/1.1 421"), "{rebound}");

    // Disconnecting ends the grant.
    assert_eq!(door.request("DELETE", "/v1/local/session", &auth).0, 204);
    assert_eq!(
        door.request("GET", "/v1/local/explorer/summary", &auth).0,
        401
    );

    // Stopping removes the record and revokes the minted token.
    let pid = door.child.id() as i32;
    // SAFETY: signals the child this test started.
    assert_eq!(unsafe { libc::kill(pid, libc::SIGTERM) }, 0);
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while record_path.exists() && std::time::Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(!record_path.exists(), "the record outlived the service");
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while !seen
        .lock()
        .unwrap()
        .iter()
        .any(|r| r.starts_with("DELETE /v1/hub/token "))
        && std::time::Instant::now() < deadline
    {
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(
        seen.lock()
            .unwrap()
            .iter()
            .any(|r| r.starts_with("DELETE /v1/hub/token ") && r.contains(MINTED))
    );
}

#[test]
fn front_door_refuses_source_flags_and_public_binds() {
    let temp = TempDir::new().unwrap();
    for args in [
        vec!["serve", "--front-door", "--public", "--bind", "0.0.0.0:0"],
        vec![
            "serve",
            "--front-door",
            "--explorer-repo",
            "/tmp",
            "--explorer-repository",
            "acme/widgets",
        ],
    ] {
        let output = Command::new(env!("CARGO_BIN_EXE_agentscm"))
            .arg("--no-telemetry")
            .arg("--data-dir")
            .arg(temp.path().join("store"))
            .args(&args)
            .env("XDG_CONFIG_HOME", temp.path().join("config"))
            .output()
            .unwrap();
        assert!(!output.status.success(), "{args:?}");
        assert!(
            String::from_utf8_lossy(&output.stderr).contains("--front-door"),
            "{args:?}"
        );
        assert!(
            !temp
                .path()
                .join("config/agentscm/front-door/service.json")
                .exists()
        );
    }
}

#[test]
fn open_explains_the_one_missing_step_without_opening_a_browser() {
    let temp = TempDir::new().unwrap();
    let config = temp.path().join("config");
    let run = |dir: &Path, args: &[&str]| {
        let output = Command::new(env!("CARGO_BIN_EXE_agentscm"))
            .arg("--no-telemetry")
            .args(args)
            .current_dir(dir)
            .env("XDG_CONFIG_HOME", &config)
            .output()
            .unwrap();
        (
            output.status.success(),
            format!(
                "{}{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            ),
        )
    };

    let (ok, text) = run(temp.path(), &["open", "--stop"]);
    assert!(ok && text.contains("not running"), "{text}");

    let (ok, text) = run(temp.path(), &["open"]);
    assert!(!ok && text.contains("agentscm client setup"), "{text}");

    // Connected, but this computer has no Hub sign-in: say how to sign in, start nothing.
    let root = fs::canonicalize(temp.path()).unwrap().join("widgets");
    checkout(&root);
    let token_file = config.join("agentscm/tokens/widgets");
    private_file(&token_file, "synthetic-agent-token\n");
    private_file(
        &config.join("agentscm/connections.json"),
        &json!({"version": 1, "connections": [{
            "root": root, "origin_host": "github.com", "hub": "http://127.0.0.1:9", "repo": REPO, "token_file": token_file
        }]})
        .to_string(),
    );
    let (ok, text) = run(&root, &["open"]);
    assert!(
        !ok && text.contains("agentscm hub login --hub http://127.0.0.1:9"),
        "{text}"
    );
    assert!(!config.join("agentscm/front-door/service.json").exists());
}

#[test]
fn one_connection_switches_between_connected_repositories() {
    let temp = TempDir::new().unwrap();
    let base = fs::canonicalize(temp.path()).unwrap();
    let widgets = base.join("widgets");
    let widgets_commit = checkout(&widgets);
    let gadgets = base.join("gadgets");
    fs::create_dir_all(&gadgets).unwrap();
    git(&gadgets, &["init", "-q", "-b", "main"]);
    git(
        &gadgets,
        &[
            "remote",
            "add",
            "origin",
            "https://github.com/acme/gadgets.git",
        ],
    );
    fs::write(gadgets.join("GADGETS.md"), "gadgets\n").unwrap();
    git(&gadgets, &["add", "GADGETS.md"]);
    git(
        &gadgets,
        &[
            "-c",
            "user.name=Synthetic",
            "-c",
            "user.email=synthetic@example.invalid",
            "commit",
            "-qm",
            "gadgets",
        ],
    );
    let gadgets_commit = git(&gadgets, &["rev-parse", "HEAD"]);
    let (hub, _) = fake_hub();
    let door = FrontDoor::start(&temp, &hub, &widgets);
    let record: Value = serde_json::from_str(
        &fs::read_to_string(door.config.join("agentscm/front-door/service.json")).unwrap(),
    )
    .unwrap();
    let admin = format!("Bearer {}", record["admin"].as_str().unwrap());
    let (_, _, minted) = door.request("POST", "/v1/local/pairings", &[("Authorization", &admin)]);
    let code = serde_json::from_str::<Value>(&minted).unwrap()["code"]
        .as_str()
        .unwrap()
        .to_owned();
    let (_, _, redeemed) = door.request(
        "POST",
        "/v1/local/pairings/redeem",
        &[
            ("Authorization", &format!("Bearer {code}")),
            ("Origin", &door.origin()),
        ],
    );
    let grant = serde_json::from_str::<Value>(&redeemed).unwrap()["grant"]
        .as_str()
        .unwrap()
        .to_owned();

    // Read once so the service holds the list of connections from before the next checkout.
    assert_eq!(
        door.request(
            "GET",
            "/v1/local/explorer/summary",
            &[("Authorization", &format!("Bearer local:{grant}:{REPO}"))]
        )
        .0,
        200
    );
    // Connect a second checkout now; it is found at once, not after the list's refresh.
    let token_file = door.config.join("agentscm/tokens/gadgets");
    private_file(&token_file, "synthetic-agent-token\n");
    let connections = door.config.join("agentscm/connections.json");
    let mut file: Value = serde_json::from_str(&fs::read_to_string(&connections).unwrap()).unwrap();
    file["connections"].as_array_mut().unwrap().push(json!({
        "root": gadgets, "origin_host": "github.com", "hub": hub, "repo": "acme/gadgets", "token_file": token_file
    }));
    private_file(&connections, &file.to_string());

    let (status, _, fresh) = door.request(
        "GET",
        "/v1/local/explorer/summary",
        &[(
            "Authorization",
            &format!("Bearer local:{grant}:acme/gadgets"),
        )],
    );
    assert_eq!(
        status, 200,
        "a just-connected checkout is found at once: {fresh}"
    );
    let (_, _, listed) = door.request(
        "GET",
        "/v1/local/repositories",
        &[("Authorization", &format!("Bearer local:{grant}:{REPO}"))],
    );
    let listed: Value = serde_json::from_str(&listed).unwrap();
    let names: Vec<_> = listed["repositories"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["repository"].as_str().unwrap().to_owned())
        .collect();
    assert_eq!(names.len(), 2);
    assert!(names.contains(&"acme/gadgets".to_owned()) && names.contains(&REPO.to_owned()));
    for (repository, commit) in [(REPO, &widgets_commit), ("acme/gadgets", &gadgets_commit)] {
        let (status, _, summary) = door.request(
            "GET",
            "/v1/local/explorer/summary",
            &[(
                "Authorization",
                &format!("Bearer local:{grant}:{repository}"),
            )],
        );
        assert_eq!(status, 200, "{summary}");
        let summary: Value = serde_json::from_str(&summary).unwrap();
        assert_eq!(
            (
                summary["repository_id"].as_str().unwrap(),
                &summary["commit"]
            ),
            (repository, &json!(commit))
        );
    }
}

#[test]
fn open_starts_the_page_hands_the_code_over_a_private_file_and_stops_it() {
    let temp = TempDir::new().unwrap();
    let config = temp.path().join("config");
    let root = fs::canonicalize(temp.path()).unwrap().join("widgets");
    checkout(&root);
    let (hub, _) = fake_hub();
    let token_file = config.join("agentscm/tokens/widgets");
    private_file(&token_file, "synthetic-agent-token\n");
    private_file(
        &config.join("agentscm/connections.json"),
        &json!({"version": 1, "connections": [{
            "root": root, "origin_host": "github.com", "hub": hub, "repo": REPO, "token_file": token_file
        }]})
        .to_string(),
    );
    private_file(
        &config.join("agentscm/hub-sessions.json"),
        &json!({"version": 2, "default_hub": hub, "sessions": [{"hub": hub, "session": HUMAN_SESSION}]}).to_string(),
    );
    // Stand-ins for the browser launchers record their arguments instead of opening anything.
    let bin = temp.path().join("bin");
    fs::create_dir_all(&bin).unwrap();
    let seen = temp.path().join("launched");
    for name in ["open", "xdg-open"] {
        let script = bin.join(name);
        fs::write(
            &script,
            format!("#!/bin/sh\nprintf '%s\\n' \"$@\" >> '{}'\n", seen.display()),
        )
        .unwrap();
        fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();
    }
    let path = format!(
        "{}:{}",
        bin.display(),
        std::env::var("PATH").unwrap_or_default()
    );
    let run = |args: &[&str]| {
        let output = Command::new(env!("CARGO_BIN_EXE_agentscm"))
            .arg("--no-telemetry")
            .args(args)
            .current_dir(&root)
            .env("XDG_CONFIG_HOME", &config)
            .env("PATH", &path)
            .env("AGENTSCM_SERVE_READ_TOKEN", "short")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout).into_owned()
    };

    let printed = run(&["open"]);
    assert!(printed.contains("Opened acme/widgets"), "{printed}");
    // The launcher runs in the background; give it a moment to record its arguments.
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while !seen.exists() && std::time::Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(50));
    }
    std::thread::sleep(Duration::from_millis(100));
    let launched = fs::read_to_string(&seen).unwrap();
    let handoff = launched.lines().next().unwrap_or_default().trim();
    assert!(
        handoff.ends_with(".html") && !handoff.contains("pair="),
        "only a file path is passed: {launched}"
    );
    assert_eq!(
        fs::metadata(handoff).unwrap().permissions().mode() & 0o077,
        0
    );
    let page = fs::read_to_string(handoff).unwrap();
    let record_path = config.join("agentscm/front-door/service.json");
    let record: Value = serde_json::from_str(&fs::read_to_string(&record_path).unwrap()).unwrap();
    let base = record["base"].as_str().unwrap();
    assert!(page.contains(&format!("{base}/ui/#pair=")), "{page}");
    assert!(page.contains(&format!("repo=acme/widgets@{hub}")), "{page}");

    // A second `open` reuses the running page.
    run(&["open"]);
    let again: Value = serde_json::from_str(&fs::read_to_string(&record_path).unwrap()).unwrap();
    assert_eq!(again["instance"], record["instance"]);

    let stopped = run(&["open", "--stop"]);
    assert!(stopped.contains("Stopped"), "{stopped}");
    assert!(!record_path.exists(), "the record outlived the page");
}

/// Pair a browser with `door` and return its grant.
fn connect(door: &FrontDoor) -> String {
    let record: Value = serde_json::from_str(
        &fs::read_to_string(door.config.join("agentscm/front-door/service.json")).unwrap(),
    )
    .unwrap();
    let admin = format!("Bearer {}", record["admin"].as_str().unwrap());
    let (_, _, minted) = door.request("POST", "/v1/local/pairings", &[("Authorization", &admin)]);
    let code = serde_json::from_str::<Value>(&minted).unwrap()["code"]
        .as_str()
        .unwrap()
        .to_owned();
    let (_, _, redeemed) = door.request(
        "POST",
        "/v1/local/pairings/redeem",
        &[
            ("Authorization", &format!("Bearer {code}")),
            ("Origin", &door.origin()),
        ],
    );
    serde_json::from_str::<Value>(&redeemed).unwrap()["grant"]
        .as_str()
        .unwrap()
        .to_owned()
}

#[test]
fn a_self_hosted_forge_gets_a_token_only_for_the_provider_its_hub_vouches_for() {
    for (minted_provider, expected) in [("gitea", 200), ("github", 502)] {
        let temp = TempDir::new().unwrap();
        let root = fs::canonicalize(temp.path()).unwrap().join("widgets");
        checkout_from(&root, "https://git.example.com/acme/widgets.git");
        let (hub, _) = fake_hub_for("gitea", "git.example.com", minted_provider);
        let door = FrontDoor::start_on(&temp, &hub, &root, "git.example.com");
        let grant = connect(&door);
        let (status, _, body) = door.request(
            "GET",
            "/v1/hub/repos/acme%2Fwidgets/work",
            &[("Authorization", &format!("Bearer local:{grant}:{REPO}"))],
        );
        assert_eq!(status, expected, "minted for {minted_provider}: {body}");
    }
}

#[test]
fn signing_this_computer_out_of_its_hub_revokes_the_page_token_on_next_use() {
    let temp = TempDir::new().unwrap();
    let root = fs::canonicalize(temp.path()).unwrap().join("widgets");
    checkout(&root);
    let (hub, seen) = fake_hub();
    let door = FrontDoor::start(&temp, &hub, &root);
    let grant = connect(&door);
    let auth = format!("Bearer local:{grant}:{REPO}");
    assert_eq!(
        door.request(
            "GET",
            "/v1/hub/repos/acme%2Fwidgets/work",
            &[("Authorization", &auth)]
        )
        .0,
        200
    );

    fs::remove_file(door.config.join("agentscm/hub-sessions.json")).unwrap();
    let (status, _, body) = door.request(
        "GET",
        "/v1/hub/repos/acme%2Fwidgets/work",
        &[("Authorization", &auth)],
    );
    assert_eq!(status, 503, "{body}");
    assert!(body.contains("hub_sign_in_required"), "{body}");
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while !seen
        .lock()
        .unwrap()
        .iter()
        .any(|r| r.starts_with("DELETE /v1/hub/token ") && r.contains(MINTED))
        && std::time::Instant::now() < deadline
    {
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(
        seen.lock()
            .unwrap()
            .iter()
            .any(|r| r.starts_with("DELETE /v1/hub/token ") && r.contains(MINTED)),
        "the token from the removed sign-in is revoked"
    );
    // The browser's connection to this computer is not what ended.
    assert_eq!(
        door.request(
            "GET",
            "/v1/local/explorer/summary",
            &[("Authorization", &auth)]
        )
        .0,
        200
    );
}

fn capture_config(door: &FrontDoor) -> Option<Value> {
    fs::read_to_string(door.config.join("agentscm/change-capture.json"))
        .ok()
        .map(|text| serde_json::from_str(&text).unwrap())
}

#[test]
fn local_settings_turn_sharing_on_only_with_confirmation_and_set_up_the_checkout_in_one_step() {
    let temp = TempDir::new().unwrap();
    let root = fs::canonicalize(temp.path()).unwrap().join("widgets");
    checkout(&root);
    let (hub, seen) = fake_hub();
    let door = FrontDoor::start(&temp, &hub, &root);
    let grant = connect(&door);
    let credential = format!("Bearer local:{grant}:{REPO}");
    let origin = door.origin();
    let auth = [
        ("Authorization", credential.as_str()),
        ("Origin", origin.as_str()),
    ];
    let post = |headers: &[(&str, &str)], body: Value| {
        door.request_with_body("POST", "/v1/local/capture", headers, &body.to_string())
    };
    let (_, _, listing) = door.request("GET", "/v1/local/capture", &auth);
    let id = serde_json::from_str::<Value>(&listing).unwrap()["checkouts"][0]["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let share = |confirm: Option<bool>| {
        let mut body =
            json!({"id": id, "repository": REPO, "hub": hub, "mode": "shared_conversation"});
        if let Some(confirm) = confirm {
            body["confirm_sharing"] = json!(confirm);
        }
        body
    };

    // Nothing is configured, and listing it does not configure it.
    let (status, head, listed) = door.request("GET", "/v1/local/capture", &auth);
    assert_eq!(status, 200, "{listed}");
    assert!(!head.contains("access-control-allow"));
    let listed: Value = serde_json::from_str(&listed).unwrap();
    assert_eq!(listed["checkouts"][0]["repository"], REPO);
    assert_eq!(listed["checkouts"][0]["id"], json!(id));
    assert!(!listing.contains(root.to_str().unwrap()));
    assert_eq!(listed["checkouts"][0]["configured"], false);
    assert_eq!(listed["checkouts"][0]["mode"], Value::Null);
    assert!(capture_config(&door).is_none());

    // Another site, a missing or unknown grant, and a missing or false confirmation change nothing.
    let foreign = [
        ("Authorization", credential.as_str()),
        ("Origin", "https://evil.example"),
    ];
    assert_eq!(post(&foreign, share(Some(true))).0, 403);
    assert_eq!(
        post(&[("Origin", origin.as_str())], share(Some(true))).0,
        401
    );
    let stale = format!("Bearer local:{}:{REPO}", "0".repeat(grant.len()));
    assert_eq!(
        post(
            &[
                ("Authorization", stale.as_str()),
                ("Origin", origin.as_str())
            ],
            share(Some(true))
        )
        .0,
        401
    );
    let (status, _, refused) = post(&auth, share(None));
    assert_eq!(status, 400, "{refused}");
    assert!(refused.contains("confirmation_required"));
    assert_eq!(post(&auth, share(Some(false))).0, 400);
    // The wrong shape, an unlisted checkout, a mode the page does not set and a large body are refused.
    assert_eq!(post(&auth, json!({"checkout": "/etc"})).0, 400);
    assert_eq!(post(&auth, json!({"id": root, "repository": REPO, "hub": hub, "mode": "shared_conversation", "confirm_sharing": true})).0, 404);
    assert_eq!(
        post(
            &auth,
            json!({"id": id, "repository": REPO, "hub": hub, "mode": "local_conversation"})
        )
        .0,
        400
    );
    let unknown = json!({"id": "0".repeat(24), "repository": REPO, "hub": hub, "mode": "shared_conversation", "confirm_sharing": true});
    assert_eq!(post(&auth, unknown).0, 404);
    let large = json!({"id": id, "repository": "x".repeat(4096), "hub": hub, "mode": "shared_conversation", "confirm_sharing": true});
    assert_eq!(post(&auth, large).0, 413);
    // A page that listed another Hub or repository for this checkout cannot turn sharing on.
    let other_hub = json!({"id": id, "repository": REPO, "hub": "http://127.0.0.1:9", "mode": "shared_conversation", "confirm_sharing": true});
    let (status, _, refused) = post(&auth, other_hub);
    assert_eq!(status, 409, "{refused}");
    assert!(refused.contains("now connected to"), "{refused}");
    // Switching a checkout with no binding to metadata-only has nothing to change.
    let off = json!({"id": id, "repository": REPO, "hub": hub, "mode": "metadata_only"});
    assert_eq!(post(&auth, off.clone()).0, 409);
    assert!(capture_config(&door).is_none());
    assert!(!root.join(".claude/settings.local.json").exists());

    // Confirmed: one request creates the binding with the documented defaults and turns sharing on.
    let (status, _, enabled) = post(&auth, share(Some(true)));
    assert_eq!(status, 200, "{enabled}");
    let enabled: Value = serde_json::from_str(&enabled).unwrap();
    assert_eq!(enabled["checkout"]["mode"], "shared_conversation");
    let config = capture_config(&door).unwrap();
    assert_eq!(config["version"], 6);
    let binding = config["bindings"][0].clone();
    assert_eq!(binding["root"], json!(root));
    assert_eq!(binding["capture_mode"], "shared_conversation");
    assert_eq!(binding["base_ref"], "origin/main");
    assert_eq!(binding["hub"], json!(hub));
    assert!(
        binding["worktree_id"]
            .as_str()
            .unwrap()
            .starts_with("checkout_")
    );
    assert_eq!(
        binding["action_rules"],
        json!([{"native_tool": "Bash", "kind": "execution", "coverage": "partial", "result_source": "process_exit_code", "recipe_digest": null}])
    );
    let spool = Path::new(binding["spool"].as_str().unwrap());
    assert!(
        spool.starts_with(
            door.config
                .join("agentscm/capture-spools")
                .canonicalize()
                .unwrap()
        )
    );
    assert_eq!(
        fs::metadata(spool).unwrap().permissions().mode() & 0o777,
        0o700
    );
    assert!(
        fs::read_to_string(root.join(".claude/settings.local.json"))
            .unwrap()
            .contains("AGENTSCM_CAPTURE_HOOK_V1")
    );

    // The same binding is what `change-capture configure` builds from those values.
    let run = |args: &[&str]| {
        let output = Command::new(env!("CARGO_BIN_EXE_agentscm"))
            .args(args)
            .env_clear()
            .env("XDG_CONFIG_HOME", &door.config)
            .env("HOME", temp.path())
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    };
    let root_arg = root.to_str().unwrap();
    run(&["change-capture", "unconfigure", "--root", root_arg]);
    run(&[
        "change-capture",
        "configure",
        "--root",
        root_arg,
        "--spool",
        spool.to_str().unwrap(),
        "--worktree-id",
        binding["worktree_id"].as_str().unwrap(),
        "--setup-digest",
        binding["setup_digest"].as_str().unwrap(),
        "--capture-mode",
        "shared-conversation",
        "--action-rule",
        "Bash=execution:partial:process_exit_code",
    ]);
    assert_eq!(capture_config(&door).unwrap()["bindings"][0], binding);

    // Off needs no confirmation and keeps the binding.
    let (status, _, disabled) = post(&auth, off);
    assert_eq!(status, 200, "{disabled}");
    assert_eq!(
        serde_json::from_str::<Value>(&disabled).unwrap()["checkout"]["mode"],
        "metadata_only"
    );
    // A disabled binding comes back through the same path with its saved spool and worktree.
    run(&["change-capture", "unconfigure", "--root", root_arg]);
    let (_, _, listed) = door.request("GET", "/v1/local/capture", &auth);
    let listed: Value = serde_json::from_str(&listed).unwrap();
    assert_eq!(listed["checkouts"][0]["configured"], true);
    assert_eq!(listed["checkouts"][0]["enabled"], false);
    assert_eq!(post(&auth, share(Some(true))).0, 200);
    let again = capture_config(&door).unwrap()["bindings"][0].clone();
    assert_eq!(again["spool"], binding["spool"]);
    assert_eq!(again["worktree_id"], binding["worktree_id"]);
    assert_eq!(again["setup_digest"], binding["setup_digest"]);
    assert_eq!(again["enabled"], true);

    // Setting this up never talked to the Hub about conversations.
    assert!(
        seen.lock()
            .unwrap()
            .iter()
            .all(|request| !request.contains("change-record") && !request.contains("conversation"))
    );
}

/// A paired browser page: lists and changes conversation sharing as the local Settings page does.
struct Page<'a> {
    door: &'a FrontDoor,
    credential: String,
    origin: String,
}

impl<'a> Page<'a> {
    fn new(door: &'a FrontDoor) -> Self {
        Self {
            door,
            credential: format!("Bearer local:{}:{REPO}", connect(door)),
            origin: door.origin(),
        }
    }

    fn headers(&self) -> [(&str, &str); 2] {
        [
            ("Authorization", self.credential.as_str()),
            ("Origin", self.origin.as_str()),
        ]
    }

    fn list(&self) -> Vec<Value> {
        let (status, _, body) = self
            .door
            .request("GET", "/v1/local/capture", &self.headers());
        assert_eq!(status, 200, "{body}");
        serde_json::from_str::<Value>(&body).unwrap()["checkouts"]
            .as_array()
            .unwrap()
            .clone()
    }

    fn id_of(&self, folder: &str) -> String {
        self.list()
            .iter()
            .find(|checkout| checkout["folder"] == folder)
            .unwrap()["id"]
            .as_str()
            .unwrap()
            .to_owned()
    }

    fn post(&self, id: &str, hub: &str, mode: &str) -> (u16, String) {
        let body = json!({"id": id, "repository": REPO, "hub": hub, "mode": mode, "confirm_sharing": mode == "shared_conversation"});
        let (status, _, reply) = self.door.request_with_body(
            "POST",
            "/v1/local/capture",
            &self.headers(),
            &body.to_string(),
        );
        (status, reply)
    }
}

/// Runs the real CLI against the door's config.
fn agentscm(door: &FrontDoor, home: &Path, args: &[&str]) -> (bool, String) {
    let output = Command::new(env!("CARGO_BIN_EXE_agentscm"))
        .args(args)
        .env_clear()
        .env("XDG_CONFIG_HOME", &door.config)
        .env("HOME", home)
        .output()
        .unwrap();
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    (output.status.success(), text)
}

fn connect_checkout(door: &FrontDoor, root: &Path, hub: &str, name: &str) {
    let token_file = door.config.join("agentscm/tokens").join(name);
    private_file(&token_file, "synthetic-agent-token\n");
    let connections = door.config.join("agentscm/connections.json");
    let mut file: Value = serde_json::from_str(&fs::read_to_string(&connections).unwrap()).unwrap();
    file["connections"].as_array_mut().unwrap().push(json!({
        "root": root, "origin_host": "github.com", "hub": hub, "repo": REPO, "token_file": token_file
    }));
    private_file(&connections, &file.to_string());
}

fn set_hub(door: &FrontDoor, hub: &str) {
    let connections = door.config.join("agentscm/connections.json");
    let mut file: Value = serde_json::from_str(&fs::read_to_string(&connections).unwrap()).unwrap();
    for connection in file["connections"].as_array_mut().unwrap() {
        connection["hub"] = json!(hub);
    }
    private_file(&connections, &file.to_string());
}

fn bindings(door: &FrontDoor) -> Vec<Value> {
    capture_config(door).map_or_else(Vec::new, |config| {
        config["bindings"].as_array().unwrap().clone()
    })
}

fn is_hex(value: &str, length: usize) -> bool {
    value.len() == length
        && value
            .bytes()
            .all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

#[test]
fn two_checkouts_of_one_repository_and_hub_are_listed_and_controlled_separately() {
    use sha2::{Digest, Sha256};

    let temp = TempDir::new().unwrap();
    let base = fs::canonicalize(temp.path()).unwrap();
    let (first, second) = (base.join("widgets"), base.join("widgets-two"));
    checkout(&first);
    checkout(&second);
    let (hub, _) = fake_hub();
    let door = FrontDoor::start(&temp, &hub, &first);
    connect_checkout(&door, &second, &hub, "two");
    let page = Page::new(&door);

    let listed = page.list();
    assert_eq!(listed.len(), 2);
    let ids: Vec<_> = listed.iter().map(|c| c["id"].as_str().unwrap()).collect();
    assert_ne!(ids[0], ids[1]);
    assert!(ids.iter().all(|id| is_hex(id, 24)));
    assert!(
        !serde_json::to_string(&listed)
            .unwrap()
            .contains(base.to_str().unwrap())
    );
    let (first_id, second_id) = (page.id_of("widgets"), page.id_of("widgets-two"));

    // Sharing the second checkout leaves the first alone.
    assert_eq!(page.post(&second_id, &hub, "shared_conversation").0, 200);
    let after = page.list();
    let mode = |folder: &str| after.iter().find(|c| c["folder"] == folder).unwrap()["mode"].clone();
    assert_eq!(mode("widgets-two"), "shared_conversation");
    assert_eq!(mode("widgets"), Value::Null);
    assert_eq!(bindings(&door).len(), 1);

    // Each checkout's saved ids are random: they differ between checkouts, are not a hash of the
    // path, and a second enable of the same checkout keeps them.
    assert_eq!(page.post(&first_id, &hub, "shared_conversation").0, 200);
    let saved = bindings(&door);
    assert_eq!(saved.len(), 2);
    for binding in &saved {
        let worktree = binding["worktree_id"].as_str().unwrap();
        assert!(
            is_hex(worktree.strip_prefix("checkout_").unwrap(), 32),
            "{worktree}"
        );
        assert!(is_hex(binding["setup_digest"].as_str().unwrap(), 64));
        let root = binding["root"].as_str().unwrap();
        let old = |label: &str| {
            format!(
                "{:x}",
                Sha256::new()
                    .chain_update(label.as_bytes())
                    .chain_update([0])
                    .chain_update(root.as_bytes())
                    .finalize()
            )
        };
        assert_ne!(
            worktree,
            format!("checkout_{}", &old("agentscm-capture-worktree-v1")[..32])
        );
        assert_ne!(
            binding["setup_digest"],
            json!(old("agentscm-one-click-setup-v1"))
        );
    }
    for field in ["worktree_id", "setup_digest", "spool"] {
        assert_ne!(saved[0][field], saved[1][field], "{field}");
    }

    assert_eq!(page.post(&first_id, &hub, "metadata_only").0, 200);
    let after = page.list();
    assert_eq!(
        after.iter().find(|c| c["folder"] == "widgets").unwrap()["mode"],
        "metadata_only"
    );
    assert_eq!(
        after.iter().find(|c| c["folder"] == "widgets-two").unwrap()["mode"],
        "shared_conversation"
    );
    let before = bindings(&door);
    assert_eq!(page.post(&first_id, &hub, "shared_conversation").0, 200);
    let again = bindings(&door);
    for field in ["worktree_id", "setup_digest", "spool"] {
        assert_eq!(
            before.iter().find(|b| b["root"] == json!(first)).unwrap()[field],
            again.iter().find(|b| b["root"] == json!(first)).unwrap()[field]
        );
    }
}

#[test]
fn sharing_is_refused_for_a_hub_other_than_the_one_the_person_saw_and_changes_nothing() {
    let temp = TempDir::new().unwrap();
    let root = fs::canonicalize(temp.path()).unwrap().join("widgets");
    checkout(&root);
    let (hub, _) = fake_hub();
    let door = FrontDoor::start(&temp, &hub, &root);
    let page = Page::new(&door);
    let id = page.id_of("widgets");

    // No binding yet: the checkout was reconnected after the page listed it.
    set_hub(&door, "http://127.0.0.1:9");
    let (status, reply) = page.post(&id, &hub, "shared_conversation");
    assert_eq!(status, 409, "{reply}");
    assert!(
        reply.contains("now connected to another repository or Hub"),
        "{reply}"
    );
    assert!(reply.contains("refresh and confirm again"));
    assert!(capture_config(&door).is_none());
    assert!(!root.join(".claude").exists());

    // An enabled binding that is only switched from metadata-only to shared is checked too.
    set_hub(&door, &hub);
    assert_eq!(page.post(&id, &hub, "shared_conversation").0, 200);
    assert_eq!(page.post(&id, &hub, "metadata_only").0, 200);
    let before = capture_config(&door).unwrap();
    set_hub(&door, "http://127.0.0.1:9");
    let (status, reply) = page.post(&id, &hub, "shared_conversation");
    assert_eq!(status, 409, "{reply}");
    assert!(reply.contains("now connected to"), "{reply}");
    assert_eq!(capture_config(&door).unwrap(), before);
}

#[test]
fn the_default_base_ref_follows_origin_head_when_it_resolves() {
    let temp = TempDir::new().unwrap();
    let base = fs::canonicalize(temp.path()).unwrap();
    let (develop, dangling) = (base.join("widgets"), base.join("widgets-two"));
    checkout(&develop);
    git(
        &develop,
        &["update-ref", "refs/remotes/origin/develop", "HEAD"],
    );
    git(
        &develop,
        &[
            "symbolic-ref",
            "refs/remotes/origin/HEAD",
            "refs/remotes/origin/develop",
        ],
    );
    checkout(&dangling);
    git(
        &dangling,
        &[
            "symbolic-ref",
            "refs/remotes/origin/HEAD",
            "refs/remotes/origin/gone",
        ],
    );
    let (hub, _) = fake_hub();
    let door = FrontDoor::start(&temp, &hub, &develop);
    connect_checkout(&door, &dangling, &hub, "two");
    let page = Page::new(&door);
    for folder in ["widgets", "widgets-two"] {
        assert_eq!(
            page.post(&page.id_of(folder), &hub, "shared_conversation")
                .0,
            200
        );
    }
    let saved = bindings(&door);
    let base_ref =
        |root: &Path| saved.iter().find(|b| b["root"] == json!(root)).unwrap()["base_ref"].clone();
    assert_eq!(base_ref(&develop), "origin/develop");
    assert_eq!(base_ref(&dangling), "origin/main");
}

#[test]
fn setup_hides_the_settings_file_it_creates_and_leaves_an_ignored_or_existing_one_alone() {
    let temp = TempDir::new().unwrap();
    let base = fs::canonicalize(temp.path()).unwrap();
    let (fresh, ignored, existing) = (
        base.join("widgets"),
        base.join("widgets-ignored"),
        base.join("widgets-existing"),
    );
    for root in [&fresh, &ignored, &existing] {
        checkout(root);
    }
    fs::write(ignored.join(".gitignore"), ".claude/\n").unwrap();
    git(&ignored, &["add", ".gitignore"]);
    fs::create_dir_all(existing.join(".claude")).unwrap();
    fs::write(existing.join(".claude/settings.local.json"), "{}\n").unwrap();
    let (hub, _) = fake_hub();
    let door = FrontDoor::start(&temp, &hub, &fresh);
    connect_checkout(&door, &ignored, &hub, "two");
    connect_checkout(&door, &existing, &hub, "three");
    let page = Page::new(&door);
    let exclude = |root: &Path| fs::read_to_string(root.join(".git/info/exclude")).unwrap();
    let before_ignored = exclude(&ignored);
    let before_existing = exclude(&existing);
    let gitignore = fs::read_to_string(ignored.join(".gitignore")).unwrap();
    for folder in ["widgets", "widgets-ignored", "widgets-existing"] {
        assert_eq!(
            page.post(&page.id_of(folder), &hub, "shared_conversation")
                .0,
            200
        );
    }
    assert!(
        exclude(&fresh)
            .lines()
            .any(|line| line == "/.claude/settings.local.json")
    );
    assert_eq!(
        git(&fresh, &["status", "--porcelain", "-uall"]),
        "",
        "{:?}",
        fs::read_dir(fresh.join(".claude"))
            .unwrap()
            .collect::<Vec<_>>()
    );
    assert_eq!(exclude(&ignored), before_ignored);
    assert_eq!(
        fs::read_to_string(ignored.join(".gitignore")).unwrap(),
        gitignore
    );
    assert_eq!(git(&ignored, &["status", "--porcelain"]), "A  .gitignore");
    // A settings file that was already there is the user's to manage.
    assert_eq!(exclude(&existing), before_existing);
    assert!(
        fs::read_to_string(existing.join(".claude/settings.local.json"))
            .unwrap()
            .contains("AGENTSCM_CAPTURE_HOOK_V1")
    );
}

#[test]
fn a_recorded_claude_settings_path_is_reused_when_capture_is_turned_back_on() {
    let temp = TempDir::new().unwrap();
    let base = fs::canonicalize(temp.path()).unwrap();
    let (custom, plain) = (base.join("widgets"), base.join("widgets-plain"));
    checkout(&custom);
    checkout(&plain);
    let (hub, _) = fake_hub();
    let door = FrontDoor::start(&temp, &hub, &custom);
    connect_checkout(&door, &plain, &hub, "two");
    let page = Page::new(&door);
    let configure = |root: &Path, settings: Option<&Path>, mode: &str| {
        let spool = base.join(format!(
            "spool-{}",
            root.file_name().unwrap().to_string_lossy()
        ));
        let mut args = vec![
            "change-capture".to_owned(),
            "configure".into(),
            "--root".into(),
            root.to_str().unwrap().into(),
            "--spool".into(),
            spool.to_str().unwrap().into(),
            "--worktree-id".into(),
            "checkout_synthetic".into(),
            "--setup-digest".into(),
            "a".repeat(64),
            "--capture-mode".into(),
            mode.into(),
            "--action-rule".into(),
            "Bash=execution:partial:process_exit_code".into(),
        ];
        if let Some(settings) = settings {
            args.push("--claude-settings".into());
            args.push(settings.to_str().unwrap().into());
        }
        let args: Vec<_> = args.iter().map(String::as_str).collect();
        let (ok, text) = agentscm(&door, temp.path(), &args);
        assert!(ok, "{text}");
    };
    let marker = |path: &Path| {
        fs::read_to_string(path)
            .unwrap()
            .matches("AGENTSCM_CAPTURE_HOOK_V1")
            .count()
    };

    // A default-path, metadata-only binding keeps the config readable by older binaries.
    configure(&plain, None, "metadata-only");
    let config = capture_config(&door).unwrap();
    assert_eq!(config["version"], 5);
    assert!(config["bindings"][0].get("claude_settings").is_none());

    let elsewhere = custom.join("tools/claude-settings.json");
    configure(&custom, Some(&elsewhere), "metadata-only");
    let config = capture_config(&door).unwrap();
    assert_eq!(config["version"], 6);
    let recorded = bindings(&door)
        .into_iter()
        .find(|b| b["root"] == json!(custom))
        .unwrap();
    assert_eq!(recorded["claude_settings"], json!(elsewhere));
    let installed = marker(&elsewhere);
    assert!(installed > 0);

    // Turning capture off removes the hooks from the recorded file without being told where it is.
    let (ok, text) = agentscm(
        &door,
        temp.path(),
        &[
            "change-capture",
            "unconfigure",
            "--root",
            custom.to_str().unwrap(),
        ],
    );
    assert!(ok, "{text}");
    assert_eq!(marker(&elsewhere), 0);

    // Turning sharing on again installs the hooks once, into the same file.
    assert_eq!(
        page.post(&page.id_of("widgets"), &hub, "shared_conversation")
            .0,
        200
    );
    assert_eq!(marker(&elsewhere), installed);
    assert!(!custom.join(".claude/settings.local.json").exists());
    let again = bindings(&door)
        .into_iter()
        .find(|b| b["root"] == json!(custom))
        .unwrap();
    assert_eq!(again["claude_settings"], json!(elsewhere));
}

#[test]
fn a_disabled_capture_for_another_hub_is_explained_and_forgotten_without_deleting_its_spool() {
    let temp = TempDir::new().unwrap();
    let root = fs::canonicalize(temp.path()).unwrap().join("widgets");
    checkout(&root);
    let (hub, _) = fake_hub();
    let door = FrontDoor::start(&temp, &hub, &root);
    let page = Page::new(&door);
    let id = page.id_of("widgets");
    let root_arg = root.to_str().unwrap();
    assert_eq!(page.post(&id, &hub, "shared_conversation").0, 200);
    let old = bindings(&door).remove(0);
    let spool = Path::new(old["spool"].as_str().unwrap()).to_owned();

    // An enabled binding cannot be forgotten.
    let (ok, text) = agentscm(
        &door,
        temp.path(),
        &["change-capture", "forget", "--root", root_arg],
    );
    assert!(!ok && text.contains("unconfigure"), "{text}");
    assert_eq!(bindings(&door).len(), 1);

    // Disabled, then the checkout moves to another Hub: sharing there is refused with the remedy.
    let (ok, text) = agentscm(
        &door,
        temp.path(),
        &["change-capture", "unconfigure", "--root", root_arg],
    );
    assert!(ok, "{text}");
    let other = "http://127.0.0.1:9";
    set_hub(&door, other);
    let (status, reply) = page.post(&id, other, "shared_conversation");
    assert_eq!(status, 409, "{reply}");
    assert!(reply.contains("change-capture forget"), "{reply}");
    assert!(!reply.contains("namespace safety lineage"), "{reply}");
    assert!(!reply.contains(root_arg), "{reply}");
    let (ok, text) = agentscm(
        &door,
        temp.path(),
        &[
            "change-capture",
            "configure",
            "--root",
            root_arg,
            "--spool",
            spool.to_str().unwrap(),
            "--worktree-id",
            old["worktree_id"].as_str().unwrap(),
            "--setup-digest",
            old["setup_digest"].as_str().unwrap(),
            "--action-rule",
            "Bash=execution:partial:process_exit_code",
        ],
    );
    assert!(!ok && text.contains("change-capture forget"), "{text}");

    // Forget removes only the binding and says where the spool is.
    let (ok, text) = agentscm(
        &door,
        temp.path(),
        &["change-capture", "forget", "--root", root_arg],
    );
    assert!(ok, "{text}");
    let printed: Value = serde_json::from_str(text.trim()).unwrap();
    assert_eq!(printed["spool"], old["spool"]);
    assert_eq!(printed["spool_retained"], true);
    assert!(bindings(&door).is_empty());
    assert!(spool.is_dir());
    let (ok, text) = agentscm(
        &door,
        temp.path(),
        &["change-capture", "forget", "--root", root_arg],
    );
    assert!(!ok && text.contains("no capture binding"), "{text}");

    // The checkout can then share with its new Hub, from a new private spool and new ids.
    assert_eq!(page.post(&id, other, "shared_conversation").0, 200);
    let fresh = bindings(&door).remove(0);
    assert_eq!(fresh["hub"], other);
    for field in ["spool", "worktree_id", "setup_digest"] {
        assert_ne!(fresh[field], old[field], "{field}");
    }
    assert!(spool.is_dir());
}

#[test]
fn sharing_can_be_turned_off_after_a_reconnect_and_the_list_says_which_hub_it_was_set_for() {
    let temp = TempDir::new().unwrap();
    let root = fs::canonicalize(temp.path()).unwrap().join("widgets");
    checkout(&root);
    let (hub, _) = fake_hub();
    let door = FrontDoor::start(&temp, &hub, &root);
    let page = Page::new(&door);
    let id = page.id_of("widgets");
    assert_eq!(page.post(&id, &hub, "shared_conversation").0, 200);
    let before = page.list().remove(0);
    assert_eq!(before["lineage_mismatch"], false);
    assert!(before["saved_hub"].is_null());

    let other = "http://127.0.0.1:9";
    set_hub(&door, other);
    let moved = page.list().remove(0);
    assert_eq!(moved["lineage_mismatch"], true);
    assert_eq!(moved["saved_hub"], json!(hub));
    assert_eq!(moved["saved_repository"], REPO);
    assert_eq!(moved["mode"], "shared_conversation");
    // Sharing cannot be (re)started for the new Hub, but it can be turned off.
    assert_eq!(page.post(&id, other, "shared_conversation").0, 409);
    let (status, reply) = page.post(&id, other, "metadata_only");
    assert_eq!(status, 200, "{reply}");
    let saved = bindings(&door).remove(0);
    assert_eq!(saved["capture_mode"], "metadata_only");
    assert_eq!(saved["hub"], json!(hub));

    // A checkout the service no longer lists is a 404 the page can recover from.
    let (status, reply) = page.post("0123456789abcdef01234567", other, "metadata_only");
    assert_eq!(status, 404, "{reply}");
    assert_eq!(
        serde_json::from_str::<Value>(&reply).unwrap()["error"],
        "unknown_checkout"
    );
}

#[test]
fn off_works_without_a_connection_and_a_reconnect_does_not_resume_sharing() {
    let temp = TempDir::new().unwrap();
    let root = fs::canonicalize(temp.path()).unwrap().join("widgets");
    checkout(&root);
    let (hub, _) = fake_hub();
    let door = FrontDoor::start(&temp, &hub, &root);
    let page = Page::new(&door);
    let id = page.id_of("widgets");
    assert_eq!(page.post(&id, &hub, "shared_conversation").0, 200);

    // The connection disappears (removed by hand), so nothing lists or reaches the Hub.
    private_file(
        &door.config.join("agentscm/connections.json"),
        &json!({"version": 1, "connections": []}).to_string(),
    );
    let root_arg = root.to_str().unwrap();
    let (ok, text) = agentscm(
        &door,
        temp.path(),
        &[
            "change-capture",
            "set-mode",
            "--root",
            root_arg,
            "--mode",
            "metadata-only",
        ],
    );
    assert!(ok, "{text}");
    assert_eq!(bindings(&door)[0]["capture_mode"], "metadata_only");

    // Reconnecting the same repository and Hub shows sharing off until it is confirmed again.
    connect_checkout(&door, &root, &hub, "again");
    let listed = page.list().remove(0);
    assert_eq!(listed["mode"], "metadata_only");
    assert_eq!(listed["lineage_mismatch"], false);
}

#[test]
fn refused_capture_changes_tell_the_page_a_fixed_message_and_never_a_local_path() {
    let temp = TempDir::new().unwrap();
    let root = fs::canonicalize(temp.path()).unwrap().join("widgets");
    checkout(&root);
    let (hub, _) = fake_hub();
    let door = FrontDoor::start(&temp, &hub, &root);
    let page = Page::new(&door);
    let id = page.id_of("widgets");
    // No path, URL or other slash-bearing detail can reach the page in a refusal.
    let refused = |reply: &str| {
        assert!(!reply.contains('/'), "{reply}");
        assert!(!reply.contains(temp.path().to_str().unwrap()), "{reply}");
        serde_json::from_str::<Value>(reply).unwrap()["message"]
            .as_str()
            .unwrap()
            .to_owned()
    };

    // A spool that cannot be opened is reported without the spool's path.
    assert_eq!(page.post(&id, &hub, "shared_conversation").0, 200);
    let spool = std::path::PathBuf::from(bindings(&door)[0]["spool"].as_str().unwrap());
    assert_eq!(page.post(&id, &hub, "metadata_only").0, 200);
    let moved = spool.with_extension("moved");
    fs::rename(&spool, &moved).unwrap();
    let (status, reply) = page.post(&id, &hub, "shared_conversation");
    assert_eq!(status, 409, "{reply}");
    assert!(refused(&reply).contains("needs attention"), "{reply}");
    fs::rename(&moved, &spool).unwrap();

    // A saved capture for another Hub is explained without paths either.
    let (ok, text) = agentscm(
        &door,
        temp.path(),
        &[
            "change-capture",
            "unconfigure",
            "--root",
            root.to_str().unwrap(),
        ],
    );
    assert!(ok, "{text}");
    let other = "http://127.0.0.1:9";
    set_hub(&door, other);
    let (status, reply) = page.post(&id, other, "shared_conversation");
    assert_eq!(status, 409, "{reply}");
    assert!(refused(&reply).contains("change-capture forget"), "{reply}");

    // So is a checkout that was reconnected after the page listed it.
    set_hub(&door, &hub);
    let (status, reply) = page.post(&id, other, "shared_conversation");
    assert_eq!(status, 409, "{reply}");
    assert!(
        refused(&reply).contains("refresh and confirm again"),
        "{reply}"
    );
}

#[test]
fn the_listing_carries_the_retention_the_hub_reported_and_null_when_it_did_not() {
    for (reported, expected) in [(Some(21), json!(21)), (None, Value::Null)] {
        let temp = TempDir::new().unwrap();
        let root = fs::canonicalize(temp.path()).unwrap().join("widgets");
        checkout(&root);
        let (hub, _) = fake_hub_reporting("github", "github.com", "github", reported);
        let door = FrontDoor::start(&temp, &hub, &root);
        let page = Page::new(&door);
        assert_eq!(
            page.list()[0]["shared_conversation_retention_days"],
            expected
        );
        // A change reply says the same, from what the listing learned.
        let id = page.id_of("widgets");
        let (status, reply) = page.post(&id, &hub, "shared_conversation");
        assert_eq!(status, 200, "{reply}");
        assert_eq!(
            serde_json::from_str::<Value>(&reply).unwrap()["checkout"]["shared_conversation_retention_days"],
            expected
        );
    }

    // A Hub that cannot be reached is unknown too, never a guessed number.
    let temp = TempDir::new().unwrap();
    let root = fs::canonicalize(temp.path()).unwrap().join("widgets");
    checkout(&root);
    let (hub, _) = fake_hub_reporting("github", "github.com", "github", Some(21));
    let door = FrontDoor::start(&temp, &hub, &root);
    let page = Page::new(&door);
    set_hub(&door, "http://127.0.0.1:9");
    assert!(page.list()[0]["shared_conversation_retention_days"].is_null());
}

const DEVICE_ID: &str = "dev_0123456789abcdef0123456789abcdef";
const DEVICE_SECRET: &str = "abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789";

#[test]
fn the_page_lists_and_revokes_computers_with_this_computers_credential() {
    let temp = TempDir::new().unwrap();
    let root = fs::canonicalize(temp.path()).unwrap().join("widgets");
    checkout(&root);
    let (hub, seen) = fake_hub();
    let door = FrontDoor::start(&temp, &hub, &root);
    let grant = connect(&door);
    let auth = format!("Bearer local:{grant}:{REPO}");
    let origin = door.origin();
    let headers = [
        ("Authorization", auth.as_str()),
        ("Origin", origin.as_str()),
    ];
    // Without a computer credential there is nothing to ask the Hub with.
    let (status, _, body) = door.request("GET", "/v1/hub/devices", &headers);
    assert_eq!(status, 503, "{body}");
    assert!(body.contains("hub_sign_in_required"), "{body}");

    private_file(
        &door.config.join("agentscm/hub-sessions.json"),
        &json!({"version": 2, "default_hub": hub, "sessions": [{"hub": hub,
            "session": HUMAN_SESSION,
            "device": {"id": DEVICE_ID, "secret": DEVICE_SECRET, "name": "laptop"}}]})
        .to_string(),
    );
    let (status, _, body) = door.request("GET", "/v1/hub/devices", &headers);
    assert_eq!(status, 200, "{body}");
    let listed: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(
        (&listed["hub"], &listed["devices"][0]["id"]),
        (&json!(hub), &json!(DEVICE_ID))
    );
    let (status, _, body) =
        door.request("DELETE", &format!("/v1/hub/devices/{DEVICE_ID}"), &headers);
    assert_eq!(status, 204, "{body}");
    assert_eq!(
        door.request("DELETE", "/v1/hub/devices/not-an-id", &headers)
            .0,
        400
    );
    // A well-formed id the Hub did not list for this person never reaches the Hub's URL.
    let other = "dev_ffffffffffffffffffffffffffffffff";
    let path = format!("/v1/hub/devices/{other}");
    assert_eq!(door.request("DELETE", &path, &headers).0, 404);
    assert!(
        !seen
            .lock()
            .unwrap()
            .iter()
            .any(|request| request.contains(other))
    );
    let requests = seen.lock().unwrap().clone();
    for line in [
        "GET /v1/hub/devices ".to_owned(),
        format!("DELETE /v1/hub/devices/{DEVICE_ID} "),
    ] {
        assert!(
            requests.iter().any(|request| request.starts_with(&line)
                && request.contains(&format!("authorization: Bearer {DEVICE_SECRET}"))),
            "{line}"
        );
    }
    // The browser never receives the credential.
    assert!(!body.contains(DEVICE_SECRET));
}
