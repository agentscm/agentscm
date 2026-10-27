#[path = "common/hub_pg.rs"]
mod hub_pg;

#[cfg(unix)]
use std::ffi::{OsStr, OsString};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

use agentscm::backend::LocalBackend;
use agentscm::hub::store::{self, ForgeIdentity, Store, mint_agent_token, unix_now};
use agentscm::hub::{HubState, router};
use serde_json::{Value, json};
use tempfile::TempDir;

fn http_get(
    address: SocketAddr,
    path: &str,
    authorization: Option<&str>,
) -> (u16, Vec<(String, String)>, String) {
    let mut stream = TcpStream::connect(address).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    write!(
        stream,
        "GET {path} HTTP/1.1\r\nHost: hub\r\nConnection: close\r\n"
    )
    .unwrap();
    if let Some(value) = authorization {
        write!(stream, "Authorization: {value}\r\n").unwrap();
    }
    stream.write_all(b"\r\n").unwrap();
    let mut raw = String::new();
    stream.read_to_string(&mut raw).unwrap();
    let (head, body) = raw.split_once("\r\n\r\n").unwrap();
    let mut lines = head.split("\r\n");
    let status = lines
        .next()
        .unwrap()
        .split(' ')
        .nth(1)
        .unwrap()
        .parse()
        .unwrap();
    let headers = lines
        .map(|line| {
            let (name, value) = line.split_once(':').unwrap();
            (name.to_ascii_lowercase(), value.trim().to_owned())
        })
        .collect();
    (status, headers, body.to_owned())
}

struct Hub {
    address: SocketAddr,
    store: std::sync::Arc<dyn Store>,
    _dir: TempDir,
}

