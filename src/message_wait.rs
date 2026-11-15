//! `agentscm hook-wait claude`: the background hook that lets a message sent from the Hub page
//! reach a Claude Code session within seconds, even while it sits idle.
//!
//! Claude Code runs it at session start and after every turn as an `asyncRewake` hook: it runs
//! in the background, and when it exits with code 2 its stderr wakes the session as a new turn.
//! It holds one request at a time open at the Hub (`/v1/hub/agent-messages/wait`) for this
//! session; when a message comes, or a coordination request to Work the session leads waits for
//! its acknowledgement, it prints the Hub's notice and exits 2. Only one waits per
//! session: a later start finds the lock held and exits at once. It ends quietly when its time is
//! up, when the process that started it is gone or the session's `SessionEnd` hook
//! (`hook-wait claude --end`) left its end marker, when the checkout is not connected, when the
//! person's mode is `ignore`, or when the Hub refuses, lacks the route or stays unreachable.
//! Every failure exits 0, so the session is never woken by an error. On Windows it exits at once:
//! it cannot tell there when its session has ended.
//!
//! `agentscm hook-wait codex` is the same waiter for a Codex chat, run as an `async` hook. A
//! finished background hook starts no turn in Codex, so on a notice it queues the notice into
//! the chat with `codex queue`, which starts it as a turn once the chat is idle, and exits 0.
//!
//! `agentscm hook-wait copilot --notification` waits only from Copilot CLI's asynchronous hook.
//! It returns a notice as `additionalContext` on stdout, which can start processing while idle.
//! There is no waiter until a notification fires; another notification rearms it after delivery.
use std::fs::{DirBuilder, File, TryLockError};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use agentscm::hub::agent_messages::{FRAME, REQUEST_FRAME, WAIT_SECS, Waker};
use agentscm::hub::reply;
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::client_config;

/// How long one waiter lives, so a session left idle over a weekend is still reached; the
/// plugin's hook timeout is a little longer, so the waiter leaves on its own and quietly.
pub const MAX_LIFE: Duration = Duration::from_secs(7 * 24 * 3600);
/// Failing this long without one answer from the Hub ends the waiter; the next turn starts one.
const UNREACHABLE: Duration = Duration::from_secs(15 * 60);
const STDIN_BYTES: u64 = 1 << 20;

#[derive(Debug, clap::Args)]
pub struct HookWaitArgs {
    /// The agent whose hook this is: `claude` (exit 2), `codex` (`codex queue`) or
    /// `copilot` (notification context); any other waits for nothing.
    pub agent: String,
    /// SessionEnd: tell this session's waiter to leave, then exit at once.
    #[arg(long)]
    pub end: bool,
    /// Installed only on Copilot's asynchronous notification event, whose payload has no name.
    #[arg(long)]
    pub notification: bool,
}

/// Never returns: Claude wakes with exit 2; other hosts and every failure exit 0.
pub async fn run(args: HookWaitArgs) -> ! {
    let Some(agent) = waiter(&args.agent).filter(|_| cfg!(unix)) else {
        std::process::exit(0)
    };
    let Some(payload) = input() else {
        std::process::exit(0)
    };
    // Inherited Claude hooks must neither wait for nor end another host's session.
    if crate::client_session::hook_agent(&args.agent, &payload).await != Some(args.agent.as_str()) {
        std::process::exit(0)
    }
    if args.end {
        // The session ended while its process may live on (/clear), so its waiter cannot see it.
        if let Some(dir) = wait_dir()
            && let Some(session) = session_of(&payload, agent)
        {
            let _ = File::create(marker(&dir, &session));
        }
        std::process::exit(0)
    }
    let found = waited(agent, args.notification, &payload).await;
    std::process::exit(match agent {
        Waker::Claude => finish(found.map(|(_, text)| text), &mut std::io::stderr()),
        Waker::Codex => found.map_or(0, |(session, text)| queue(&session, &text)),
        Waker::Copilot => finish_copilot(found.map(|(_, text)| text), &mut std::io::stdout()),
    })
}

/// Only hosts with a documented background delivery mechanism may open a wait.
fn waiter(agent: &str) -> Option<Waker> {
    match agent {
        "claude" => Some(Waker::Claude),
        "codex" => Some(Waker::Codex),
        "copilot" => Some(Waker::Copilot),
        _ => None,
    }
}

