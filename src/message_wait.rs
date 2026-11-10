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
use std::fs::{DirBuilder, File, TryLockError};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use agentscm::hub::agent_messages::{FRAME, REQUEST_FRAME, WAIT_SECS};
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
    /// The agent whose hook this is; only `claude` has a background hook that wakes its session.
    pub agent: String,
    /// SessionEnd: tell this session's waiter to leave, then exit at once.
    #[arg(long)]
    pub end: bool,
}

/// Never returns: exits 2 with the Hub's notice on stderr when a message came, else 0.
pub async fn run(args: HookWaitArgs) -> ! {
    if args.end {
        // The session ended while its process may live on (/clear), so its waiter cannot see it.
        if let (Some(payload), Some(dir)) = (input(), wait_dir())
            && let Some(session) = session_of(&payload)
        {
            let _ = File::create(marker(&dir, &session));
        }
        std::process::exit(0)
    }
    let found = if args.agent == "claude" && cfg!(unix) {
        waited().await
    } else {
        None
    };
    std::process::exit(finish(found, &mut std::io::stderr()))
}

/// Writes the notice and returns 2 only for the Hub's framed notice (a message, or a coordination
/// request to the session's Work); anything else is 0, silent.
fn finish(found: Option<String>, err: &mut impl Write) -> i32 {
    match found.filter(|text| text.starts_with(FRAME) || text.starts_with(REQUEST_FRAME)) {
        Some(text) => {
            // A write error still wakes the session; the notice is then empty.
            let _ = writeln!(err, "{text}");
            let _ = err.flush();
            2
        }
        None => 0,
    }
}

/// The session id from the hook's input; `None` for a subagent's call, which never waits.
fn session_of(payload: &Value) -> Option<String> {
    if payload.get("agent_id").is_some() {
        return None;
    }
    payload["session_id"]
        .as_str()
        .filter(|s| !s.is_empty() && s.len() <= 128 && !s.chars().any(char::is_control))
        .map(str::to_owned)
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

async fn waited() -> Option<String> {
    let payload = input()?;
    let session = session_of(&payload)?;
    let cwd = Path::new(payload["cwd"].as_str()?);
    let connections = client_config::load().ok()?;
    let (connection, checkout) = client_config::connection_for_checkout(&connections, cwd)?;
    if !client_config::is_current_in(connection, &checkout) {
        return None;
    }
    let dir = wait_dir()?;
    // A resumed session keeps its id, so an old end marker must not stop its new waiter.
    let ended = marker(&dir, &session);
    let _ = std::fs::remove_file(&ended);
    // Held until this process exits; a second waiter for the session leaves at once.
    let _lock = lock(&dir, &session)?;
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
        body: serde_json::json!({ "session": session }).to_string(),
        token_file: &connection.token_file,
    };
    request
        .until(
            MAX_LIFE,
            || parent() == started && !ended.exists(),
            Duration::from_secs(5),
        )
        .await
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
                // Signed out, a read-only token, Ignore, or a Hub without the route.
                Ok((401 | 403 | 404 | 405 | 410, _)) => return None,
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
        let session = |v: Value| session_of(&v);
        assert_eq!(
            session(serde_json::json!({"session_id":"abc"})).as_deref(),
            Some("abc")
        );
        assert_eq!(
            session(serde_json::json!({"session_id":"abc","agent_id":"sub"})),
            None
        );
        assert_eq!(session(serde_json::json!({"session_id":""})), None);
        assert_eq!(session(serde_json::json!({"session_id":"a\nb"})), None);
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

    /// A Hub that answers each request with the next of `replies` (status, body).
    /// With `end`, the session's end marker is written before the last reply goes out.
    async fn hub(
        replies: Vec<(u16, &'static str)>,
        end: Option<PathBuf>,
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
                assert!(text.ends_with(r#"{"session":"s1"}"#), "{text}");
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
            body: r#"{"session":"s1"}"#.into(),
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
        // Ignore (410) and an older Hub without the route (404) end it at once.
        for status in [410, 404] {
            let (url, served) = hub(vec![(status, "")], None).await;
            assert_eq!(ask(&url, MAX_LIFE, || true).await, None);
            assert_eq!(served.await.unwrap(), 1);
        }
    }
}
