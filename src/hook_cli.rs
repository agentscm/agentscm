//! `agentscm hook <agent>`: the client for command-hook agents. See docs/agents.md.
//!
//! Advisory by construction: whatever goes wrong, the process prints valid JSON for that agent
//! and exits 0, because some agents treat a non-zero exit or invalid JSON as "block the action".
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use agentscm::hub::forge_http::{self, check_base_url, send};
use clap::Args;
use serde_json::{Value, json};

use crate::hub_cli::config_dir;
use crate::intent_cli::{HubFile, parse_hub_file, repo_root, token_for, valid_repo};
use crate::{client_config, client_config::Connection};

const LIMIT: Duration = Duration::from_secs(3);
/// Session start hooks are given 5 seconds, which leaves room to connect a checkout first.
const SESSION_START_LIMIT: Duration = Duration::from_millis(4500);
/// How long a session start waits for auto-connect before going on without it.
const AUTO_CONNECT_WAIT: Duration = Duration::from_millis(2500);
/// A failed auto-connect or renewal is not retried for this long.
const RETRY_EVERY: Duration = Duration::from_secs(5 * 60);
const STDIN_BYTES: usize = 4 << 20;
const REPLY_BYTES: usize = 1 << 20;

#[derive(Debug, Args)]
pub struct HookArgs {
    /// Agent name, followed by explicit --hub-url/--repo routing or local
    /// --client-connection routing. The modes cannot be combined.
    #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
    pub args: Vec<String>,
}

pub async fn run(args: HookArgs) {
    run_unparsed(args.args).await
}

/// Never returns: prints the agent's reply and exits 0.
pub async fn run_unparsed(args: Vec<String>) -> ! {
    let (agent, flag_url, repo) = parse_args(&args);
    let client_connection = args.iter().any(|arg| arg == "--client-connection");
    let destination_args = args.iter().any(|arg| {
        ["--hub-url", "--repo"].iter().any(|flag| {
            arg == *flag
                || arg
                    .strip_prefix(*flag)
                    .is_some_and(|rest| rest.starts_with('='))
        })
    });
    let reply = hook(&agent, flag_url, repo, client_connection, destination_args).await;
    let mut out = std::io::stdout();
    // Ignore write errors (a closed pipe must not panic or change the exit code).
    let _ = writeln!(out, "{reply}");
    let _ = out.flush();
    std::process::exit(0)
}

/// The agent name (first argument that is not a flag), `--hub-url` and `--repo` (kept only when
/// it is a valid `owner/name`). Unknown flags are ignored.
fn parse_args(args: &[String]) -> (String, Option<String>, Option<String>) {
    let mut agent = None;
    let mut url = None;
    let mut repo = None;
    let mut rest = args.iter();
    while let Some(arg) = rest.next() {
        if arg == "--hub-url" {
            url = rest.next().cloned();
        } else if let Some(value) = arg.strip_prefix("--hub-url=") {
            url = Some(value.to_owned());
        } else if arg == "--repo" {
            repo = rest.next().cloned();
        } else if let Some(value) = arg.strip_prefix("--repo=") {
            repo = Some(value.to_owned());
        } else if !arg.starts_with('-') && agent.is_none() {
            agent = Some(arg.clone());
        }
    }
    (
        agent.unwrap_or_default(),
        url,
        repo.filter(|r| valid_repo(r)),
    )
}

async fn hook(
    agent: &str,
    flag_url: Option<String>,
    repo: Option<String>,
    client_connection: bool,
    destination_args: bool,
) -> Value {
    let mut deadline = Instant::now() + LIMIT;
    let read = tokio::time::timeout(LIMIT, tokio::task::spawn_blocking(read_stdin)).await;
    let Ok(Ok(Some(body))) = read else {
        return unknown_output(agent, &Value::Null, "payload unreadable");
    };
    let payload = std::sync::Arc::new(serde_json::from_slice(&body).unwrap_or(Value::Null));
    if is_session_start(agent, &payload) {
        deadline += SESSION_START_LIMIT - LIMIT;
    }
    let remaining = deadline.saturating_duration_since(Instant::now());
    let activity = std::sync::Arc::new(ActivityTracker::new(agent));
    let local_note = std::sync::Arc::new(std::sync::Mutex::new(None));
    let mut call = tokio::spawn(call_hub(
        agent.to_owned(),
        flag_url,
        repo,
        client_connection,
        destination_args,
        std::sync::Arc::clone(&payload),
        HubCallContext {
            original_body: body,
            activity_tracker: activity.clone(),
            local_note: local_note.clone(),
            deadline,
        },
    ));
    let reply = match tokio::time::timeout(remaining, &mut call).await {
        Ok(Ok(Ok(reply))) => reply,
        Ok(Ok(Err(reason))) => unknown_output(agent, &payload, &reason),
        Ok(Err(_)) => unknown_output(agent, &payload, "bad reply"),
        Err(_) => {
            activity.finish(crate::client_activity::ActivityOutcome::Unavailable);
            call.abort();
            unknown_output(agent, &payload, "hub unreachable")
        }
    };
    finish_local_observation(reply, agent, &payload, &local_note, deadline).await
}

/// Consume the local observation after either a Hub reply or its fail-open
/// replacement. Waiting cannot extend the original hook deadline.
async fn finish_local_observation(
    reply: Value,
    agent: &str,
    payload: &Value,
    observation: &LocalObservation,
    deadline: Instant,
) -> Value {
    let task = observation.lock().ok().and_then(|mut task| task.take());
    let note = if let Some(mut task) = task {
        let remaining = deadline.saturating_duration_since(Instant::now());
        match tokio::time::timeout(remaining, &mut task).await {
            Ok(Ok(note)) => note,
            _ => {
                // Cancels a task not started yet. A running task has bounded
                // filesystem work and Git commands bound to the hook deadline.
                task.abort();
                None
            }
        }
    } else {
        None
    };
    match note {
        Some(note) => with_local_note(reply, agent, payload, &note),
        None => reply,
    }
}

/// The whole of stdin, or `None` when it is over the cap or unreadable.
fn read_stdin() -> Option<Vec<u8>> {
    let mut body = Vec::new();
    let limit = STDIN_BYTES as u64 + 1;
    std::io::stdin().take(limit).read_to_end(&mut body).ok()?;
    (body.len() <= STDIN_BYTES).then_some(body)
}

type LocalObservation =
    std::sync::Arc<std::sync::Mutex<Option<tokio::task::JoinHandle<Option<String>>>>>;

struct HubCallContext {
    original_body: Vec<u8>,
    activity_tracker: std::sync::Arc<ActivityTracker>,
    local_note: LocalObservation,
    deadline: Instant,
}

async fn call_hub(
    agent: String,
    flag_url: Option<String>,
    repo: Option<String>,
    client_connection: bool,
    destination_args: bool,
    payload: std::sync::Arc<Value>,
    context: HubCallContext,
) -> Result<Value, String> {
    let HubCallContext {
        original_body,
        activity_tracker,
        local_note,
        deadline,
    } = context;
    let supported_client = matches!(
        agent.as_str(),
        "claude" | "codex" | "cursor" | "copilot" | "gemini"
    );
    if client_connection && destination_args {
        return Err("client connection mode cannot use destination flags".into());
    }
    if client_connection && !supported_client {
        return Err("client connection mode does not support this agent".into());
    }
    let plugin_route = client_connection;
    // One line for the agent's context about this computer's connection, when there is news.
    let mut note = None;
    let (url, token, repo, root, endpoint_agent, outbound, client_connection, report_completion) =
        if plugin_route {
            let mut routed = match route_client(&agent, &payload) {
                Err(reason) if reason == NO_CONNECTION && is_session_start(&agent, &payload) => {
                    match auto_connect(&agent, &payload).await {
                        Some(Ok(repo)) => {
                            note = Some(format!("AgentSCM: coordinating on {repo}."));
                            route_client(&agent, &payload)
                        }
                        Some(Err(refusal)) => Err(refusal),
                        None => Err(reason),
                    }
                }
                routed => routed,
            }
            .inspect_err(|reason| {
                if [NO_CONNECTION, NO_EDIT_CONNECTION].contains(&reason.as_str())
                    || reason.starts_with(AUTO_CONNECT_REFUSED)
                {
                    report_unconnected(&agent, &payload);
                }
            })?;
            // Routing has verified the checkout identity. Start local observation
            // without delaying token renewal or the Hub registration request.
            #[cfg(unix)]
            {
                let checkout = routed.checkout.clone();
                let local_payload = payload.clone();
                let normalized = routed.payload.clone();
                let local_agent = agent.clone();
                let observed = tokio::task::spawn_blocking(move || {
                    branch_advisory(
                        &local_agent,
                        &local_payload,
                        &normalized,
                        &checkout,
                        deadline,
                    )
                });
                if let Ok(mut slot) = local_note.lock() {
                    *slot = Some(observed);
                }
            }
            #[cfg(not(unix))]
            let _ = (&local_note, deadline);
            let session_start = is_session_start(&agent, &payload);
            if renew(&routed.connection, session_start).await {
                routed = route_client(&agent, &payload)?;
            }
            if note.is_none() && session_start {
                note = crate::client_cli::attention(&routed.connection.root)
                    .map(|message| format!("AgentSCM: {message}."));
            }
            (
                routed.connection.hub.clone(),
                Token::PrivateFile(routed.connection.token_file.clone()),
                Some(routed.connection.repo.clone()),
                None,
                routed.endpoint_agent,
                std::borrow::Cow::Owned(routed.payload),
                Some(routed.connection.clone()),
                routed.connection.report_completion,
            )
        } else {
            let env = |name| std::env::var(name).ok().filter(|v| !v.trim().is_empty());
            let settings = Settings {
                url: env("AGENTSCM_HUB_URL"),
                token: env("AGENTSCM_HUB_TOKEN"),
                token_file: env("AGENTSCM_HUB_TOKEN_FILE"),
            };
            let file = config_dir()
                .ok()
                .and_then(|dir| std::fs::read_to_string(dir.join("agentscm").join("hub.json")).ok())
                .and_then(|text| parse_hub_file(&text).ok());
            let (url, token) = resolve(
                &settings,
                flag_url.as_deref(),
                &cwd_of(&payload),
                file.as_ref(),
            )?;
            let root = repo_root(&cwd_of(&payload));
            let outbound = if agent == "codex" {
                std::borrow::Cow::Owned(sanitized_codex(&payload, None)?)
            } else {
                std::borrow::Cow::Borrowed(&*payload)
            };
            (url, token, repo, root, agent.clone(), outbound, None, false)
        };
    let token = match token {
        Token::Value(token) => token,
        Token::File(path) => std::fs::read_to_string(path)
            .map_err(|_| "no token")?
            .trim()
            .to_owned(),
        Token::PrivateFile(path) => crate::client_cli::read_token(&path).map_err(|_| "no token")?,
    };
    if token.is_empty() {
        return Err("no token".into());
    }
    let activity_binding = client_connection
        .as_ref()
        .map(|connection| crate::client_activity::activity_binding(connection, &token));
    let mut activity = ActivityAttempt::new(activity_tracker, activity_binding);
    let client = forge_http::client().map_err(|_| "hub unreachable")?;
    let outbound_body = if plugin_route || agent == "codex" {
        serde_json::to_vec(&outbound).map_err(|_| "payload unreadable")?
    } else {
        // Preserve the legacy transport contract, including original JSON bytes.
        original_body
    };
    let mut request = hub_request(
        &client,
        &url,
        &endpoint_agent,
        repo.as_deref(),
        &token,
        &outbound_body,
        root.as_deref(),
    )?;
    if let Some(connection) = client_connection.as_ref() {
        request = request.header(
            agentscm::hub::onboarding::CLIENT_HEADER,
            client_config::onboarding_client(&connection.root, &token),
        );
    }
    request = client_config::with_completion_reporting(request, report_completion);
    // `send` errors carry no URL or credentials.
    let (status, reply) = send(request, REPLY_BYTES)
        .await
        .map_err(|_| "hub unreachable")?;
    match status {
        200 if reply.is_object() => {
            activity.finish(successful_hook_outcome(&outbound));
            let config = config_dir().ok();
            let reply = if agent == "gemini" && plugin_route {
                gemini_reply(reply, &payload, config.as_deref())
            } else {
                defanged(reply, &agent, &payload, config.as_deref())
            };
            Ok(match note {
                Some(note) => with_note(reply, &agent, &payload, &note),
                None => reply,
            })
        }
        200 => {
            activity.finish(crate::client_activity::ActivityOutcome::InvalidResponse);
            Err("bad reply".into())
        }
        401 | 403 => {
            activity.finish(crate::client_activity::ActivityOutcome::TokenRejected);
            Err("token rejected".into())
        }
        _ => {
            activity.finish(crate::client_activity::ActivityOutcome::Unavailable);
            Err(format!("hub error {status}"))
        }
    }
}

/// This observation stays local. Edits only read the bounded cache; Git runs once
/// at session start and shares the hook deadline. Missing coverage never blocks.
#[cfg(unix)]
fn branch_advisory(
    agent: &str,
    payload: &Value,
    normalized: &Value,
    checkout: &Path,
    deadline: Instant,
) -> Option<String> {
    branch_advisory_at(
        &config_dir().ok()?,
        agent,
        payload,
        normalized,
        checkout,
        deadline.min(Instant::now() + Duration::from_millis(700)),
    )
}