/// The Hub's own framed notices: a message, or a coordination request to the session's Work.
fn framed(text: &str) -> bool {
    text.starts_with(FRAME) || text.starts_with(REQUEST_FRAME)
}

fn runs(program: &str) -> bool {
    std::process::Command::new(program)
        .arg("--version")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

/// `codex queue` for the chat; the `=` forms keep a value from being read as an option.
fn queue_args(session: &str, text: &str) -> [String; 3] {
    [
        "queue".into(),
        format!("--thread={session}"),
        format!("--message={text}"),
    ]
}

/// Queues the Hub's framed notice into Codex chat `session`, which Codex starts as a turn once
/// the chat is idle; anything else is dropped. Always 0: a background hook's exit wakes nothing.
fn queue(session: &str, text: &str) -> i32 {
    if framed(text) {
        let _ = std::process::Command::new("codex")
            .args(queue_args(session, text))
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
    }
    0
}

/// Writes the notice and returns 2 only for the Hub's framed notice (a message, or a coordination
/// request to the session's Work); anything else is 0, silent.
fn finish(found: Option<String>, err: &mut impl Write) -> i32 {
    match found.filter(|text| framed(text)) {
        Some(text) => {
            // A write error still wakes the session; the notice is then empty.
            let _ = writeln!(err, "{text}");
            let _ = err.flush();
            2
        }
        None => 0,
    }
}

/// Copilot notification output can wake an idle session; plain text or exit 2 cannot.
fn finish_copilot(found: Option<String>, out: &mut impl Write) -> i32 {
    if let Some(text) = found.filter(|text| framed(text)) {
        let context = serde_json::json!({"additionalContext": text});
        let _ = writeln!(out, "{context}");
        let _ = out.flush();
    }
    0
}

/// The session id from the hook's input; `None` for a subagent's call, which never waits.
fn session_of(payload: &Value, agent: Waker) -> Option<String> {
    if payload.get("agent_id").is_some() || payload.get("agentId").is_some() {
        return None;
    }
    let field = match agent {
        Waker::Copilot => "sessionId",
        Waker::Claude | Waker::Codex => "session_id",
    };
    payload[field]
        .as_str()
        .filter(|s| !s.is_empty() && s.len() <= 128 && !s.chars().any(char::is_control))
        .map(str::to_owned)
}

/// A synchronous Copilot hook must never hold a long wait, even if invoked by a bad config.
fn can_wait(payload: &Value, agent: Waker, notification: bool) -> bool {
    agent != Waker::Copilot
        || (notification
            && payload
                .get("hook_event_name")
                .is_none_or(|event| event == "Notification")
            && payload["notification_type"]
                .as_str()
                .is_some_and(|kind| !kind.is_empty()))
}

/// The host's hook input, not a Hub reply.
fn input() -> Option<Value> {
    let mut input = String::new();
    std::io::stdin()
        .take(STDIN_BYTES)
        .read_to_string(&mut input)
        .ok()?;
    input.parse().ok()
}

fn wait_dir() -> Option<PathBuf> {
    Some(
        crate::hub_cli::config_dir()
            .ok()?
            .join("agentscm")
            .join("hook-wait"),
    )
}

/// This session's file name in the wait directory; the session id itself is not a safe name.
fn file_name(session: &str) -> String {
    let name = format!("{:x}", Sha256::digest(session.as_bytes()));
    name[..32].to_owned()
}

/// Present once the session ended; its waiter leaves before its next request.
fn marker(dir: &Path, session: &str) -> PathBuf {
    dir.join(format!("{}.ended", file_name(session)))
}

/// The session and the Hub's notice, once one comes; the agent names its delivery mechanism.
async fn waited(agent: Waker, notification: bool, payload: &Value) -> Option<(String, String)> {
    let session = session_of(payload, agent)?;
    if !can_wait(payload, agent, notification) {
        return None;
    }
    let cwd = Path::new(payload["cwd"].as_str()?);
    let connections = client_config::load().ok()?;
    let (connection, checkout) = client_config::connection_for_checkout(&connections, cwd)?;
    if !client_config::is_current_in(connection, &checkout) {
        return None;
    }
    let dir = wait_dir()?;
    // Held until this process exits; a second waiter for the session leaves at once.
    let _lock = lock(&dir, &session)?;
    // A resumed session keeps its id, so an old end marker must not stop its new waiter.
    // A duplicate waiter must not remove the marker that ends the first one.
    let ended = marker(&dir, &session);
    let _ = std::fs::remove_file(&ended);
    // Without a runnable `codex` a notice could not be queued, so this chat is not waited for.
    if agent == Waker::Codex && !runs("codex") {
        return None;
    }
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(Duration::from_secs(10))
        .timeout(Duration::from_secs(WAIT_SECS + 15))
        .build()
        .ok()?;
    let started = parent();
    let request = Wait {
        client: &client,
        url: format!("{}/v1/hub/agent-messages/wait", connection.hub),
        body: wait_body(&session, agent),
        token_file: &connection.token_file,
    };
    let text = request
        .until(
            MAX_LIFE,
            || parent() == started && !ended.exists(),
            Duration::from_secs(5),
        )
        .await?;
    Some((session, text))
}

/// Preserve the original Claude body; other waiters identify their delivery mechanism.
fn wait_body(session: &str, agent: Waker) -> String {
    if agent == Waker::Claude {
        serde_json::json!({ "session": session })
    } else {
        serde_json::json!({ "session": session, "agent": agent })
    }
    .to_string()
}

/// The Hub's answer when a message came.
#[derive(serde::Deserialize)]
struct Notice {
    text: String,
}

struct Wait<'a> {
    client: &'a reqwest::Client,
    url: String,
    body: String,
    token_file: &'a Path,
}

