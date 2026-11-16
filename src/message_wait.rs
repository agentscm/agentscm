//! `agentscm hook-wait claude`: the background hook that lets a message sent from the Hub page
//! attempt delivery to an existing Claude Code session through its native hook contract.
//!
//! Claude Code runs it at session start and after every turn as an `asyncRewake` hook: it runs
//! in the background, and when it exits with code 2 its stderr wakes the session as a new turn.
//! It holds one request at a time open at the Hub (`/v1/hub/agent-messages/wait`) for this
//! session; when a message comes, or a coordination request to Work the session leads waits for
//! its acknowledgement, it prints the Hub's notice and exits 2. Only one waits per
//! session: a SessionStart briefly waits for a predecessor's lock; other duplicates leave at once.
//! It ends quietly when its time is
//! up, when the process that started it is gone or the session's `SessionEnd` hook
//! (`hook-wait claude --end`) left its end marker, when the checkout is not connected, when the
//! person's mode is `ignore`, or when the Hub refuses, lacks the route or stays unreachable.
//! Every failure exits 0, so the session is never woken by an error. On Windows it exits at once:
//! it cannot tell there when its session has ended.
//!
//! `agentscm hook-wait codex` is the same waiter for a Codex chat, run as an `async` hook. A
//! finished background hook starts no turn in Codex, so on a notice it queues the notice into
//! the chat with `codex queue` from a qualified live host and exits 0. Queue acceptance does not
//! prove that the model read the notice or started a turn.
//!
//! `agentscm hook-wait copilot --notification` waits only from Copilot CLI's asynchronous hook.
//! It returns a notice as `additionalContext` on stdout, which can start processing while idle.
//! There is no waiter until a notification fires; another notification rearms it after delivery.
use std::fs::{DirBuilder, File, TryLockError};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crate::client_journal::{Kind, Phase, Record};
use agentscm::hub::agent_messages::{FRAME, REQUEST_FRAME, WAIT_SECS, Waker};
use agentscm::hub::notices::{AttemptOutcome, NoticeTransportReport};
use agentscm::hub::reply;
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::client_config;