#[cfg(unix)]
fn branch_advisory_at(
    config: &Path,
    agent: &str,
    payload: &Value,
    normalized: &Value,
    checkout: &Path,
    deadline: Instant,
) -> Option<String> {
    // Copilot CLI's preToolUse cannot carry advisory text. Its separate live
    // contention refusal contract is unchanged.
    if agent == "copilot" && payload.get("hook_event_name").is_none() {
        return None;
    }
    let session = session_of(payload).filter(|session| session.len() <= 1024)?;
    let key = format!(
        "branch-observation\0{agent}\0{session}\0{}",
        checkout.display()
    );
    let cache = branch_cache_path(config, &key);
    if is_session_start(agent, payload) {
        if !prepare_branch_session(config, &key, deadline) {
            return Some("AgentSCM branch advisory: local branch coverage unknown; observation storage unavailable or full. Edits remain allowed.".into());
        }
        if !branch_marker(config, &key, "start", deadline) {
            return None;
        }
        let git = crate::quickstart::detect::which("git")
            .and_then(|path| path.canonicalize().ok())
            .unwrap_or_else(|| PathBuf::from("/usr/bin/git"));
        let snapshot = match agentscm::git_collisions::advisory_snapshot(&git, checkout, deadline) {
            Ok(snapshot) => snapshot,
            Err(_) => return Some("AgentSCM branch advisory: local branch coverage unknown; session-start scan unavailable. Edits remain allowed.".into()),
        };
        if !write_branch_cache(&cache, &snapshot) {
            return Some("AgentSCM branch advisory: local branch coverage unknown; observation could not be retained. Edits remain allowed.".into());
        }
        return Some(format!(
            "AgentSCM branch advisory: observed {} peer branches/dirty worktrees at Unix {} (HEAD {}, base {} {}). {} Historical local paths only; later changes and other clones are unknown; squash/rebase merges may appear. First edits receive same-file advice, never proof of conflict.",
            snapshot.peers.len(),
            snapshot.observed_at,
            snapshot.head,
            snapshot.base_name,
            snapshot.base_commit,
            if snapshot.incomplete {
                "Scan incomplete."
            } else {
                "Bounded scan completed."
            },
        ));
    }
    let paths = if agent == "gemini" {
        normalized["paths"]
            .as_array()?
            .iter()
            .filter_map(Value::as_str)
            .map(|path| EditPath {
                header: None,
                path: path.to_owned(),
            })
            .collect()
    } else {
        edit_paths(agent, normalized).ok()?
    };
    if paths.is_empty() {
        return None;
    }
    let snapshot = read_branch_cache(&cache);
    let Some(snapshot) = snapshot else {
        return branch_marker(config, &key, "unknown", deadline).then(||
            "AgentSCM branch advisory: local branch coverage unknown; session-start observation missing, stale or invalid. Edits remain allowed.".into());
    };
    let mut lines = Vec::new();
    for edit in paths.into_iter().take(MAX_EDIT_PATHS) {
        if !agentscm::git_collisions::advisory_path(&edit.path) {
            continue;
        }
        let peers = snapshot.touching(&edit.path);
        if peers.is_empty()
            || !branch_marker(config, &key, &format!("path\0{}", edit.path), deadline)
        {
            continue;
        }
        let peers = peers
            .iter()
            .map(|peer| {
                format!(
                    "{} [{} {}, fork {}]",
                    agentscm::hub::advice::sanitize(&peer.name, 120),
                    peer.kind,
                    peer.commit,
                    peer.fork.as_deref().unwrap_or("uncommitted"),
                )
            })
            .collect::<Vec<_>>()
            .join(", ");
        lines.push(format!(
            "{}: {peers}",
            agentscm::hub::advice::sanitize(&edit.path, 160)
        ));
        if lines.len() == 3 {
            break;
        }
    }
    (!lines.is_empty()).then(|| format!(
        "AgentSCM same-file advisory (session-start observation at Unix {}, HEAD {}, base {} {}{}): {}. Later changes/other clones unknown; squash/rebase merged branches may appear. Same file is not proof of conflict; edits remain allowed.",
        snapshot.observed_at, snapshot.head, snapshot.base_name, snapshot.base_commit,
        if snapshot.incomplete { ", incomplete" } else { "" }, lines.join("; "),
    ))
}

#[cfg(unix)]
const BRANCH_SESSIONS: usize = 32;
#[cfg(unix)]
const BRANCH_MARKERS: usize = 256;

#[cfg(unix)]
fn branch_cache_path(config: &Path, key: &str) -> PathBuf {
    use sha2::{Digest, Sha256};
    config
        .join("agentscm")
        .join("branch-observations")
        .join(format!("{:x}", Sha256::digest(key)))
        .join("cache.json")
}

#[cfg(unix)]
fn branch_lock(path: &Path, deadline: Instant) -> Option<std::fs::File> {
    use std::fs::TryLockError;
    use std::os::unix::fs::OpenOptionsExt;
    if Instant::now() >= deadline {
        return None;
    }
    let retry_until = deadline.min(Instant::now() + Duration::from_millis(50));
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK)
        .open(path)
        .ok()?;
    if !file.metadata().ok()?.is_file() {
        return None;
    }
    loop {
        if Instant::now() >= retry_until {
            return None;
        }
        match file.try_lock() {
            Ok(()) => return Some(file),
            Err(TryLockError::WouldBlock) => {}
            Err(TryLockError::Error(_)) => return None,
        }
        let remaining = retry_until.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return None;
        }
        std::thread::sleep(remaining.min(Duration::from_millis(5)));
    }
}

#[cfg(unix)]
fn branch_name(name: &str) -> bool {
    name.len() == 64 && name.bytes().all(|byte| byte.is_ascii_hexdigit())
}

/// Maintenance is only at SessionStart. Both directory and child enumeration are
/// capped; only our regular files are removed, with no symlink traversal.
#[cfg(unix)]
fn prepare_branch_session(config: &Path, key: &str, deadline: Instant) -> bool {
    use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
    let cache = branch_cache_path(config, key);
    let Some(session) = cache.parent() else {
        return false;
    };
    let Some(root) = session.parent() else {
        return false;
    };
    if std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(root)
        .is_err()
        || !std::fs::symlink_metadata(root).is_ok_and(|metadata| metadata.is_dir())
    {
        return false;
    }
    let Some(_lock) = branch_lock(&root.join(".lock"), deadline) else {
        return false;
    };
    let Ok(entries) = std::fs::read_dir(root) else {
        return false;
    };
    let entries: Vec<_> = entries.take(BRANCH_SESSIONS + 2).collect();
    for entry in entries {
        let Ok(entry) = entry else {
            return false;
        };
        if entry.file_name() == ".lock" {
            continue;
        }
        let path = entry.path();
        let is_session = entry.file_name().to_str().is_some_and(branch_name)
            && entry.file_type().is_ok_and(|kind| kind.is_dir());
        let old = is_session
            && std::fs::symlink_metadata(path.join("created"))
                .ok()
                .filter(|metadata| metadata.is_file())
                .or_else(|| {
                    std::fs::symlink_metadata(&path)
                        .ok()
                        .filter(|metadata| metadata.is_dir())
                })
                .and_then(|metadata| metadata.modified().ok())
                .and_then(|modified| modified.elapsed().ok())
                .is_some_and(|age| age > Duration::from_secs(4 * 60 * 60));
        if old {
            remove_branch_session(&path);
        }
    }
    // Count after bounded expiry work, so one stray over-cap entry cannot
    // permanently prevent reclaiming the sessions inspected above.
    let Ok(entries) = std::fs::read_dir(root) else {
        return false;
    };
    let entries: Vec<_> = entries.take(BRANCH_SESSIONS + 2).collect();
    if entries.len() > BRANCH_SESSIONS + 1 || entries.iter().any(Result::is_err) {
        return false;
    }
    let retained = entries
        .iter()
        .flatten()
        .filter(|entry| entry.file_name() != ".lock")
        .count();
    if std::fs::symlink_metadata(session).is_ok_and(|metadata| metadata.is_dir()) {
        return true;
    }
    if retained >= BRANCH_SESSIONS {
        return false;
    }
    if std::fs::DirBuilder::new()
        .mode(0o700)
        .create(session)
        .is_err()
    {
        return false;
    }
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(session.join("created"))
        .is_ok()
}

#[cfg(unix)]
fn remove_branch_session(path: &Path) {
    let Ok(entries) = std::fs::read_dir(path) else {
        return;
    };
    let entries: Vec<_> = entries.take(BRANCH_MARKERS + 5).collect();
    if entries.len() > BRANCH_MARKERS + 4 {
        return;
    }
    for entry in entries.into_iter().flatten() {
        let name = entry.file_name();
        let recognized = name.to_str().is_some_and(|name| {
            matches!(name, "cache.json" | "created" | ".lock" | "count")
                || name.strip_suffix(".seen").is_some_and(branch_name)
        });
        if recognized && entry.file_type().is_ok_and(|kind| kind.is_file()) {
            let _ = std::fs::remove_file(entry.path());
        }
    }
    let _ = std::fs::remove_dir(path);
}

/// Constant work per edit. Reserve one of a fixed number of markers while holding
/// a lock with a short, deadline-bounded contention retry; reserving before
/// creation bounds files even after a crash.
#[cfg(unix)]
fn branch_marker(config: &Path, key: &str, marker: &str, deadline: Instant) -> bool {
    use sha2::{Digest, Sha256};
    use std::io::{Seek, SeekFrom};
    use std::os::unix::fs::OpenOptionsExt;
    let cache = branch_cache_path(config, key);
    let Some(parent) = cache.parent() else {
        return false;
    };
    if !std::fs::symlink_metadata(parent).is_ok_and(|metadata| metadata.is_dir()) {
        return false;
    }
    let Some(_lock) = branch_lock(&parent.join(".lock"), deadline) else {
        return false;
    };
    let path = parent.join(format!("{:x}.seen", Sha256::digest(marker)));
    if std::fs::symlink_metadata(&path).is_ok() {
        return false;
    }
    let Ok(mut count) = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK)
        .open(parent.join("count"))
    else {
        return false;
    };
    if !count
        .metadata()
        .is_ok_and(|metadata| metadata.is_file() && metadata.len() <= 16)
    {
        return false;
    }
    let mut text = String::new();
    if Read::by_ref(&mut count)
        .take(16)
        .read_to_string(&mut text)
        .is_err()
    {
        return false;
    }
    let used = if text.is_empty() {
        0
    } else {
        text.parse::<usize>().unwrap_or(BRANCH_MARKERS)
    };
    if used >= BRANCH_MARKERS
        || count.seek(SeekFrom::Start(0)).is_err()
        || count.set_len(0).is_err()
        || write!(count, "{}", used + 1).is_err()
    {
        return false;
    }
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(path)
        .is_ok()
}

#[cfg(unix)]
fn read_branch_cache(path: &Path) -> Option<agentscm::git_collisions::AdvisorySnapshot> {
    use agentscm::git_collisions::{ADVISORY_BYTES, AdvisorySnapshot};
    use std::os::unix::fs::OpenOptionsExt;
    let file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK)
        .open(path)
        .ok()?;
    if !file.metadata().ok()?.is_file() {
        return None;
    }
    if file.metadata().ok()?.len() > ADVISORY_BYTES as u64 {
        return None;
    }
    let mut bytes = Vec::new();
    file.take(ADVISORY_BYTES as u64 + 1)
        .read_to_end(&mut bytes)
        .ok()?;
    if bytes.len() > ADVISORY_BYTES {
        return None;
    }
    let snapshot: AdvisorySnapshot = serde_json::from_slice(&bytes).ok()?;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()?
        .as_secs();
    snapshot.valid(now).then_some(snapshot)
}

#[cfg(unix)]
fn write_branch_cache(path: &Path, snapshot: &agentscm::git_collisions::AdvisorySnapshot) -> bool {
    use std::os::unix::fs::OpenOptionsExt;
    let Some(parent) = path.parent() else {
        return false;
    };
    let Ok(bytes) = serde_json::to_vec(snapshot) else {
        return false;
    };
    if bytes.len() > agentscm::git_collisions::ADVISORY_BYTES {
        return false;
    }
    if !std::fs::symlink_metadata(parent).is_ok_and(|metadata| metadata.is_dir()) {
        return false;
    }
    // A per-session create_new file needs no replacement or shared cache lock.
    let Ok(mut file) = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
    else {
        return false;
    };
    file.write_all(&bytes).is_ok()
}

struct ActivityTracker {
    agent: String,
    binding: std::sync::OnceLock<String>,
    finished: std::sync::atomic::AtomicBool,
    #[cfg(test)]
    recorded:
        Option<std::sync::Arc<std::sync::Mutex<Vec<crate::client_activity::ActivityOutcome>>>>,
}

impl ActivityTracker {
    fn new(agent: &str) -> Self {
        Self {
            agent: agent.to_owned(),
            binding: std::sync::OnceLock::new(),
            finished: std::sync::atomic::AtomicBool::new(false),
            #[cfg(test)]
            recorded: None,
        }
    }

    #[cfg(test)]
    fn recording(
        recorded: std::sync::Arc<std::sync::Mutex<Vec<crate::client_activity::ActivityOutcome>>>,
    ) -> Self {
        Self {
            agent: "codex".into(),
            binding: std::sync::OnceLock::from("0".repeat(64)),
            finished: std::sync::atomic::AtomicBool::new(false),
            recorded: Some(recorded),
        }
    }

    fn set_binding(&self, binding: Option<String>) {
        if let Some(binding) = binding {
            let _ = self.binding.set(binding);
        }
    }

    fn binding(&self) -> Option<&str> {
        self.binding.get().map(String::as_str)
    }

    fn finish(&self, outcome: crate::client_activity::ActivityOutcome) {
        use std::sync::atomic::Ordering;
        if self.finished.swap(true, Ordering::AcqRel) {
            return;
        }
        let Some(binding) = self.binding() else {
            return;
        };
        #[cfg(test)]
        if let Some(recorded) = &self.recorded {
            recorded.lock().unwrap().push(outcome);
        } else {
            crate::client_activity::observe(
                binding,
                Some(&self.agent),
                crate::client_activity::ActivityPath::Hook,
                outcome,
            );
        }
        #[cfg(not(test))]
        crate::client_activity::observe(
            binding,
            Some(&self.agent),
            crate::client_activity::ActivityPath::Hook,
            outcome,
        );
    }
}

struct ActivityAttempt {
    tracker: std::sync::Arc<ActivityTracker>,
}

impl ActivityAttempt {
    fn new(tracker: std::sync::Arc<ActivityTracker>, binding: Option<String>) -> Self {
        tracker.set_binding(binding);
        Self { tracker }
    }

    fn finish(&mut self, outcome: crate::client_activity::ActivityOutcome) {
        self.tracker.finish(outcome);
    }
}

impl Drop for ActivityAttempt {
    fn drop(&mut self) {
        self.tracker
            .finish(crate::client_activity::ActivityOutcome::Unavailable);
    }
}

fn successful_hook_outcome(payload: &Value) -> crate::client_activity::ActivityOutcome {
    let ended = matches!(
        payload["hook_event_name"].as_str(),
        Some("SessionEnd" | "sessionEnd")
    ) || payload["event"].as_str() == Some("done")
        || payload["reason"].as_str() == Some("client-session-end");
    if ended {
        crate::client_activity::ActivityOutcome::SessionEnded
    } else {
        crate::client_activity::ActivityOutcome::Acknowledged
    }
}

fn hub_request(
    client: &reqwest::Client,
    url: &str,
    agent: &str,
    repo: Option<&str>,
    token: &str,
    outbound: &[u8],
    root: Option<&Path>,
) -> Result<reqwest::RequestBuilder, String> {
    // The hub answers "coordination unknown" when the repository differs from the token's.
    let query = repo.map_or_else(String::new, |repo| format!("?repo={repo}"));
    let request = client
        .post(format!("{url}/v1/hub/hook/{}{query}", path_segment(agent)))
        .header(reqwest::header::CONTENT_TYPE, "application/json")
        .bearer_auth(token)
        .body(outbound.to_vec());
    Ok(with_root_header(request, root))
}

fn with_root_header(
    mut request: reqwest::RequestBuilder,
    root: Option<&Path>,
) -> reqwest::RequestBuilder {
    if let Some(root) = root
        .and_then(Path::to_str)
        .and_then(|root| reqwest::header::HeaderValue::from_str(root).ok())
    {
        request = request.header("X-Agentscm-Root", root);
    }
    request
}

const MAX_WORKSPACE_ROOTS: usize = 16;
/// Every new worktree of a connected checkout is covered, so this means the repository itself.
const NO_CONNECTION: &str =
    "no client connection for this repository; run `agentscm quickstart` in it once";
const NO_EDIT_CONNECTION: &str = "no client connection for an edit path";
/// How often one agent re-reports the same unconnected checkout to the tray, and how long that
/// report may hold the hook.
const UNCONNECTED_EVERY: Duration = Duration::from_secs(5 * 60);
const UNCONNECTED_WAIT: Duration = Duration::from_millis(500);
const MAX_EDIT_PATHS: usize = 256;
const PATCH_HEADERS: [&str; 4] = [
    "*** Update File: ",
    "*** Add File: ",
    "*** Delete File: ",
    "*** Move to: ",
];

#[derive(Clone, Debug, PartialEq, Eq)]
struct EditPath {
    header: Option<&'static str>,
    path: String,
}