impl Wait<'_> {
    /// Asks the Hub again and again until a notice comes, `life` is up, `alive` turns false, or
    /// the Hub says there is nothing to wait for. Failures back off from `pause` to a minute.
    async fn until(
        &self,
        life: Duration,
        alive: impl Fn() -> bool,
        pause: Duration,
    ) -> Option<String> {
        let started = Instant::now();
        let mut answered = Instant::now();
        let mut backoff = pause;
        loop {
            if !alive() {
                return None;
            }
            // Read each time: the session's other hooks renew the token file.
            let token = crate::client_cli::read_token(self.token_file).ok()?;
            let sent = self
                .client
                .post(&self.url)
                .bearer_auth(token)
                .header(reqwest::header::CONTENT_TYPE, "application/json")
                .body(self.body.clone())
                .send()
                .await;
            match sent.map(|reply| (reply.status().as_u16(), reply)) {
                Ok((200, answer)) => {
                    let bytes = answer.bytes().await.ok()?;
                    // SessionEnd or parent death can happen while the request is in flight.
                    if !alive() {
                        return None;
                    }
                    return reply::decode_slice::<Notice>(&bytes).ok().map(|n| n.text);
                }
                // Nothing came in this request: ask again at once.
                Ok((204, _)) => {
                    answered = Instant::now();
                    backoff = pause;
                    if started.elapsed() >= life {
                        return None;
                    }
                    continue;
                }
                // Signed out, a read-only token, Ignore, a Hub without the route, or one that
                // refuses the request (an older Hub may not recognize this waiter).
                Ok((400 | 401 | 403 | 404 | 405 | 410, _)) => return None,
                // Another wait for this session is still open at the Hub: ask again shortly.
                Ok((409, _)) => {
                    answered = Instant::now();
                    backoff = pause;
                }
                // 429 and other answers: the Hub is there but busy.
                Ok(_) => answered = Instant::now(),
                Err(_) if answered.elapsed() >= UNREACHABLE => return None,
                Err(_) => {}
            }
            if started.elapsed() >= life {
                return None;
            }
            tokio::time::sleep(backoff).await;
            backoff = (backoff * 2).min(Duration::from_secs(60));
        }
    }
}

/// The process that started this one; when it changes, the session is gone.
#[cfg(unix)]
fn parent() -> u32 {
    std::os::unix::process::parent_id()
}

/// Windows never starts a waiter ([`run`]), so this is never compared there.
#[cfg(not(unix))]
fn parent() -> u32 {
    0
}

