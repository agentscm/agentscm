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
            Reply::Status(status, body) => answer(&mut stream, status, body),
            Reply::Silent => thread::sleep(Duration::from_secs(8)),
        }
    });
    (url, seen)
}

fn answer(stream: &mut TcpStream, status: u16, body: &str) {
    let text = format!(
        "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\n\
         Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    let _ = stream.write_all(text.as_bytes());
}

/// Serves every request on a loopback port with what `respond` says for it, and reports each.
#[cfg(unix)]
fn serve(respond: impl Fn(&Seen) -> (u16, Value) + Send + 'static) -> (String, Receiver<Seen>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let (send, seen) = channel();
    thread::spawn(move || {
        for stream in listener.incoming() {
            let mut stream = stream.unwrap();
            let request = read_request(&mut stream);
            let (status, body) = respond(&request);
            let _ = send.send(request);
            answer(&mut stream, status, &body.to_string());
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
        .contains(&format!("coordination unknown ({reason}"));
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
        (
            Reply::Status(500, r#"{"error":"boom"}"#),
            "The Hub refused the request",
        ),
        (Reply::Status(401, "{}"), "The credential was not accepted"),
        (Reply::Status(403, "{}"), "This operation is not allowed"),
        (
            Reply::Status(200, "this is not json"),
            "The Hub reply could not be read",
        ),
        (
            Reply::Status(200, "[1,2]"),
            "The Hub reply could not be read",
        ),
        (
            Reply::Status(200, "\"text\""),
            "The Hub reply could not be read",
        ),
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
    assert_eq!(seen.path, "/v1/hub/hook/claude?repo=acme%2Fwidgets");
    let root = std::fs::canonicalize(repo.path()).unwrap();
    assert_eq!(seen.root, root.to_str().unwrap());
    // The equals form works too.
    let seen = seen_for(&sub, &["--repo=acme/widgets"]);
    assert_eq!(seen.path, "/v1/hub/hook/claude?repo=acme%2Fwidgets");
}

#[test]
fn no_git_means_no_root_header_and_a_bad_repo_is_ignored() {
    let plain = TempDir::new().unwrap();
    let seen = seen_for(plain.path(), &["--repo", "a b/c?x=1"]);
    assert_eq!(seen.path, "/v1/hub/hook/claude");
    assert_eq!(seen.root, "");
}

/// A saved connection to `hub` for a real checkout whose `origin` can only be read by running
/// `git`.
#[cfg(unix)]
fn connected_checkout(dir: &TempDir, hub: &str) -> std::path::PathBuf {
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
        "hub": hub,
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
    // The first launch of a newly built binary can take seconds (macOS checks it once), which is
    // not the hook's time: launch it once before timing.
    let warm = Command::new(env!("CARGO_BIN_EXE_agentscm"))
        .arg("--version")
        .output()
        .unwrap();
    assert!(warm.status.success());
    for (event, bound) in [("PreToolUse", 4000), ("SessionStart", 5500)] {
        let dir = home();
        let root = connected_checkout(&dir, "http://127.0.0.1:9");
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

/// What the stub Hub's poll replies, given the hook's own input hash.
#[cfg(unix)]
type Poll = fn(&Value) -> Value;
/// Extra environment for one hook run.
#[cfg(unix)]
type Env<'a> = &'a [(&'a str, &'a str)];

/// A stub Hub for one permission prompt: it records the wait, registers the request with
/// `register`'s status, and answers each poll with what `poll` makes of the hook's own hash.
#[cfg(unix)]
fn permission_hub(register: u16, poll: Poll) -> (String, Receiver<Seen>) {
    serve(move |seen| {
        let body: Value = serde_json::from_slice(&seen.body).unwrap_or(Value::Null);
        let hash = &body["input_hash"];
        if seen.path.starts_with("/v1/hub/permission/register") {
            let made = json!({"request_id": "00ff", "input_hash": hash, "state": "pending"});
            (register, made)
        } else if seen.path.starts_with("/v1/hub/permission/poll") {
            (200, poll(hash))
        } else {
            (200, json!({}))
        }
    })
}

/// Runs the plugin's Claude Code hook for a permission prompt (changed by `edit`) against `hub`;
/// returns the run and the paths it called.
#[cfg(unix)]
fn permission_prompt(
    hub: (String, Receiver<Seen>),
    env: Env,
    edit: fn(&mut Value),
) -> (Run, Vec<Seen>) {
    permission_prompt_in(home(), hub, env, edit)
}

/// [`permission_prompt`] in a home the caller made, whose agent token is `client.token` in it.
#[cfg(unix)]
fn permission_prompt_in(
    dir: TempDir,
    (hub, seen): (String, Receiver<Seen>),
    env: Env,
    edit: fn(&mut Value),
) -> (Run, Vec<Seen>) {
    prompt_of(CLAUDE, dir, (hub, seen), env, edit)
}

/// The hook commands the plugins write: Claude Code's, and Codex's for its approval request.
#[cfg(unix)]
const CLAUDE: &[&str] = &["claude", "--client-connection"];
#[cfg(unix)]
const CODEX: &[&str] = &["codex", "--client-connection", "--hold-limit", "600"];

/// [`permission_prompt_in`] for the hook command `hook`.
#[cfg(unix)]
fn prompt_of(
    hook: &[&str],
    dir: TempDir,
    (hub, seen): (String, Receiver<Seen>),
    env: Env,
    edit: fn(&mut Value),
) -> (Run, Vec<Seen>) {
    let root = connected_checkout(&dir, &hub);
    let mut payload = json!({"hook_event_name": "PermissionRequest", "session_id": "s1",
        "cwd": root, "tool_name": "Bash",
        "tool_input": {"command": "deploy --token hunter2hunter2"}});
    edit(&mut payload);
    let path = std::env::var("PATH").unwrap_or_default();
    let mut env = env.to_vec();
    env.push(("PATH", &path));
    let run = run_with(hook, &env, &dir, payload.to_string().as_bytes());
    assert_eq!(run.code, Some(0));
    (run, seen.try_iter().collect())
}

/// A poll reply for the stub Hub's one request.
#[cfg(unix)]
fn polled(hash: &Value, state: &str, decision: Option<&str>) -> Value {
    json!({"request_id": "00ff", "input_hash": hash, "state": state, "decision": decision})
}

#[cfg(unix)]
fn answered(hash: &Value, decision: &str) -> Value {
    polled(hash, "answered", Some(decision))
}

/// The hook prints a decision only for an exact answer to its own request, in a shape it knows.
/// Anything else leaves the prompt to the session: the ordinary reply, which has no decision.
#[cfg(unix)]
#[test]
fn a_permission_prompt_takes_only_an_exact_answer_to_its_own_request() {
    const ALLOW: &str = "{\"hookSpecificOutput\":{\"decision\":{\"behavior\":\"allow\"},\"hookEventName\":\"PermissionRequest\"}}\n";
    const DENY: &str = "{\"hookSpecificOutput\":{\"decision\":{\"behavior\":\"deny\",\"message\":\"Denied from the AgentSCM Hub by its person.\"},\"hookEventName\":\"PermissionRequest\"}}\n";
    const DENY_SAYING: &str = "{\"hookSpecificOutput\":{\"decision\":{\"behavior\":\"deny\",\"message\":\"Denied from the AgentSCM Hub by its person, who says: use the staging key\"},\"hookEventName\":\"PermissionRequest\"}}\n";
    let cases: [(Poll, &str); 8] = [
        (|hash| answered(hash, "allow"), ALLOW),
        (|hash| answered(hash, "deny"), DENY),
        // What the person typed with a deny reaches the agent.
        (
            |hash| {
                let mut reply = answered(hash, "deny");
                reply["reason"] = json!("use the staging key");
                reply
            },
            DENY_SAYING,
        ),
        // A choice this prompt did not offer fits nothing.
        (
            |hash| {
                let mut reply = answered(hash, "allow");
                reply["choice"] = json!(0);
                reply
            },
            "{}\n",
        ),
        // Another prompt's hash, input the hook never takes, a decision it does not know, and a
        // request that ended because the session moved on.
        (|_| answered(&json!("b".repeat(64)), "allow"), "{}\n"),
        (
            |hash| {
                let mut reply = answered(hash, "allow");
                reply["updatedInput"] = json!({"command": "rm -rf /"});
                reply
            },
            "{}\n",
        ),
        (|hash| answered(hash, "always"), "{}\n"),
        (|hash| polled(hash, "ended", None), "{}\n"),
    ];
    for (poll, printed) in cases {
        let (run, seen) = permission_prompt(permission_hub(200, poll), &[], |_| {});
        assert_eq!(run.stdout, printed);
        // What the Hub is told of the request: never the tool input, and no recognizable secret.
        let register = seen
            .iter()
            .find(|seen| seen.path.starts_with("/v1/hub/permission/register"))
            .expect("the prompt was offered");
        assert_eq!(register.authorization, "Bearer synthetic-private-token");
        let body: Value = serde_json::from_slice(&register.body).unwrap();
        assert_eq!(
            body.as_object().unwrap().keys().collect::<Vec<_>>(),
            [
                "content_hidden",
                "display",
                "folder",
                "input_hash",
                "missing",
                "session",
                "summary",
                "takes_reason",
                "tool"
            ]
        );
        assert_eq!(body["summary"], "run a shell command");
        assert_eq!(body["folder"], "repo");
        assert_eq!(body["display"], "deploy --token [REDACTED:flag_value]");
        assert_eq!(
            body["missing"],
            json!({"cut_from": null, "redacted": 1, "unshown": []}),
            "the person is told a secret was replaced"
        );
        assert!(
            seen.iter()
                .all(|seen| !String::from_utf8_lossy(&seen.body).contains("hunter2")),
        );
    }
}

/// Codex shows its own prompt only after the hook returns, so its approval request is offered
/// to the Hub only while nobody is at the computer. A person who is there, and a computer that
/// cannot say, get the prompt at once; a held prompt is given back before the hook's bound.
#[cfg(unix)]
#[test]
fn a_codex_approval_is_held_for_the_hub_only_while_nobody_is_at_the_computer() {
    const ALLOW: &str = "{\"hookSpecificOutput\":{\"decision\":{\"behavior\":\"allow\"},\"hookEventName\":\"PermissionRequest\"}}\n";
    const DENY_SAYING: &str = "{\"hookSpecificOutput\":{\"decision\":{\"behavior\":\"deny\",\"message\":\"Denied from the AgentSCM Hub by its person, who says: use the staging key\"},\"hookEventName\":\"PermissionRequest\"}}\n";
    const IDLE: &str = "AGENTSCM_TEST_IDLE_MS";
    let offered = |seen: &[Seen], path: &str| -> Vec<Value> {
        seen.iter()
            .filter(|seen| seen.path.starts_with(path))
            .map(|seen| serde_json::from_slice(&seen.body).unwrap())
            .collect()
    };
    // Nobody has touched the computer for ten minutes: the Hub is asked, and told that the
    // prompt is not on screen meanwhile. Its answer is printed in Codex's own form.
    let away: Env = &[(IDLE, "600000")];
    let hub = permission_hub(200, |hash| answered(hash, "allow"));
    let (run, seen) = prompt_of(CODEX, home(), hub, away, |_| {});
    assert_eq!(run.stdout, ALLOW);
    let registered = offered(&seen, "/v1/hub/permission/register");
    assert_eq!(registered.len(), 1);
    assert_eq!(registered[0]["held"], true);
    assert_eq!(registered[0]["takes_reason"], true);
    assert_eq!(
        registered[0]["display"],
        "deploy --token [REDACTED:flag_value]"
    );
    let hub = permission_hub(200, |hash| {
        let mut reply = answered(hash, "deny");
        reply["reason"] = json!("use the staging key");
        reply
    });
    let (run, _) = prompt_of(CODEX, home(), hub, away, |_| {});
    assert_eq!(run.stdout, DENY_SAYING);
    // Someone is at the computer, the computer cannot say, a subagent asks, or the plugin entry
    // is one from before it stated Codex's limit for the hook: the request is offered and taken
    // back at once, so the Hub lists it as answered at the computer, and Codex shows its own
    // prompt.
    let lead: fn(&mut Value) = |_| {};
    let subagent: fn(&mut Value) = |payload| payload["agent_id"] = json!("sub-1");
    let older = &["codex", "--client-connection"][..];
    for (hook, idle, edit) in [
        (CODEX, "1000", lead),
        (CODEX, "unknown", lead),
        (CODEX, "600000", subagent),
        (older, "600000", lead),
    ] {
        let hub = permission_hub(200, |hash| polled(hash, "ended", None));
        let (run, seen) = prompt_of(hook, home(), hub, &[(IDLE, idle)], edit);
        assert_eq!(run.stdout, "{}\n", "{idle}");
        let registered = offered(&seen, "/v1/hub/permission/register");
        assert_eq!(registered.len(), 1, "{idle}");
        assert_eq!(registered[0]["held"], true, "{idle}");
        let polls = offered(&seen, "/v1/hub/permission/poll");
        assert_eq!(polls.len(), 1, "{idle}");
        assert_eq!(polls[0]["release"], true, "{idle}");
        assert!(run.elapsed < Duration::from_secs(5), "{idle}");
    }
    // Nobody answers: before Codex would stop the hook it takes the prompt back, so the Hub
    // stops offering it, and prints no decision.
    let waiting = [
        (IDLE, "600000"),
        ("AGENTSCM_TEST_PERMISSION_BOUND_MS", "1500"),
    ];
    let hub = permission_hub(200, |hash| polled(hash, "pending", None));
    let (run, seen) = prompt_of(CODEX, home(), hub, &waiting, |_| {});
    assert_eq!(run.stdout, "{}\n");
    let polls = offered(&seen, "/v1/hub/permission/poll");
    assert_eq!(polls.last().unwrap()["release"], true);
    assert!(
        polls[..polls.len() - 1]
            .iter()
            .all(|poll| poll.get("release").is_none())
    );
    // The person comes back while the prompt is held: within a look or two the hook takes it
    // back from the Hub and Codex shows it.
    let warm = Command::new(env!("CARGO_BIN_EXE_agentscm"))
        .arg("--version")
        .output()
        .unwrap();
    assert!(warm.status.success());
    let idle_dir = TempDir::new().unwrap();
    let idle_file = idle_dir.path().join("idle-ms");
    std::fs::write(&idle_file, "600000").unwrap();
    let flip = {
        let idle_file = idle_file.clone();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_secs(1));
            std::fs::write(idle_file, "1000").unwrap();
        })
    };
    let hub = permission_hub(200, |hash| polled(hash, "pending", None));
    let idle_path = idle_file.display().to_string();
    let (run, seen) = prompt_of(CODEX, home(), hub, &[(IDLE, idle_path.as_str())], |_| {});
    flip.join().unwrap();
    assert_eq!(run.stdout, "{}\n");
    let polls = offered(&seen, "/v1/hub/permission/poll");
    let releases = polls.iter().filter(|poll| poll["release"] == true).count();
    assert_eq!(releases, 1, "{polls:?}");
    assert_eq!(polls.last().unwrap()["release"], true);
    assert!(polls.len() >= 2, "it held the prompt first: {polls:?}");
    assert!(run.elapsed < Duration::from_secs(6), "{:?}", run.elapsed);
    // The Hub cannot be reached while the prompt is held: nobody can answer it there, so the
    // hook gives it back at once instead of trying again.
    let unreachable = serve(|seen| {
        let body: Value = serde_json::from_slice(&seen.body).unwrap_or(Value::Null);
        let hash = &body["input_hash"];
        if seen.path.starts_with("/v1/hub/permission/register") {
            (
                200,
                json!({"request_id": "00ff", "input_hash": hash, "state": "pending"}),
            )
        } else if seen.path.starts_with("/v1/hub/permission/poll") && body["release"] == true {
            (200, polled(hash, "ended", None))
        } else if seen.path.starts_with("/v1/hub/permission/poll") {
            (503, json!({"error": "unavailable"}))
        } else {
            (200, json!({}))
        }
    });
    let (run, seen) = prompt_of(CODEX, home(), unreachable, away, |_| {});
    assert_eq!(run.stdout, "{}\n");
    let polls = offered(&seen, "/v1/hub/permission/poll");
    assert_eq!(polls.len(), 2, "{polls:?}");
    assert_eq!(polls[1]["release"], true);
    assert!(run.elapsed < Duration::from_secs(5), "{:?}", run.elapsed);
    // A Hub that does not know a held prompt is asked once and no more.
    let hub = permission_hub(400, |hash| answered(hash, "allow"));
    let (run, seen) = prompt_of(CODEX, home(), hub, away, |_| {});
    assert_eq!(run.stdout, "{}\n");
    assert_eq!(offered(&seen, "/v1/hub/permission/register").len(), 1);
    // Claude Code shows its dialog beside the hook, so its prompt is offered wherever its
    // person is, and is not said to be held.
    let hub = permission_hub(200, |hash| answered(hash, "allow"));
    let (run, seen) = prompt_of(CLAUDE, home(), hub, &[(IDLE, "1000")], |_| {});
    assert_eq!(run.stdout, ALLOW);
    assert!(
        offered(&seen, "/v1/hub/permission/register")[0]
            .get("held")
            .is_none()
    );
}

/// A choice is the host's own suggestion, returned whole. A Hub from before choices is still
/// offered the plain prompt.
#[cfg(unix)]
#[test]
fn a_permission_prompt_offers_the_hosts_own_choices_and_saves_the_one_picked() {
    fn suggest(payload: &mut Value) {
        payload["permission_suggestions"] = json!([
            {"type": "setMode", "mode": "bypassPermissions", "destination": "session"},
            {"type": "addRules", "behavior": "allow", "destination": "localSettings",
                "rules": [{"toolName": "Bash", "ruleContent": "deploy"}]},
        ]);
    }
    let registered = |seen: &[Seen]| -> Vec<Value> {
        seen.iter()
            .filter(|seen| seen.path.starts_with("/v1/hub/permission/register"))
            .map(|seen| serde_json::from_slice(&seen.body).unwrap())
            .collect()
    };
    let hub = permission_hub(200, |hash| {
        let mut reply = answered(hash, "allow");
        reply["choice"] = json!(0);
        reply
    });
    let (run, seen) = permission_prompt(hub, &[], suggest);
    // The mode this hook cannot describe is not offered, so the rule is choice 0.
    let printed: Value = serde_json::from_str(&run.stdout).unwrap();
    assert_eq!(
        printed["hookSpecificOutput"]["decision"],
        json!({"behavior": "allow", "updatedPermissions": [{"type": "addRules",
            "behavior": "allow", "destination": "localSettings",
            "rules": [{"toolName": "Bash", "ruleContent": "deploy"}]}]})
    );
    let bodies = registered(&seen);
    assert_eq!(
        (&bodies[0]["takes_reason"], &bodies[0]["choices"]),
        (
            &json!(true),
            &json!([{"label": "Allow, and do not ask again for this",
                "saves": "the rule Bash(deploy), in this checkout's local settings (.claude/settings.local.json)"}])
        )
    );
    // An older Hub refuses the fields it does not know, and then takes the plain prompt.
    let older = serve(|seen| {
        let body: Value = serde_json::from_slice(&seen.body).unwrap_or(Value::Null);
        let hash = &body["input_hash"];
        if seen.path.starts_with("/v1/hub/permission/register") {
            if body.get("takes_reason").is_some() || body.get("choices").is_some() {
                return (400, json!({"error": "invalid_request"}));
            }
            let made = json!({"request_id": "00ff", "input_hash": hash, "state": "pending"});
            (200, made)
        } else if seen.path.starts_with("/v1/hub/permission/poll") {
            (200, answered(hash, "allow"))
        } else {
            (200, json!({}))
        }
    });
    let (run, seen) = permission_prompt(older, &[], suggest);
    assert_eq!(
        run.stdout,
        "{\"hookSpecificOutput\":{\"decision\":{\"behavior\":\"allow\"},\"hookEventName\":\"PermissionRequest\"}}\n"
    );
    let bodies = registered(&seen);
    assert_eq!(
        bodies.len(),
        2,
        "the plain prompt was offered after the refusal"
    );
    assert!(bodies[1].get("choices").is_none() && bodies[1].get("takes_reason").is_none());
}

/// A Hub without these routes, a request nobody answers, the off switch, a subagent's prompt and
/// a question whose questions cannot be read all leave the prompt to the session.
#[cfg(unix)]
#[test]
fn a_permission_prompt_is_left_to_the_session_when_it_cannot_be_answered_from_the_hub() {
    let offered = |seen: &[Seen]| {
        seen.iter()
            .filter(|seen| seen.path.starts_with("/v1/hub/permission/"))
            .count()
    };
    // An older Hub: one refused registration, then nothing more.
    let hub = permission_hub(404, |hash| answered(hash, "allow"));
    let (run, seen) = permission_prompt(hub, &[], |_| {});
    assert_eq!((run.stdout.as_str(), offered(&seen)), ("{}\n", 1));
    // Nobody answers: the hook ends at its bound, here cut to a second and a half.
    let hub = permission_hub(200, |hash| polled(hash, "pending", None));
    let bound = [("AGENTSCM_TEST_PERMISSION_BOUND_MS", "1500")];
    let (run, seen) = permission_prompt(hub, &bound, |_| {});
    assert_eq!(run.stdout, "{}\n");
    assert!(offered(&seen) >= 2, "it registered and polled");
    assert!(run.elapsed < Duration::from_secs(10), "{:?}", run.elapsed);
    // Never offered, even to a Hub that would allow it.
    let never_offered = |env: Env, edit: fn(&mut Value)| {
        let hub = permission_hub(200, |hash| answered(hash, "allow"));
        let (run, seen) = permission_prompt(hub, env, edit);
        assert_eq!((run.stdout.as_str(), offered(&seen)), ("{}\n", 0));
        assert!(
            seen.iter()
                .any(|seen| seen.path.starts_with("/v1/hub/hook/claude"))
        );
    };
    never_offered(&[("AGENTSCM_REMOTE_ANSWERS", "0")], |_| {});
    never_offered(&[], |payload| {
        payload["tool_name"] = json!("AskUserQuestion")
    });
    // A subagent's prompt: in a terminal the host shows its dialog only after the hook returns,
    // so it is left to the session. Started by an app, the host asks the app while the hook
    // waits, so it is offered like any other.
    let subagent: fn(&mut Value) = |payload| payload["agent_id"] = json!("sub-1");
    never_offered(&[("CLAUDE_CODE_ENTRYPOINT", "cli")], subagent);
    let hub = permission_hub(200, |hash| answered(hash, "deny"));
    let app = [("CLAUDE_CODE_ENTRYPOINT", "claude-desktop")];
    let (run, seen) = permission_prompt(hub, &app, subagent);
    assert!(
        run.stdout.contains("\"behavior\":\"deny\""),
        "{}",
        run.stdout
    );
    let registered = seen
        .iter()
        .find(|seen| seen.path.starts_with("/v1/hub/permission/register"))
        .expect("the subagent's prompt was offered");
    let body: Value = serde_json::from_slice(&registered.body).unwrap();
    assert_eq!(body["agent_id"], "sub-1");
}

/// The agent token ends while the prompt is up. The hook goes on under the token this computer
/// holds by then, and leaves the prompt to the session when there is no other.
#[cfg(unix)]
#[test]
fn a_permission_prompt_goes_on_under_the_renewed_agent_token() {
    const OLD: &str = "Bearer synthetic-private-token";
    let polls = |seen: &[Seen]| -> Vec<String> {
        seen.iter()
            .filter(|seen| seen.path.starts_with("/v1/hub/permission/poll"))
            .map(|seen| seen.authorization.clone())
            .collect()
    };
    // The stub refuses the first token on a poll; `renew` says whether this computer holds a
    // new one by then.
    let ended = |renew: bool| {
        let dir = home();
        let token = dir.path().join("client.token");
        let hub = serve(move |seen| {
            let body: Value = serde_json::from_slice(&seen.body).unwrap_or(Value::Null);
            let hash = &body["input_hash"];
            if seen.path.starts_with("/v1/hub/permission/register") {
                let made = json!({"request_id": "00ff", "input_hash": hash, "state": "pending"});
                (200, made)
            } else if !seen.path.starts_with("/v1/hub/permission/poll") {
                (200, json!({}))
            } else if seen.authorization == OLD {
                if renew {
                    std::fs::write(&token, "renewed-private-token\n").unwrap();
                }
                (401, json!({"error": "unauthorized"}))
            } else {
                (200, answered(hash, "allow"))
            }
        });
        permission_prompt_in(dir, hub, &[], |_| {})
    };
    let (run, seen) = ended(true);
    assert!(
        run.stdout.contains("\"behavior\":\"allow\""),
        "{}",
        run.stdout
    );
    assert_eq!(polls(&seen), [OLD, "Bearer renewed-private-token"]);
    let (run, seen) = ended(false);
    assert_eq!(run.stdout, "{}\n");
    assert_eq!(polls(&seen), [OLD]);
}

/// A Hub that restarted does not know the request. The hook offers the prompt again with the id
/// it holds, and leaves a Hub alone that forgets it again at once.
#[cfg(unix)]
#[test]
fn a_permission_prompt_is_offered_again_to_a_hub_that_forgot_it() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    // The stub answers the first `forgets` polls as a Hub does that never heard of the request.
    let forgetting = |forgets: usize| {
        let polls = AtomicUsize::new(0);
        serve(move |seen| {
            let body: Value = serde_json::from_slice(&seen.body).unwrap_or(Value::Null);
            let hash = &body["input_hash"];
            if seen.path.starts_with("/v1/hub/permission/register") {
                let made = json!({"request_id": "00ff", "input_hash": hash, "state": "pending"});
                (200, made)
            } else if !seen.path.starts_with("/v1/hub/permission/poll") {
                (200, json!({}))
            } else if polls.fetch_add(1, Ordering::Relaxed) < forgets {
                (404, json!({"error": "not_found"}))
            } else {
                (200, answered(hash, "allow"))
            }
        })
    };
    let offers = |seen: &[Seen]| -> Vec<Value> {
        seen.iter()
            .filter(|seen| seen.path.starts_with("/v1/hub/permission/register"))
            .map(|seen| serde_json::from_slice(&seen.body).unwrap())
            .collect()
    };
    let (run, seen) = permission_prompt(forgetting(1), &[], |_| {});
    assert!(
        run.stdout.contains("\"behavior\":\"allow\""),
        "{}",
        run.stdout
    );
    let offered = offers(&seen);
    assert_eq!(offered.len(), 2);
    assert_eq!(
        (offered[0].get("again"), &offered[1]["again"]),
        (None, &json!("00ff"))
    );
    let (run, seen) = permission_prompt(forgetting(2), &[], |_| {});
    assert_eq!(run.stdout, "{}\n");
    assert_eq!(offers(&seen).len(), 2);
}

/// A stub Hub that answers every hook with an empty reply.
#[cfg(unix)]
fn empty_hub() -> (String, Receiver<Seen>) {
    serve(|_| (200, json!({})))
}

#[cfg(unix)]
fn git_in(root: &std::path::Path, args: &[&str]) {
    let status = Command::new("git")
        .current_dir(root)
        .args(["-c", "user.name=t", "-c", "user.email=t@example.test"])
        .args(args)
        .status()
        .unwrap();
    assert!(status.success(), "git {args:?}");
}

/// One shell tool event for `hook` (`CLAUDE` or `CODEX`); returns the request the Hub saw for it.
#[cfg(unix)]
fn shell_event(
    hook: &[&str],
    dir: &TempDir,
    seen: &Receiver<Seen>,
    root: &std::path::Path,
    call: (&str, &str, &str),
    path: &str,
) -> Option<Value> {
    let input = json!({"command": "echo SYNTHETIC-COMMAND-TEXT > out.txt"});
    shell_event_with(hook, dir, seen, root, call, path, input)
}

#[cfg(unix)]
fn shell_event_with(
    hook: &[&str],
    dir: &TempDir,
    seen: &Receiver<Seen>,
    root: &std::path::Path,
    (event, tool, id): (&str, &str, &str),
    path: &str,
    input: Value,
) -> Option<Value> {
    let payload = json!({"hook_event_name": event, "session_id": "s1", "cwd": root,
        "tool_name": tool, "tool_use_id": id,
        "tool_input": input,
        "tool_response": {"stdout": "SYNTHETIC-OUTPUT-TEXT"}});
    let run = run_with(hook, &[("PATH", path)], dir, payload.to_string().as_bytes());
    assert_eq!(run.code, Some(0), "{event}");
    assert!(run.json().is_object());
    let seen = seen.try_iter().last()?;
    let text = String::from_utf8_lossy(&seen.body).into_owned();
    assert!(!text.contains("SYNTHETIC-"), "{text}");
    Some(serde_json::from_slice(&seen.body).unwrap())
}

/// A `git` that behaves except that `status`, which a snapshot needs and routing does not,
/// stalls for eight seconds.
#[cfg(unix)]
fn status_stalling_git(dir: &TempDir) -> String {
    use std::os::unix::fs::PermissionsExt;
    let real = String::from_utf8(
        Command::new("sh")
            .args(["-c", "command -v git"])
            .output()
            .unwrap()
            .stdout,
    )
    .unwrap();
    let bin = dir.path().join("status-stall-bin");
    std::fs::create_dir(&bin).unwrap();
    let script = bin.join("git");
    let text = format!(
        "#!/bin/sh\ncase \"$*\" in *status*) /bin/sleep 8;; *) exec {} \"$@\";; esac\n",
        real.trim()
    );
    std::fs::write(&script, text).unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    bin.display().to_string()
}

#[cfg(unix)]
fn real_path() -> String {
    std::env::var("PATH").unwrap_or_default()
}

#[cfg(unix)]
#[test]
fn a_shell_call_that_writes_a_file_reports_the_path() {
    for (hook, tool) in [
        (CLAUDE, "Bash"),
        (CODEX, "Bash"),
        (CODEX, "exec_command"),
        (CODEX, "local_shell"),
    ] {
        let dir = home();
        let (hub, seen) = empty_hub();
        let root = connected_checkout(&dir, &hub);
        let path = real_path();
        shell_event(hook, &dir, &seen, &root, ("PreToolUse", tool, "t1"), &path);
        std::fs::write(root.join("out.txt"), "x").unwrap();
        let post = shell_event(hook, &dir, &seen, &root, ("PostToolUse", tool, "t1"), &path)
            .expect("a post request");
        assert_eq!(
            post["observed"],
            json!({"outcome": "changed", "paths": ["out.txt"], "truncated": false}),
            "{post}"
        );
    }
}

#[cfg(unix)]
#[test]
fn a_commit_made_inside_the_call_reports_its_paths() {
    let dir = home();
    let (hub, seen) = empty_hub();
    let root = connected_checkout(&dir, &hub);
    git_in(&root, &["commit", "--allow-empty", "-m", "base", "--quiet"]);
    let path = real_path();
    let at = |event| (event, "Bash", "t1");
    shell_event(CLAUDE, &dir, &seen, &root, at("PreToolUse"), &path);
    std::fs::write(root.join("done.txt"), "x").unwrap();
    git_in(&root, &["add", "done.txt"]);
    git_in(&root, &["commit", "-m", "work", "--quiet"]);
    let post = shell_event(CLAUDE, &dir, &seen, &root, at("PostToolUse"), &path).unwrap();
    assert_eq!(post["observed"]["outcome"], "changed", "{post}");
    assert_eq!(post["observed"]["paths"], json!(["done.txt"]), "{post}");
}

#[cfg(unix)]
#[test]
fn a_shell_call_that_changes_nothing_sends_no_observation() {
    let dir = home();
    let (hub, seen) = empty_hub();
    let root = connected_checkout(&dir, &hub);
    let path = real_path();
    let at = |event| (event, "Bash", "t1");
    shell_event(CLAUDE, &dir, &seen, &root, at("PreToolUse"), &path);
    let post = shell_event(CLAUDE, &dir, &seen, &root, at("PostToolUse"), &path).unwrap();
    assert!(post.get("observed").is_none(), "{post}");
    assert_eq!(post["hook_event_name"], "PostToolUse");
}

#[cfg(unix)]
#[test]
fn a_post_without_a_pre_is_unknown_and_a_snapshot_is_used_once() {
    let dir = home();
    let (hub, seen) = empty_hub();
    let root = connected_checkout(&dir, &hub);
    let path = real_path();
    let at = |event, id| (event, "Bash", id);
    let post = shell_event(CLAUDE, &dir, &seen, &root, at("PostToolUse", "t0"), &path).unwrap();
    assert_eq!(
        post["observed"],
        json!({"outcome": "unknown", "reason": "missing_snapshot"})
    );
    shell_event(CLAUDE, &dir, &seen, &root, at("PreToolUse", "t1"), &path);
    let first = shell_event(CLAUDE, &dir, &seen, &root, at("PostToolUse", "t1"), &path).unwrap();
    assert!(first.get("observed").is_none(), "{first}");
    let again = shell_event(CLAUDE, &dir, &seen, &root, at("PostToolUse", "t1"), &path).unwrap();
    assert_eq!(again["observed"]["reason"], "missing_snapshot", "{again}");
}

#[cfg(unix)]
#[test]
fn a_background_shell_call_is_unknown_not_unchanged() {
    let dir = home();
    let (hub, seen) = empty_hub();
    let root = connected_checkout(&dir, &hub);
    let path = real_path();
    let input = json!({"command": "sleep 1; echo x > out.txt", "run_in_background": true});
    let at = |event| (event, "Bash", "bg1");
    let pre = shell_event_with(
        CLAUDE,
        &dir,
        &seen,
        &root,
        at("PreToolUse"),
        &path,
        input.clone(),
    );
    assert!(pre.is_none_or(|pre| pre.get("observed").is_none()));
    let post = shell_event_with(CLAUDE, &dir, &seen, &root, at("PostToolUse"), &path, input)
        .expect("a post request");
    assert_eq!(
        post["observed"],
        json!({"outcome": "unknown", "reason": "background"}),
        "{post}"
    );
}

#[cfg(unix)]
#[test]
fn a_call_without_a_tool_use_id_is_unknown() {
    let dir = home();
    let (hub, seen) = empty_hub();
    let root = connected_checkout(&dir, &hub);
    let payload = json!({"hook_event_name": "PostToolUse", "session_id": "s1", "cwd": root,
        "tool_name": "Bash", "tool_input": {"command": "true"}});
    let path = real_path();
    let run = run_with(
        CLAUDE,
        &[("PATH", &path)],
        &dir,
        payload.to_string().as_bytes(),
    );
    assert_eq!(run.code, Some(0));
    let seen = seen.try_iter().last().unwrap();
    let body: Value = serde_json::from_slice(&seen.body).unwrap();
    assert_eq!(
        body["observed"],
        json!({"outcome": "unknown", "reason": "no_tool_use_id"})
    );
}

#[cfg(unix)]
#[test]
fn interleaved_calls_keep_their_own_snapshots() {
    let dir = home();
    let (hub, seen) = empty_hub();
    let root = connected_checkout(&dir, &hub);
    let path = real_path();
    let at = |event, id| (event, "Bash", id);
    shell_event(CLAUDE, &dir, &seen, &root, at("PreToolUse", "a"), &path);
    std::fs::write(root.join("one.txt"), "1").unwrap();
    shell_event(CLAUDE, &dir, &seen, &root, at("PreToolUse", "b"), &path);
    std::fs::write(root.join("two.txt"), "2").unwrap();
    let b = shell_event(CLAUDE, &dir, &seen, &root, at("PostToolUse", "b"), &path).unwrap();
    assert_eq!(b["observed"]["paths"], json!(["two.txt"]), "{b}");
    let a = shell_event(CLAUDE, &dir, &seen, &root, at("PostToolUse", "a"), &path).unwrap();
    assert_eq!(a["observed"]["paths"], json!(["one.txt", "two.txt"]), "{a}");
}

#[cfg(unix)]
#[test]
fn a_stalled_git_after_a_shell_call_is_unknown_within_the_limit() {
    let warm = Command::new(env!("CARGO_BIN_EXE_agentscm"))
        .arg("--version")
        .output()
        .unwrap();
    assert!(warm.status.success());
    let dir = home();
    let (hub, seen) = empty_hub();
    let root = connected_checkout(&dir, &hub);
    let real = real_path();
    let at = |event| (event, "Bash", "t1");
    shell_event(CLAUDE, &dir, &seen, &root, at("PreToolUse"), &real);
    let stalled = status_stalling_git(&dir);
    let started = Instant::now();
    let post = shell_event(CLAUDE, &dir, &seen, &root, at("PostToolUse"), &stalled);
    assert!(started.elapsed() < Duration::from_millis(4000));
    let post = post.expect("the Hub still hears of the call");
    assert_eq!(
        post["observed"],
        json!({"outcome": "unknown", "reason": "git_failed"})
    );
}

#[cfg(unix)]
#[test]
fn a_failed_pre_snapshot_is_reported_as_such() {
    let dir = home();
    let (hub, seen) = empty_hub();
    let root = connected_checkout(&dir, &hub);
    let stalled = status_stalling_git(&dir);
    let at = |event| (event, "Bash", "t1");
    shell_event(CLAUDE, &dir, &seen, &root, at("PreToolUse"), &stalled);
    let post = shell_event(CLAUDE, &dir, &seen, &root, at("PostToolUse"), &real_path()).unwrap();
    assert_eq!(
        post["observed"],
        json!({"outcome": "unknown", "reason": "pre_snapshot_failed"}),
        "{post}"
    );
}

#[cfg(unix)]
#[test]
fn only_a_shell_tool_failure_reaches_the_hub() {
    let dir = home();
    let (hub, seen) = empty_hub();
    let root = connected_checkout(&dir, &hub);
    let path = real_path();
    let at = |tool, id| ("PostToolUseFailure", tool, id);
    let other = shell_event(CLAUDE, &dir, &seen, &root, at("Edit", "t2"), &path);
    assert!(other.is_none(), "{other:?}");
    let shell = shell_event(CLAUDE, &dir, &seen, &root, at("Bash", "t3"), &path).unwrap();
    assert_eq!(shell["hook_event_name"], "PostToolUseFailure");
    assert_eq!(shell["observed"]["reason"], "missing_snapshot", "{shell}");
}