/// Maximum lifetime of one waiter; the plugin timeout is slightly longer. Host availability
/// and current session identity still bound delivery throughout this lifetime.
pub const MAX_LIFE: Duration = Duration::from_secs(7 * 24 * 3600);
/// Failing this long without one answer from the Hub ends the waiter; the next turn starts one.
const UNREACHABLE: Duration = Duration::from_secs(15 * 60);
const REGISTRATION_GRACE: Duration = Duration::from_secs(15);
// A predecessor can be in a 40-second request followed by a 60-second backoff.
const START_LOCK_GRACE: Duration = Duration::from_secs(WAIT_SECS + 15 + 60);
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
    std::process::exit(match found {
        Some((session, notice, connection)) if agent == Waker::Codex => {
            enqueue(&session, &notice, &connection).await;
            0
        }
        Some((session, notice, connection)) => {
            let framed = framed(&notice.text);
            let status = match agent {
                Waker::Claude => finish(Some(notice.text.clone()), &mut std::io::stderr()),
                Waker::Copilot => finish_copilot(Some(notice.text.clone()), &mut std::io::stdout()),
                Waker::Codex => unreachable!("Codex enqueues above"),
            };
            if framed {
                report_delivery(
                    &session,
                    &notice,
                    &connection,
                    AttemptOutcome::TransportReported,
                )
                .await;
            }
            status
        }
        None => 0,
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
    text.starts_with(FRAME)
        || text.starts_with(REQUEST_FRAME)
        || text.starts_with(agentscm::hub::notices::FRAME)
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

/// Queue only from this live native session, never resume or replace a conversation.
async fn enqueue(session: &str, notice: &Notice, connection: &client_config::Connection) {
    if !framed(&notice.text) {
        return;
    }
    let Some(context) = notice.incarnation.as_deref().and_then(|incarnation| {
        crate::client_cooperation::delivery_context(connection, session, incarnation)
    }) else {
        return;
    };
    let Some(attempt) = notice.attempt_id.as_deref() else {
        return;
    };
    let Ok(config) = crate::hub_cli::config_dir() else {
        return;
    };
    let Some(report) = transport_report(session, notice, AttemptOutcome::Unknown) else {
        return;
    };
    let payload = journal_payload(connection, &context, &report);
    let record = Record {
        id: attempt.into(),
        kind: Kind::Enqueue,
        phase: Phase::Prepared,
        payload: payload.clone(),
    };
    if crate::client_journal::put(&config, &record).is_err() {
        eprintln!(
            "AgentSCM: delivery evidence is unavailable; inspect `agentscm client status`. This waiter did not run a queue command."
        );
        return;
    }
    match crate::client_journal::admit_enqueue(&config, &record) {
        Ok(true) => {}
        Ok(false) => return,
        Err(_) => {
            eprintln!(
                "AgentSCM: delivery admission is unavailable; inspect `agentscm client status`. This waiter did not run a queue command."
            );
            return;
        }
    }
    let host = crate::client_session::qualified_host(session).filter(|_| {
        !context.terminal && !context.explicit_end_requested && context.exit_observed_at.is_none()
    });
    let Some(host) = host else {
        report_delivery(
            session,
            notice,
            connection,
            AttemptOutcome::ProvenNoDispatch,
        )
        .await;
        return;
    };
    // This exact process fingerprint is checked immediately before the only external action.
    let still_current =
        crate::client_cooperation::delivery_context(connection, session, &report.incarnation)
            .is_some_and(|current| {
                current.start_nonce == context.start_nonce
                    && !current.terminal
                    && !current.explicit_end_requested
                    && current.exit_observed_at.is_none()
            });
    if !still_current
        || crate::client_session::observe_host(&host)
            != crate::client_session::HostObservation::Same
    {
        report_delivery(
            session,
            notice,
            connection,
            AttemptOutcome::ProvenNoDispatch,
        )
        .await;
        return;
    }
    let outcome = spawn_queue(Path::new("codex"), session, &notice.text);
    let outcome = if outcome != AttemptOutcome::ProvenNoDispatch
        && crate::client_session::observe_host(&host)
            != crate::client_session::HostObservation::Same
    {
        AttemptOutcome::Unknown
    } else {
        outcome
    };
    report_delivery(session, notice, connection, outcome).await;
}

fn spawn_queue(program: &Path, session: &str, text: &str) -> AttemptOutcome {
    let child = std::process::Command::new(program)
        .args(queue_args(session, text))
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();
    let Ok(mut child) = child else {
        return AttemptOutcome::ProvenNoDispatch;
    };
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        match child.try_wait() {
            Ok(Some(status)) if status.success() => return AttemptOutcome::TransportReported,
            Ok(Some(_)) | Err(_) => return AttemptOutcome::Unknown,
            Ok(None) if Instant::now() >= deadline => {
                // Only this owned queue command is stopped; whether it enqueued stays unknown.
                let _ = child.kill();
                let _ = child.wait();
                return AttemptOutcome::Unknown;
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(20)),
        }
    }
}

fn transport_report(
    session: &str,
    notice: &Notice,
    outcome: AttemptOutcome,
) -> Option<NoticeTransportReport> {
    Some(NoticeTransportReport {
        session: session.into(),
        incarnation: notice.incarnation.clone()?,
        delivery_id: notice.delivery_id.clone()?,
        attempt_id: notice.attempt_id.clone()?,
        outcome,
        observed_at: agentscm::hub::store::unix_now(),
    })
}

fn journal_payload(
    connection: &client_config::Connection,
    context: &crate::client_session::SessionContext,
    report: &NoticeTransportReport,
) -> Value {
    serde_json::json!({"hub": connection.hub,"repo": connection.repo,"root": connection.root,
        "credential_id":context.credential_id,"binding_id":context.binding_id,
        "session":report.session,"incarnation":report.incarnation,
        "endpoint":"/v1/hub/cooperation/transport","body":report,"observed_at":report.observed_at})
}

async fn report_delivery(
    session: &str,
    notice: &Notice,
    connection: &client_config::Connection,
    outcome: AttemptOutcome,
) {
    let Some(report) = transport_report(session, notice, outcome) else {
        return;
    };
    let Some(context) =
        crate::client_cooperation::delivery_context(connection, session, &report.incarnation)
    else {
        return;
    };
    let Ok(config) = crate::hub_cli::config_dir() else {
        return;
    };
    let payload = journal_payload(connection, &context, &report);
    // Claude's native stderr delivery has no external queue step but retains its HTTP outcome.
    let record = Record {
        id: report.attempt_id.clone(),
        kind: Kind::Enqueue,
        phase: Phase::Prepared,
        payload: payload.clone(),
    };
    let _ = crate::client_journal::put(&config, &record);
    if crate::client_journal::update(&config, &report.attempt_id, Phase::Outcome, payload).is_err()
    {
        return;
    }
    let Ok(token) = crate::client_cli::read_token(&connection.token_file) else {
        return;
    };
    let Ok(client) = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(Duration::from_secs(2))
        .timeout(Duration::from_secs(3))
        .build()
    else {
        return;
    };
    let Ok(body) = serde_json::to_vec(&report) else {
        return;
    };
    if client
        .post(format!("{}/v1/hub/cooperation/transport", connection.hub))
        .bearer_auth(token)
        .header(reqwest::header::CONTENT_TYPE, "application/json")
        .body(body)
        .send()
        .await
        .is_ok_and(|response| response.status().is_success())
    {
        let _ = crate::client_journal::remove(&config, &report.attempt_id);
    }
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
async fn waited(
    agent: Waker,
    notification: bool,
    payload: &Value,
) -> Option<(String, Notice, client_config::Connection)> {
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
    let started = parent();
    let startup = payload["hook_event_name"] == "SessionStart";
    let registration_host = if startup {
        crate::client_session::registration_host()
    } else {
        None
    };
    // A start may follow a waiter still draining its predecessor's request or backoff.
    let _lock = lock_until(&dir, &session, startup, START_LOCK_GRACE, || {
        parent() == started
    })
    .await?;
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
    let mut waiter = Waiter {
        connection,
        checkout: &checkout,
        session: &session,
        agent,
        startup,
        registration_host,
        nonce: None,
        incarnation: None,
        sent: false,
    };
    let (text, request) = wait_until(
        &client,
        MAX_LIFE,
        || parent() == started && !ended.exists(),
        Duration::from_secs(5),
        |grace| waiter.prepare(grace),
        |request, notice| request.valid(&checkout, &session, agent, notice),
    )
    .await?;
    Some((session, text, request.connection))
}

struct Waiter<'a> {
    connection: &'a client_config::Connection,
    checkout: &'a Path,
    session: &'a str,
    agent: Waker,
    startup: bool,
    registration_host: Option<crate::client_session::HostIdentity>,
    nonce: Option<String>,
    incarnation: Option<String>,
    sent: bool,
}

enum Admission {
    Ready(Box<WaitRequest>),
    Pending,
    Stop,
}

struct WaitRequest {
    connection: client_config::Connection,
    token: String,
    body: String,
    nonce: Option<String>,
    incarnation: Option<String>,
}

impl Waiter<'_> {
    fn prepare(&mut self, grace: bool) -> Admission {
        self.request(grace).unwrap_or(Admission::Stop)
    }

    fn request(&mut self, grace: bool) -> Option<Admission> {
        let saved = client_config::load().ok()?;
        let (connection, checkout) = client_config::connection_for_checkout(&saved, self.checkout)?;
        if checkout != self.checkout
            || connection.root != self.connection.root
            || connection.hub != self.connection.hub
            || connection.repo != self.connection.repo
            || connection.origin_host != self.connection.origin_host
            || !client_config::is_current_in(connection, &checkout)
        {
            return None;
        }
        let token = crate::client_cli::read_token(&connection.token_file).ok()?;
        let config = crate::hub_cli::config_dir().ok()?;
        let context = crate::client_session::context(
            &config,
            self.session,
            &connection.hub,
            &connection.repo,
        )
        .ok()?;
        // The relay alone mints a start. A foreign or ended route is a predecessor, so leave
        // it unpinned while the parallel relay catches up. Absence keeps legacy compatibility.
        if self.startup
            && !self.sent
            && self.nonce.is_none()
            && context.as_ref().is_some_and(|context| {
                self.registration_host.is_none()
                    || context.registration_host != self.registration_host
                    || context.terminal
                    || context.revoked
                    || context.explicit_end_requested
                    || context.exit_observed_at.is_some()
            })
        {
            return grace.then_some(Admission::Pending);
        }
        let nonce = context.as_ref().map(|context| &context.start_nonce);
        if self
            .nonce
            .as_ref()
            .is_some_and(|prior| nonce != Some(prior))
            || (self.sent && self.nonce.is_none() && nonce.is_some())
        {
            return None;
        }
        self.nonce = nonce.cloned();
        let mut incarnation = None;
        if let Some(context) = context {
            if context.root != connection.root
                || waiter(&context.agent) != Some(self.agent)
                || context.terminal
                || context.revoked
                || context.explicit_end_requested
                || context.exit_observed_at.is_some()
                || context.protocol > 1
            {
                return None;
            }
            let binding = client_config::onboarding_client(&connection.root, &token);
            let same_binding = context.binding_id == binding
                && context.credential_id
                    == connection.owned_token_id.as_deref().unwrap_or(&binding);
            if same_binding && context.protocol == 1 {
                if !context.registered || context.incarnation.is_none() {
                    return grace.then_some(Admission::Pending);
                }
                let current = crate::client_cooperation::delivery_context(
                    connection,
                    self.session,
                    context.incarnation.as_deref()?,
                )?;
                if current.start_nonce != context.start_nonce
                    || current.terminal
                    || current.explicit_end_requested
                    || current.exit_observed_at.is_some()
                {
                    return None;
                }
                if self
                    .incarnation
                    .as_ref()
                    .is_some_and(|prior| current.incarnation.as_ref() != Some(prior))
                {
                    return None;
                }
                incarnation = current.incarnation;
            } else if same_binding && context.protocol_checked_at == 0 && grace {
                return Some(Admission::Pending);
            }
        } else if grace {
            return Some(Admission::Pending);
        }
        if self.agent == Waker::Codex {
            incarnation.as_ref()?;
            if crate::client_session::qualified_host(self.session).is_none() {
                return grace.then_some(Admission::Pending);
            }
        }
        let body = match incarnation.as_deref() {
            Some(incarnation) => wait_body_current(self.session, self.agent, incarnation),
            None => wait_body(self.session, self.agent),
        };
        self.sent = true;
        if incarnation.is_some() {
            self.incarnation = incarnation.clone();
        }
        Some(Admission::Ready(Box::new(WaitRequest {
            connection: connection.clone(),
            token,
            body,
            nonce: self.nonce.clone(),
            incarnation,
        })))
    }
}