#[derive(Debug)]
struct RoutedClient {
    connection: Connection,
    /// The registered root, or a linked worktree of it.
    checkout: PathBuf,
    endpoint_agent: String,
    payload: Value,
}

fn patch_text(input: &Value) -> Option<&str> {
    input.as_str().or_else(|| input["command"].as_str())
}

fn valid_path_text(path: &str) -> bool {
    !path.is_empty() && path.len() <= 1024 && !path.chars().any(char::is_control)
}

fn tool_call(agent: &str, payload: &Value) -> (String, Value) {
    if agent == "copilot" && payload.get("hook_event_name").is_none() {
        let input = match payload.get("toolArgs") {
            Some(Value::String(text)) => serde_json::from_str(text).unwrap_or_else(|_| {
                if payload["toolName"]
                    .as_str()
                    .is_some_and(|tool| tool.eq_ignore_ascii_case("apply_patch"))
                {
                    Value::String(text.clone())
                } else {
                    Value::Null
                }
            }),
            Some(input) => input.clone(),
            None => Value::Null,
        };
        return (
            payload["toolName"].as_str().unwrap_or_default().to_owned(),
            input,
        );
    }
    (
        payload["tool_name"].as_str().unwrap_or_default().to_owned(),
        payload.get("tool_input").cloned().unwrap_or(Value::Null),
    )
}

fn edit_paths(agent: &str, payload: &Value) -> Result<Vec<EditPath>, String> {
    let (tool, input) = tool_call(agent, payload);
    let mut paths = Vec::new();
    if tool.eq_ignore_ascii_case("apply_patch") {
        let patch = patch_text(&input).ok_or_else(|| "edit path not understood".to_owned())?;
        for line in patch.lines() {
            if let Some((header, path)) = PATCH_HEADERS
                .iter()
                .find_map(|header| line.strip_prefix(header).map(|path| (*header, path)))
            {
                let path = path.trim();
                if valid_path_text(path) {
                    paths.push(EditPath {
                        header: Some(header),
                        path: path.to_owned(),
                    });
                } else {
                    return Err("edit path not understood".into());
                }
            }
            if paths.len() > MAX_EDIT_PATHS {
                return Err("too many edit paths".into());
            }
        }
        if paths.is_empty() {
            return Err("edit path not understood".into());
        }
        return Ok(paths);
    }
    if [
        "edit",
        "write",
        "multiedit",
        "notebookedit",
        "create",
        "str_replace_editor",
        "delete",
        "write_file",
        "replace",
    ]
    .iter()
    .any(|name| tool.eq_ignore_ascii_case(name))
    {
        let path = ["file_path", "notebook_path", "path", "filePath"]
            .iter()
            .find_map(|field| input[*field].as_str())
            .ok_or_else(|| "edit path not understood".to_owned())?;
        if !valid_path_text(path) {
            return Err("edit path not understood".into());
        }
        paths.push(EditPath {
            header: None,
            path: path.to_owned(),
        });
    }
    Ok(paths)
}

fn unique_connection(
    connections: &[Connection],
    candidates: impl IntoIterator<Item = PathBuf>,
) -> Result<Option<(&Connection, PathBuf)>, String> {
    let mut found: Vec<(&Connection, PathBuf)> = Vec::new();
    for candidate in candidates {
        let connection = match client_config::connection_for_checkout(connections, &candidate) {
            Some((connection, _)) => connection,
            // An unregistered nested Git checkout must never inherit the parent's credential.
            None if client_config::connection_for_path(connections, &candidate).is_some() => {
                return Err("path belongs to another Git checkout".into());
            }
            None => continue,
        };
        if !found.iter().any(|(saved, _)| saved.root == connection.root) {
            found.push((connection, candidate));
        }
    }
    match found.len() {
        0 => Ok(None),
        1 => Ok(found.pop()),
        _ => Err("repository route is ambiguous".into()),
    }
}

fn match_edit<'a>(
    payload: &Value,
    connections: &'a [Connection],
    path: &str,
) -> Result<Option<(&'a Connection, PathBuf)>, String> {
    if payload["workspace_roots"]
        .as_array()
        .is_some_and(|roots| roots.len() > MAX_WORKSPACE_ROOTS)
    {
        return Err("too many workspace roots".into());
    }
    // A containing root without a checkout match is kept so the caller refuses a nested checkout.
    let route = |candidate: &Path| {
        client_config::connection_for_checkout(connections, candidate)
            .map(|(connection, _)| connection)
            .or_else(|| client_config::connection_for_path(connections, candidate))
    };
    let path = Path::new(path);
    if path.is_absolute() {
        return Ok(route(path).map(|connection| (connection, path.to_owned())));
    }
    if let Some(cwd) = payload["cwd"].as_str() {
        let candidate = Path::new(cwd).join(path);
        if let Some(connection) = route(&candidate) {
            return Ok(Some((connection, candidate)));
        }
    }
    let workspace_candidates = payload["workspace_roots"]
        .as_array()
        .into_iter()
        .flatten()
        .take(MAX_WORKSPACE_ROOTS)
        .filter_map(Value::as_str)
        .map(|root| Path::new(root).join(path));
    if let Some(found) = unique_connection(connections, workspace_candidates)? {
        return Ok(Some(found));
    }
    Ok(None)
}

fn match_session<'a>(
    payload: &Value,
    connections: &'a [Connection],
) -> Result<Option<(&'a Connection, PathBuf)>, String> {
    if payload["workspace_roots"]
        .as_array()
        .is_some_and(|roots| roots.len() > MAX_WORKSPACE_ROOTS)
    {
        return Err("too many workspace roots".into());
    }
    let cwd = payload["cwd"].as_str().map(PathBuf::from);
    let workspace_roots = payload["workspace_roots"]
        .as_array()
        .into_iter()
        .flatten()
        .take(MAX_WORKSPACE_ROOTS)
        .filter_map(Value::as_str)
        .map(PathBuf::from);
    unique_connection(connections, cwd.into_iter().chain(workspace_roots))
}

fn route_client(agent: &str, payload: &Value) -> Result<RoutedClient, String> {
    let connections = client_config::load().map_err(|_| "client connections unavailable")?;
    let routed = route_client_with(agent, payload, &connections)?;
    if !client_config::is_current_in(&routed.connection, &routed.checkout) {
        return Err("client connection no longer matches this checkout".into());
    }
    Ok(routed)
}

fn route_client_with(
    agent: &str,
    payload: &Value,
    connections: &[Connection],
) -> Result<RoutedClient, String> {
    if connections.is_empty() {
        return Err(NO_CONNECTION.into());
    }
    let edits = edit_paths(agent, payload)?;
    let mut selected: Option<(Connection, PathBuf)> = None;
    let mut normalized = Vec::with_capacity(edits.len());
    for edit in &edits {
        let Some((connection, candidate)) = match_edit(payload, connections, &edit.path)? else {
            return Err(NO_EDIT_CONNECTION.into());
        };
        if let Some((selected, _)) = &selected
            && selected.root != connection.root
        {
            return Err("edit crosses configured repositories".into());
        }
        // An unregistered nested Git checkout must never inherit the parent's credential.
        let Some((_, checkout)) = client_config::connection_for_checkout(connections, &candidate)
            .filter(|(routed, _)| routed.root == connection.root)
        else {
            return Err("edit path belongs to another Git checkout".into());
        };
        if selected
            .as_ref()
            .is_some_and(|(_, selected)| *selected != checkout)
        {
            return Err("edit crosses Git worktrees".into());
        }
        let relative = client_config::relative(&checkout, &candidate)
            .ok_or_else(|| "edit path is outside the configured repository".to_owned())?;
        selected = Some((connection.clone(), checkout));
        normalized.push(EditPath {
            header: edit.header,
            path: relative,
        });
    }
    let (connection, checkout) = match selected {
        Some(selected) => selected,
        None => match_session(payload, connections)?
            .and_then(|(_, candidate)| {
                client_config::connection_for_checkout(connections, &candidate)
            })
            .map(|(connection, checkout)| (connection.clone(), checkout))
            .ok_or_else(|| NO_CONNECTION.to_owned())?,
    };
    let endpoint_agent = if agent == "gemini" { "generic" } else { agent };
    Ok(RoutedClient {
        payload: sanitized_client(agent, payload, &normalized)?,
        connection,
        checkout,
        endpoint_agent: endpoint_agent.to_owned(),
    })
}

/// A start-of-session event, when an agent begins a conversation.
fn is_session_start(agent: &str, payload: &Value) -> bool {
    match payload["hook_event_name"].as_str() {
        Some(name) => matches!(name, "SessionStart" | "sessionStart"),
        // Copilot CLI's camelCase payload names no event.
        None => {
            agent == "copilot"
                && (payload.get("source").is_some() || payload.get("initialPrompt").is_some())
        }
    }
}

const AUTO_CONNECT_REFUSED: &str = "auto-connect refused: ";

