//! Real hook and MCP processes, including fallback and non-idempotent failure boundaries.
use std::io::{BufRead, BufReader, Write};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdout, Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver};
use std::time::{Duration, Instant};

use axum::extract::{ConnectInfo, State};
use axum::http::{HeaderMap, Uri};
use axum::{Json, Router, body::Bytes};
use serde_json::{Value, json};
use tempfile::TempDir;

const TOKEN: &str = "synthetic-relay-token";
const WAIT: Duration = Duration::from_secs(8);

struct Seen {
    peer: SocketAddr,
    uri: String,
    headers: HeaderMap,
    body: Vec<u8>,
}

#[derive(Clone)]
struct HubState {
    send: mpsc::Sender<Seen>,
    stalled: Arc<AtomicBool>,
}

async fn answer(
    State(state): State<HubState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    uri: Uri,
    headers: HeaderMap,
    body: Bytes,
) -> Json<Value> {
    let payload: Value = serde_json::from_slice(&body).unwrap();
    let hook = uri.path().starts_with("/v1/hub/hook/");
    let _ = state.send.send(Seen {
        peer,
        uri: uri.to_string(),
        headers,
        body: body.to_vec(),
    });
    if hook && state.stalled.load(Ordering::SeqCst) {
        std::future::pending::<()>().await;
    }
    Json(if hook {
        json!({"hookSpecificOutput": {
            "hookEventName": payload["hook_event_name"],
            "additionalContext": "synthetic Hub reply"
        }})
    } else {
        json!({"jsonrpc":"2.0", "id":payload["id"], "result":{}})
    })
}

struct Fixture {
    temp: TempDir,
    root: PathBuf,
    config: PathBuf,
    token: PathBuf,
    hub: String,
    seen: Receiver<Seen>,
    stalled: Arc<AtomicBool>,
    server: tokio::task::JoinHandle<()>,
}

fn private_file(file: &Path, bytes: &[u8]) {
    std::fs::write(file, bytes).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(file, std::fs::Permissions::from_mode(0o600)).unwrap();
    }
}