impl WaitRequest {
    fn valid(&self, checkout: &Path, session: &str, agent: Waker, notice: &Notice) -> bool {
        let Some(current) = client_config::load().ok().and_then(|saved| {
            client_config::connection_for_checkout(&saved, checkout)
                .map(|(connection, _)| connection.clone())
        }) else {
            return false;
        };
        if current != self.connection
            || !client_config::is_current_in(&current, checkout)
            || crate::client_cli::read_token(&current.token_file)
                .ok()
                .as_deref()
                != Some(self.token.as_str())
        {
            return false;
        }
        let Some(context) = crate::hub_cli::config_dir().ok().and_then(|config| {
            crate::client_session::context(&config, session, &current.hub, &current.repo).ok()
        }) else {
            return false;
        };
        if context.as_ref().map(|context| &context.start_nonce) != self.nonce.as_ref()
            || context.as_ref().is_some_and(|context| {
                context.root != current.root
                    || waiter(&context.agent) != Some(agent)
                    || context.terminal
                    || context.revoked
                    || context.explicit_end_requested
                    || context.exit_observed_at.is_some()
            })
        {
            return false;
        }
        match self.incarnation.as_deref() {
            Some(incarnation) => {
                notice.incarnation.as_deref() == Some(incarnation)
                    && crate::client_cooperation::delivery_context(&current, session, incarnation)
                        .is_some_and(|context| {
                            Some(&context.start_nonce) == self.nonce.as_ref()
                                && !context.terminal
                                && !context.explicit_end_requested
                                && context.exit_observed_at.is_none()
                        })
            }
            None => {
                agent != Waker::Codex
                    && notice.incarnation.is_none()
                    && notice.attempt_id.is_none()
                    && notice.delivery_id.is_none()
                    && (notice.text.starts_with(FRAME) || notice.text.starts_with(REQUEST_FRAME))
            }
        }
    }
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

fn wait_body_current(session: &str, agent: Waker, incarnation: &str) -> String {
    let mut value: Value =
        serde_json::from_str(&wait_body(session, agent)).expect("wait body is JSON");
    value["cooperation_protocol"] = serde_json::json!(agentscm::hub::notices::PROTOCOL);
    value["incarnation"] = serde_json::json!(incarnation);
    value.to_string()
}

/// The Hub's answer when a message came; absent receipt fields are never enqueue permission.
#[derive(serde::Deserialize)]
struct Notice {
    text: String,
    #[serde(default)]
    delivery_id: Option<String>,
    #[serde(default)]
    attempt_id: Option<String>,
    #[serde(default)]
    incarnation: Option<String>,
}

/// Each retry gets its own credential and lifecycle snapshot; a response cannot borrow the next one.
async fn wait_until(
    client: &reqwest::Client,
    life: Duration,
    alive: impl Fn() -> bool,
    pause: Duration,
    mut prepare: impl FnMut(bool) -> Admission,
    valid: impl Fn(&WaitRequest, &Notice) -> bool,
) -> Option<(Notice, WaitRequest)> {
    let started = Instant::now();
    let mut answered = Instant::now();
    let mut backoff = pause;
    loop {
        if !alive() {
            return None;
        }
        let request = match prepare(started.elapsed() < REGISTRATION_GRACE) {
            Admission::Ready(request) => *request,
            Admission::Pending if started.elapsed() < life => {
                tokio::time::sleep(Duration::from_millis(500)).await;
                continue;
            }
            Admission::Pending | Admission::Stop => return None,
        };
        let sent = client
            .post(format!(
                "{}/v1/hub/agent-messages/wait",
                request.connection.hub
            ))
            .bearer_auth(&request.token)
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .body(request.body.clone())
            .send()
            .await;
        match sent.map(|reply| (reply.status().as_u16(), reply)) {
            Ok((200, answer)) => {
                let bytes = answer.bytes().await.ok()?;
                // SessionEnd or parent death can happen while the request is in flight.
                if !alive() {
                    return None;
                }
                let notice = reply::decode_slice::<Notice>(&bytes).ok()?;
                return valid(&request, &notice).then_some((notice, request));
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
    let file = File::create(dir.join(file_name(session))).ok()?;
    match file.try_lock() {
        Ok(()) => {}
        Err(TryLockError::WouldBlock | TryLockError::Error(_)) => return None,
    }
    // Files of sessions long gone. Each start rewrites its lock file, so a live waiter's file
    // is younger than its life. Only the holder may inspect or remove its predecessor's marker.
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
    Some(file)
}

/// Only SessionStart waits for a predecessor. Its end marker is not ours until we hold the lock.
async fn lock_until(
    dir: &Path,
    session: &str,
    startup: bool,
    grace: Duration,
    alive: impl Fn() -> bool,
) -> Option<File> {
    let deadline = tokio::time::Instant::now() + grace;
    loop {
        if !alive() {
            return None;
        }
        if let Some(file) = lock(dir, session) {
            return Some(file);
        }
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if !startup || remaining.is_zero() {
            return None;
        }
        tokio::time::sleep(remaining.min(Duration::from_millis(500))).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    fn test_connection(root: &Path, hub: &str, token: &Path) -> client_config::Connection {
        client_config::Connection {
            root: root.to_owned(),
            origin_host: "github.com".into(),
            hub: hub.into(),
            repo: "acme/widgets".into(),
            token_file: token.to_owned(),
            report_completion: false,
            owned_token_id: None,
            owned_token_expires_at: None,
            agents: vec![],
        }
    }

    #[cfg(unix)]
    fn save_connection(config: &Path, connection: &client_config::Connection) {
        crate::hub_cli::write_private(
            &config.join("agentscm/connections.json"),
            &serde_json::json!({"version":1,"connections":[connection]}).to_string(),
        )
        .unwrap();
    }

    #[cfg(unix)]
    fn session_fixture(
        config: &Path,
        hub: &str,
        agent: &str,
    ) -> (
        tempfile::TempDir,
        client_config::Connection,
        crate::client_session::SessionContext,
    ) {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
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
                std::process::Command::new("git")
                    .current_dir(&root)
                    .args(args)
                    .status()
                    .unwrap()
                    .success()
            );
        }
        crate::client_journal::locked(config, || Ok(())).unwrap();
        let token = config.join("synthetic-token");
        crate::hub_cli::write_private(&token, "secret").unwrap();
        let connection = test_connection(&root, hub, &token);
        save_connection(config, &connection);
        let binding = client_config::onboarding_client(&root, "secret");
        let context = serde_json::from_value(serde_json::json!({
            "session":"s1", "start_nonce":"start-a", "incarnation":"inc-a", "agent":agent,
            "hub":hub, "repo":"acme/widgets", "root":root, "protocol":1, "protocol_checked_at":1,
            "terminal":false, "registered":true, "revoked":false, "observed_at":1,
            "host":null, "credential_id":binding, "binding_id":binding,
        }))
        .unwrap();
        (directory, connection, context)
    }

    #[cfg(unix)]
    fn test_waiter(connection: &client_config::Connection, agent: Waker) -> Waiter<'_> {
        Waiter {
            connection,
            checkout: &connection.root,
            session: "s1",
            agent,
            startup: false,
            registration_host: None,
            nonce: None,
            incarnation: None,
            sent: false,
        }
    }

    #[cfg(unix)]
    fn startup_waiter(
        connection: &client_config::Connection,
        registration_host: Option<crate::client_session::HostIdentity>,
    ) -> Waiter<'_> {
        Waiter {
            startup: true,
            registration_host,
            ..test_waiter(connection, Waker::Claude)
        }
    }

    #[cfg(unix)]
    fn test_host(pid: u32) -> crate::client_session::HostIdentity {
        crate::client_session::HostIdentity {
            pid,
            started: "synthetic process start and boot".into(),
        }
    }

    #[cfg(unix)]
    fn ready(admission: Admission) -> WaitRequest {
        match admission {
            Admission::Ready(request) => *request,
            _ => panic!("expected an admitted request"),
        }
    }

    #[cfg(unix)]
    fn notice(incarnation: Option<&str>) -> Notice {
        Notice {
            text: format!("{FRAME}msg_synthetic"),
            delivery_id: None,
            attempt_id: None,
            incarnation: incarnation.map(str::to_owned),
        }
    }

    #[cfg(unix)]
    #[test]
    fn registration_race_is_retried_then_opens_an_age_free_modern_wait() {
        client_config::in_isolated_config_home(|config| {
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap()
                .block_on(async {
                    let (url, served) = hub_request(vec![(200,
                    r#"{"text":"AgentSCM Hub message msg_synthetic","incarnation":"inc-a"}"#)],
                    None, wait_body_current("s1", Waker::Claude, "inc-a")).await;
                    let (_directory, connection, mut context) =
                        session_fixture(config, &url, "claude");
                    context.registered = false;
                    context.incarnation = None;
                    crate::client_session::save_context(config, &context).unwrap();
                    let mut waiter = test_waiter(&connection, Waker::Claude);
                    let mut polls = 0;
                    let client = reqwest::Client::new();
                    let found = wait_until(
                        &client,
                        Duration::from_secs(3),
                        || true,
                        Duration::ZERO,
                        |grace| {
                            polls += 1;
                            let admission = waiter.prepare(grace);
                            if polls == 1 {
                                assert!(matches!(admission, Admission::Pending));
                                context.registered = true;
                                context.incarnation = Some("inc-a".into());
                                crate::client_session::save_context(config, &context).unwrap();
                            }
                            admission
                        },
                        |request, notice| {
                            request.valid(&connection.root, "s1", Waker::Claude, notice)
                        },
                    )
                    .await;
                    assert!(found.is_some());
                    assert_eq!(polls, 2);
                    assert_eq!(served.await.unwrap(), 1);
                    assert!(
                        crate::client_cooperation::transport_context(&connection, "s1").is_none(),
                        "ordinary transport still has its capability-age bound"
                    );
                });
        });
    }

    #[cfg(unix)]
    #[test]
    fn pending_registration_and_unknown_codex_authority_are_bounded() {
        client_config::in_isolated_config_home(|config| {
            let (_directory, connection, mut context) =
                session_fixture(config, "https://hub.example", "codex");
            let mut waiter = test_waiter(&connection, Waker::Codex);
            assert!(matches!(waiter.prepare(true), Admission::Pending));
            assert!(matches!(waiter.prepare(false), Admission::Stop));
            context.registered = false;
            context.incarnation = None;
            crate::client_session::save_context(config, &context).unwrap();
            assert!(matches!(waiter.prepare(true), Admission::Pending));
            assert!(matches!(waiter.prepare(false), Admission::Stop));
            context.registered = true;
            context.incarnation = Some("inc-a".into());
            crate::client_session::save_context(config, &context).unwrap();
            assert!(
                matches!(waiter.prepare(true), Admission::Pending),
                "host qualification is also allowed to arrive"
            );
            assert!(
                matches!(waiter.prepare(false), Admission::Stop),
                "no qualified host never authorizes Codex wait"
            );
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap()
                .block_on(async {
                    assert!(
                        wait_until(
                            &reqwest::Client::new(),
                            Duration::ZERO,
                            || true,
                            Duration::ZERO,
                            |_| Admission::Pending,
                            |_, _| true
                        )
                        .await
                        .is_none()
                    );
                });
        });
    }

    #[cfg(unix)]
    #[test]
    fn claude_and_copilot_keep_legacy_bodies_without_modern_authority() {
        client_config::in_isolated_config_home(|config| {
            for (agent, name, hub) in [
                (Waker::Claude, "claude", "https://claude.example"),
                (Waker::Copilot, "copilot", "https://copilot.example"),
            ] {
                let (_directory, connection, mut context) = session_fixture(config, hub, name);
                let mut waiter = test_waiter(&connection, agent);
                assert!(matches!(waiter.prepare(true), Admission::Pending));
                let absent = ready(waiter.prepare(false));
                assert_eq!(absent.body, wait_body("s1", agent));
                assert!(absent.valid(&connection.root, "s1", agent, &notice(None)));
                assert!(!absent.valid(&connection.root, "s1", agent, &notice(Some("inc-a"))));
                context.protocol = 0;
                crate::client_session::save_context(config, &context).unwrap();
                assert!(
                    matches!(waiter.prepare(true), Admission::Stop),
                    "an unbound old wait cannot adopt a later start"
                );
                let legacy = ready(test_waiter(&connection, agent).prepare(true));
                assert_eq!(legacy.body, wait_body("s1", agent));
                assert!(legacy.valid(&connection.root, "s1", agent, &notice(None)));
            }
        });
    }

    #[cfg(unix)]
    #[test]
    fn renewal_reloads_bearer_and_managed_path_without_retired_metadata() {
        client_config::in_isolated_config_home(|config| {
            let (_directory, connection, mut context) =
                session_fixture(config, "https://hub.example", "claude");
            crate::client_session::save_context(config, &context).unwrap();
            let mut waiter = test_waiter(&connection, Waker::Claude);
            let original = ready(waiter.prepare(true));
            crate::hub_cli::write_private(&connection.token_file, "renewed").unwrap();
            assert!(!original.valid(
                &connection.root,
                "s1",
                Waker::Claude,
                &notice(Some("inc-a"))
            ));
            let renewed = ready(waiter.prepare(false));
            assert_eq!(renewed.token, "renewed");
            assert_eq!(renewed.body, wait_body("s1", Waker::Claude));
            assert!(renewed.valid(&connection.root, "s1", Waker::Claude, &notice(None)));
            let mut managed = connection.clone();
            managed.token_file =
                client_config::owned_token_path(&managed, "tok_synthetic").unwrap();
            managed.owned_token_id = Some("tok_synthetic".into());
            managed.owned_token_expires_at = Some(agentscm::hub::store::unix_now() + 3600);
            crate::hub_cli::write_private(&managed.token_file, "managed-renewal").unwrap();
            save_connection(config, &managed);
            assert!(!renewed.valid(&connection.root, "s1", Waker::Claude, &notice(None)));
            let selected = ready(waiter.prepare(false));
            assert_eq!(selected.token, "managed-renewal");
            assert_eq!(selected.connection, managed);
            assert!(selected.incarnation.is_none());
            assert!(selected.valid(&connection.root, "s1", Waker::Claude, &notice(None)));
            context.agent = "codex".into();
            crate::client_session::save_context(config, &context).unwrap();
            assert!(matches!(
                test_waiter(&connection, Waker::Codex).prepare(false),
                Admission::Stop
            ));
        });
    }

    #[cfg(unix)]
    #[test]
    fn modern_response_rejects_successor_end_corruption_and_rebinding() {
        client_config::in_isolated_config_home(|config| {
            let (_directory, connection, mut context) =
                session_fixture(config, "https://hub.example", "claude");
            crate::client_session::save_context(config, &context).unwrap();
            let mut waiter = test_waiter(&connection, Waker::Claude);
            let request = ready(waiter.prepare(true));
            assert!(request.valid(
                &connection.root,
                "s1",
                Waker::Claude,
                &notice(Some("inc-a"))
            ));
            assert!(!request.valid(&connection.root, "s1", Waker::Claude, &notice(None)));
            context.explicit_end_requested = true;
            crate::client_session::save_context(config, &context).unwrap();
            assert!(!request.valid(
                &connection.root,
                "s1",
                Waker::Claude,
                &notice(Some("inc-a"))
            ));
            assert!(matches!(waiter.prepare(true), Admission::Stop));
            context.start_nonce = "start-b".into();
            context.incarnation = Some("inc-b".into());
            context.explicit_end_requested = false;
            crate::client_session::begin_context(config, Some("start-a"), &context).unwrap();
            assert!(!request.valid(
                &connection.root,
                "s1",
                Waker::Claude,
                &notice(Some("inc-a"))
            ));
            assert!(matches!(waiter.prepare(true), Admission::Stop));
            let mut next = test_waiter(&connection, Waker::Claude);
            let next_request = ready(next.prepare(true));
            let route = std::fs::read_dir(config.join("agentscm/session-routes"))
                .unwrap()
                .next()
                .unwrap()
                .unwrap()
                .path();
            std::fs::write(&route, "corrupt").unwrap();
            assert!(
                matches!(next.prepare(false), Admission::Stop),
                "corruption is not legacy absence"
            );
            assert!(!next_request.valid(
                &connection.root,
                "s1",
                Waker::Claude,
                &notice(Some("inc-b"))
            ));
            let mut rebound = connection.clone();
            rebound.hub = "https://another.example".into();
            save_connection(config, &rebound);
            assert!(matches!(
                test_waiter(&connection, Waker::Claude).prepare(false),
                Admission::Stop
            ));
        });
    }

    #[cfg(unix)]
    #[test]
    fn startup_waits_unpinned_for_foreign_or_ended_routes() {
        for (same_host, state) in [
            (false, "live"),
            (false, "terminal"),
            (false, "explicit-end"),
            (false, "exited"),
            (false, "revoked"),
            (true, "terminal"),
            (true, "explicit-end"),
            (true, "exited"),
            (true, "revoked"),
        ] {
            client_config::in_isolated_config_home(|config| {
                tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .unwrap()
                    .block_on(async {
                        let (url, served) = hub_request(
                            vec![(200, r#"{"text":"AgentSCM Hub message msg_synthetic","incarnation":"inc-b"}"#)],
                            None,
                            wait_body_current("s1", Waker::Claude, "inc-b"),
                        ).await;
                        let (_directory, connection, mut context) =
                            session_fixture(config, &url, "claude");
                        let own = test_host(101);
                        context.registration_host = Some(if same_host { own.clone() } else { test_host(202) });
                        context.terminal = state == "terminal";
                        context.explicit_end_requested = state == "explicit-end";
                        context.exit_observed_at = (state == "exited").then_some(1);
                        context.revoked = state == "revoked";
                        crate::client_session::save_context(config, &context).unwrap();
                        let mut waiter = startup_waiter(&connection, Some(own.clone()));
                        assert!(matches!(waiter.prepare(true), Admission::Pending), "{same_host}/{state}");
                        assert!(waiter.nonce.is_none());
                        assert!(!waiter.sent);
                        assert!(matches!(
                            startup_waiter(&connection, Some(own.clone())).prepare(false),
                            Admission::Stop
                        ));

                        // Only the relay's replacement can be the first HTTP request.
                        context.start_nonce = "start-b".into();
                        context.incarnation = Some("inc-b".into());
                        context.registration_host = Some(own);
                        context.terminal = false;
                        context.explicit_end_requested = false;
                        context.exit_observed_at = None;
                        context.revoked = false;
                        crate::client_session::begin_context(config, Some("start-a"), &context).unwrap();
                        let found = wait_until(
                            &reqwest::Client::new(), MAX_LIFE, || true, Duration::ZERO,
                            |grace| waiter.prepare(grace),
                            |request, notice| request.valid(&connection.root, "s1", Waker::Claude, notice),
                        ).await;
                        assert!(found.is_some());
                        assert_eq!(served.await.unwrap(), 1);
                        assert_eq!(waiter.nonce.as_deref(), Some("start-b"));
                    });
            });
        }
    }

    #[cfg(unix)]
    #[test]
    fn startup_unknown_or_foreign_host_never_uses_legacy_context() {
        client_config::in_isolated_config_home(|config| {
            let (_directory, connection, mut context) =
                session_fixture(config, "https://hub.example", "claude");
            // A genuinely absent context retains legacy compatibility after the bounded grace.
            let mut absent = startup_waiter(&connection, None);
            assert!(matches!(absent.prepare(true), Admission::Pending));
            assert_eq!(
                ready(absent.prepare(false)).body,
                wait_body("s1", Waker::Claude)
            );
            context.protocol = 0;
            context.registration_host = Some(test_host(101));
            crate::client_session::save_context(config, &context).unwrap();
            for own in [None, Some(test_host(202))] {
                let mut waiter = startup_waiter(&connection, own);
                assert!(matches!(waiter.prepare(true), Admission::Pending));
                assert!(matches!(waiter.prepare(false), Admission::Stop));
                assert!(waiter.nonce.is_none());
                assert!(!waiter.sent);
            }
        });
    }

    #[cfg(unix)]
    #[test]
    fn startup_lock_handoff_adopts_only_the_published_successor() {
        client_config::in_isolated_config_home(|config| {
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap()
                .block_on(async {
                    let (url, served) = hub_request(
                        vec![(200, r#"{"text":"AgentSCM Hub message msg_synthetic","incarnation":"inc-b"}"#)],
                        None,
                        wait_body_current("s1", Waker::Claude, "inc-b"),
                    ).await;
                    let (_directory, connection, mut context) = session_fixture(config, &url, "claude");
                    let own = test_host(101);
                    context.registration_host = Some(own.clone());
                    crate::client_session::save_context(config, &context).unwrap();
                    let dir = tempfile::tempdir().unwrap();
                    let held = lock(dir.path(), "s1").unwrap();
                    let ended = marker(dir.path(), "s1");
                    File::create(&ended).unwrap().set_modified(
                        std::time::SystemTime::now() - MAX_LIFE - Duration::from_secs(2 * 24 * 3600),
                    ).unwrap();
                    let mut waiter = startup_waiter(&connection, Some(own));
                    let contender = lock_until(dir.path(), "s1", true, Duration::from_secs(2), || true);
                    tokio::pin!(contender);
                    tokio::select! {
                        biased;
                        _ = &mut contender => panic!("SessionStart must wait for its predecessor"),
                        _ = std::future::ready(()) => {}
                    }
                    assert!(ended.exists(), "a contender cannot remove even a stale predecessor marker");
                    assert!(waiter.nonce.is_none());
                    context.start_nonce = "start-b".into();
                    context.incarnation = Some("inc-b".into());
                    crate::client_session::begin_context(config, Some("start-a"), &context).unwrap();
                    drop(held);
                    let _acquired = contender.await.expect("the successor acquires the released lock");
                    assert!(!ended.exists(), "stale marker cleanup happens only while holding the lock");
                    let found = wait_until(
                        &reqwest::Client::new(), MAX_LIFE, || true, Duration::ZERO,
                        |grace| {
                            assert!(grace, "registration grace starts after lock handoff");
                            waiter.prepare(grace)
                        },
                        |request, notice| request.valid(&connection.root, "s1", Waker::Claude, notice),
                    ).await;
                    assert!(found.is_some());
                    assert_eq!(served.await.unwrap(), 1);
                    assert_eq!(waiter.nonce.as_deref(), Some("start-b"));
                });
        });
    }

    #[cfg(unix)]
    #[test]
    fn startup_relay_first_and_live_predecessor_both_keep_successor_fences() {
        // Relay-first B is immediately usable. A live same-host predecessor A with no holder
        // is indistinguishable: it may be pinned, but the later relay mint must fence its reply.
        for (nonce, incarnation, response) in [
            (
                "start-b",
                "inc-b",
                r#"{"text":"AgentSCM Hub message msg_synthetic","incarnation":"inc-b"}"#,
            ),
            (
                "start-a",
                "inc-a",
                r#"{"text":"AgentSCM Hub message msg_synthetic","incarnation":"inc-a"}"#,
            ),
        ] {
            client_config::in_isolated_config_home(|config| {
                tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .unwrap()
                    .block_on(async {
                        let (url, served) = hub_request(
                            vec![(200, response)],
                            None,
                            wait_body_current("s1", Waker::Claude, incarnation),
                        )
                        .await;
                        let (_directory, connection, mut context) =
                            session_fixture(config, &url, "claude");
                        let own = test_host(101);
                        context.registration_host = Some(own.clone());
                        context.start_nonce = nonce.into();
                        context.incarnation = Some(incarnation.into());
                        crate::client_session::save_context(config, &context).unwrap();
                        let mut waiter = startup_waiter(&connection, Some(own));
                        assert_eq!(
                            ready(waiter.prepare(true)).incarnation.as_deref(),
                            Some(incarnation)
                        );
                        let mut successor = context.clone();
                        successor.start_nonce = "start-c".into();
                        successor.incarnation = Some("inc-c".into());
                        let found = wait_until(
                            &reqwest::Client::new(),
                            MAX_LIFE,
                            || true,
                            Duration::ZERO,
                            |grace| waiter.prepare(grace),
                            |request, reply| {
                                crate::client_session::begin_context(
                                    config,
                                    Some(nonce),
                                    &successor,
                                )
                                .unwrap();
                                assert!(!request.valid(
                                    &connection.root,
                                    "s1",
                                    Waker::Claude,
                                    &notice(Some("inc-c"))
                                ));
                                request.valid(&connection.root, "s1", Waker::Claude, reply)
                            },
                        )
                        .await;
                        assert!(
                            found.is_none(),
                            "the in-flight predecessor reply is discarded"
                        );
                        assert_eq!(served.await.unwrap(), 1);
                        assert!(matches!(waiter.prepare(true), Admission::Stop));
                        assert_eq!(waiter.nonce.as_deref(), Some(nonce));
                    });
            });
        }
    }

    #[tokio::test(start_paused = true)]
    async fn startup_lock_wait_is_bounded_and_other_duplicates_exit_immediately() {
        let dir = tempfile::tempdir().unwrap();
        let _held = lock(dir.path(), "s1").unwrap();
        let started = tokio::time::Instant::now();
        assert!(
            lock_until(dir.path(), "s1", true, Duration::from_secs(1), || true)
                .await
                .is_none()
        );
        assert_eq!(started.elapsed(), Duration::from_secs(1));
        let started = tokio::time::Instant::now();
        assert!(
            lock_until(dir.path(), "s1", false, START_LOCK_GRACE, || true)
                .await
                .is_none()
        );
        assert_eq!(started.elapsed(), Duration::ZERO);

        let alive = std::cell::Cell::new(true);
        let contender = lock_until(dir.path(), "s1", true, START_LOCK_GRACE, || alive.get());
        tokio::pin!(contender);
        tokio::select! {
            biased;
            _ = &mut contender => panic!("the holder has not released"),
            _ = std::future::ready(()) => {}
        }
        alive.set(false);
        assert!(contender.await.is_none(), "parent loss stops lock retries");
        assert_eq!(started.elapsed(), Duration::from_millis(500));
    }

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
        assert!(!framed("something else"));
        assert!(
            !queue_args("019a7c-thread", "notice")
                .iter()
                .any(|arg| arg == "--resume")
        );
    }

    #[cfg(unix)]
    #[test]
    fn native_queue_failure_and_unknown_outcomes_are_distinct() {
        use std::os::unix::fs::PermissionsExt;
        let directory = tempfile::tempdir().unwrap();
        let program = directory.path().join("synthetic-queue");
        assert_eq!(
            spawn_queue(&program, "synthetic", "notice"),
            AttemptOutcome::ProvenNoDispatch
        );
        std::fs::write(&program, "#!/bin/sh\nexit 0\n").unwrap();
        std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o700)).unwrap();
        assert_eq!(
            spawn_queue(&program, "synthetic", "notice"),
            AttemptOutcome::TransportReported
        );
        std::fs::write(&program, "#!/bin/sh\nexit 7\n").unwrap();
        assert_eq!(
            spawn_queue(&program, "synthetic", "notice"),
            AttemptOutcome::Unknown
        );
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
        hub_request(replies, end, wait_body("s1", agent)).await
    }

    async fn hub_request(
        replies: Vec<(u16, &'static str)>,
        end: Option<PathBuf>,
        body_expected: String,
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
                assert!(text.ends_with(&body_expected), "{text}");
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
        wait_until(
            &client,
            life,
            alive,
            Duration::ZERO,
            |_| {
                Admission::Ready(Box::new(WaitRequest {
                    connection: test_connection(dir.path(), url, &token_file),
                    token: crate::client_cli::read_token(&token_file).unwrap(),
                    body: wait_body("s1", agent),
                    nonce: None,
                    incarnation: None,
                }))
            },
            |_, _| true,
        )
        .await
        .map(|(notice, _)| notice.text)
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