/// Runs `agentscm client <command> --root <root>` as its own process, so its work finishes even
/// after this hook has answered. With `wait`, returns its one-line JSON result if it ends in
/// time; otherwise it runs on unwatched.
fn client_child(command: &[&str], root: &Path, wait: Option<Duration>) -> Option<Value> {
    use std::process::{Command, Stdio};
    let program = std::env::current_exe().ok()?;
    let mut child = Command::new(program)
        .args(["--no-telemetry", "client"])
        .args(command)
        .arg("--root")
        .arg(root)
        .stdin(Stdio::null())
        .stdout(if wait.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let wait = wait?;
    let stdout = child.stdout.take()?;
    let (sender, receiver) = std::sync::mpsc::sync_channel(1);
    std::thread::spawn(move || {
        let mut text = String::new();
        let _ = stdout.take(16 * 1024).read_to_string(&mut text);
        let _ = sender.send(text);
        let _ = child.wait();
    });
    let text = receiver.recv_timeout(wait).ok()?;
    serde_json::from_str(text.trim()).ok()
}

/// At the start of a session in a checkout with no connection: connect it through a Hub with
/// auto-connect on. Some(Ok(repository)) when connected, Some(Err(reason and fix)) when a Hub
/// that has the repository refused, None when there is nothing to say (auto-connect is off, no
/// Hub has the repository, or it did not finish in time; it may still finish for the next
/// session).
async fn auto_connect(agent: &str, payload: &Value) -> Option<std::result::Result<String, String>> {
    let checkout = repo_root(&cwd_of(payload))?;
    let main = client_config::main_checkout(&checkout);
    let config = config_dir().ok()?;
    if crate::hub_cli::auto_connect_hubs()
        .ok()
        .is_none_or(|hubs| hubs.is_empty())
    {
        return None;
    }
    let key = format!("auto-connect\0{}", main.display());
    if marked_within(&config, &key, RETRY_EVERY) {
        return None;
    }
    // Marked first, so a hung Hub or a slow child is not retried at every session start.
    mark(&config, &key);
    let agent = agent.to_owned();
    let result = tokio::task::spawn_blocking(move || {
        client_child(
            &["auto-connect", "--agent", &agent],
            &main,
            Some(AUTO_CONNECT_WAIT),
        )
    })
    .await
    .ok()??;
    let message = || {
        result["message"]
            .as_str()
            .filter(|message| !message.chars().any(char::is_control))
            .map(|message| Err(format!("{AUTO_CONNECT_REFUSED}{message}")))
    };
    match result["result"].as_str() {
        Some("connected") => {
            let _ = std::fs::remove_file(marker_path(&config, &key));
            result["repository"]
                .as_str()
                .filter(|repo| valid_repo(repo))
                .map(|repo| Ok(repo.to_owned()))
        }
        // A refusal, or a fork's hint, with its fix.
        Some("refused" | "not_connected") => message(),
        _ => None,
    }
}

/// Starts renewing the checkout's agent token in its own process when it is due, at most once
/// every few minutes per checkout. It waits only at session start for a token that has already
/// ended (a computer idle for a day), within the hook's budget; true when that renewed it.
async fn renew(connection: &Connection, session_start: bool) -> bool {
    let Ok(config) = config_dir() else {
        return false;
    };
    let now = agentscm::hub::store::unix_now();
    let key = format!("renew\0{}", connection.root.display());
    if !crate::client_cli::due(connection, now) || marked_within(&config, &key, RETRY_EVERY) {
        return false;
    }
    mark(&config, &key);
    let ended = connection
        .owned_token_expires_at
        .is_some_and(|expires| expires <= now);
    if !(session_start && ended) {
        client_child(&["renew"], &connection.root, None);
        return false;
    }
    let root = connection.root.clone();
    tokio::task::spawn_blocking(move || client_child(&["renew"], &root, Some(AUTO_CONNECT_WAIT)))
        .await
        .ok()
        .flatten()
        .is_some_and(|result| result["result"] == "renewed")
}

/// Append historical local advice after connection and Hub context. Local scan
/// availability must not replace or take priority over coordination messages.
fn with_local_note(mut reply: Value, agent: &str, payload: &Value, note: &str) -> Value {
    for (field, supported) in [
        ("/hookSpecificOutput/additionalContext", true),
        ("/additional_context", agent == "cursor"),
        ("/systemMessage", agent == "gemini"),
        ("/additionalContext", agent == "copilot"),
    ] {
        if !supported {
            continue;
        }
        if let Some(value) = reply.pointer_mut(field)
            && let Some(existing) = value.as_str()
        {
            *value = if existing.is_empty() {
                json!(note)
            } else {
                json!(format!("{existing}\n\n{note}"))
            };
            return reply;
        }
    }
    with_note(reply, agent, payload, note)
}

/// The hub's reply with `note` first in the agent's context.
fn with_note(reply: Value, agent: &str, payload: &Value, note: &str) -> Value {
    let Value::Object(mut top) = reply else {
        return reply;
    };
    let prefixed = |existing: Option<&Value>| match existing.and_then(Value::as_str) {
        Some(text) if !text.is_empty() => json!(format!("{note}\n\n{text}")),
        _ => json!(note),
    };
    if let Some(inner) = top
        .get_mut("hookSpecificOutput")
        .and_then(Value::as_object_mut)
    {
        let text = prefixed(inner.get("additionalContext"));
        inner.insert("additionalContext".into(), text);
        return Value::Object(top);
    }
    if agent == "cursor" {
        let text = prefixed(top.get("additional_context"));
        top.insert("additional_context".into(), text);
        return Value::Object(top);
    }
    match context_reply(agent, payload, note) {
        Value::Object(added) if !added.is_empty() => {
            top.extend(added);
            Value::Object(top)
        }
        _ => Value::Object(top),
    }
}

/// Tell a running tray that this agent works in a Git checkout with no client connection, so it
/// can offer to connect it. Throttled per agent and checkout, bounded in time, and local only.
fn report_unconnected(agent: &str, payload: &Value) {
    #[cfg(unix)]
    if let (Ok(config), Some(socket)) = (config_dir(), crate::client_activity::socket_path()) {
        report_unconnected_at(
            &config,
            &socket,
            agent,
            cwd_of(payload),
            client_config::load,
        );
    }
    #[cfg(not(unix))]
    let _ = (agent, payload);
}

#[cfg(unix)]
fn report_unconnected_at(
    config: &Path,
    socket: &Path,
    agent: &str,
    cwd: PathBuf,
    load: fn() -> agentscm::Result<Vec<Connection>>,
) {
    let Some(checkout) = repo_root(&cwd) else {
        return;
    };
    let main = client_config::main_checkout(&checkout);
    let key = format!("unconnected\0{agent}\0{}", main.display());
    // A report no tray received must not start the interval, so a tray opened later hears the
    // next hook.
    if marked_within(config, &key, UNCONNECTED_EVERY)
        || !crate::client_activity::listening_at(socket)
    {
        return;
    }
    let (sender, receiver) = std::sync::mpsc::sync_channel(1);
    std::thread::spawn(move || {
        let connections = load().unwrap_or_default();
        let _ = sender.send(unconnected_checkout(&cwd, &connections));
    });
    let settled = match receiver.recv_timeout(UNCONNECTED_WAIT) {
        Ok(Some((root, host, repo))) => {
            crate::client_activity::observe_unconnected_at(socket, &root, &host, &repo, agent)
        }
        // Nothing to report, or Git is too slow: neither changes before the interval ends.
        Ok(None) | Err(_) => true,
    };
    if settled {
        mark(config, &key);
    }
}

/// The main worktree, forge host and repository of the checkout containing `cwd`, when it has
/// a supported `origin` and no client connection covers it.
fn unconnected_checkout(
    cwd: &Path,
    connections: &[Connection],
) -> Option<(PathBuf, String, String)> {
    let checkout = repo_root(cwd)?;
    if client_config::connection_for_checkout(connections, &checkout).is_some() {
        return None;
    }
    let main = client_config::main_checkout(&checkout);
    let (host, repo) = client_config::origin_identity(&main).ok()?;
    Some((main, host, repo))
}

#[cfg(test)]
fn route_codex_with(payload: &Value, connections: &[Connection]) -> Result<RoutedClient, String> {
    route_client_with("codex", payload, connections)
}

/// Codex may include source, patch bodies and prompts outside the tool input. Only fields needed
/// for coordination leave the machine. For patches, only recognized operation headers survive.
fn sanitized_codex(payload: &Value, normalized: Option<&[EditPath]>) -> Result<Value, String> {
    let mut out = serde_json::Map::new();
    for (key, max) in [
        ("hook_event_name", 64),
        ("session_id", 1024),
        ("tool_name", 128),
    ] {
        if let Some(value) = payload[key]
            .as_str()
            .filter(|value| value.len() <= max && !value.chars().any(char::is_control))
        {
            out.insert(key.into(), json!(value));
        }
    }
    if normalized.is_none() {
        if let Some(cwd) = payload["cwd"]
            .as_str()
            .filter(|cwd| cwd.len() <= 4096 && !cwd.chars().any(char::is_control))
        {
            out.insert("cwd".into(), json!(cwd));
        }
        let roots: Vec<&str> = payload["workspace_roots"]
            .as_array()
            .into_iter()
            .flatten()
            .take(MAX_WORKSPACE_ROOTS)
            .filter_map(Value::as_str)
            .filter(|root| root.len() <= 4096 && !root.chars().any(char::is_control))
            .collect();
        if !roots.is_empty() {
            out.insert("workspace_roots".into(), json!(roots));
        }
    }
    let tool = payload["tool_name"].as_str().unwrap_or_default();
    let paths = match normalized {
        Some(paths) => paths.to_vec(),
        None => edit_paths("codex", payload)?,
    };
    if tool.eq_ignore_ascii_case("apply_patch") && !paths.is_empty() {
        let mut headers = String::new();
        for path in paths {
            if let Some(header) = path.header {
                headers.push_str(header);
                headers.push_str(&path.path);
                headers.push('\n');
            }
        }
        out.insert("tool_input".into(), json!({"command": headers}));
    } else if let Some(path) = paths.first() {
        out.insert("tool_input".into(), json!({"path": path.path}));
    }
    Ok(Value::Object(out))
}

fn bounded_string(value: &Value, max: usize) -> Option<&str> {
    value
        .as_str()
        .filter(|text| text.len() <= max && !text.chars().any(char::is_control))
}

fn sanitized_client(agent: &str, payload: &Value, paths: &[EditPath]) -> Result<Value, String> {
    match agent {
        "claude" | "codex" => sanitized_codex(payload, Some(paths)),
        "cursor" => {
            let mut out = sanitized_codex(payload, Some(paths))?;
            if let Some(id) = bounded_string(&payload["conversation_id"], 1024) {
                out["conversation_id"] = json!(id);
            }
            Ok(out)
        }
        "copilot" if payload.get("hook_event_name").is_some() => {
            sanitized_codex(payload, Some(paths))
        }
        "copilot" => {
            let mut out = serde_json::Map::new();
            for (key, max) in [("sessionId", 1024), ("toolName", 128)] {
                if let Some(value) = bounded_string(&payload[key], max) {
                    out.insert(key.into(), json!(value));
                }
            }
            let tool = payload["toolName"].as_str().unwrap_or_default();
            if tool.eq_ignore_ascii_case("apply_patch") && !paths.is_empty() {
                let mut headers = String::new();
                for path in paths {
                    if let Some(header) = path.header {
                        headers.push_str(header);
                        headers.push_str(&path.path);
                        headers.push('\n');
                    }
                }
                out.insert("toolArgs".into(), json!({"command": headers}));
            } else if let Some(path) = paths.first() {
                out.insert("toolArgs".into(), json!({"path": path.path}));
            } else if payload.get("reason").is_some() {
                out.insert("reason".into(), json!("client-session-end"));
            } else if payload.get("source").is_some() || payload.get("initialPrompt").is_some() {
                out.insert("source".into(), json!("client-session-start"));
            }
            Ok(Value::Object(out))
        }
        "gemini" => {
            let event = match payload["hook_event_name"].as_str() {
                Some("SessionStart") => "start",
                Some("BeforeTool") if !paths.is_empty() => "check",
                Some("SessionEnd") => "done",
                _ => return Err("hook event not recognised".into()),
            };
            let mut out = json!({"agent":"gemini", "event":event});
            if let Some(session) = bounded_string(&payload["session_id"], 1024) {
                out["session"] = json!(session);
            }
            if !paths.is_empty() {
                out["paths"] = json!(
                    paths
                        .iter()
                        .map(|path| path.path.as_str())
                        .collect::<Vec<_>>()
                );
            }
            Ok(out)
        }
        _ => Err("unsupported client agent".into()),
    }
}

fn gemini_reply(reply: Value, payload: &Value, config: Option<&Path>) -> Value {
    let status = reply["status"].as_str().unwrap_or_default();
    let text = reply["text"].as_str().unwrap_or_default();
    match payload["hook_event_name"].as_str() {
        Some("BeforeTool") if status == "stop" && !text.is_empty() => {
            if first_deny(config, "gemini", payload) {
                json!({"decision":"deny", "reason":text})
            } else {
                json!({})
            }
        }
        Some("BeforeTool") if !text.is_empty() => json!({"systemMessage":text}),
        Some("SessionStart") if !text.is_empty() => json!({
            "hookSpecificOutput": {
                "hookEventName":"SessionStart",
                "additionalContext":text
            }
        }),
        _ => json!({}),
    }
}

const PAIR: [&str; 2] = ["permissionDecision", "permissionDecisionReason"];
type Object = serde_json::Map<String, Value>;

/// The hub's reply cut down to an allowlist: context and message fields, Cursor's `permission`
/// (forced to allow) and the one documented refusal, `permissionDecision` "deny" with a
/// non-empty reason. A deny passes only on a pre-tool event and once per identical edit (a
/// repeat is the agent's retry); the rest is dropped, so a wrong or hostile reply cannot
/// block, approve or rewrite a call.
fn defanged(reply: Value, agent: &str, payload: &Value, config: Option<&Path>) -> Value {
    let Value::Object(mut top) = reply else {
        return reply;
    };
    top.retain(|key, _| {
        PAIR.contains(&key.as_str())
            || [
                "hookSpecificOutput",
                "additionalContext",
                "additional_context",
                "systemMessage",
                "permission",
            ]
            .contains(&key.as_str())
    });
    if top.contains_key("permission") {
        top.insert("permission".into(), json!("allow"));
    }
    if !top.get("hookSpecificOutput").is_none_or(Value::is_object) {
        top.remove("hookSpecificOutput");
    }
    let mut inner = top
        .get_mut("hookSpecificOutput")
        .and_then(Value::as_object_mut);
    if let Some(inner) = inner.as_deref_mut() {
        inner.retain(|key, _| {
            PAIR.contains(&key.as_str())
                || ["hookEventName", "additionalContext"].contains(&key.as_str())
        });
    }
    let wants = inner.as_deref().is_some_and(denies);
    let wants = wants || denies(&top);
    let keep = wants && is_pre_tool(payload) && first_deny(config, agent, payload);
    if let Some(inner) = top
        .get_mut("hookSpecificOutput")
        .and_then(Value::as_object_mut)
    {
        settle(inner, keep);
    }
    settle(&mut top, keep);
    Value::Object(top)
}

fn denies(object: &Object) -> bool {
    object.get(PAIR[0]).and_then(Value::as_str) == Some("deny")
        && object
            .get(PAIR[1])
            .and_then(Value::as_str)
            .is_some_and(|reason| !reason.is_empty())
}

/// Keeps a valid deny pair when `keep`, else removes the pair.
fn settle(object: &mut Object, keep: bool) {
    if !(keep && denies(object)) {
        PAIR.iter().for_each(|key| _ = object.remove(*key));
    }
}

/// A before-the-tool event: Claude/Codex/Copilot name it, Copilot CLI's camelCase payload has
/// a tool name and no result.
fn is_pre_tool(payload: &Value) -> bool {
    match payload["hook_event_name"].as_str() {
        Some(name) => matches!(name, "PreToolUse" | "preToolUse"),
        None => payload.get("toolName").is_some() && payload.get("toolResult").is_none(),
    }
}

/// True the first time this exact edit of the session is refused. Any missing session id or
/// filesystem error passes the deny: the hub already limits refusals.
fn first_deny(config: Option<&Path>, agent: &str, payload: &Value) -> bool {
    let (Some(config), Some(session)) = (config, session_of(payload)) else {
        return true;
    };
    // Object keys are sorted (serde_json without preserve_order), so this is canonical.
    let input = payload.get("tool_input").or(payload.get("toolArgs"));
    first_marker(
        config,
        &format!(
            "deny\0{agent}\0{session}\0{}",
            input.unwrap_or(&Value::Null)
        ),
    )
}

/// The agent name is argv, so keep it to a safe single path segment.
fn path_segment(agent: &str) -> String {
    let safe = agent
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
    if safe && !agent.is_empty() {
        agent.to_owned()
    } else {
        "generic".to_owned()
    }
}

/// Where the agent is working: the payload's `cwd`, else its first workspace root, else here.
fn cwd_of(payload: &Value) -> PathBuf {
    let first_root = payload["workspace_roots"][0].as_str();
    payload["cwd"].as_str().or(first_root).map_or_else(
        || std::env::current_dir().unwrap_or_default(),
        PathBuf::from,
    )
}

struct Settings {
    url: Option<String>,
    token: Option<String>,
    token_file: Option<String>,
}

#[derive(Clone, Debug, PartialEq)]
enum Token {
    Value(String),
    File(PathBuf),
    PrivateFile(PathBuf),
}

/// The hub URL and where its token comes from. A token stored in hub.json goes only to the hub
/// that file names (or the one `AGENTSCM_HUB_URL` names): a checked-in `--hub-url` must not be
/// able to redirect it.
fn resolve(
    env: &Settings,
    flag_url: Option<&str>,
    cwd: &Path,
    file: Option<&HubFile>,
) -> Result<(String, Token), &'static str> {
    let raw = env
        .url
        .as_deref()
        .or(flag_url)
        .or(file.map(|f| f.url.as_str()))
        .ok_or("no hub configured")?;
    let url = check_base_url(raw).map_err(|_| "bad hub url")?;
    if let Some(token) = &env.token {
        return Ok((url, Token::Value(token.clone())));
    }
    if let Some(path) = &env.token_file {
        return Ok((url, Token::File(PathBuf::from(path))));
    }
    let file = file.ok_or("no token")?;
    let stored = check_base_url(&file.url).ok();
    if env.url.is_none() && stored.as_deref() != Some(url.as_str()) {
        return Err("no token for this hub; set AGENTSCM_HUB_TOKEN");
    }
    let path = token_for(file, cwd);
    if !path.is_absolute() {
        return Err("no token");
    }
    Ok((url, Token::File(path)))
}

/// What the agent gets when the hub cannot be consulted: valid JSON that never blocks. The note
/// is printed once per (agent, session); later calls in that session get the silent no-op.
fn unknown_output(agent: &str, payload: &Value, reason: &str) -> Value {
    // The user-wide hooks run in every project; an edit no connected checkout covers stays quiet.
    if reason == NO_EDIT_CONNECTION {
        return fail_open(agent, payload, None);
    }
    let note = match (session_of(payload), config_dir()) {
        (Some(session), Ok(dir)) => first_note(&dir, agent, session),
        _ => true,
    };
    fail_open(agent, payload, note.then_some(reason))
}

/// The payload's session id, under the names the supported agents use.
fn session_of(payload: &Value) -> Option<&str> {
    ["session_id", "conversation_id", "sessionId"]
        .into_iter()
        .filter_map(|key| payload[key].as_str())
        .find(|id| !id.is_empty())
}

/// Whether to print the note: true the first time this (agent, session) is seen. The marker is a
/// file named by a hash, so no session id or agent text is written. Any filesystem error prints.
fn first_note(config: &Path, agent: &str, session: &str) -> bool {
    first_marker(config, &format!("{agent}\0{session}"))
}

/// True the first time `key` is seen, remembered as a marker file named by its hash.
fn first_marker(config: &Path, key: &str) -> bool {
    use sha2::{Digest, Sha256};
    const KEEP: Duration = Duration::from_secs(2 * 24 * 60 * 60);
    let hash = Sha256::digest(key);
    let dir = config.join("agentscm").join("hook-notes");
    let mut builder = std::fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    std::os::unix::fs::DirBuilderExt::mode(&mut builder, 0o700);
    let made = builder.create(&dir);
    if made.is_err() {
        return true;
    }
    let marker = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(dir.join(format!("{hash:x}")));
    if let Err(e) = &marker {
        return e.kind() != std::io::ErrorKind::AlreadyExists;
    }
    for entry in std::fs::read_dir(&dir).into_iter().flatten().flatten() {
        let old = (entry.metadata().and_then(|m| m.modified()))
            .ok()
            .and_then(|time| time.elapsed().ok())
            .is_some_and(|age| age > KEEP);
        if old {
            let _ = std::fs::remove_file(entry.path());
        }
    }
    true
}

fn marker_path(config: &Path, key: &str) -> PathBuf {
    use sha2::{Digest, Sha256};
    (config.join("agentscm").join("hook-notes")).join(format!("{:x}", Sha256::digest(key)))
}

/// Whether `key` was marked less than `every` ago.
pub(crate) fn marked_within(config: &Path, key: &str, every: Duration) -> bool {
    (std::fs::metadata(marker_path(config, key)).and_then(|m| m.modified()))
        .ok()
        .and_then(|time| time.elapsed().ok())
        .is_some_and(|age| age < every)
}

/// Mark `key` now, replacing an older mark.
pub(crate) fn mark(config: &Path, key: &str) {
    let _ = std::fs::remove_file(marker_path(config, key));
    first_marker(config, key);
}