impl Fixture {
    async fn new() -> Self {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("checkout");
        std::fs::create_dir(&root).unwrap();
        for args in [
            vec!["init", "--quiet"],
            vec![
                "remote",
                "add",
                "origin",
                "https://github.com/acme/widgets.git",
            ],
        ] {
            assert!(
                Command::new("git")
                    .current_dir(&root)
                    .args(args)
                    .status()
                    .unwrap()
                    .success()
            );
        }
        let root = root.canonicalize().unwrap();
        let config = temp.path().join("config");
        let directory = config.join("agentscm");
        std::fs::create_dir_all(&directory).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            for directory in [&config, &directory] {
                std::fs::set_permissions(directory, std::fs::Permissions::from_mode(0o700))
                    .unwrap();
            }
        }
        let token = directory.join("token");
        private_file(&token, TOKEN.as_bytes());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let hub = format!("http://{}", listener.local_addr().unwrap());
        let (send, seen) = mpsc::channel();
        let stalled = Arc::new(AtomicBool::new(false));
        let app = Router::new().fallback(answer).with_state(HubState {
            send,
            stalled: stalled.clone(),
        });
        let server = tokio::spawn(async move {
            axum::serve(
                listener,
                app.into_make_service_with_connect_info::<SocketAddr>(),
            )
            .await
            .unwrap();
        });
        private_file(
            &directory.join("connections.json"),
            &serde_json::to_vec(&json!({
                "version":1, "connections":[{
                    "root":root, "origin_host":"github.com", "hub":hub,
                    "repo":"acme/widgets", "token_file":token, "report_completion":true
                }]
            }))
            .unwrap(),
        );
        Self {
            temp,
            root,
            config,
            token,
            hub,
            seen,
            stalled,
            server,
        }
    }

    fn command(&self, binary: &Path) -> Command {
        let mut command = Command::new(binary);
        command
            .env_clear()
            .envs(std::env::var_os("SystemRoot").map(|value| ("SystemRoot", value)))
            .env("PATH", std::env::var_os("PATH").unwrap_or_default())
            .env("HOME", self.temp.path())
            .env("USERPROFILE", self.temp.path())
            .env("XDG_CONFIG_HOME", &self.config)
            .current_dir(&self.root)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        command
    }

    fn host(&self) -> Host {
        let mut child = self
            .command(Path::new(env!("CARGO_BIN_EXE_agentscm")))
            .args(["client", "mcp"])
            .spawn()
            .unwrap();
        let output = BufReader::new(child.stdout.take().unwrap());
        let mut host = Host { child, output };
        writeln!(
            host.child.stdin.as_mut().unwrap(),
            "{}",
            json!({"jsonrpc":"2.0", "id":1, "method":"ping"})
        )
        .unwrap();
        let mut reply = String::new();
        host.output.read_line(&mut reply).unwrap();
        assert_eq!(
            serde_json::from_str::<Value>(&reply).unwrap()["result"],
            json!({})
        );
        host
    }

    fn hook(&self, event: &str, plugin: bool, extra: impl FnOnce(&mut Command)) -> Hook {
        let mut command = self.command(Path::new(env!("CARGO_BIN_EXE_agentscm")));
        command.args(["hook", "claude"]);
        if plugin {
            command.arg("--client-connection");
        } else {
            command
                .args(["--hub-url", &self.hub, "--repo", "acme/widgets"])
                .env("AGENTSCM_HUB_TOKEN", TOKEN);
        }
        extra(&mut command);
        let body = format!(
            "  {{ \"hook_event_name\":\"{event}\", \"session_id\":\"synthetic-session\", \"cwd\":{}, \"tool_name\":\"Edit\", \"tool_input\":{{\"file_path\":\"src/synthetic.rs\"}} }}\n",
            serde_json::to_string(&self.root).unwrap()
        );
        let started = Instant::now();
        let mut child = command.spawn().unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(body.as_bytes())
            .unwrap();
        Hook {
            child,
            started,
            body,
        }
    }

    fn received(&self) -> Seen {
        self.seen.recv_timeout(WAIT).unwrap()
    }

    fn only_hook(&self) -> Seen {
        let seen = self.received();
        assert_eq!(seen.uri, "/v1/hub/hook/claude?repo=acme%2Fwidgets");
        assert!(self.seen.recv_timeout(Duration::from_millis(50)).is_err());
        seen
    }

    /// The relay's pooled Hub connection. The host answers MCP before its relay has bound, and a
    /// hook that finds no relay sends directly, so wait for two hooks in a row on one connection.
    async fn relay_owner(&self) -> SocketAddr {
        let started = Instant::now();
        let mut previous = None;
        loop {
            self.hook("PreToolUse", true, |_| {}).finish(true);
            let peer = self.only_hook().peer;
            if previous == Some(peer) {
                return peer;
            }
            assert!(
                started.elapsed() < WAIT,
                "the relay must start pooling hooks"
            );
            previous = Some(peer);
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        self.server.abort();
    }
}

struct Host {
    child: Child,
    output: BufReader<ChildStdout>,
}

impl Host {
    fn stop(&mut self) {
        drop(self.child.stdin.take());
        assert!(self.child.wait().unwrap().success());
    }
}

