//! The real `agentscm hook` binary against a stub hub: it always exits 0 with valid JSON.
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::process::{Command, Stdio};
use std::sync::mpsc::{Receiver, channel};
use std::thread;
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use tempfile::TempDir;

const TOKEN: &str = "synthetic-hook-token";
const AGENTS: [&str; 5] = ["claude", "codex", "cursor", "copilot", "generic"];

#[derive(Debug)]
struct Seen {
    method: String,
    path: String,
    authorization: String,
    content_type: String,
    root: String,
    body: Vec<u8>,
}

enum Reply {
    Status(u16, &'static str),
    Silent,
}

/// Serves one request on a loopback port, then reports what it saw.
fn stub(reply: Reply) -> (String, Receiver<Seen>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let (send, seen) = channel();
    thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let request = read_request(&mut stream);
        let _ = send.send(request);
        match reply {
            Reply::Status(status, body) => {
                let text = format!(
                    "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\n\
                     Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = stream.write_all(text.as_bytes());
            }
            Reply::Silent => thread::sleep(Duration::from_secs(8)),
        }
    });
    (url, seen)
}

fn read_request(stream: &mut TcpStream) -> Seen {
    let mut data = Vec::new();
    let mut chunk = [0u8; 4096];
    let head_end = loop {
        let n = stream.read(&mut chunk).unwrap();
        data.extend_from_slice(&chunk[..n]);
        if let Some(at) = data.windows(4).position(|w| w == b"\r\n\r\n") {
            break at + 4;
        }
        assert!(n > 0, "request ended before its headers did");
    };
    let head = String::from_utf8_lossy(&data[..head_end]).into_owned();
    let header = |name: &str| {
        head.lines()
            .find_map(|line| {
                let (key, value) = line.split_once(':')?;
                key.eq_ignore_ascii_case(name)
                    .then(|| value.trim().to_owned())
            })
            .unwrap_or_default()
    };
    let length: usize = header("content-length").parse().unwrap_or(0);
    while data.len() < head_end + length {
        let n = stream.read(&mut chunk).unwrap();
        assert!(n > 0, "request ended before its body did");
        data.extend_from_slice(&chunk[..n]);
    }
    let mut first = head.lines().next().unwrap().split(' ');
    Seen {
        method: first.next().unwrap().to_owned(),
        path: first.next().unwrap().to_owned(),
        authorization: header("authorization"),
        content_type: header("content-type"),
        root: header("x-agentscm-root"),
        body: data[head_end..head_end + length].to_vec(),
    }
}

struct Run {
    code: Option<i32>,
    stdout: String,
    elapsed: Duration,
}

impl Run {
    fn json(&self) -> Value {
        serde_json::from_str(&self.stdout)
            .unwrap_or_else(|_| panic!("stdout is not JSON: {:?}", self.stdout))
    }
}

/// Runs the binary with only the given environment, so a developer's real config is never read.
fn hook(args: &[&str], env: &[(&str, &str)], stdin: &[u8]) -> Run {
    let started = Instant::now();
    let mut child = Command::new(env!("CARGO_BIN_EXE_agentscm"))
        .arg("hook")
        .args(args)
        .env_clear()
        // Windows sockets fail to start without SystemRoot.
        .envs(std::env::var_os("SystemRoot").map(|v| ("SystemRoot", v)))
        .envs(env.iter().copied())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(stdin).unwrap();
    let output = child.wait_with_output().unwrap();
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(!stdout.contains(TOKEN) && !String::from_utf8_lossy(&output.stderr).contains(TOKEN));
    Run {
        code: output.status.code(),
        stdout,
        elapsed: started.elapsed(),
    }
}

/// An empty config directory, as `HOME` and `XDG_CONFIG_HOME`.
fn home() -> TempDir {
    TempDir::new().unwrap()
}

fn base_env(home: &TempDir) -> Vec<(&'static str, String)> {
    let path = home.path().display().to_string();
    vec![
        ("HOME", path.clone()),
        ("USERPROFILE", path.clone()),
        ("XDG_CONFIG_HOME", path),
    ]
}

fn run_with(args: &[&str], extra: &[(&str, &str)], home: &TempDir, stdin: &[u8]) -> Run {
    let mut env: Vec<(&str, String)> = base_env(home);
    env.extend(extra.iter().map(|(k, v)| (*k, (*v).to_owned())));
    let env: Vec<(&str, &str)> = env.iter().map(|(k, v)| (*k, v.as_str())).collect();
    hook(args, &env, stdin)
}