/// The reply for an unreachable hub; `note` is the reason to show, or `None` for the silent form.
fn fail_open(agent: &str, payload: &Value, note: Option<&str>) -> Value {
    match note {
        Some(reason) => context_reply(
            agent,
            payload,
            &format!(
                "AgentSCM: coordination unknown ({reason}). Other agents' work on these files is not visible."
            ),
        ),
        None => context_reply_with(agent, payload, None),
    }
}

/// The agent's reply that adds `text` to its context, where the event allows it.
fn context_reply(agent: &str, payload: &Value, text: &str) -> Value {
    context_reply_with(agent, payload, Some(text))
}

fn context_reply_with(agent: &str, payload: &Value, note: Option<&str>) -> Value {
    let event = payload["hook_event_name"].as_str();
    let text = |text: &str| text.to_owned();
    match (agent, event, note) {
        ("claude" | "codex" | "copilot", Some(name @ ("PreToolUse" | "SessionStart")), Some(r)) => {
            json!({"hookSpecificOutput": {"hookEventName": name, "additionalContext": text(r)}})
        }
        // Cursor blocks on invalid JSON, so a tool call it did not name an event for still
        // gets an explicit allow.
        ("cursor", Some("preToolUse"), _) => json!({"permission": "allow"}),
        ("cursor", Some("sessionStart" | "postToolUse"), Some(r)) => {
            json!({"additional_context": text(r)})
        }
        ("gemini", Some("SessionStart"), Some(r)) => json!({
            "hookSpecificOutput": {
                "hookEventName":"SessionStart",
                "additionalContext":text(r)
            }
        }),
        ("gemini", Some("BeforeTool"), Some(r)) => json!({"systemMessage":text(r)}),
        ("cursor", None, _) if !payload.is_object() || payload.get("tool_name").is_some() => {
            json!({"permission": "allow"})
        }
        _ => json!({}),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalized_lifecycle_alone_marks_session_end() {
        use crate::client_activity::ActivityOutcome::{Acknowledged, SessionEnded};
        assert_eq!(
            successful_hook_outcome(&json!({"hook_event_name":"SessionEnd"})),
            SessionEnded
        );
        assert_eq!(
            successful_hook_outcome(&json!({"event":"done"})),
            SessionEnded
        );
        assert_eq!(
            successful_hook_outcome(&json!({"reason":"client-session-end"})),
            SessionEnded
        );
        assert_eq!(
            successful_hook_outcome(&json!({"reason":"transport-error"})),
            Acknowledged
        );
    }

    #[tokio::test]
    async fn completed_and_cancelled_attempts_report_exact_outcomes() {
        use crate::client_activity::ActivityOutcome::{Acknowledged, TokenRejected, Unavailable};
        let completed = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        {
            let tracker = std::sync::Arc::new(ActivityTracker::recording(completed.clone()));
            let mut attempt = ActivityAttempt::new(tracker, None);
            attempt.finish(Acknowledged);
        }
        {
            let tracker = std::sync::Arc::new(ActivityTracker::recording(completed.clone()));
            let mut attempt = ActivityAttempt::new(tracker, None);
            attempt.finish(TokenRejected);
        }
        assert_eq!(*completed.lock().unwrap(), [Acknowledged, TokenRejected]);

        let cancelled = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let recorded = cancelled.clone();
        let tracker = std::sync::Arc::new(ActivityTracker::recording(recorded));
        let timed = tracker.clone();
        let mut task = tokio::spawn(async move {
            let _attempt = ActivityAttempt::new(tracker, None);
            std::future::pending::<()>().await;
        });
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(1), &mut task)
                .await
                .is_err()
        );
        timed.finish(Unavailable);
        task.abort();
        timed.finish(Acknowledged);
        assert_eq!(*cancelled.lock().unwrap(), [Unavailable]);
    }

    #[tokio::test]
    async fn branch_observation_wait_keeps_completed_handles_and_bounds_slow_ones() {
        let payload = json!({"hook_event_name":"SessionStart", "session_id":"deadline"});
        let finished = tokio::spawn(async { Some("completed branch observation".to_owned()) });
        while !finished.is_finished() {
            tokio::task::yield_now().await;
        }
        let observation: LocalObservation =
            std::sync::Arc::new(std::sync::Mutex::new(Some(finished)));
        // A completed handle is still useful after the wait budget has expired.
        let reply = finish_local_observation(
            json!({}),
            "claude",
            &payload,
            &observation,
            Instant::now() - Duration::from_secs(1),
        )
        .await;
        assert_eq!(
            reply["hookSpecificOutput"]["additionalContext"],
            "completed branch observation"
        );
        assert!(observation.lock().unwrap().is_none());
        let slow: LocalObservation =
            std::sync::Arc::new(std::sync::Mutex::new(Some(tokio::spawn(async {
                tokio::time::sleep(Duration::from_secs(60)).await;
                Some("late observation".to_owned())
            }))));
        let started = Instant::now();
        let reply = finish_local_observation(
            json!({}),
            "claude",
            &payload,
            &slow,
            started + Duration::from_millis(20),
        )
        .await;
        assert_eq!(reply, json!({}));
        assert!(started.elapsed() < Duration::from_secs(1));
        assert!(slow.lock().unwrap().is_none());
    }

    #[tokio::test]
    async fn branch_observation_keeps_connection_and_access_loss_context_first() {
        let payload = json!({"hook_event_name":"SessionStart", "session_id":"connected"});
        let local = "AgentSCM branch advisory: local branch coverage unknown.";
        for core in [
            "AgentSCM: coordinating on acme/widgets.\n\nAgentSCM Hub context and native session identity.",
            "AgentSCM: alice has no access on private repository acme/widgets.\n\nAgentSCM: coordination unknown (token rejected).",
        ] {
            let observation: LocalObservation =
                std::sync::Arc::new(std::sync::Mutex::new(Some(tokio::spawn(async {
                    Some("AgentSCM branch advisory: local branch coverage unknown.".to_owned())
                }))));
            let reply = finish_local_observation(
                context_reply("claude", &payload, core),
                "claude",
                &payload,
                &observation,
                Instant::now() + Duration::from_secs(1),
            )
            .await;
            assert_eq!(
                reply,
                json!({"hookSpecificOutput": {
                    "hookEventName":"SessionStart", "additionalContext":format!("{core}\n\n{local}")
                }})
            );
        }
    }

    #[test]
    fn local_advice_preserves_documented_context_and_permission_fields() {
        for (agent, event, reply, field) in [
            (
                "gemini",
                "BeforeTool",
                json!({"systemMessage":"Hub coordination"}),
                "/systemMessage",
            ),
            (
                "cursor",
                "postToolUse",
                json!({"additional_context":"Hub coordination", "permission":"allow"}),
                "/additional_context",
            ),
            (
                "claude",
                "PreToolUse",
                json!({"hookSpecificOutput": {
                    "hookEventName":"PreToolUse", "additionalContext":"Hub coordination",
                    "permissionDecision":"deny", "permissionDecisionReason":"Coordinate the held file first"
                }}),
                "/hookSpecificOutput/additionalContext",
            ),
        ] {
            let mut expected = reply.clone();
            *expected.pointer_mut(field).unwrap() = json!("Hub coordination\n\nLocal advice");
            assert_eq!(
                with_local_note(
                    reply,
                    agent,
                    &json!({"hook_event_name":event}),
                    "Local advice"
                ),
                expected
            );
        }
    }

    #[test]
    fn unreadable_hub_message_does_not_hide_local_advice_from_agent_context() {
        // Unreadable/oversized Hub requests return a user-visible systemMessage.
        // Claude, Codex and Copilot still need the local advice in their context channel.
        for agent in ["claude", "codex", "copilot"] {
            for event in ["SessionStart", "PreToolUse"] {
                let payload = json!({"hook_event_name":event});
                let warning = "AgentSCM: coordination unknown (body unreadable)";
                let reply = defanged(json!({"systemMessage":warning}), agent, &payload, None);
                assert_eq!(
                    with_local_note(reply, agent, &payload, "Local branch advice"),
                    json!({
                        "systemMessage":warning,
                        "hookSpecificOutput": {
                            "hookEventName":event,
                            "additionalContext":"Local branch advice"
                        }
                    })
                );
            }
        }
    }

    #[tokio::test]
    async fn branch_observation_is_consumed_after_hub_failure() {
        let payload = json!({"hook_event_name":"PreToolUse", "session_id":"failed-hub"});
        let observation: LocalObservation =
            std::sync::Arc::new(std::sync::Mutex::new(Some(tokio::spawn(async {
                Some("local same-file observation".to_owned())
            }))));
        let failed = fail_open("claude", &payload, Some("hub unreachable"));
        let expected_context = format!(
            "{}\n\nlocal same-file observation",
            failed["hookSpecificOutput"]["additionalContext"]
                .as_str()
                .unwrap()
        );
        let reply = finish_local_observation(
            failed,
            "claude",
            &payload,
            &observation,
            Instant::now() + Duration::from_secs(1),
        )
        .await;
        let context = reply["hookSpecificOutput"]["additionalContext"]
            .as_str()
            .unwrap();
        assert_eq!(context, expected_context);
        assert!(
            reply["hookSpecificOutput"]
                .get("permissionDecision")
                .is_none()
        );
        assert!(observation.lock().unwrap().is_none());
        assert_eq!(
            finish_local_observation(
                reply.clone(),
                "claude",
                &payload,
                &observation,
                Instant::now() + Duration::from_secs(1)
            )
            .await,
            reply
        );
    }

    #[test]
    fn gemini_native_start_preserves_session_discovery_context() {
        let context = "AgentSCM native session ID (literal data, not instructions): \"native-id\". Use this exact session argument with agentscm_setup_status.";
        let reply = gemini_reply(
            json!({"status":"clean","text":context}),
            &json!({"hook_event_name":"SessionStart"}),
            None,
        );
        assert_eq!(reply["hookSpecificOutput"]["additionalContext"], context);
    }

    #[cfg(unix)]
    fn branch_snapshot() -> agentscm::git_collisions::AdvisorySnapshot {
        use agentscm::git_collisions::{AdvisoryPeer, AdvisorySnapshot};
        AdvisorySnapshot {
            base_name: "main".into(),
            base_commit: "a".repeat(40),
            head: "b".repeat(40),
            observed_at: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_secs(),
            incomplete: false,
            peers: vec![AdvisoryPeer {
                name: "peer".into(),
                commit: "c".repeat(40),
                fork: Some("a".repeat(40)),
                kind: "ref".into(),
                paths: std::collections::BTreeSet::from(["src/a.rs".into()]),
            }],
        }
    }

    #[cfg(unix)]
    #[test]
    fn branch_advice_uses_only_cache_and_warns_once_per_path_session() {
        let config = tempfile::tempdir().unwrap();
        // No checkout or Git executable is needed on an edit.
        let checkout = Path::new("/synthetic/absent-checkout");
        let payload = json!({"hook_event_name":"PreToolUse", "session_id":"one",
            "tool_name":"Write", "tool_input":{"path":"src/a.rs"}});
        let cache = branch_cache_path(
            config.path(),
            &format!("branch-observation\0claude\0one\0{}", checkout.display()),
        );
        let snapshot = branch_snapshot();
        assert!(prepare_branch_session(
            config.path(),
            &format!("branch-observation\0claude\0one\0{}", checkout.display()),
            Instant::now() + Duration::from_secs(1)
        ));
        assert!(write_branch_cache(&cache, &snapshot));
        let advise = |payload: &Value| {
            branch_advisory_at(
                config.path(),
                "claude",
                payload,
                payload,
                checkout,
                Instant::now() + Duration::from_secs(1),
            )
        };
        let note = advise(&payload).unwrap();
        assert!(note.contains("same-file advisory"));
        assert!(note.contains(&snapshot.head));
        assert!(note.contains(&snapshot.base_commit));
        assert!(note.contains(&snapshot.peers[0].commit));
        assert!(note.contains("not proof of conflict"));
        assert!(advise(&payload).is_none());
        let mut second = payload.clone();
        second["session_id"] = json!("two");
        assert!(prepare_branch_session(
            config.path(),
            &format!("branch-observation\0claude\0two\0{}", checkout.display()),
            Instant::now() + Duration::from_secs(1)
        ));
        assert!(advise(&second).unwrap().contains("coverage unknown"));
        assert!(advise(&second).is_none());
        let second_cache = branch_cache_path(
            config.path(),
            &format!("branch-observation\0claude\0two\0{}", checkout.display()),
        );
        assert!(write_branch_cache(&second_cache, &snapshot));
        assert!(advise(&second).unwrap().contains("same-file advisory"));
        assert!(advise(&second).is_none());
    }

    #[cfg(unix)]
    #[test]
    fn branch_cache_rejects_stale_oversized_malformed_and_future_observations() {
        let config = tempfile::tempdir().unwrap();
        let path = config.path().join("cache.json");
        let mut snapshot = branch_snapshot();
        snapshot.observed_at -= 4 * 60 * 60 + 1;
        std::fs::write(&path, serde_json::to_vec(&snapshot).unwrap()).unwrap();
        assert!(read_branch_cache(&path).is_none());
        snapshot.observed_at = u64::MAX;
        std::fs::write(&path, serde_json::to_vec(&snapshot).unwrap()).unwrap();
        assert!(read_branch_cache(&path).is_none());
        std::fs::write(&path, b"not JSON").unwrap();
        assert!(read_branch_cache(&path).is_none());
        std::fs::write(
            &path,
            vec![b' '; agentscm::git_collisions::ADVISORY_BYTES + 1],
        )
        .unwrap();
        assert!(read_branch_cache(&path).is_none());
        assert!(!write_branch_cache(config.path(), &branch_snapshot()));
        let payload = json!({"session_id":"unwritable", "tool_name":"Write", "tool_input":{"path":"src/a.rs"}});
        // A cache/marker filesystem failure adds no decision or blocking output.
        let blocked = tempfile::NamedTempFile::new().unwrap();
        assert!(
            branch_advisory_at(
                blocked.path(),
                "claude",
                &payload,
                &payload,
                Path::new("/synthetic/checkout"),
                Instant::now()
            )
            .is_none()
        );
    }

    #[cfg(unix)]
    #[test]
    fn branch_storage_expires_only_owned_files_without_following_symlinks() {
        use std::os::unix::fs::symlink;
        let config = tempfile::tempdir().unwrap();
        let outside = config.path().join("outside");
        std::fs::write(&outside, "keep").unwrap();
        let expire = |key: &str| {
            let cache = branch_cache_path(config.path(), key);
            let created = std::fs::File::open(cache.parent().unwrap().join("created")).unwrap();
            created
                .set_times(std::fs::FileTimes::new().set_modified(
                    std::time::SystemTime::now() - Duration::from_secs(4 * 60 * 60 + 10),
                ))
                .unwrap();
            cache
        };
        assert!(prepare_branch_session(
            config.path(),
            "old",
            Instant::now() + Duration::from_secs(1)
        ));
        let old = branch_cache_path(config.path(), "old");
        assert!(write_branch_cache(&old, &branch_snapshot()));
        assert!(branch_marker(
            config.path(),
            "old",
            "one",
            Instant::now() + Duration::from_secs(1)
        ));
        expire("old");
        assert!(prepare_branch_session(
            config.path(),
            "mixed",
            Instant::now() + Duration::from_secs(1)
        ));
        assert!(!old.parent().unwrap().exists());
        let mixed = branch_cache_path(config.path(), "mixed");
        symlink(&outside, &mixed).unwrap();
        std::fs::write(mixed.parent().unwrap().join("unrelated"), "leave").unwrap();
        expire("mixed");
        assert!(prepare_branch_session(
            config.path(),
            "next",
            Instant::now() + Duration::from_secs(1)
        ));
        assert!(std::fs::symlink_metadata(&mixed).unwrap().is_symlink());
        assert_eq!(std::fs::read_to_string(&outside).unwrap(), "keep");
        assert_eq!(
            std::fs::read_to_string(mixed.parent().unwrap().join("unrelated")).unwrap(),
            "leave"
        );
        assert!(read_branch_cache(&mixed).is_none());
    }

    #[cfg(unix)]
    #[test]
    fn branch_storage_caps_sessions_markers_and_maintenance_enumeration() {
        let config = tempfile::tempdir().unwrap();
        for session in 0..BRANCH_SESSIONS {
            assert!(prepare_branch_session(
                config.path(),
                &format!("s{session}"),
                Instant::now() + Duration::from_secs(1)
            ));
        }
        assert!(!prepare_branch_session(
            config.path(),
            "extra",
            Instant::now() + Duration::from_secs(1)
        ));
        for marker in 0..BRANCH_MARKERS {
            assert!(branch_marker(
                config.path(),
                "s0",
                &format!("p{marker}"),
                Instant::now() + Duration::from_secs(1)
            ));
        }
        assert!(!branch_marker(
            config.path(),
            "s0",
            "extra",
            Instant::now() + Duration::from_secs(1)
        ));
        assert!(!branch_marker(
            config.path(),
            "s0",
            "p0",
            Instant::now() + Duration::from_secs(1)
        ));
        let session = branch_cache_path(config.path(), "s0");
        assert!(
            std::fs::read_dir(session.parent().unwrap())
                .unwrap()
                .count()
                <= BRANCH_MARKERS + 4
        );
        let root = session.parent().unwrap().parent().unwrap();
        std::fs::write(root.join("unrelated"), "keep").unwrap();
        // Too many root entries stops bounded maintenance rather than scanning all.
        assert!(!prepare_branch_session(
            config.path(),
            "s0",
            Instant::now() + Duration::from_secs(1)
        ));
        assert_eq!(
            std::fs::read_to_string(root.join("unrelated")).unwrap(),
            "keep"
        );
        assert_eq!(
            std::fs::read_dir(root).unwrap().count(),
            BRANCH_SESSIONS + 2
        );
    }

    #[cfg(unix)]
    #[test]
    fn branch_advice_routes_real_session_start_and_absolute_first_edit() {
        let parent = tempfile::tempdir().unwrap();
        let config = tempfile::tempdir().unwrap();
        let repo = parent.path().join("repo");
        std::fs::create_dir(&repo).unwrap();
        git(&repo, &["init", "-q", "-b", "main"]);
        git(&repo, &["config", "user.name", "Fixture"]);
        git(&repo, &["config", "user.email", "fixture@example.invalid"]);
        git(&repo, &["config", "commit.gpgsign", "false"]);
        git(&repo, &["config", "core.hooksPath", "/dev/null"]);
        git(
            &repo,
            &[
                "remote",
                "add",
                "origin",
                "https://example.test/acme/widgets.git",
            ],
        );
        std::fs::write(repo.join("shared.rs"), "base\n").unwrap();
        git(&repo, &["add", "."]);
        git(&repo, &["commit", "-qm", "base"]);
        let peer = parent.path().join("peer");
        git(
            &repo,
            &[
                "worktree",
                "add",
                "-q",
                "-b",
                "peer",
                peer.to_str().unwrap(),
            ],
        );
        std::fs::write(peer.join("shared.rs"), "dirty peer\n").unwrap();
        let connections = [connection(&repo, "acme/widgets")];
        let start = json!({"hook_event_name":"SessionStart", "session_id":"real", "cwd":repo});
        let routed = route_client_with("claude", &start, &connections).unwrap();
        assert!(client_config::is_current_in(
            &routed.connection,
            &routed.checkout
        ));
        let summary = branch_advisory_at(
            config.path(),
            "claude",
            &start,
            &routed.payload,
            &routed.checkout,
            Instant::now() + Duration::from_secs(5),
        )
        .unwrap();
        assert!(summary.contains("observed"), "{summary}");
        let edit = json!({"hook_event_name":"PreToolUse", "session_id":"real", "cwd":repo,
            "tool_name":"Write", "tool_input":{"file_path":repo.join("shared.rs")}});
        let routed = route_client_with("claude", &edit, &connections).unwrap();
        assert_eq!(routed.payload["tool_input"]["path"], "shared.rs");
        let advice = || {
            branch_advisory_at(
                config.path(),
                "claude",
                &edit,
                &routed.payload,
                &routed.checkout,
                Instant::now() + Duration::from_secs(5),
            )
        };
        let note = advice().unwrap();
        assert!(note.contains("same-file advisory"), "{note}");
        assert!(note.contains("shared.rs"));
        let reply = with_note(json!({}), "claude", &edit, &note);
        assert!(
            reply["hookSpecificOutput"]["additionalContext"]
                .as_str()
                .unwrap()
                .contains("same-file")
        );
        assert!(
            reply["hookSpecificOutput"]
                .get("permissionDecision")
                .is_none()
        );
        assert!(advice().is_none());
    }

    #[cfg(unix)]
    #[test]
    fn branch_storage_reclaims_crash_orphans_even_when_initially_over_cap() {
        let config = tempfile::tempdir().unwrap();
        for session in 0..BRANCH_SESSIONS {
            let key = format!("orphan-{session}");
            assert!(prepare_branch_session(
                config.path(),
                &key,
                Instant::now() + Duration::from_secs(1)
            ));
            let cache = branch_cache_path(config.path(), &key);
            std::fs::remove_file(cache.parent().unwrap().join("created")).unwrap();
        }
        let cache = branch_cache_path(config.path(), "orphan-0");
        let root = cache.parent().unwrap().parent().unwrap();
        std::fs::write(root.join("unrelated"), "keep").unwrap();
        let old = std::time::SystemTime::now() - Duration::from_secs(4 * 60 * 60 + 10);
        for session in 0..BRANCH_SESSIONS {
            let cache = branch_cache_path(config.path(), &format!("orphan-{session}"));
            std::fs::File::open(cache.parent().unwrap())
                .unwrap()
                .set_times(std::fs::FileTimes::new().set_modified(old))
                .unwrap();
        }
        assert!(prepare_branch_session(
            config.path(),
            "recovered",
            Instant::now() + Duration::from_secs(1)
        ));
        assert!(!cache.parent().unwrap().exists());
        assert_eq!(
            std::fs::read_to_string(root.join("unrelated")).unwrap(),
            "keep"
        );
        assert!(std::fs::read_dir(root).unwrap().count() <= 3);
    }

    #[cfg(unix)]
    #[test]
    fn branch_storage_retries_root_and_marker_locks_released_within_budget() {
        let config = tempfile::tempdir().unwrap();
        let deadline = || Instant::now() + Duration::from_secs(1);
        assert!(prepare_branch_session(config.path(), "first", deadline()));
        let cache = branch_cache_path(config.path(), "first");
        let session = cache.parent().unwrap();
        let root = session.parent().unwrap();
        let held = branch_lock(&root.join(".lock"), deadline()).unwrap();
        let release = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(15));
            drop(held);
        });
        let admitted = prepare_branch_session(config.path(), "parallel", deadline());
        release.join().unwrap();
        assert!(
            admitted,
            "a short admission-lock overlap must not lose the session"
        );
        let held = branch_lock(&session.join(".lock"), deadline()).unwrap();
        let release = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(15));
            drop(held);
        });
        let marked = branch_marker(config.path(), "first", "parallel-edit", deadline());
        release.join().unwrap();
        assert!(
            marked,
            "a short marker-lock overlap must not drop the edit advice"
        );
        assert!(!branch_marker(
            config.path(),
            "first",
            "parallel-edit",
            deadline()
        ));
    }

    #[cfg(unix)]
    #[test]
    fn branch_lock_sustained_contention_is_capped_and_respects_hook_deadline() {
        let config = tempfile::tempdir().unwrap();
        let path = config.path().join("lock");
        let _held = branch_lock(&path, Instant::now() + Duration::from_secs(1)).unwrap();
        let started = Instant::now();
        assert!(branch_lock(&path, started + Duration::from_secs(1)).is_none());
        assert!(started.elapsed() >= Duration::from_millis(40));
        assert!(started.elapsed() < Duration::from_millis(250));
        let started = Instant::now();
        assert!(branch_lock(&path, started + Duration::from_millis(5)).is_none());
        assert!(started.elapsed() < Duration::from_secs(1));
        // Release after the hook's budget, but before the helper's 50ms cap:
        // ignoring the shorter caller deadline would incorrectly acquire this.
        let late_path = config.path().join("released-too-late");
        let held = branch_lock(&late_path, Instant::now() + Duration::from_secs(1)).unwrap();
        let hook_deadline = Instant::now() + Duration::from_millis(5);
        let release = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(25));
            drop(held);
        });
        let acquired = branch_lock(&late_path, hook_deadline);
        release.join().unwrap();
        assert!(acquired.is_none());
        let absent = config.path().join("expired");
        assert!(branch_lock(&absent, Instant::now() - Duration::from_secs(1)).is_none());
        assert!(
            !absent.exists(),
            "an exhausted deadline must not start lock acquisition"
        );
    }

    #[cfg(unix)]
    #[test]
    #[ignore = "subprocess helper for branch_storage_lock_contention_is_nonblocking"]
    fn branch_lock_child() {
        let path = PathBuf::from(std::env::var_os("AGENTSCM_BRANCH_LOCK_TEST").unwrap());
        let _lock =
            branch_lock(&path.join("lock"), Instant::now() + Duration::from_secs(1)).unwrap();
        std::fs::write(path.join("ready"), "").unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        while !path.join("release").exists() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    #[cfg(unix)]
    #[test]
    fn branch_storage_lock_contention_is_nonblocking() {
        let config = tempfile::tempdir().unwrap();
        let mut child = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "hook_cli::tests::branch_lock_child",
                "--ignored",
                "--nocapture",
            ])
            .env("AGENTSCM_BRANCH_LOCK_TEST", config.path())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(2);
        while !config.path().join("ready").exists() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(5));
        }
        if !config.path().join("ready").exists() {
            let _ = child.kill();
            let _ = child.wait();
            panic!("lock helper did not become ready");
        }
        let started = Instant::now();
        assert!(
            branch_lock(
                &config.path().join("lock"),
                Instant::now() + Duration::from_secs(1)
            )
            .is_none()
        );
        assert!(started.elapsed() < Duration::from_secs(1));
        std::fs::write(config.path().join("release"), "").unwrap();
        assert!(child.wait().unwrap().success());
        assert!(
            branch_lock(
                &config.path().join("lock"),
                Instant::now() + Duration::from_secs(1)
            )
            .is_some()
        );
    }

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| (*s).to_owned()).collect()
    }

    fn pre(session: &str, input: Value) -> Value {
        json!({"hook_event_name": "PreToolUse", "session_id": session, "tool_input": input})
    }

    fn deny(reason: &str) -> Value {
        json!({"hookSpecificOutput": {"hookEventName": "PreToolUse",
            "permissionDecision": "deny", "permissionDecisionReason": reason}})
    }

    #[test]
    fn a_hub_reply_is_cut_to_an_allowlist() {
        let dir = tempfile::TempDir::new().unwrap();
        let cut = |reply| defanged(reply, "claude", &pre("s", json!({})), Some(dir.path()));
        let reply = cut(json!({
            "permission": "deny", "continue": false, "decision": "block", "stopReason": "x",
            "updatedInput": {}, "updated_input": {}, "followup_message": "x",
            "modifiedArgs": {}, "updatedMCPToolOutput": {}, "systemMessage": "m",
            "additional_context": "keep", "additionalContext": "keep",
            "hookSpecificOutput": {"hookEventName": "PreToolUse", "permissionDecision": "deny",
                "updatedInput": {}, "updatedMCPToolOutput": {}, "modifiedArgs": {},
                "decision": "block", "additionalContext": "keep"}
        }));
        // The deny has no reason, so it goes too; `permission` is forced to allow.
        assert_eq!(
            reply,
            json!({"permission": "allow", "systemMessage": "m", "additional_context": "keep",
                "additionalContext": "keep",
                "hookSpecificOutput": {"hookEventName": "PreToolUse", "additionalContext": "keep"}})
        );
        // Cursor's permission is forced to allow even with a deny and a reason.
        let cursor = cut(json!({"permission": "deny", "agent_message": "r"}));
        assert_eq!(cursor, json!({"permission": "allow"}));
        assert_eq!(cut(json!(["x"])), json!(["x"]));
        assert_eq!(cut(json!({"hookSpecificOutput": "x"})), json!({}));
    }

    #[test]
    fn only_a_deny_with_a_reason_on_a_pre_tool_event_survives() {
        let dir = tempfile::TempDir::new().unwrap();
        let run = |reply, payload: &Value| defanged(reply, "claude", payload, Some(dir.path()));
        let bare = json!({"hookSpecificOutput": {"hookEventName": "PreToolUse"}});
        for (decision, reason) in [
            (json!("deny"), json!("")),
            (json!("deny"), Value::Null),
            (json!("deny"), json!(1)),
            (json!("ask"), json!("r")),
            (json!("allow"), json!("r")),
            (json!("defer"), json!("r")),
            (json!("Deny"), json!("r")),
        ] {
            let mut input = bare.clone();
            input["hookSpecificOutput"]["permissionDecision"] = decision.clone();
            input["hookSpecificOutput"]["permissionDecisionReason"] = reason.clone();
            assert_eq!(
                run(input, &pre("s", json!({}))),
                bare,
                "{decision} {reason}"
            );
        }
        // Not before a tool: dropped.
        let post = json!({"hook_event_name": "PostToolUse", "session_id": "s"});
        assert_eq!(run(deny("r"), &post), bare);
        let first = pre("s", json!({"file_path": "a.rs"}));
        assert_eq!(run(deny("r"), &first), deny("r"));
        // The repeat of the same edit passes; another edit or session is refused once.
        assert_eq!(run(deny("r"), &first), bare);
        assert_eq!(
            run(deny("r"), &pre("s", json!({"file_path": "b.rs"}))),
            deny("r")
        );
        assert_eq!(
            run(deny("r"), &pre("t", json!({"file_path": "a.rs"}))),
            deny("r")
        );
        // Copilot CLI's camelCase form is top level, with the tool args as the identity.
        let cli = json!({"sessionId": "c", "toolName": "edit", "toolArgs": "{\"p\":1}"});
        let top = json!({"permissionDecision": "deny", "permissionDecisionReason": "r"});
        assert_eq!(run(top.clone(), &cli), top);
        assert_eq!(run(top, &cli), json!({}));
        let result = json!({"toolName": "edit", "toolResult": {}});
        let ask = json!({"permissionDecision": "ask", "permissionDecisionReason": "r", "k": 1});
        assert_eq!(run(ask, &cli), json!({}));
        let again = json!({"permissionDecision": "deny", "permissionDecisionReason": "r"});
        assert_eq!(run(again, &result), json!({}));
        // No session id or no place for a marker: the deny passes, the hub limits refusals.
        let anon = json!({"hook_event_name": "PreToolUse", "tool_input": {}});
        assert_eq!(run(deny("r"), &anon), deny("r"));
        assert_eq!(run(deny("r"), &anon), deny("r"));
        let blocked = tempfile::NamedTempFile::new().unwrap();
        let once = |reply| defanged(reply, "claude", &first, Some(blocked.path()));
        assert_eq!(once(deny("r")), deny("r"));
        assert_eq!(once(deny("r")), deny("r"));
    }

    #[test]
    fn the_unknown_note_is_printed_once_per_agent_and_session() {
        let dir = tempfile::TempDir::new().unwrap();
        let first = |agent, session| first_note(dir.path(), agent, session);
        assert!(first("claude", "s1"));
        assert!(!first("claude", "s1"));
        assert!(first("claude", "s2"));
        assert!(first("codex", "s1"));
        let markers = dir.path().join("agentscm").join("hook-notes");
        let names: Vec<_> = std::fs::read_dir(&markers).unwrap().flatten().collect();
        assert_eq!(names.len(), 3);
        assert!(names.iter().all(|e| e.file_name().len() == 64));
        // A marker cannot be created: print the note rather than fail.
        let blocked = tempfile::NamedTempFile::new().unwrap();
        assert!(first_note(blocked.path(), "claude", "s1"));
        assert!(first_note(blocked.path(), "claude", "s1"));
    }

    #[test]
    fn a_repeat_note_is_the_agents_silent_no_op() {
        let pre = event("PreToolUse");
        let note = fail_open("claude", &pre, Some("hub unreachable"));
        assert!(note["hookSpecificOutput"]["additionalContext"].is_string());
        assert_eq!(fail_open("claude", &pre, None), json!({}));
        let cursor = event("preToolUse");
        assert_eq!(
            fail_open("cursor", &cursor, None),
            json!({"permission": "allow"})
        );
        let post = event("postToolUse");
        assert!(fail_open("cursor", &post, Some("no token"))["additional_context"].is_string());
        assert_eq!(fail_open("cursor", &post, None), json!({}));
        let id = |key: &str| json!({ key: "s" });
        for key in ["session_id", "conversation_id", "sessionId"] {
            assert_eq!(session_of(&id(key)), Some("s"));
        }
        assert_eq!(session_of(&json!({"session_id": ""})), None);
        assert_eq!(session_of(&Value::Null), None);
    }

    fn hub_file(url: &str, token_file: &str) -> HubFile {
        parse_hub_file(&json!({"url": url, "token_file": token_file}).to_string()).unwrap()
    }

    fn settings() -> Settings {
        Settings {
            url: None,
            token: None,
            token_file: None,
        }
    }

    fn abs(name: &str) -> String {
        std::env::temp_dir().join(name).display().to_string()
    }

    #[test]
    fn arguments_parse_leniently() {
        let parsed = |list: &[&str]| parse_args(&args(list));
        assert_eq!(parsed(&["claude"]), ("claude".into(), None, None));
        assert_eq!(
            parsed(&["codex", "--hub-url", "https://h.test"]),
            ("codex".into(), Some("https://h.test".into()), None)
        );
        assert_eq!(
            parsed(&["--hub-url=https://h.test", "cursor", "--wat", "x"]),
            ("cursor".into(), Some("https://h.test".into()), None)
        );
        assert_eq!(parsed(&["--hub-url"]), (String::new(), None, None));
        assert_eq!(
            parsed(&["codex", "--client-connection"]),
            ("codex".into(), None, None)
        );
        assert_eq!(parsed(&[]), (String::new(), None, None));
        let repo = |list: &[&str]| parsed(list).2;
        assert_eq!(
            repo(&["x", "--repo", "acme/widgets"]),
            Some("acme/widgets".into())
        );
        assert_eq!(
            repo(&["x", "--repo=acme/widgets"]),
            Some("acme/widgets".into())
        );
        for bad in ["acme", "a b/c", "a/b?x=1", "a//b", ""] {
            assert_eq!(repo(&["x", "--repo", bad]), None, "{bad}");
        }
    }

    #[test]
    fn agent_names_are_one_path_segment() {
        assert_eq!(path_segment("claude"), "claude");
        assert_eq!(path_segment("../x"), "generic");
        assert_eq!(path_segment(""), "generic");
    }

    #[test]
    fn nothing_configured_is_no_hub() {
        let cwd = Path::new(".");
        assert_eq!(
            resolve(&settings(), None, cwd, None),
            Err("no hub configured")
        );
        let flag = "https://hub.example.test";
        assert_eq!(resolve(&settings(), Some(flag), cwd, None), Err("no token"));
    }

    #[test]
    fn environment_wins() {
        let cwd = Path::new(".");
        let file = hub_file("https://stored.example.test/", &abs("stored-token"));
        let env = Settings {
            url: Some("https://env.example.test/".into()),
            token: Some("synthetic-token".into()),
            token_file: Some(abs("ignored")),
        };
        let want = (
            "https://env.example.test".into(),
            Token::Value("synthetic-token".into()),
        );
        assert_eq!(
            resolve(&env, Some("https://flag.example.test"), cwd, Some(&file)),
            Ok(want)
        );
        let env = Settings { token: None, ..env };
        let want = (
            "https://env.example.test".into(),
            Token::File(abs("ignored").into()),
        );
        assert_eq!(resolve(&env, None, cwd, Some(&file)), Ok(want));
        // AGENTSCM_HUB_URL names the hub, so the stored token may go there.
        let env = Settings {
            token_file: None,
            ..env
        };
        let want = (
            "https://env.example.test".into(),
            Token::File(abs("stored-token").into()),
        );
        assert_eq!(resolve(&env, None, cwd, Some(&file)), Ok(want));
    }

    #[test]
    fn stored_token_only_goes_to_the_stored_hub() {
        let cwd = Path::new(".");
        let file = hub_file("https://stored.example.test", &abs("stored-token"));
        let stored = (
            "https://stored.example.test".to_owned(),
            Token::File(abs("stored-token").into()),
        );
        assert_eq!(
            resolve(&settings(), None, cwd, Some(&file)),
            Ok(stored.clone())
        );
        // A trailing slash is the same hub.
        let flag = Some("https://stored.example.test/");
        assert_eq!(resolve(&settings(), flag, cwd, Some(&file)), Ok(stored));
        let other = Some("https://other.example.test");
        assert_eq!(
            resolve(&settings(), other, cwd, Some(&file)),
            Err("no token for this hub; set AGENTSCM_HUB_TOKEN")
        );
        // With an environment token the other hub is fine.
        let env = Settings {
            token: Some("synthetic-token".into()),
            ..settings()
        };
        assert!(resolve(&env, other, cwd, Some(&file)).is_ok());
    }

    #[test]
    fn bad_urls_and_relative_token_files_are_refused() {
        let cwd = Path::new(".");
        let env = Settings {
            token: Some("synthetic-token".into()),
            ..settings()
        };
        assert_eq!(
            resolve(&env, Some("http://example.test"), cwd, None),
            Err("bad hub url")
        );
        assert!(resolve(&env, Some("http://127.0.0.1:9"), cwd, None).is_ok());
        let file = hub_file("https://stored.example.test", "relative-token");
        assert_eq!(
            resolve(&settings(), None, cwd, Some(&file)),
            Err("no token")
        );
    }

    #[test]
    fn payload_directory_comes_from_cwd_then_workspace_roots() {
        assert_eq!(
            cwd_of(&json!({"cwd": "/a", "workspace_roots": ["/b"]})),
            PathBuf::from("/a")
        );
        assert_eq!(
            cwd_of(&json!({"workspace_roots": ["/b", "/c"]})),
            PathBuf::from("/b")
        );
        assert_eq!(
            cwd_of(&Value::Null),
            std::env::current_dir().unwrap_or_default()
        );
    }

    fn connection(root: &Path, repo: &str) -> Connection {
        Connection {
            root: root.canonicalize().unwrap(),
            origin_host: "example.test".into(),
            hub: "https://hub.example.test".into(),
            repo: repo.into(),
            token_file: std::env::temp_dir().join(format!("{}-token", repo.replace('/', "-"))),
            report_completion: false,
            owned_token_id: None,
            owned_token_expires_at: None,
            agents: Vec::new(),
        }
    }

    fn git_repo(parent: &Path, name: &str) -> PathBuf {
        let root = parent.join(name);
        std::fs::create_dir_all(root.join(".git")).unwrap();
        root.canonicalize().unwrap()
    }

    #[test]
    fn codex_payload_keeps_only_patch_headers_for_string_or_command_input() {
        let patch = "*** Begin Patch\n*** Update File: src/a.rs\n@@\n-old secret\n+new secret\n\
                     *** Move to: src/b.rs\n*** End Patch";
        for input in [json!(patch), json!({"command": patch})] {
            let payload = json!({
                "hook_event_name": "PreToolUse",
                "session_id": "s",
                "tool_name": "apply_patch",
                "tool_input": input,
                "source": "private source",
                "patch": "private patch",
                "prompt": "private prompt"
            });
            let safe = sanitized_codex(&payload, None).unwrap();
            assert_eq!(safe["hook_event_name"], "PreToolUse");
            assert_eq!(safe["session_id"], "s");
            assert_eq!(safe["tool_name"], "apply_patch");
            let sent = safe.to_string();
            assert!(sent.contains("*** Update File: src/a.rs"));
            assert!(sent.contains("*** Move to: src/b.rs"));
            for secret in [
                "old secret",
                "new secret",
                "private source",
                "private patch",
                "private prompt",
            ] {
                assert!(
                    !sent.contains(secret),
                    "excluded payload field was forwarded"
                );
            }
        }
    }

    #[test]
    fn client_payloads_keep_only_bounded_session_and_relative_path_metadata() {
        let path = [EditPath {
            header: None,
            path: "src/new.rs".into(),
        }];
        let private = "/private/work/acme/widgets";
        let cases = [
            (
                "claude",
                json!({"hook_event_name":"PreToolUse", "session_id":"s",
                    "cwd":private, "tool_name":"Write",
                    "tool_input":{"file_path":format!("{private}/src/new.rs"), "content":"private-content"},
                    "prompt":"private-prompt", "source":"private-source"}),
            ),
            (
                "cursor",
                json!({"hook_event_name":"postToolUse", "conversation_id":"c",
                    "cwd":private, "tool_name":"Write",
                    "tool_input":{"file_path":format!("{private}/src/new.rs"), "content":"private-content"},
                    "tool_result":"private-result"}),
            ),
            (
                "copilot",
                json!({"sessionId":"c", "cwd":private, "toolName":"edit",
                    "toolArgs":"{\"path\":\"/private/work/acme/widgets/src/new.rs\",\"content\":\"private-content\"}",
                    "initialPrompt":"private-prompt"}),
            ),
            (
                "gemini",
                json!({"hook_event_name":"BeforeTool", "session_id":"g", "cwd":private,
                    "tool_name":"write_file",
                    "tool_input":{"file_path":format!("{private}/src/new.rs"), "content":"private-content"},
                    "transcript_path":"/private/transcript.json"}),
            ),
        ];
        for (agent, payload) in cases {
            let safe = sanitized_client(agent, &payload, &path).unwrap();
            let sent = safe.to_string();
            assert!(sent.contains("src/new.rs"), "missing edit path for {agent}");
            for secret in [
                private,
                "private-content",
                "private-prompt",
                "private-source",
                "private-result",
                "private/transcript",
            ] {
                assert!(!sent.contains(secret), "excluded payload field for {agent}");
            }
        }
    }

    #[test]
    fn copilot_native_raw_patch_is_routed_and_source_is_not_forwarded() {
        let parent = tempfile::tempdir().unwrap();
        let root = git_repo(parent.path(), "repo");
        let connections = [connection(&root, "acme/widgets")];
        let patch = "*** Begin Patch\n*** Add File: src/native.rs\n+private body\n*** End Patch\n";
        for input in [
            json!(patch),
            json!({"command":patch}),
            json!(json!(patch).to_string()),
        ] {
            let payload = json!({"sessionId":"native", "cwd":root, "toolName":"apply_patch",
                "toolArgs":input});
            let routed = route_client_with("copilot", &payload, &connections).unwrap();
            assert_eq!(routed.connection.repo, "acme/widgets");
            assert_eq!(
                routed.payload,
                json!({"sessionId":"native", "toolName":"apply_patch",
                "toolArgs":{"command":"*** Add File: src/native.rs\n"}})
            );
        }
        let malformed = json!({"toolName":"edit", "toolArgs":"not JSON"});
        assert!(edit_paths("copilot", &malformed).is_err());
    }

    #[test]
    fn client_routes_each_native_agent_without_a_root_header() {
        let parent = tempfile::tempdir().unwrap();
        let root = git_repo(parent.path(), "repo");
        let mut reporting = connection(&root, "acme/widgets");
        reporting.report_completion = true;
        let connections = [reporting];
        let payloads = [
            (
                "claude",
                json!({"hook_event_name":"PreToolUse", "session_id":"s", "cwd":root.clone(),
                    "tool_name":"Write", "tool_input":{"file_path":"src/a.rs", "content":"secret"}}),
            ),
            (
                "codex",
                json!({"hook_event_name":"PreToolUse", "session_id":"s", "cwd":root.clone(),
                    "tool_name":"Write", "tool_input":{"file_path":"src/a.rs", "content":"secret"}}),
            ),
            (
                "cursor",
                json!({"hook_event_name":"postToolUse", "conversation_id":"s", "cwd":root.clone(),
                    "tool_name":"Write", "tool_input":{"file_path":"src/a.rs", "content":"secret"}}),
            ),
            (
                "copilot",
                json!({"sessionId":"s", "cwd":root.clone(), "toolName":"edit",
                    "toolArgs":{"path":"src/a.rs", "content":"secret"}}),
            ),
            (
                "gemini",
                json!({"hook_event_name":"BeforeTool", "session_id":"s", "cwd":root,
                    "tool_name":"write_file", "tool_input":{"file_path":"src/a.rs", "content":"secret"}}),
            ),
        ];
        for (agent, payload) in payloads {
            let routed = route_client_with(agent, &payload, &connections).unwrap();
            assert_eq!(routed.connection.repo, "acme/widgets");
            assert!(routed.connection.report_completion);
            assert_eq!(
                routed.endpoint_agent,
                if agent == "gemini" { "generic" } else { agent }
            );
            let sent = routed.payload.to_string();
            assert!(sent.contains("src/a.rs"), "{agent}: {sent}");
            assert!(!sent.contains("secret"), "{agent}: {sent}");
            assert!(
                !sent.contains(parent.path().to_str().unwrap()),
                "{agent}: {sent}"
            );
        }
    }

    #[test]
    fn gemini_stop_is_denied_once_without_forwarding_generic_fields() {
        let config = tempfile::tempdir().unwrap();
        let payload = json!({"hook_event_name":"BeforeTool", "session_id":"g",
            "tool_name":"write_file", "tool_input":{"file_path":"src/a.rs", "content":"private"}});
        let reply = json!({"status":"stop", "text":"coordinate this edit", "extra":"drop"});
        assert_eq!(
            gemini_reply(reply.clone(), &payload, Some(config.path())),
            json!({"decision":"deny", "reason":"coordinate this edit"})
        );
        assert_eq!(
            gemini_reply(reply, &payload, Some(config.path())),
            json!({})
        );
    }

    #[test]
    fn codex_route_handles_parent_cwd_and_rejects_cross_root_patches() {
        let parent = tempfile::tempdir().unwrap();
        let one = git_repo(parent.path(), "one");
        let two = git_repo(parent.path(), "two");
        let connections = [connection(&one, "acme/one"), connection(&two, "acme/two")];
        let payload = json!({
            "hook_event_name": "PreToolUse",
            "session_id": "s",
            "cwd": parent.path(),
            "workspace_roots": [one.clone()],
            "tool_name": "apply_patch",
            "tool_input": "*** Begin Patch\n*** Add File: one/new.rs\n+secret\n*** End Patch"
        });
        let routed = route_codex_with(&payload, &connections).unwrap();
        assert_eq!(routed.connection.repo, "acme/one");
        let body = routed.payload.to_string();
        assert!(body.contains("*** Add File: new.rs"));
        assert!(!body.contains("secret") && !body.contains(parent.path().to_str().unwrap()));

        let crossing = json!({
            "hook_event_name": "PreToolUse",
            "session_id": "s",
            "cwd": parent.path(),
            "tool_name": "apply_patch",
            "tool_input": format!(
                "*** Begin Patch\n*** Update File: {}/a.rs\n*** Move to: {}/a.rs\n*** End Patch",
                one.display(), two.display()
            )
        });
        assert_eq!(
            route_codex_with(&crossing, &connections).unwrap_err(),
            "edit crosses configured repositories"
        );
    }

    #[test]
    fn unregistered_nested_checkout_never_uses_parent_binding() {
        let parent = tempfile::tempdir().unwrap();
        let outer = git_repo(parent.path(), "outer");
        let inner = git_repo(&outer, "inner");
        let connections = [connection(&outer, "acme/outer")];
        let payload = json!({"hook_event_name":"PreToolUse", "session_id":"nested",
            "cwd":outer, "tool_name":"Write", "tool_input":{"file_path":inner.join("new.rs")}});
        assert_eq!(
            route_codex_with(&payload, &connections).unwrap_err(),
            "edit path belongs to another Git checkout"
        );
        let mixed = json!({"hook_event_name":"SessionStart", "session_id":"nested", "cwd":connections[0].root, "workspace_roots":[inner.clone()]});
        assert_eq!(
            route_codex_with(&mixed, &connections).unwrap_err(),
            "path belongs to another Git checkout"
        );
        let session = json!({"hook_event_name":"SessionStart", "session_id":"nested", "cwd":inner});
        assert_eq!(
            route_codex_with(&session, &connections).unwrap_err(),
            "path belongs to another Git checkout"
        );
    }

    /// The layout `git worktree add` leaves: a `.git` file and a back-pointing private directory.
    fn linked_worktree(main: &Path, path: &Path) -> PathBuf {
        let own = main.join(".git/worktrees").join(path.file_name().unwrap());
        std::fs::create_dir_all(&own).unwrap();
        std::fs::create_dir_all(path).unwrap();
        std::fs::write(path.join(".git"), format!("gitdir: {}\n", own.display())).unwrap();
        std::fs::write(
            own.join("gitdir"),
            format!("{}\n", path.join(".git").display()),
        )
        .unwrap();
        std::fs::write(own.join("commondir"), "../..\n").unwrap();
        path.canonicalize().unwrap()
    }

    /// Runs Git for a synthetic repository. Callers pass the plain temporary path: Windows
    /// canonical `\\?\` paths are not accepted by every Git command.
    fn git(root: &Path, arguments: &[&str]) {
        let status = std::process::Command::new("git")
            .args(arguments)
            .current_dir(root)
            .env_remove("GIT_DIR")
            .env_remove("GIT_WORK_TREE")
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .unwrap();
        assert!(status.success(), "git {arguments:?}");
    }

    #[test]
    fn only_unconnected_checkouts_with_a_supported_origin_are_reported() {
        let parent = tempfile::tempdir().unwrap();
        let plain = parent.path().join("plain");
        std::fs::create_dir(&plain).unwrap();
        assert_eq!(unconnected_checkout(&plain, &[]), None);

        let checkout = parent.path().join("main");
        std::fs::create_dir_all(checkout.join("src")).unwrap();
        git(&checkout, &["init", "--quiet"]);
        let main = checkout.canonicalize().unwrap();
        assert_eq!(unconnected_checkout(&main, &[]), None, "no origin");
        git(
            &checkout,
            &["remote", "add", "origin", "file:///synthetic/widgets.git"],
        );
        assert_eq!(unconnected_checkout(&main, &[]), None, "unsupported origin");

        git(
            &checkout,
            &[
                "remote",
                "set-url",
                "origin",
                "https://github.com/acme/widgets.git",
            ],
        );
        let linked = linked_worktree(&main, &main.join(".claude/worktrees/task"));
        let expected = Some((main.clone(), "github.com".into(), "acme/widgets".into()));
        for cwd in [main.join("src"), linked.clone()] {
            assert_eq!(unconnected_checkout(&cwd, &[]), expected);
        }
        let connections = [connection(&main, "acme/widgets")];
        for cwd in [main.join("src"), linked] {
            assert_eq!(unconnected_checkout(&cwd, &connections), None);
        }
    }

    #[test]
    fn markers_record_when_a_key_was_last_marked() {
        let config = tempfile::tempdir().unwrap();
        let (config, every, key) = (
            config.path(),
            Duration::from_secs(60),
            "unconnected\0claude\0/a",
        );
        assert!(!marked_within(config, key, every));
        mark(config, key);
        assert!(marked_within(config, key, every));
        assert!(!marked_within(config, "unconnected\0codex\0/a", every));
        assert!(!marked_within(config, key, Duration::ZERO));
        mark(config, key);
        assert!(marked_within(config, key, every));
    }

    #[cfg(unix)]
    #[test]
    fn only_a_delivered_report_starts_the_throttle_interval() {
        use std::os::unix::fs::PermissionsExt;
        use std::os::unix::net::UnixDatagram;

        fn no_connections() -> agentscm::Result<Vec<Connection>> {
            Ok(Vec::new())
        }
        let parent = tempfile::tempdir().unwrap();
        let repo = parent.path().join("repo");
        std::fs::create_dir(&repo).unwrap();
        git(&repo, &["init", "--quiet"]);
        git(
            &repo,
            &[
                "remote",
                "add",
                "origin",
                "https://github.com/acme/widgets.git",
            ],
        );
        let repo = repo.canonicalize().unwrap();
        let config = parent.path().join("config");
        let tray = parent.path().join("tray");
        std::fs::create_dir(&tray).unwrap();
        std::fs::set_permissions(&tray, std::fs::Permissions::from_mode(0o700)).unwrap();
        let socket = tray.join("activity.sock");
        let key = format!("unconnected\0claude\0{}", repo.display());
        let report = || {
            report_unconnected_at(&config, &socket, "claude", repo.clone(), no_connections);
        };

        report();
        assert!(!marked_within(&config, &key, UNCONNECTED_EVERY), "no tray");
        drop(UnixDatagram::bind(&socket).unwrap());
        report();
        assert!(
            !marked_within(&config, &key, UNCONNECTED_EVERY),
            "stale socket"
        );
        std::fs::remove_file(&socket).unwrap();

        let receiver = UnixDatagram::bind(&socket).unwrap();
        receiver.set_nonblocking(true).unwrap();
        report();
        assert!(marked_within(&config, &key, UNCONNECTED_EVERY));
        let mut bytes = [0_u8; 4096];
        let size = receiver.recv(&mut bytes).unwrap();
        let sent: Value = serde_json::from_slice(&bytes[..size]).unwrap();
        assert_eq!(sent["root"], json!(repo));
        assert_eq!(sent["repo"], "acme/widgets");
        report();
        assert!(receiver.recv(&mut bytes).is_err(), "throttled while marked");

        // A checkout with nothing to report is not checked again on every hook.
        let plain = parent.path().join("plain");
        std::fs::create_dir(&plain).unwrap();
        git(&plain, &["init", "--quiet"]);
        let plain = plain.canonicalize().unwrap();
        report_unconnected_at(&config, &socket, "claude", plain.clone(), no_connections);
        assert!(marked_within(
            &config,
            &format!("unconnected\0claude\0{}", plain.display()),
            UNCONNECTED_EVERY
        ));
        assert!(receiver.recv(&mut bytes).is_err(), "nothing to report");
    }

    #[test]
    fn linked_worktree_sessions_and_edits_use_the_registered_connection() {
        let parent = tempfile::tempdir().unwrap();
        let main = git_repo(parent.path(), "main");
        let nested = linked_worktree(&main, &main.join(".claude/worktrees/task"));
        let beside = linked_worktree(&main, &parent.path().join("beside"));
        let connections = [connection(&main, "acme/widgets")];
        for checkout in [&nested, &beside] {
            let session =
                json!({"hook_event_name":"SessionStart", "session_id":"s", "cwd":checkout});
            let routed = route_client_with("claude", &session, &connections).unwrap();
            assert_eq!(routed.connection, connections[0]);
            assert_eq!(&routed.checkout, checkout);
            let edit = json!({"hook_event_name":"PreToolUse", "session_id":"s", "cwd":checkout,
                "tool_name":"Write", "tool_input":{"file_path":checkout.join("src/a.rs")}});
            let routed = route_client_with("claude", &edit, &connections).unwrap();
            assert_eq!(&routed.checkout, checkout);
            let sent = routed.payload.to_string();
            assert!(
                sent.contains("src/a.rs") && !sent.contains("worktrees"),
                "{sent}"
            );
        }
        let crossing = json!({"hook_event_name":"PreToolUse", "session_id":"s", "cwd":main,
            "tool_name":"apply_patch", "tool_input":format!(
                "*** Begin Patch\n*** Update File: {}/a.rs\n*** Move to: {}/a.rs\n*** End Patch",
                main.display(), beside.display())});
        assert_eq!(
            route_codex_with(&crossing, &connections).unwrap_err(),
            "edit crosses Git worktrees"
        );
    }

    #[test]
    fn codex_session_route_rejects_multiple_registered_workspace_roots() {
        let parent = tempfile::tempdir().unwrap();
        let one = git_repo(parent.path(), "one");
        let two = git_repo(parent.path(), "two");
        let connections = [connection(&one, "acme/one"), connection(&two, "acme/two")];
        let payload = json!({
            "hook_event_name": "SessionStart",
            "session_id": "s",
            "cwd": parent.path(),
            "workspace_roots": [one.clone(), two.clone()],
            "prompt": "must stay local"
        });
        assert_eq!(
            route_codex_with(&payload, &connections).unwrap_err(),
            "repository route is ambiguous"
        );

        let cwd_inside_one = json!({
            "hook_event_name": "SessionEnd",
            "session_id": "s",
            "cwd": one,
            "workspace_roots": [two]
        });
        assert_eq!(
            route_codex_with(&cwd_inside_one, &connections).unwrap_err(),
            "repository route is ambiguous"
        );
        assert_eq!(
            route_codex_with(&json!({"hook_event_name":"SessionEnd"}), &connections).unwrap_err(),
            NO_CONNECTION
        );
    }

    #[test]
    fn client_request_omits_the_private_root_header() {
        let parent = tempfile::tempdir().unwrap();
        let root = git_repo(parent.path(), "private-checkout");
        let connections = [connection(&root, "acme/widgets")];
        let routed = route_codex_with(
            &json!({
                "hook_event_name":"PreToolUse",
                "session_id":"s",
                "cwd":root.clone(),
                "tool_name":"Write",
                "tool_input":{"file_path":"src/new.rs", "content":"private"}
            }),
            &connections,
        )
        .unwrap();
        let request = hub_request(
            &reqwest::Client::new(),
            "https://hub.example.test",
            "codex",
            Some("acme/widgets"),
            "synthetic-token",
            &serde_json::to_vec(&routed.payload).unwrap(),
            None,
        )
        .unwrap()
        .build()
        .unwrap();
        assert!(!request.headers().contains_key("X-Agentscm-Root"));
        let body = request.body().and_then(reqwest::Body::as_bytes).unwrap();
        let body = std::str::from_utf8(body).unwrap();
        assert!(!body.contains(root.to_str().unwrap()));
        assert!(!body.contains("private"));
    }

    fn event(name: &str) -> Value {
        json!({"hook_event_name": name})
    }

    #[test]
    fn claude_codex_and_copilot_get_context_on_two_events() {
        for agent in ["claude", "codex", "copilot"] {
            for name in ["PreToolUse", "SessionStart"] {
                let out = unknown_output(agent, &event(name), "hub unreachable");
                assert_eq!(out["hookSpecificOutput"]["hookEventName"], name);
                let text = out["hookSpecificOutput"]["additionalContext"]
                    .as_str()
                    .unwrap();
                assert_eq!(
                    text,
                    "AgentSCM: coordination unknown (hub unreachable). \
                     Other agents' work on these files is not visible."
                );
                assert_eq!(out.as_object().unwrap().len(), 1);
            }
            for other in [
                event("SessionEnd"),
                event("preToolUse"),
                Value::Null,
                json!({}),
            ] {
                assert_eq!(unknown_output(agent, &other, "no token"), json!({}));
            }
        }
    }

    #[test]
    fn edits_no_connection_covers_get_no_note() {
        let out = unknown_output("claude", &event("PreToolUse"), NO_EDIT_CONNECTION);
        assert!(!out.to_string().contains("coordination unknown"), "{out}");
        let out = unknown_output("claude", &event("SessionStart"), NO_CONNECTION);
        assert!(out.to_string().contains("quickstart"), "{out}");
    }

    #[test]
    fn cursor_never_gets_invalid_or_blocking_output() {
        let out = |payload: &Value| unknown_output("cursor", payload, "no token");
        assert_eq!(out(&event("preToolUse")), json!({"permission": "allow"}));
        assert_eq!(
            out(&json!({"tool_name": "Write"})),
            json!({"permission": "allow"})
        );
        assert_eq!(out(&Value::Null), json!({"permission": "allow"}));
        for name in ["sessionStart", "postToolUse"] {
            let reply = out(&event(name));
            assert!(
                reply["additional_context"]
                    .as_str()
                    .unwrap()
                    .contains("coordination unknown (no token)")
            );
        }
        assert_eq!(out(&event("sessionEnd")), json!({}));
        assert_eq!(out(&json!({"conversation_id": "c"})), json!({}));
    }

    #[test]
    fn gemini_fail_open_uses_only_documented_non_blocking_outputs() {
        let before = fail_open(
            "gemini",
            &json!({"hook_event_name":"BeforeTool", "session_id":"s"}),
            Some("no token"),
        );
        assert!(
            before["systemMessage"]
                .as_str()
                .unwrap()
                .contains("no token")
        );
        assert!(before.get("decision").is_none());
        let start = fail_open(
            "gemini",
            &json!({"hook_event_name":"SessionStart", "session_id":"other"}),
            Some("hub unreachable"),
        );
        assert_eq!(start["hookSpecificOutput"]["hookEventName"], "SessionStart");
        assert!(
            start["hookSpecificOutput"]["additionalContext"]
                .as_str()
                .unwrap()
                .contains("hub unreachable")
        );
        assert_eq!(
            fail_open(
                "gemini",
                &json!({"hook_event_name":"SessionEnd", "session_id":"end"}),
                Some("no token")
            ),
            json!({})
        );
    }

    #[test]
    fn generic_and_unknown_agents_get_an_empty_object() {
        for agent in ["generic", "mystery", ""] {
            assert_eq!(
                unknown_output(agent, &event("PreToolUse"), "no token"),
                json!({})
            );
        }
    }
}