impl Hub {
    async fn start() -> Self {
        let dir = TempDir::new().unwrap();
        let url = format!("sqlite://{}", dir.path().join("hub.sqlite").display());
        let store = store::open(&url).unwrap();
        let objects = LocalBackend::open(dir.path().join("objects")).unwrap();
        let app = router(HubState::new(store.clone(), std::sync::Arc::new(objects)));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await });
        Self {
            address,
            store,
            _dir: dir,
        }
    }

    fn get(&self, path: &str, authorization: Option<&str>) -> (u16, Vec<(String, String)>, String) {
        http_get(self.address, path, authorization)
    }

    /// Mint a token for a fresh user; returns (token id, bearer secret, user id).
    fn token(&self, user_id: i64, login: &str, ttl: i64) -> (String, String, String) {
        let identity = ForgeIdentity {
            provider: "github".into(),
            host: "github.com".into(),
            user_id,
        };
        let user = self.store.upsert_user(&identity, login).unwrap();
        let (id, secret) =
            mint_agent_token(&*self.store, &user.id, "builder", "acme/widgets", ttl).unwrap();
        (id, secret.expose().to_owned(), user.id)
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn health_reports_storage() {
    let hub = Hub::start().await;
    let (status, _, body) = hub.get("/v1/hub/health", None);
    assert_eq!(status, 200);
    let (alias, _, alias_body) = hub.get("/health", None);
    assert_eq!(alias, 200);
    assert_eq!(
        serde_json::from_str::<Value>(&alias_body).unwrap()["status"],
        "ok"
    );
    let health = serde_json::from_str::<Value>(&body).unwrap();
    assert_eq!(
        (&health["status"], &health["storage"]),
        (&json!("ok"), &json!("sqlite"))
    );
    // Which build answers, so a client can tell it is older than the Hub.
    assert_eq!(health["version"], env!("CARGO_PKG_VERSION"));
    assert_eq!(
        health["commit"].as_str().unwrap_or(""),
        env!("AGENTSCM_BUILD_COMMIT")
    );
    // Counters of this process: numbers only.
    let counters = health["coordination"].as_object().unwrap();
    let mut names: Vec<_> = counters.keys().map(String::as_str).collect();
    names.sort_unstable();
    assert_eq!(
        names,
        [
            "denied",
            "fail_open",
            "hook_p95_ms",
            "rate_limited",
            "repeated"
        ]
    );
    assert!(counters.values().all(Value::is_u64), "{health}");
    assert_eq!(hub.get("/v1/hub/missing", None).0, 404);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn whoami_attributes_the_owner() {
    let hub = Hub::start().await;
    let (id, secret, owner) = hub.token(42, "alice", 3600);
    // The scheme name is case-insensitive.
    for scheme in ["Bearer", "bearer", "BEARER"] {
        let (status, _, body) = hub.get("/v1/hub/whoami", Some(&format!("{scheme} {secret}")));
        assert_eq!(status, 200);
        assert!(!body.contains(&secret));
        let body: Value = serde_json::from_str(&body).unwrap();
        let expires = body["token"]["expires_at"].as_i64().unwrap();
        assert!((unix_now() + 3595..=unix_now() + 3605).contains(&expires));
        assert_eq!(body["token"]["id"], json!(id));
        assert_eq!(body["token"]["name"], "builder");
        assert_eq!(body["token"]["repository"], "acme/widgets");
        assert!(body["token"]["repository_id"].is_null());
        assert_eq!(
            body["owner"],
            json!({"provider": "github", "host": "github.com", "user_id": 42, "login": "alice"})
        );
        assert_eq!(body["shared_conversation_retention_days"], 30);
        assert_eq!(body.as_object().unwrap().len(), 3);
    }
    assert!(owner.starts_with("usr_"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn whoami_rejects_every_failure_identically() {
    let hub = Hub::start().await;
    let (_, expired, _) = hub.token(1, "expired", 1);
    let (revoked_id, revoked, _) = hub.token(2, "revoked", 3600);
    let (_, disabled, owner) = hub.token(3, "disabled", 3600);
    hub.store.revoke_agent_token(&revoked_id).unwrap();
    hub.store.set_user_disabled(&owner, true).unwrap();
    let (_, valid, _) = hub.token(4, "valid", 3600);
    tokio::time::sleep(Duration::from_millis(2100)).await;
    let unknown = "0".repeat(64);
    let attempts = [
        None,
        Some("Basic abc".to_owned()),
        Some("Bearer".to_owned()),
        Some("Bearer short".to_owned()),
        Some(format!("Bearer {}", valid.to_uppercase())),
        Some(format!("Bearer {valid} extra")),
        Some(format!("Bearer  {valid}")),
        Some(format!("Bearer\t{valid}")),
        Some(format!("Bearer {unknown}")),
        Some(format!("Bearer {expired}")),
        Some(format!("Bearer {revoked}")),
        Some(format!("Bearer {disabled}")),
    ];
    assert_eq!(
        hub.get("/v1/hub/whoami", Some(&format!("Bearer {valid}")))
            .0,
        200
    );
    for attempt in attempts {
        let (status, headers, body) = hub.get("/v1/hub/whoami", attempt.as_deref());
        assert_eq!(status, 401, "{attempt:?}");
        assert_eq!(body, r#"{"error":"unauthorized"}"#, "{attempt:?}");
        assert!(headers.contains(&("www-authenticate".into(), "Bearer".into())));
    }
}

fn hub_serve(url: &str, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_agentscm"))
        .args(["hub", "serve"])
        .args(args)
        .env("AGENTSCM_DATABASE_URL", url)
        .output()
        .unwrap()
}

#[test]
fn serve_requires_public_for_non_loopback_bind() {
    let output = hub_serve("sqlite:///unused", &["--bind", "0.0.0.0:0"]);
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("requires --public"));
}

#[test]
fn serve_rejects_unknown_scheme_without_echoing_the_url() {
    let output = hub_serve("mysql://u:s3cret@h/db", &["--bind", "127.0.0.1:0"]);
    assert!(!output.status.success());
    let text = [output.stdout, output.stderr].concat();
    let text = String::from_utf8_lossy(&text);
    assert!(
        text.contains("must start with sqlite:// or postgres://"),
        "{text}"
    );
    assert!(!text.contains("s3cret"), "{text}");
}

struct Server(Child);

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn wait_for(deadline: Duration, mut done: impl FnMut() -> bool) {
    let start = Instant::now();
    while !done() {
        assert!(start.elapsed() < deadline, "timed out");
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[cfg(unix)]
fn slack_hub_command(dir: &TempDir, slack_file: Option<&OsStr>) -> Command {
    use std::os::unix::process::CommandExt;

    let mut command = Command::new(env!("CARGO_BIN_EXE_agentscm"));
    command
        .process_group(0)
        .env_clear()
        .env(
            "AGENTSCM_DATABASE_URL",
            format!("sqlite://{}", dir.path().join("hub.sqlite").display()),
        )
        .args(["hub", "serve", "--bind", "127.0.0.1:0", "--objects"])
        .arg(dir.path().join("objects"))
        .arg("--secrets")
        .arg(dir.path().join("secrets"))
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    if let Some(value) = slack_file {
        command.env("AGENTSCM_SLACK_FILE", value);
    }
    command
}

#[cfg(unix)]
fn kill_owned_process_group(child: &mut Child) {
    let group = -(child.id() as i32);
    // SAFETY: the child created a new process group whose id is its pid, and this test owns it.
    unsafe {
        libc::kill(group, libc::SIGKILL);
    }
    let _ = child.kill();
    let _ = child.wait();
}

#[cfg(unix)]
fn bound_address(
    server: &mut Server,
    stderr: std::process::ChildStderr,
    timeout: Duration,
) -> Result<
    (
        SocketAddr,
        BufReader<std::io::Take<std::process::ChildStderr>>,
        String,
    ),
    String,
> {
    const OUTPUT_LIMIT: usize = 64 * 1024;

    let (send, receive) = std::sync::mpsc::channel();
    let reader = std::thread::spawn(move || {
        let mut stderr = BufReader::new(stderr.take((OUTPUT_LIMIT + 1) as u64));
        let mut seen = String::new();
        let result = loop {
            let mut line = String::new();
            match stderr.read_line(&mut line) {
                Ok(0) => break Err(format!("hub exited before binding: {seen}")),
                Ok(_) if seen.len() + line.len() > OUTPUT_LIMIT => {
                    break Err(format!(
                        "hub startup output exceeded {OUTPUT_LIMIT} bytes: {seen}"
                    ));
                }
                Ok(_) => {
                    seen.push_str(&line);
                    if let Some(rest) = line.strip_prefix("hub listening on ") {
                        break rest
                            .split(' ')
                            .next()
                            .ok_or_else(|| format!("hub printed an empty address: {seen}"))
                            .and_then(|address| {
                                address.parse().map_err(|error| {
                                    format!("invalid hub address: {error}: {seen}")
                                })
                            })
                            .map(|address| (address, stderr, seen));
                    }
                }
                Err(error) => break Err(format!("reading hub startup output failed: {error}")),
            }
        };
        let _ = send.send(result);
    });

    let result = match receive.recv_timeout(timeout) {
        Ok(result) => result,
        Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
            kill_owned_process_group(&mut server.0);
            let _ = reader.join();
            return Err(format!(
                "hub did not print its bound address within {timeout:?}"
            ));
        }
        Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
            kill_owned_process_group(&mut server.0);
            let _ = reader.join();
            return Err("hub startup output reader stopped unexpectedly".into());
        }
    };
    reader
        .join()
        .map_err(|_| "hub startup output reader panicked".to_owned())?;
    if result.is_err() {
        kill_owned_process_group(&mut server.0);
    }
    result
}

#[cfg(unix)]
fn try_http_status(address: SocketAddr, path: &str) -> Option<u16> {
    let mut stream = TcpStream::connect_timeout(&address, Duration::from_millis(100)).ok()?;
    stream
        .set_read_timeout(Some(Duration::from_millis(500)))
        .ok()?;
    stream
        .set_write_timeout(Some(Duration::from_millis(500)))
        .ok()?;
    write!(
        stream,
        "GET {path} HTTP/1.1\r\nHost: hub\r\nConnection: close\r\n\r\n"
    )
    .ok()?;
    let mut raw = String::new();
    stream.read_to_string(&mut raw).ok()?;
    raw.split_once("\r\n")?.0.split(' ').nth(1)?.parse().ok()
}

#[cfg(unix)]
fn assert_slack_setting_reaches_health(slack_file: Option<&OsStr>) {
    let dir = TempDir::new().unwrap();
    let mut server = Server(slack_hub_command(&dir, slack_file).spawn().unwrap());
    let stderr = server.0.stderr.take().unwrap();
    let (address, _stderr, seen) = bound_address(&mut server, stderr, Duration::from_secs(10))
        .unwrap_or_else(|error| {
            panic!("{error}");
        });
    wait_for(Duration::from_secs(10), || {
        assert!(
            server.0.try_wait().unwrap().is_none(),
            "hub exited before health: {seen}"
        );
        try_http_status(address, "/v1/hub/health") == Some(200)
    });
}

#[cfg(unix)]
#[test]
fn bound_address_deadline_kills_and_reaps_a_silent_child() {
    use std::os::unix::process::CommandExt;

    let mut command = Command::new("/bin/sh");
    command
        .process_group(0)
        .args(["-c", "exec /bin/sleep 120"])
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    let mut server = Server(command.spawn().unwrap());
    let stderr = server.0.stderr.take().unwrap();
    let error = bound_address(&mut server, stderr, Duration::from_millis(100)).unwrap_err();
    assert!(error.contains("within 100ms"), "{error}");
    assert!(server.0.try_wait().unwrap().is_some());
}

#[cfg(unix)]
fn assert_slack_setting_fails(slack_file: &OsStr) {
    let dir = TempDir::new().unwrap();
    let mut server = Server(slack_hub_command(&dir, Some(slack_file)).spawn().unwrap());
    let mut status = None;
    wait_for(Duration::from_secs(10), || {
        status = server.0.try_wait().unwrap();
        status.is_some()
    });
    let mut stderr = String::new();
    server
        .0
        .stderr
        .take()
        .unwrap()
        .read_to_string(&mut stderr)
        .unwrap();
    assert!(!status.unwrap().success(), "{stderr}");
}

#[cfg(unix)]
#[test]
fn absent_and_empty_slack_settings_start_the_hub() {
    assert_slack_setting_reaches_health(None);
    assert_slack_setting_reaches_health(Some(OsStr::new("")));
}

#[cfg(unix)]
#[test]
fn nonempty_invalid_slack_settings_still_fail_startup() {
    use std::os::unix::ffi::OsStringExt;
    use std::os::unix::fs::{PermissionsExt, symlink};

    let files = TempDir::new().unwrap();
    let missing = files.path().join("missing.json");
    assert_slack_setting_fails(missing.as_os_str());

    let malformed = files.path().join("malformed.json");
    std::fs::write(&malformed, b"not json").unwrap();
    std::fs::set_permissions(&malformed, std::fs::Permissions::from_mode(0o600)).unwrap();
    assert_slack_setting_fails(malformed.as_os_str());

    for value in [OsStr::new("relative.json"), OsStr::new(" ")] {
        assert_slack_setting_fails(value);
    }

    let unsafe_permissions = files.path().join("unsafe.json");
    std::fs::write(&unsafe_permissions, b"{}").unwrap();
    std::fs::set_permissions(&unsafe_permissions, std::fs::Permissions::from_mode(0o644)).unwrap();
    assert_slack_setting_fails(unsafe_permissions.as_os_str());

    let symlink_path = files.path().join("symlink.json");
    symlink(&malformed, &symlink_path).unwrap();
    assert_slack_setting_fails(symlink_path.as_os_str());

    // SAFETY: geteuid has no arguments, pointers, or mutable state requirements.
    if unsafe { libc::geteuid() } != 0 {
        let unreadable = files.path().join("unreadable.json");
        std::fs::write(&unreadable, b"{}").unwrap();
        std::fs::set_permissions(&unreadable, std::fs::Permissions::from_mode(0o200)).unwrap();
        assert_slack_setting_fails(unreadable.as_os_str());
    }

    let mut non_unicode = files.path().as_os_str().as_encoded_bytes().to_vec();
    non_unicode.extend_from_slice(b"/non-unicode-");
    non_unicode.push(0xff);
    assert_slack_setting_fails(&OsString::from_vec(non_unicode));
}

/// Serve `url` in a child process, check health and whoami, SIGTERM it and expect a clean exit.
#[cfg(unix)]
fn serve_and_terminate(url: &str) {
    let dir = TempDir::new().unwrap();
    let store = store::open(url).unwrap();
    let user = store
        .upsert_user(
            &ForgeIdentity {
                provider: "gitlab".into(),
                host: "gitlab.example".into(),
                user_id: 9,
            },
            "carol",
        )
        .unwrap();
    let (_, secret) = mint_agent_token(&*store, &user.id, "builder", "grp/sub/repo", 600).unwrap();
    let mut server = Server(
        Command::new(env!("CARGO_BIN_EXE_agentscm"))
            .args(["hub", "serve", "--bind", "127.0.0.1:0", "--objects"])
            .arg(dir.path().join("objects"))
            .arg("--secrets")
            .arg(dir.path().join("secrets"))
            .env("AGENTSCM_DATABASE_URL", url)
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap(),
    );
    // The hub prints its bound address; reading it avoids a free-port race.
    let mut stderr = BufReader::new(server.0.stderr.take().unwrap());
    let mut seen = String::new();
    let address: SocketAddr = loop {
        let mut line = String::new();
        assert!(
            stderr.read_line(&mut line).unwrap() > 0,
            "hub exited early: {seen}"
        );
        seen.push_str(&line);
        if let Some(rest) = line.strip_prefix("hub listening on ") {
            break rest.split(' ').next().unwrap().parse().unwrap();
        }
    };
    wait_for(Duration::from_secs(10), || {
        http_get(address, "/v1/hub/health", None).0 == 200
    });
    let (status, _, body) = http_get(
        address,
        "/v1/hub/whoami",
        Some(&format!("Bearer {}", secret.expose())),
    );
    assert_eq!(status, 200);
    let body: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(body["owner"]["login"], "carol");
    assert_eq!(body["token"]["repository"], "grp/sub/repo");

    // SAFETY: kill(2) takes a pid of our own child and a valid signal number.
    assert_eq!(
        unsafe { libc::kill(server.0.id() as i32, libc::SIGTERM) },
        0
    );
    let mut status = None;
    wait_for(Duration::from_secs(10), || {
        status = server.0.try_wait().unwrap();
        status.is_some()
    });
    stderr.read_to_string(&mut seen).unwrap();
    assert!(status.unwrap().success(), "{seen}");
    assert!(!seen.contains("panicked"), "{seen}");
}

#[cfg(unix)]
#[test]
fn serve_runs_and_shuts_down_cleanly_on_sqlite() {
    let dir = TempDir::new().unwrap();
    serve_and_terminate(&format!(
        "sqlite://{}",
        dir.path().join("hub.sqlite").display()
    ));
}

#[cfg(unix)]
#[test]
fn serve_runs_and_shuts_down_cleanly_on_postgres() {
    if let Some(pg) = hub_pg::PgSchema::create() {
        serve_and_terminate(&pg.url);
    }
}

/// Start the real `hub serve` with only `env` set and return its stderr up to the first-run line.
#[cfg(unix)]
fn first_run_stderr(dir: &TempDir, env: &[(&str, &str)]) -> String {
    let mut command = Command::new(env!("CARGO_BIN_EXE_agentscm"));
    command
        .env_clear()
        .env(
            "AGENTSCM_DATABASE_URL",
            format!("sqlite://{}", dir.path().join("hub.sqlite").display()),
        )
        .args(["hub", "serve", "--bind", "127.0.0.1:0"])
        .args(["--public-url", "http://127.0.0.1:7342", "--objects"])
        .arg(dir.path().join("objects"))
        .arg("--secrets")
        .arg(dir.path().join("secrets"))
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    for (name, value) in env {
        command.env(name, value);
    }
    let mut server = Server(command.spawn().unwrap());
    let mut stderr = BufReader::new(server.0.stderr.take().unwrap());
    let mut seen = String::new();
    loop {
        let mut line = String::new();
        assert!(
            stderr.read_line(&mut line).unwrap() > 0,
            "hub exited: {seen}"
        );
        seen.push_str(&line);
        if line.contains("first-run setup link") || line.contains("first run:") {
            return seen;
        }
    }
}

#[cfg(unix)]
#[test]
fn serve_writes_the_first_run_link_on_an_empty_hub() {
    use std::os::unix::fs::PermissionsExt;
    let dir = TempDir::new().unwrap();
    let stderr = first_run_stderr(&dir, &[]);
    assert!(stderr.contains("login providers: none"), "{stderr}");
    assert!(stderr.contains("hub: first run:"), "{stderr}");
    let path = dir.path().join("secrets/bootstrap-setup-url");
    assert!(path.exists(), "no link file: {stderr}");
    let link = std::fs::read_to_string(&path).unwrap();
    let token = link
        .strip_prefix("http://127.0.0.1:7342/v1/hub/github/setup/")
        .unwrap_or_else(|| panic!("unexpected link {link}"));
    assert_eq!(token.len(), 64, "{link}");
    assert!(!stderr.contains(token), "the log must not hold the token");
    let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o600);

    // With sign-in configured there is no link, and the log says why.
    let configured = TempDir::new().unwrap();
    let stderr = first_run_stderr(
        &configured,
        &[
            ("AGENTSCM_GITHUB_CLIENT_ID", "Iv1.synthetic"),
            ("AGENTSCM_GITHUB_CLIENT_SECRET", "synthetic-client-secret"),
        ],
    );
    assert!(
        stderr.contains("hub: no first-run setup link: a sign-in provider is configured"),
        "{stderr}"
    );
    assert!(
        !configured
            .path()
            .join("secrets/bootstrap-setup-url")
            .exists()
    );
}