impl Drop for Host {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

struct Hook {
    child: Child,
    started: Instant,
    body: String,
}

impl Hook {
    fn finish(self, success: bool) -> Duration {
        let output = self.child.wait_with_output().unwrap();
        let elapsed = self.started.elapsed();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let reply: Value = serde_json::from_slice(&output.stdout).unwrap();
        let context = reply["hookSpecificOutput"]["additionalContext"]
            .as_str()
            .unwrap();
        if success {
            assert!(context.contains("synthetic Hub reply"), "{reply}");
        } else {
            assert!(
                context.contains("coordination unknown (hub unreachable;"),
                "{reply}"
            );
            assert!(
                reply["hookSpecificOutput"]
                    .get("permissionDecision")
                    .is_none()
            );
            assert!(reply.get("decision").is_none());
        }
        elapsed
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn real_hooks_share_the_relays_own_pool_and_keep_legacy_bytes_and_rotated_tokens() {
    let fixture = Fixture::new().await;
    let mut host = fixture.host();
    let warm = fixture.received();
    assert!(warm.uri.starts_with("/mcp?"));
    let owner = fixture.relay_owner().await;
    assert_ne!(
        owner, warm.peer,
        "relay and MCP must have independent pools"
    );
    for plugin in [true, false] {
        let hook = fixture.hook("PreToolUse", plugin, |_| {});
        let raw = hook.body.clone();
        hook.finish(true);
        let seen = fixture.only_hook();
        assert_eq!(
            seen.peer, owner,
            "separate hook processes must reuse the relay connection"
        );
        assert_eq!(seen.headers["authorization"], format!("Bearer {TOKEN}"));
        if plugin {
            assert!(seen.headers.contains_key("x-agentscm-client"));
            assert!(
                seen.headers
                    .contains_key(agentscm::hub::insights::COMPLETION_HEADER)
            );
            assert!(!seen.headers.contains_key("x-agentscm-root"));
        } else {
            assert_eq!(seen.body, raw.as_bytes());
            assert!(seen.headers.contains_key("x-agentscm-root"));
        }
    }
    private_file(&fixture.token, b"synthetic-rotated-token");
    fixture.hook("PreToolUse", true, |_| {}).finish(true);
    let rotated = fixture.only_hook();
    assert_eq!(rotated.peer, owner);
    assert_eq!(
        rotated.headers["authorization"],
        "Bearer synthetic-rotated-token"
    );
    host.stop();
    fixture.hook("PreToolUse", true, |_| {}).finish(true);
    assert_ne!(
        fixture.only_hook().peer,
        owner,
        "stdio EOF must release the relay"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn missing_or_proxy_or_binary_mismatched_relays_fall_back_without_loss() {
    let fixture = Fixture::new().await;
    fixture.hook("PreToolUse", true, |_| {}).finish(true);
    fixture.only_hook();
    let _host = fixture.host();
    fixture.received();
    let owner = fixture.relay_owner().await;
    fixture
        .hook("PreToolUse", true, |command| {
            command.env("NO_PROXY", "127.0.0.1");
        })
        .finish(true);
    assert_ne!(fixture.only_hook().peer, owner);
    let binary = fixture
        .temp
        .path()
        .join(if cfg!(windows) { "other.exe" } else { "other" });
    std::fs::copy(env!("CARGO_BIN_EXE_agentscm"), &binary).unwrap();
    std::fs::File::options()
        .write(true)
        .open(&binary)
        .unwrap()
        .set_modified(std::time::UNIX_EPOCH + Duration::from_secs(1000))
        .unwrap();
    fixture
        .hook("PreToolUse", true, |command| {
            *command = fixture.command(&binary);
            command.args(["hook", "claude", "--client-connection"]);
        })
        .finish(true);
    assert_ne!(fixture.only_hook().peer, owner);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_second_host_takes_over_when_the_owners_stdio_ends() {
    let fixture = Fixture::new().await;
    let mut first = fixture.host();
    fixture.received();
    let owner = fixture.relay_owner().await;
    let _second = fixture.host();
    let standby = fixture.received().peer;
    fixture.hook("PreToolUse", true, |_| {}).finish(true);
    assert_eq!(fixture.only_hook().peer, owner);
    first.stop();
    let started = Instant::now();
    let mut previous = None;
    loop {
        fixture.hook("PreToolUse", true, |_| {}).finish(true);
        let successor = fixture.only_hook().peer;
        assert_ne!(successor, owner);
        assert_ne!(successor, standby, "standby's MCP pool stays independent");
        if previous == Some(successor) {
            break;
        }
        assert!(
            started.elapsed() < WAIT,
            "standby must take over and pool hooks"
        );
        previous = Some(successor);
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn accepted_then_dead_is_not_resent_and_wedged_calls_keep_the_hook_deadlines() {
    for (event, kill, budget) in [
        ("PreToolUse", true, Duration::from_secs(3)),
        ("PreToolUse", false, Duration::from_secs(3)),
        ("SessionStart", false, Duration::from_millis(4500)),
    ] {
        let fixture = Fixture::new().await;
        let mut host = fixture.host();
        fixture.received();
        let owner = fixture.relay_owner().await;
        fixture.stalled.store(true, Ordering::SeqCst);
        let hook = fixture.hook(event, true, |_| {});
        assert_eq!(
            fixture.received().peer,
            owner,
            "must accept the hook before the failure"
        );
        if kill {
            host.child.kill().unwrap();
            host.child.wait().unwrap();
        }
        let elapsed = hook.finish(false);
        assert!(
            elapsed <= budget + Duration::from_millis(500),
            "{elapsed:?} > {budget:?}"
        );
        if !kill {
            assert!(elapsed >= budget - Duration::from_millis(300));
        }
        assert!(
            fixture
                .seen
                .recv_timeout(Duration::from_millis(100))
                .is_err(),
            "accepted call must never be resent"
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_real_hub_registers_and_ends_a_session_over_the_relay_pool() {
    use agentscm::backend::LocalBackend;
    use agentscm::hub::store::{self, ForgeIdentity, mint_agent_token};
    use agentscm::hub::{HubState, router};

    let mut fixture = Fixture::new().await;
    let store = store::open(&format!(
        "sqlite://{}",
        fixture.temp.path().join("hub.sqlite").display()
    ))
    .unwrap();
    let identity = ForgeIdentity {
        provider: "github".into(),
        host: "github.com".into(),
        user_id: 1,
    };
    let user = store.upsert_user(&identity, "synthetic-user").unwrap();
    let (_, secret) =
        mint_agent_token(&*store, &user.id, "synthetic-agent", "acme/widgets", 3600).unwrap();
    private_file(&fixture.token, secret.expose().as_bytes());
    let objects = LocalBackend::open(fixture.temp.path().join("objects")).unwrap();
    let (send, seen) = mpsc::channel();
    fixture.seen = seen;
    let app = router(HubState::new(store, Arc::new(objects))).layer(axum::middleware::from_fn(
        move |request: axum::extract::Request, next: axum::middleware::Next| {
            let _ = send.send(Seen {
                peer: request
                    .extensions()
                    .get::<ConnectInfo<SocketAddr>>()
                    .unwrap()
                    .0,
                uri: request.uri().to_string(),
                headers: request.headers().clone(),
                body: Vec::new(),
            });
            async move { next.run(request).await }
        },
    ));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    fixture.hub = format!("http://{}", listener.local_addr().unwrap());
    fixture.server.abort();
    fixture.server = tokio::spawn(async move {
        axum::serve(
            listener,
            app.into_make_service_with_connect_info::<SocketAddr>(),
        )
        .await
        .unwrap();
    });
    private_file(
        &fixture.config.join("agentscm/connections.json"),
        &serde_json::to_vec(&json!({
            "version":1, "connections":[{
                "root":fixture.root, "origin_host":"github.com", "hub":fixture.hub,
                "repo":"acme/widgets", "token_file":fixture.token
            }]
        }))
        .unwrap(),
    );
    let _host = fixture.host();
    let mcp = fixture.received().peer;
    let mut owner = None;
    for event in ["SessionStart", "SessionEnd"] {
        let output = fixture
            .hook(event, true, |_| {})
            .child
            .wait_with_output()
            .unwrap();
        assert!(output.status.success());
        let reply: Value = serde_json::from_slice(&output.stdout).unwrap();
        if event == "SessionStart" {
            assert!(
                reply["hookSpecificOutput"]["additionalContext"]
                    .as_str()
                    .unwrap()
                    .contains("native session ID")
            );
        } else {
            assert_eq!(reply, json!({}));
        }
        let peer = if event == "SessionStart" {
            // Capture also asks, once, whether the repository shares agent conversations.
            let first = fixture.received();
            let mut seen = vec![first];
            while let Ok(more) = fixture.seen.recv_timeout(Duration::from_millis(200)) {
                seen.push(more);
            }
            let (hooks, rest): (Vec<_>, Vec<_>) = seen
                .into_iter()
                .partition(|s| s.uri == "/v1/hub/hook/claude?repo=acme%2Fwidgets");
            assert_eq!(hooks.len(), 1);
            assert!(
                rest.iter()
                    .all(|s| s.uri == "/v1/hub/repos/acme%2Fwidgets/conversation-sharing"),
                "{:?}",
                rest.iter().map(|s| &s.uri).collect::<Vec<_>>()
            );
            hooks[0].peer
        } else {
            fixture.only_hook().peer
        };
        assert_ne!(peer, mcp);
        assert_eq!(*owner.get_or_insert(peer), peer);
    }
}
