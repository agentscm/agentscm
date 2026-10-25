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
    fs::create_dir_all(root).unwrap();
    git(root, &["init", "-q", "-b", "main"]);
    git(
        root,
        &[
            "remote",
            "add",
            "origin",
            "https://github.com/acme/widgets.git",
        ],
    );
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
            } else if line.starts_with("GET /v1/hub/whoami ") {
                (
                    200,
                    json!({"token": {"id": MINTED_ID, "name": "agentscm-tray", "repository": REPO, "repository_id": 7, "expires_at": expiry, "read_only": true}, "owner": {"provider": "github", "host": "github.com", "user_id": 1, "login": "synthetic"}}),
                )
            } else if line.starts_with("GET /v1/hub/repos/acme%2Fwidgets/refused") {
                (401, json!({"error": "unauthorized"}))
            } else if line.starts_with("GET /v1/hub/repos/acme%2Fwidgets/work") {
                (200, json!({"synthetic": "work page"}))
            } else if line.starts_with("DELETE /v1/hub/token ") {
                (204, Value::Null)
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
        let config = temp.path().join("config");
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
        request.push_str("Content-Length: 0\r\n\r\n");
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