/// An exclusive lock on this session's file in `dir`, or `None` when another waiter holds it.
fn lock(dir: &Path, session: &str) -> Option<File> {
    let mut builder = DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    std::os::unix::fs::DirBuilderExt::mode(&mut builder, 0o700);
    builder.create(dir).ok()?;
    // Files of sessions long gone. Each start rewrites its lock file, so a live waiter's file
    // is younger than its life.
    for entry in std::fs::read_dir(dir).into_iter().flatten().flatten() {
        let stale = entry
            .metadata()
            .and_then(|m| m.modified())
            .ok()
            .and_then(|at| at.elapsed().ok())
            .is_some_and(|age| age > MAX_LIFE + Duration::from_secs(24 * 3600));
        if stale {
            let _ = std::fs::remove_file(entry.path());
        }
    }
    let file = File::create(dir.join(file_name(session))).ok()?;
    match file.try_lock() {
        Ok(()) => Some(file),
        Err(TryLockError::WouldBlock | TryLockError::Error(_)) => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    #[test]
    fn one_waiter_per_session() {
        let dir = tempfile::tempdir().unwrap();
        let held = lock(dir.path(), "s1").expect("first waiter");
        assert!(lock(dir.path(), "s1").is_none(), "a second waiter leaves");
        assert!(
            lock(dir.path(), "s2").is_some(),
            "another session waits too"
        );
        drop(held);
        assert!(
            lock(dir.path(), "s1").is_some(),
            "free again once the first exits"
        );
    }

    #[test]
    fn a_subagent_or_a_bad_session_never_waits() {
        for (agent, field, wrong_field) in [
            (Waker::Claude, "session_id", "sessionId"),
            (Waker::Codex, "session_id", "sessionId"),
            (Waker::Copilot, "sessionId", "session_id"),
        ] {
            let mut payload = serde_json::json!({field: "abc"});
            assert_eq!(session_of(&payload, agent).as_deref(), Some("abc"));
            for helper in ["agent_id", "agentId"] {
                for value in [Value::String("child".into()), Value::Null] {
                    payload[helper] = value;
                    assert_eq!(session_of(&payload, agent), None);
                }
                payload.as_object_mut().unwrap().remove(helper);
            }
            for bad in ["".to_owned(), "a\nb".into(), "x".repeat(129)] {
                payload[field] = Value::String(bad);
                assert_eq!(session_of(&payload, agent), None);
            }
            assert_eq!(
                session_of(&serde_json::json!({wrong_field: "abc"}), agent),
                None
            );
            assert_eq!(session_of(&serde_json::json!({}), agent), None);
        }
    }

    #[test]
    fn copilot_waits_only_from_an_asynchronous_notification() {
        let notice: Value = serde_json::from_str(include_str!(
            "../tests/fixtures/hub_hooks/copilot-notification.json"
        ))
        .unwrap();
        assert!(can_wait(&notice, Waker::Copilot, true));
        assert!(!can_wait(&notice, Waker::Copilot, false));
        for event in ["SessionStart", "Stop", "SessionEnd", "PostToolUse", ""] {
            let mut other = notice.clone();
            other["hook_event_name"] = Value::String(event.into());
            assert!(!can_wait(&other, Waker::Copilot, true));
        }
        let mut incomplete = notice;
        incomplete
            .as_object_mut()
            .unwrap()
            .remove("notification_type");
        assert!(!can_wait(&incomplete, Waker::Copilot, true));
        for unsupported in ["cursor", "gemini", "generic", "unknown", ""] {
            assert_eq!(waiter(unsupported), None, "{unsupported}");
        }
        for (name, agent) in [
            ("claude", Waker::Claude),
            ("codex", Waker::Codex),
            ("copilot", Waker::Copilot),
        ] {
            assert_eq!(waiter(name), Some(agent));
        }
    }

    #[test]
    fn copilot_returns_only_framed_context_as_json_with_exit_zero() {
        for frame in [FRAME, REQUEST_FRAME] {
            let notice = format!("{frame} msg_1: \"quoted\"\nnext line\\end");
            let mut out = Vec::new();
            assert_eq!(finish_copilot(Some(notice.clone()), &mut out), 0);
            assert_eq!(
                serde_json::from_slice::<Value>(&out).unwrap(),
                serde_json::json!({"additionalContext": notice})
            );
        }
        let mut out = Vec::new();
        assert_eq!(finish_copilot(None, &mut out), 0);
        assert_eq!(finish_copilot(Some("unframed".into()), &mut out), 0);
        assert!(out.is_empty());
    }

    #[test]
    fn only_the_framed_notice_wakes_the_session() {
        let mut err = Vec::new();
        let notice = format!("{FRAME} msg_1 for this session, from octo");
        assert_eq!(finish(Some(notice.clone()), &mut err), 2);
        assert_eq!(String::from_utf8(err).unwrap().trim_end(), notice);
        let mut err = Vec::new();
        let request = format!("{REQUEST_FRAME} crq_1 for your Work wrk_1");
        assert_eq!(finish(Some(request.clone()), &mut err), 2);
        assert_eq!(String::from_utf8(err).unwrap().trim_end(), request);
        let mut err = Vec::new();
        assert_eq!(finish(None, &mut err), 0);
        assert_eq!(finish(Some("something else".into()), &mut err), 0);
        assert!(err.is_empty());
    }

    #[test]
    fn a_codex_notice_is_queued_for_its_chat_as_values_never_options() {
        assert_eq!(
            queue_args("019a7c-thread", "--remote=ws://elsewhere"),
            [
                "queue".to_owned(),
                "--thread=019a7c-thread".to_owned(),
                "--message=--remote=ws://elsewhere".to_owned()
            ]
        );
        // Only the Hub's framed notice is queued: anything else runs nothing and exits 0.
        assert_eq!(queue("019a7c-thread", "something else"), 0);
    }

    /// A Hub that answers each request with the next of `replies` (status, body).
    /// With `end`, the session's end marker is written before the last reply goes out.
    async fn hub(
        replies: Vec<(u16, &'static str)>,
        end: Option<PathBuf>,
    ) -> (String, tokio::task::JoinHandle<usize>) {
        hub_for(replies, end, Waker::Claude).await
    }

    async fn hub_for(
        replies: Vec<(u16, &'static str)>,
        end: Option<PathBuf>,
        agent: Waker,
    ) -> (String, tokio::task::JoinHandle<usize>) {
        let last = replies.len();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let served = tokio::spawn(async move {
            let mut count = 0;
            for (status, body) in replies {
                let (mut stream, _) = listener.accept().await.unwrap();
                let mut request = Vec::new();
                let mut buf = [0u8; 4096];
                // Read the head and the whole body before answering.
                loop {
                    let n = stream.read(&mut buf).await.unwrap();
                    request.extend_from_slice(&buf[..n]);
                    let text = String::from_utf8_lossy(&request);
                    if let Some(head) = text.find("\r\n\r\n") {
                        let length = text[..head]
                            .lines()
                            .find_map(|l| {
                                l.to_ascii_lowercase()
                                    .strip_prefix("content-length:")
                                    .map(|v| v.trim().parse::<usize>().unwrap())
                            })
                            .unwrap_or(0);
                        if request.len() >= head + 4 + length || n == 0 {
                            break;
                        }
                    }
                    if n == 0 {
                        break;
                    }
                }
                let text = String::from_utf8_lossy(&request);
                assert!(
                    text.starts_with("POST /v1/hub/agent-messages/wait "),
                    "{text}"
                );
                assert!(
                    text.contains("authorization: Bearer secret")
                        || text.contains("Authorization: Bearer secret"),
                    "{text}"
                );
                assert!(text.ends_with(&wait_body("s1", agent)), "{text}");
                let reply = format!(
                    "HTTP/1.1 {status} X\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                );
                count += 1;
                if count == last
                    && let Some(end) = &end
                {
                    File::create(end).unwrap();
                }
                stream.write_all(reply.as_bytes()).await.unwrap();
            }
            count
        });
        (url, served)
    }

    async fn ask(url: &str, life: Duration, alive: impl Fn() -> bool) -> Option<String> {
        ask_for(url, life, alive, Waker::Claude).await
    }

    async fn ask_for(
        url: &str,
        life: Duration,
        alive: impl Fn() -> bool,
        agent: Waker,
    ) -> Option<String> {
        let dir = tempfile::tempdir().unwrap();
        let token_file = dir.path().join("token");
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
        options
            .open(&token_file)
            .unwrap()
            .write_all(b"secret\n")
            .unwrap();
        let client = reqwest::Client::new();
        let wait = Wait {
            client: &client,
            url: format!("{url}/v1/hub/agent-messages/wait"),
            body: wait_body("s1", agent),
            token_file: &token_file,
        };
        wait.until(life, alive, Duration::ZERO).await
    }

    #[tokio::test]
    async fn a_session_end_marker_ends_the_wait_without_another_request() {
        let dir = tempfile::tempdir().unwrap();
        let ended = marker(dir.path(), "s1");
        assert!(ended.starts_with(dir.path()));
        let (url, served) = hub(vec![(204, "")], Some(ended.clone())).await;
        assert_eq!(ask(&url, MAX_LIFE, || !ended.exists()).await, None);
        assert_eq!(served.await.unwrap(), 1);
    }

    #[tokio::test]
    async fn another_wait_still_open_at_the_hub_is_asked_again() {
        let notice = r#"{"text":"AgentSCM Hub message msg_1 for this session, from octo"}"#;
        let (url, served) = hub(vec![(409, ""), (409, ""), (200, notice)], None).await;
        let text = ask(&url, MAX_LIFE, || true).await.expect("the notice");
        assert_eq!(served.await.unwrap(), 3);
        assert!(text.starts_with(FRAME));
    }

    #[test]
    fn wait_bodies_preserve_claude_and_identify_other_delivery_mechanisms() {
        assert_eq!(wait_body("s1", Waker::Claude), r#"{"session":"s1"}"#);
        for (agent, name) in [(Waker::Codex, "codex"), (Waker::Copilot, "copilot")] {
            let body: Value = serde_json::from_str(&wait_body("s1", agent)).unwrap();
            assert_eq!(body, serde_json::json!({"session": "s1", "agent": name}));
        }
    }

    #[tokio::test]
    async fn a_copilot_notice_returns_context_after_empty_waits() {
        let notice = r#"{"text":"AgentSCM Hub message msg_1 for this session, from octo"}"#;
        let (url, served) = hub_for(vec![(204, ""), (200, notice)], None, Waker::Copilot).await;
        let found = ask_for(&url, MAX_LIFE, || true, Waker::Copilot).await;
        assert_eq!(served.await.unwrap(), 2);
        let mut out = Vec::new();
        assert_eq!(finish_copilot(found, &mut out), 0);
        assert!(
            serde_json::from_slice::<Value>(&out).unwrap()["additionalContext"]
                .as_str()
                .unwrap()
                .starts_with(FRAME)
        );
    }

    #[tokio::test]
    async fn an_older_hub_refusing_a_copilot_waiter_produces_no_context() {
        let (url, served) = hub_for(vec![(400, "")], None, Waker::Copilot).await;
        let found = ask_for(&url, MAX_LIFE, || true, Waker::Copilot).await;
        assert_eq!(served.await.unwrap(), 1);
        assert_eq!(found, None);
        let mut out = Vec::new();
        assert_eq!(finish_copilot(found, &mut out), 0);
        assert!(out.is_empty());
    }

    #[tokio::test]
    async fn a_notice_after_the_session_ends_never_wakes_it() {
        let dir = tempfile::tempdir().unwrap();
        let ended = marker(dir.path(), "s1");
        let notice = r#"{"text":"AgentSCM Hub message msg_1 for this session, from octo"}"#;
        let (url, served) = hub_for(vec![(200, notice)], Some(ended.clone()), Waker::Copilot).await;
        let found = ask_for(&url, MAX_LIFE, || !ended.exists(), Waker::Copilot).await;
        assert_eq!(served.await.unwrap(), 1);
        assert_eq!(found, None);
        let mut out = Vec::new();
        assert_eq!(finish_copilot(found, &mut out), 0);
        assert!(out.is_empty());
    }

    #[tokio::test]
    async fn a_message_after_empty_waits_is_the_notice_and_wakes_with_exit_2() {
        let notice = r#"{"text":"AgentSCM Hub message msg_1 for this session, from octo"}"#;
        let (url, served) = hub(vec![(204, ""), (204, ""), (200, notice)], None).await;
        let text = ask(&url, MAX_LIFE, || true).await.expect("the notice");
        assert_eq!(served.await.unwrap(), 3);
        let mut err = Vec::new();
        assert_eq!(finish(Some(text), &mut err), 2);
        assert!(String::from_utf8(err).unwrap().starts_with(FRAME));
    }

    #[tokio::test]
    async fn time_up_or_a_hub_with_nothing_to_wait_for_exits_0() {
        // The life is up after the first empty wait.
        let (url, served) = hub(vec![(204, "")], None).await;
        let found = ask(&url, Duration::ZERO, || true).await;
        assert_eq!(found, None);
        assert_eq!(served.await.unwrap(), 1);
        assert_eq!(finish(found, &mut Vec::new()), 0);
        // Ignore (410), an older Hub without the route (404) and one that refuses the request
        // (400, as a Hub older than the Codex waiter does its `agent`) end it at once.
        for status in [410, 404, 400] {
            let (url, served) = hub(vec![(status, "")], None).await;
            assert_eq!(ask(&url, MAX_LIFE, || true).await, None);
            assert_eq!(served.await.unwrap(), 1);
        }
    }
}