fn assert_unknown(run: &Run, agent: &str, reason: &str) {
    assert_eq!(run.code, Some(0));
    let out = run.json();
    assert!(out.is_object(), "{agent}: {out}");
    assert!(!run.stdout.contains("hub.json"));
    if reason.is_empty() {
        return;
    }
    let has_reason = run
        .stdout
        .contains(&format!("coordination unknown ({reason})"));
    let carries_text =
        out.get("hookSpecificOutput").is_some() || out.get("additional_context").is_some();
    assert!(has_reason || !carries_text, "{agent}: {out}");
}

const CLAUDE_PRE: &str = r#"{"hook_event_name":"PreToolUse","tool_name":"Edit","session_id":"s1","cwd":"/work/repo","tool_input":{"file_path":"/work/repo/src/a.rs"}}"#;

#[test]
fn success_passes_the_hub_reply_through() {
    let (url, seen) = stub(Reply::Status(
        200,
        r#"{ "hookSpecificOutput": {"hookEventName":"PreToolUse","additionalContext":"STOP: x"} }"#,
    ));
    let dir = home();
    let run = run_with(
        &["claude", "--hub-url", &url],
        &[("AGENTSCM_HUB_TOKEN", TOKEN)],
        &dir,
        CLAUDE_PRE.as_bytes(),
    );
    assert_eq!(run.code, Some(0));
    assert_eq!(
        run.stdout,
        "{\"hookSpecificOutput\":{\"additionalContext\":\"STOP: x\",\"hookEventName\":\"PreToolUse\"}}\n"
    );
    let seen = seen.recv_timeout(Duration::from_secs(2)).unwrap();
    assert_eq!(seen.method, "POST");
    assert_eq!(seen.path, "/v1/hub/hook/claude");
    assert_eq!(seen.authorization, format!("Bearer {TOKEN}"));
    assert_eq!(seen.content_type, "application/json");
    assert_eq!(seen.body, CLAUDE_PRE.as_bytes());
}

#[test]
fn nothing_configured_exits_zero_with_valid_json_for_every_agent() {
    let dir = home();
    for agent in AGENTS {
        for payload in [
            json!({"hook_event_name": "PreToolUse"}),
            json!({"hook_event_name": "preToolUse", "tool_name": "Write"}),
            json!({"hook_event_name": "sessionStart"}),
        ] {
            let run = run_with(&[agent], &[], &dir, payload.to_string().as_bytes());
            assert_unknown(&run, agent, "no hub configured");
            let out = run.json();
            match (agent, payload["hook_event_name"].as_str().unwrap()) {
                ("claude" | "codex" | "copilot", "PreToolUse") => {
                    assert_eq!(out["hookSpecificOutput"]["hookEventName"], "PreToolUse");
                }
                ("cursor", "preToolUse") => assert_eq!(out, json!({"permission": "allow"})),
                ("cursor", "sessionStart") => assert!(out["additional_context"].is_string()),
                _ => assert_eq!(out, json!({})),
            }
        }
    }
}

#[test]
fn hub_failures_fail_open() {
    let cases = [
        (Reply::Status(500, r#"{"error":"boom"}"#), "hub error 500"),
        (Reply::Status(401, "{}"), "token rejected"),
        (Reply::Status(403, "{}"), "token rejected"),
        (Reply::Status(200, "this is not json"), "bad reply"),
        (Reply::Status(200, "[1,2]"), "bad reply"),
        (Reply::Status(200, "\"text\""), "bad reply"),
    ];
    for (reply, reason) in cases {
        let (url, _seen) = stub(reply);
        let dir = home();
        let run = run_with(
            &["claude", "--hub-url", &url],
            &[("AGENTSCM_HUB_TOKEN", TOKEN)],
            &dir,
            CLAUDE_PRE.as_bytes(),
        );
        assert_unknown(&run, "claude", reason);
        assert!(run.stdout.contains(reason), "{reason}: {}", run.stdout);
        assert!(!run.stdout.contains(&url));
    }
}

#[test]
fn a_silent_hub_times_out_quickly() {
    let (url, _seen) = stub(Reply::Silent);
    let dir = home();
    let run = run_with(
        &["cursor", "--hub-url", &url],
        &[("AGENTSCM_HUB_TOKEN", TOKEN)],
        &dir,
        br#"{"hook_event_name":"preToolUse","tool_name":"Write"}"#,
    );
    // The hook's 3 s limit plus process start on a loaded host; a silent hub never answers.
    assert!(run.elapsed < Duration::from_secs(15), "{:?}", run.elapsed);
    assert_eq!(run.code, Some(0));
    assert_eq!(run.json(), json!({"permission": "allow"}));
}

#[test]
fn garbage_and_empty_stdin_exit_zero() {
    let dir = home();
    let env = [
        ("AGENTSCM_HUB_URL", "http://127.0.0.1:9"),
        ("AGENTSCM_HUB_TOKEN", TOKEN),
    ];
    for stdin in [&b""[..], b"not json at all", b"\xff\xfe\x00", b"{\"a\":"] {
        for agent in AGENTS {
            let run = run_with(&[agent], &env, &dir, stdin);
            assert_unknown(&run, agent, "");
        }
    }
    let run = run_with(&["cursor"], &env, &dir, b"garbage");
    assert_eq!(run.json(), json!({"permission": "allow"}));
}

#[test]
fn unknown_agents_and_flags_exit_zero() {
    let dir = home();
    for args in [
        vec!["mystery"],
        vec![],
        vec!["claude", "--bogus", "--hub-url"],
        vec!["--only-flags", "-x"],
        vec!["generic", "--hub-url", "not a url"],
    ] {
        let run = run_with(&args, &[("AGENTSCM_HUB_TOKEN", TOKEN)], &dir, b"{}");
        assert_eq!(run.code, Some(0), "{args:?}");
        assert_eq!(run.json(), json!({}), "{args:?}");
    }
}

#[test]
fn token_file_from_the_environment_works() {
    let (url, seen) = stub(Reply::Status(200, r#"{"additionalContext":"ok"}"#));
    let dir = home();
    let token_file = dir.path().join("token");
    std::fs::write(&token_file, format!("{TOKEN}\n")).unwrap();
    let run = run_with(
        &["codex", "--hub-url", &url],
        &[("AGENTSCM_HUB_TOKEN_FILE", token_file.to_str().unwrap())],
        &dir,
        b"{}",
    );
    assert_eq!(run.json(), json!({"additionalContext": "ok"}));
    let seen = seen.recv_timeout(Duration::from_secs(2)).unwrap();
    assert_eq!(seen.path, "/v1/hub/hook/codex");
    assert_eq!(seen.authorization, format!("Bearer {TOKEN}"));
}

fn write_hub_json(dir: &TempDir, url: &str) {
    let token_file = dir.path().join("token");
    std::fs::write(&token_file, format!("{TOKEN}\n")).unwrap();
    let config = dir.path().join("agentscm");
    std::fs::create_dir_all(&config).unwrap();
    let text = json!({"url": url, "token_file": token_file}).to_string();
    std::fs::write(config.join("hub.json"), text).unwrap();
}

#[test]
fn hub_json_token_works_with_or_without_a_matching_hub_url() {
    for with_flag in [false, true] {
        let (url, seen) = stub(Reply::Status(200, r#"{"additionalContext":"hub"}"#));
        let dir = home();
        write_hub_json(&dir, &url);
        // The same hub with a trailing slash is still the stored hub.
        let slash = format!("{url}/");
        let args = if with_flag {
            vec!["generic", "--hub-url", &slash]
        } else {
            vec!["generic"]
        };
        let run = run_with(&args, &[], &dir, b"{}");
        assert_eq!(run.json(), json!({"additionalContext": "hub"}));
        let seen = seen.recv_timeout(Duration::from_secs(2)).unwrap();
        assert_eq!(seen.authorization, format!("Bearer {TOKEN}"));
    }
}

#[test]
fn hub_json_token_is_not_sent_to_a_different_hub() {
    let (stored, stored_seen) = stub(Reply::Status(200, "{}"));
    let (other, other_seen) = stub(Reply::Status(200, "{}"));
    let dir = home();
    write_hub_json(&dir, &stored);
    let run = run_with(
        &["claude", "--hub-url", &other],
        &[],
        &dir,
        br#"{"hook_event_name":"SessionStart"}"#,
    );
    assert_eq!(run.code, Some(0));
    assert!(
        run.stdout
            .contains("no token for this hub; set AGENTSCM_HUB_TOKEN"),
        "{}",
        run.stdout
    );
    assert!(other_seen.recv_timeout(Duration::from_millis(500)).is_err());
    assert!(
        stored_seen
            .recv_timeout(Duration::from_millis(100))
            .is_err()
    );
}

/// Posts a Claude payload whose cwd is `cwd` and returns what the stub hub saw.
fn seen_for(cwd: &std::path::Path, extra: &[&str]) -> Seen {
    let (url, seen) = stub(Reply::Status(200, "{}"));
    let dir = home();
    let payload = json!({"hook_event_name": "PreToolUse", "cwd": cwd}).to_string();
    let mut args = vec!["claude", "--hub-url", &url];
    args.extend(extra);
    let run = run_with(
        &args,
        &[("AGENTSCM_HUB_TOKEN", TOKEN)],
        &dir,
        payload.as_bytes(),
    );
    assert_eq!(run.code, Some(0));
    seen.recv_timeout(Duration::from_secs(2)).unwrap()
}

#[test]
fn the_repo_guard_and_git_root_are_sent() {
    let repo = TempDir::new().unwrap();
    std::fs::create_dir(repo.path().join(".git")).unwrap();
    let sub = repo.path().join("src/deep");
    std::fs::create_dir_all(&sub).unwrap();
    let seen = seen_for(&sub, &["--repo", "acme/widgets"]);
    assert_eq!(seen.path, "/v1/hub/hook/claude?repo=acme/widgets");
    let root = std::fs::canonicalize(repo.path()).unwrap();
    assert_eq!(seen.root, root.to_str().unwrap());
    // The equals form works too.
    let seen = seen_for(&sub, &["--repo=acme/widgets"]);
    assert_eq!(seen.path, "/v1/hub/hook/claude?repo=acme/widgets");
}

#[test]
fn no_git_means_no_root_header_and_a_bad_repo_is_ignored() {
    let plain = TempDir::new().unwrap();
    let seen = seen_for(plain.path(), &["--repo", "a b/c?x=1"]);
    assert_eq!(seen.path, "/v1/hub/hook/claude");
    assert_eq!(seen.root, "");
}

/// A saved connection for a real checkout whose `origin` can only be read by running `git`.
#[cfg(unix)]
fn connected_checkout(dir: &TempDir) -> std::path::PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let root = dir.path().join("repo");
    std::fs::create_dir(&root).unwrap();
    for args in [
        &["init", "--quiet"][..],
        &[
            "remote",
            "add",
            "origin",
            "https://github.com/acme/widgets.git",
        ],
    ] {
        let status = Command::new("git")
            .current_dir(&root)
            .args(args)
            .status()
            .unwrap();
        assert!(status.success());
    }
    let root = root.canonicalize().unwrap();
    let token = dir.path().join("client.token");
    std::fs::write(&token, "synthetic-private-token\n").unwrap();
    std::fs::set_permissions(&token, std::fs::Permissions::from_mode(0o600)).unwrap();
    let config = dir.path().join("agentscm");
    std::fs::create_dir_all(&config).unwrap();
    let saved = json!({"version": 1, "connections": [{
        "root": root,
        "origin_host": "github.com",
        "hub": "http://127.0.0.1:9",
        "repo": "acme/widgets",
        "token_file": token,
    }]});
    std::fs::write(config.join("connections.json"), saved.to_string()).unwrap();
    root
}

/// A directory holding a `git` that stalls for eight seconds in a child that outlives a kill of
/// the script, as a hung credential helper or a wedged network filesystem would.
#[cfg(unix)]
fn stalled_git(dir: &TempDir) -> std::path::PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let bin = dir.path().join("stalled-bin");
    std::fs::create_dir(&bin).unwrap();
    let script = bin.join("git");
    std::fs::write(&script, "#!/bin/sh\n/bin/sleep 8\n").unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    bin
}

#[cfg(unix)]
#[test]
fn a_stalled_git_cannot_hold_a_client_hook_past_its_deadline() {
    // (event, wall-clock bound): the 3 s limit, or 4.5 s at session start, plus process start on
    // a loaded host. Waiting for the stalled child would take 8 s.
    for (event, bound) in [("PreToolUse", 4000), ("SessionStart", 5500)] {
        let dir = home();
        let root = connected_checkout(&dir);
        let bin = stalled_git(&dir);
        let payload = json!({"hook_event_name": event, "session_id": "s1", "cwd": root});
        let path = bin.display().to_string();
        let run = run_with(
            &["claude", "--client-connection"],
            &[("PATH", &path)],
            &dir,
            payload.to_string().as_bytes(),
        );
        assert!(
            run.elapsed < Duration::from_millis(bound),
            "{event}: {:?}",
            run.elapsed
        );
        assert_unknown(&run, event, "");
        assert!(
            run.stdout.contains("coordination unknown"),
            "{event}: {}",
            run.stdout
        );
    }
}
