//! Sanitized, read-only status for native client integrations.

use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use reqwest::Method;
use serde::{Deserialize, Serialize};

use agentscm::change_record::CaptureModeV1;
use agentscm::hub::forge_http::{self, send};
use agentscm::hub::onboarding::{Blocker, CLIENT_HEADER};
use agentscm::hub::store::unix_now;
use agentscm::{Error, Result};

use crate::client_cli::read_token;
use crate::client_config::{Connection, is_current, load, onboarding_client};
use crate::hub_cli::map_hub_error;

const REPLY_BYTES: usize = 64 * 1024;
const CONNECTION_CHECK_TIMEOUT: Duration = Duration::from_secs(5);
const SNAPSHOT_CHECK_TIMEOUT: Duration = Duration::from_secs(15);
const CHECK_CONCURRENCY: usize = 4;
const ONBOARDING_FRESH_SECONDS: i64 = 15 * 60;

#[derive(Debug, Serialize)]
pub(crate) struct Snapshot {
    pub(crate) version: u8,
    pub(crate) snapshot_at: i64,
    pub(crate) connections: Vec<ConnectionStatus>,
}

#[derive(Debug, Serialize)]
pub(crate) struct ConnectionStatus {
    pub(crate) root: PathBuf,
    pub(crate) repository: String,
    pub(crate) hub: String,
    pub(crate) agents: Vec<String>,
    pub(crate) binding: BindingStatus,
    pub(crate) credential: CredentialStatus,
    pub(crate) hub_check: HubCheck,
    /// What the saved token, its renewal and the Hub's answer together support saying about this
    /// connection. A stored token alone is only `configured`, never live coordination.
    pub(crate) health: Health,
    /// Whether any agent's hook and MCP use was verified by the Hub; separate from `health`.
    pub(crate) coverage: Coverage,
    pub(crate) recent_participation: Vec<Participation>,
    /// Capture mode of this checkout's enabled change-capture binding. Absent when the checkout
    /// has no enabled binding or the capture config cannot be read.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) capture_mode: Option<CaptureModeV1>,
    /// The repository and Hub this checkout's saved capture binding belongs to when that is not
    /// what the checkout is connected to now. Sharing can then only be turned off.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) capture_mismatch: Option<CaptureMismatch>,
}

/// Change capture is Unix-only, so only Unix code builds one.
#[cfg_attr(not(unix), allow(dead_code))]
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub(crate) struct CaptureMismatch {
    pub(crate) repository: String,
    pub(crate) hub: String,
}

#[derive(Debug, Serialize)]
pub(crate) struct BindingStatus {
    pub(crate) state: BindingState,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) generation: Option<String>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum BindingState {
    Current,
    Mismatch,
    Unknown,
}

#[derive(Debug, Serialize)]
pub(crate) struct CredentialStatus {
    pub(crate) kind: CredentialKind,
    pub(crate) state: CredentialState,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) expires_at: Option<i64>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum CredentialKind {
    Managed,
    External,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum CredentialState {
    Available,
    Expired,
    Unknown,
}

#[derive(Debug, Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub(crate) enum HubCheck {
    Skipped,
    Valid {
        checked_at: i64,
        provider: String,
        host: String,
        login: String,
        expires_at: i64,
        /// Days the Hub keeps an uploaded shared conversation, as the Hub reported. Absent from an
        /// older Hub, so callers must not assume a number.
        #[serde(skip_serializing_if = "Option::is_none")]
        shared_conversation_retention_days: Option<i64>,
        /// The Hub's deployed version and commit, for diagnosis. Absent from an older Hub or
        /// when its health route did not answer.
        #[serde(skip_serializing_if = "Option::is_none")]
        hub_build: Option<HubBuild>,
    },
    Rejected {
        checked_at: i64,
    },
    Unavailable {
        checked_at: i64,
    },
    InvalidResponse {
        checked_at: i64,
    },
}

/// How far a connection can be trusted, from the strongest evidence available. Order of
/// precedence: an ended token, a Hub that refused it, a Hub that did not answer, then a valid
/// answer qualified by the renewal state.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub(crate) enum Health {
    /// A token is saved and nothing has asked the Hub, so coordination is not established.
    Configured,
    /// The saved token file is missing, unreadable or not private.
    NoUsableToken,
    /// The Hub accepted the token just now.
    Authenticated,
    /// Accepted, but within the renewal window and not yet renewed.
    RenewalPending,
    /// The last renewal was refused: the token still works until it ends, then coordination stops.
    RenewalFailed { message: String },
    /// The token's end passed and nothing renewed it.
    Expired,
    /// The Hub rejected a token that has not ended: revoked, or for another repository or user.
    Revoked,
    /// The Hub did not answer, or answered something unreadable; the token's state is unknown.
    Unreachable,
}

impl Health {
    /// One line for a person: the state and, when there is one, what to do about it.
    pub(crate) fn summary(&self) -> String {
        match self {
            Self::Configured => "configured; not checked with the Hub, so live coordination is not shown (pass --check)".into(),
            Self::NoUsableToken => "the saved token cannot be read; run client setup again".into(),
            Self::Authenticated => "authenticated; the Hub accepted the token".into(),
            Self::RenewalPending => "authenticated; the token is due for renewal, which the next hook call starts".into(),
            Self::RenewalFailed { message } => format!("authenticated but renewal failed: {message}"),
            Self::Expired => "expired; the next hook call tries to renew it, or run client renew".into(),
            Self::Revoked => "the Hub rejected the token (revoked or no longer valid); run client setup again".into(),
            Self::Unreachable => "the Hub did not answer; hooks fail open and coordination is unknown until it does".into(),
        }
    }
}

/// Whether the Hub has seen an agent's hook and MCP in this checkout.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Coverage {
    /// An agent's MCP and native hook were both verified.
    Verified,
    /// Some proof, not both.
    Partial,
    /// The Hub answered and has seen neither.
    Unverified,
    /// The Hub was not asked or did not answer.
    Unknown,
}

pub(crate) fn health(
    credential: &CredentialStatus,
    renewal_failure: Option<String>,
    hub_check: &HubCheck,
    now: i64,
) -> Health {
    let managed = matches!(credential.kind, CredentialKind::Managed);
    if matches!(credential.state, CredentialState::Unknown) {
        return Health::NoUsableToken;
    }
    if matches!(credential.state, CredentialState::Expired) {
        return match renewal_failure.filter(|_| managed) {
            Some(message) => Health::RenewalFailed { message },
            None => Health::Expired,
        };
    }
    match hub_check {
        HubCheck::Rejected { .. } => Health::Revoked,
        HubCheck::Unavailable { .. } | HubCheck::InvalidResponse { .. } => Health::Unreachable,
        HubCheck::Valid { .. } => {
            match (renewal_failure.filter(|_| managed), credential.expires_at) {
                (Some(message), _) => Health::RenewalFailed { message },
                (None, Some(expires))
                    if expires - now < crate::client_cli::RENEW_BEFORE_SECONDS =>
                {
                    Health::RenewalPending
                }
                _ => Health::Authenticated,
            }
        }
        HubCheck::Skipped => Health::Configured,
    }
}

pub(crate) fn coverage(hub_check: &HubCheck, rows: &[Participation]) -> Coverage {
    if !matches!(hub_check, HubCheck::Valid { .. }) {
        Coverage::Unknown
    } else if rows
        .iter()
        .any(|row| row.mcp_verified && row.native_verified)
    {
        Coverage::Verified
    } else if rows
        .iter()
        .any(|row| row.mcp_verified || row.native_verified)
    {
        Coverage::Partial
    } else {
        Coverage::Unverified
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub(crate) struct HubBuild {
    pub(crate) version: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) commit: Option<String>,
}

impl HubBuild {
    /// "0.1.0 (3f9c2a1b4d5e)", or the version alone when the Hub gave no commit.
    pub(crate) fn label(&self) -> String {
        match self.commit.as_deref().and_then(|commit| commit.get(..12)) {
            Some(short) => format!("{} ({short})", self.version),
            None => self.version.clone(),
        }
    }
}

/// The Hub's build from its public health route; None when it does not say. Both fields are
/// bounded and restricted to version and hex characters, because the Hub is not trusted to
/// send terminal text.
pub(crate) async fn hub_build(client: &reqwest::Client, hub: &str) -> Option<HubBuild> {
    let (status, reply) = send(
        client.request(Method::GET, format!("{hub}/v1/hub/health")),
        REPLY_BYTES,
    )
    .await
    .ok()?;
    (status == 200).then_some(())?;
    Some(HubBuild {
        version: plain_build_text(reply["version"].as_str()?)?,
        commit: reply["commit"].as_str().and_then(plain_build_text),
    })
}

fn plain_build_text(text: &str) -> Option<String> {
    (!text.is_empty()
        && text.len() <= 64
        && text
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "._+-".contains(c)))
    .then(|| text.to_owned())
}

#[derive(Debug, Serialize)]
pub(crate) struct Participation {
    pub(crate) agent: String,
    pub(crate) state: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) blocker: Option<Blocker>,
    pub(crate) mcp_verified: bool,
    pub(crate) native_verified: bool,
    pub(crate) expires_at: i64,
    pub(crate) next_action: String,
}

#[derive(Debug, Deserialize)]
struct OnboardingRow {
    agent: serde_json::Value,
    state: String,
    blocker: Option<Blocker>,
    mcp_verified: bool,
    native_verified: bool,
    expires_at: i64,
    #[allow(dead_code)]
    next_action: String,
}

fn invalid(message: &str) -> Error {
    Error::Invalid(message.to_owned())
}

pub(crate) async fn snapshot_for(check: bool, root: Option<&Path>) -> Result<Snapshot> {
    let snapshot_at = unix_now();
    let mut connections = load()?;
    if let Some(root) = root {
        connections.retain(|connection| connection.root == root);
        if connections.is_empty() {
            return Err(invalid("no client connection for this checkout"));
        }
    }
    let statuses = if check {
        checked_connections(connections, snapshot_at).await
    } else {
        let mut statuses = Vec::with_capacity(connections.len());
        for connection in connections {
            statuses.push(connection_status(connection, false, snapshot_at).await);
        }
        statuses
    };
    Ok(Snapshot {
        version: 1,
        snapshot_at,
        connections: statuses,
    })
}

async fn checked_connections(connections: Vec<Connection>, now: i64) -> Vec<ConnectionStatus> {
    checked_connections_with_timeouts(
        connections,
        now,
        CONNECTION_CHECK_TIMEOUT,
        SNAPSHOT_CHECK_TIMEOUT,
    )
    .await
}

async fn checked_connections_with_timeouts(
    connections: Vec<Connection>,
    now: i64,
    connection_timeout: Duration,
    snapshot_timeout: Duration,
) -> Vec<ConnectionStatus> {
    let semaphore = Arc::new(tokio::sync::Semaphore::new(CHECK_CONCURRENCY));
    let mut tasks = tokio::task::JoinSet::new();
    for connection in connections.iter().cloned() {
        let semaphore = Arc::clone(&semaphore);
        tasks.spawn(async move {
            let _permit = semaphore.acquire_owned().await.ok();
            let root = connection.root.clone();
            let status = match tokio::time::timeout(
                connection_timeout,
                connection_status(connection.clone(), true, now),
            )
            .await
            {
                Ok(status) => status,
                Err(_) => unresolved_status(connection, now),
            };
            (root, status)
        });
    }
    let mut completed = collect_until(&mut tasks, snapshot_timeout).await;
    let completed_roots: HashSet<PathBuf> = completed.keys().cloned().collect();
    for connection in connections {
        if !completed_roots.contains(&connection.root) {
            completed.insert(connection.root.clone(), unresolved_status(connection, now));
        }
    }
    completed.into_values().collect()
}

async fn collect_until(
    tasks: &mut tokio::task::JoinSet<(PathBuf, ConnectionStatus)>,
    timeout: Duration,
) -> BTreeMap<PathBuf, ConnectionStatus> {
    let deadline = tokio::time::Instant::now() + timeout;
    let mut completed = BTreeMap::new();
    loop {
        match tokio::time::timeout_at(deadline, tasks.join_next()).await {
            Ok(Some(Ok((root, status)))) => {
                completed.insert(root, status);
            }
            Ok(Some(Err(_))) => {}
            Ok(None) => break,
            Err(_) => {
                tasks.abort_all();
                while let Some(result) = tasks.try_join_next() {
                    if let Ok((root, status)) = result {
                        completed.insert(root, status);
                    }
                }
                break;
            }
        }
    }
    completed
}

/// Change capture is Unix-only, so elsewhere no checkout reports a capture mode.
fn capture_for(connection: &Connection) -> (Option<CaptureModeV1>, Option<CaptureMismatch>) {
    #[cfg(unix)]
    {
        crate::change_capture_client::capture_setting(connection).map_or((None, None), |setting| {
            (setting.mode, setting.lineage_mismatch)
        })
    }
    #[cfg(not(unix))]
    {
        let _ = connection;
        (None, None)
    }
}

fn unresolved_status(connection: Connection, now: i64) -> ConnectionStatus {
    let credential = credential_status(&connection, false, now);
    let (capture_mode, capture_mismatch) = capture_for(&connection);
    ConnectionStatus {
        root: connection.root,
        repository: connection.repo,
        hub: connection.hub,
        agents: connection.agents,
        binding: BindingStatus {
            state: BindingState::Unknown,
            generation: None,
        },
        credential,
        health: Health::Unreachable,
        coverage: Coverage::Unknown,
        hub_check: HubCheck::Unavailable {
            checked_at: unix_now(),
        },
        recent_participation: Vec::new(),
        capture_mode,
        capture_mismatch,
    }
}

async fn connection_status(connection: Connection, check: bool, now: i64) -> ConnectionStatus {
    let current = is_current(&connection);
    let token = read_token(&connection.token_file).ok();
    let generation = token
        .as_deref()
        .map(|token| onboarding_client(&connection.root, token));
    let credential = credential_status(&connection, token.is_some(), now);
    let mut hub_check = HubCheck::Skipped;
    let mut recent_participation = Vec::new();
    if check && current {
        if let Some(token) = token.as_deref() {
            let (result, participation) =
                check_hub(&connection, token, generation.as_deref()).await;
            hub_check = result;
            recent_participation = participation;
        } else {
            hub_check = HubCheck::Unavailable {
                checked_at: unix_now(),
            };
        }
    }
    if check && current && !still_current(&connection, generation.as_deref()) {
        return unresolved_status(connection, now);
    }
    let (capture_mode, capture_mismatch) = capture_for(&connection);
    let health = health(
        &credential,
        crate::client_cli::attention(&connection.root),
        &hub_check,
        now,
    );
    let coverage = coverage(&hub_check, &recent_participation);
    ConnectionStatus {
        root: connection.root,
        repository: connection.repo,
        hub: connection.hub,
        agents: connection.agents,
        binding: BindingStatus {
            state: if current {
                BindingState::Current
            } else {
                BindingState::Mismatch
            },
            generation,
        },
        credential,
        hub_check,
        health,
        coverage,
        recent_participation,
        capture_mode,
        capture_mismatch,
    }
}

fn still_current(connection: &Connection, generation: Option<&str>) -> bool {
    let Ok(saved) = load() else {
        return false;
    };
    let Some(saved) = saved.iter().find(|saved| saved.root == connection.root) else {
        return false;
    };
    if saved != connection || !is_current(saved) {
        return false;
    }
    read_token(&saved.token_file)
        .ok()
        .map(|token| onboarding_client(&saved.root, &token))
        .as_deref()
        == generation
}

fn credential_status(connection: &Connection, readable: bool, now: i64) -> CredentialStatus {
    let expires_at = connection.owned_token_expires_at;
    CredentialStatus {
        kind: if expires_at.is_some() {
            CredentialKind::Managed
        } else {
            CredentialKind::External
        },
        state: match (readable, expires_at) {
            (true, Some(expiry)) if expiry <= now => CredentialState::Expired,
            (true, Some(_)) => CredentialState::Available,
            (true, None) => CredentialState::Available,
            _ => CredentialState::Unknown,
        },
        expires_at,
    }
}

pub(crate) async fn check_hub(
    connection: &Connection,
    token: &str,
    client_id: Option<&str>,
) -> (HubCheck, Vec<Participation>) {
    let Ok(client) = forge_http::client().map_err(map_hub_error) else {
        return (
            HubCheck::Unavailable {
                checked_at: unix_now(),
            },
            Vec::new(),
        );
    };
    let request = client
        .request(Method::GET, format!("{}/v1/hub/whoami", connection.hub))
        .bearer_auth(token);
    let (status, reply) = match send(request, REPLY_BYTES).await {
        Ok(reply) => reply,
        Err(_) => {
            return (
                HubCheck::Unavailable {
                    checked_at: unix_now(),
                },
                Vec::new(),
            );
        }
    };
    if status == 401 || status == 403 {
        return (
            HubCheck::Rejected {
                checked_at: unix_now(),
            },
            Vec::new(),
        );
    }
    if status != 200 {
        return (
            HubCheck::Unavailable {
                checked_at: unix_now(),
            },
            Vec::new(),
        );
    }
    let Some(repository) = reply["token"]["repository"].as_str() else {
        return (
            HubCheck::InvalidResponse {
                checked_at: unix_now(),
            },
            Vec::new(),
        );
    };
    let Some(provider) = reply["owner"]["provider"].as_str() else {
        return (
            HubCheck::InvalidResponse {
                checked_at: unix_now(),
            },
            Vec::new(),
        );
    };
    let Some(host) = reply["owner"]["host"].as_str() else {
        return (
            HubCheck::InvalidResponse {
                checked_at: unix_now(),
            },
            Vec::new(),
        );
    };
    let Some(login) = reply["owner"]["login"].as_str() else {
        return (
            HubCheck::InvalidResponse {
                checked_at: unix_now(),
            },
            Vec::new(),
        );
    };
    let Some(expires_at) = reply["token"]["expires_at"].as_i64() else {
        return (
            HubCheck::InvalidResponse {
                checked_at: unix_now(),
            },
            Vec::new(),
        );
    };
    let host_matches = reqwest::Url::parse(&format!("https://{host}"))
        .ok()
        .and_then(|url| url.host_str().map(str::to_ascii_lowercase))
        .as_deref()
        == Some(connection.origin_host.as_str());
    let provider_matches = (connection.origin_host != "github.com" || provider == "github")
        && (connection.origin_host != "gitlab.com" || provider == "gitlab");
    if !repository.eq_ignore_ascii_case(&connection.repo)
        || reply["token"]["read_only"].as_bool() != Some(false)
        || expires_at <= unix_now()
        || !host_matches
        || !provider_matches
    {
        return (
            HubCheck::Rejected {
                checked_at: unix_now(),
            },
            Vec::new(),
        );
    }
    let participation = match client_id {
        Some(client_id) => onboarding(&client, connection, token, client_id).await,
        None => Vec::new(),
    };
    let retention_days = reply["shared_conversation_retention_days"]
        .as_i64()
        .filter(|days| (0..=3650).contains(days));
    let (mut check, participation) = finalize_hub_check(
        expires_at,
        unix_now(),
        provider,
        host,
        login,
        retention_days,
        participation,
    );
    if let HubCheck::Valid {
        hub_build: slot, ..
    } = &mut check
    {
        *slot = hub_build(&client, &connection.hub).await;
    }
    (check, participation)
}

fn finalize_hub_check(
    expires_at: i64,
    checked_at: i64,
    provider: &str,
    host: &str,
    login: &str,
    shared_conversation_retention_days: Option<i64>,
    mut participation: Vec<Participation>,
) -> (HubCheck, Vec<Participation>) {
    if expires_at <= checked_at {
        return (HubCheck::Rejected { checked_at }, Vec::new());
    }
    participation.retain(|row| row.expires_at > checked_at);
    (
        HubCheck::Valid {
            checked_at,
            provider: crate::hub_cli::clean(provider),
            host: crate::hub_cli::clean(host),
            login: crate::hub_cli::clean(login),
            expires_at,
            shared_conversation_retention_days,
            hub_build: None,
        },
        participation,
    )
}

async fn onboarding(
    client: &reqwest::Client,
    connection: &Connection,
    token: &str,
    client_id: &str,
) -> Vec<Participation> {
    let mut out = Vec::new();
    for agent in &connection.agents {
        let request = client
            .get(format!("{}/v1/hub/onboarding", connection.hub))
            .query(&[("agent", agent.as_str())])
            .bearer_auth(token)
            .header(CLIENT_HEADER, client_id);
        let Ok((200, reply)) = send(request, REPLY_BYTES).await else {
            continue;
        };
        let Ok(rows) = agentscm::hub::reply::decode::<Vec<OnboardingRow>>(&reply) else {
            continue;
        };
        let now = unix_now();
        out.extend(rows.into_iter().filter_map(|row| {
            let returned_agent = row.agent.as_str()?.to_owned();
            let known_state = matches!(row.state.as_str(), "ready" | "blocked" | "unverified");
            (returned_agent == *agent
                && known_state
                && row.expires_at > now
                && row.expires_at <= now + ONBOARDING_FRESH_SECONDS)
                .then_some(Participation {
                    agent: returned_agent,
                    state: row.state,
                    blocker: row.blocker,
                    mcp_verified: row.mcp_verified,
                    native_verified: row.native_verified,
                    expires_at: row.expires_at,
                    next_action: repair_action(row.blocker, row.mcp_verified, row.native_verified)
                        .to_owned(),
                })
        }));
    }
    out
}

fn repair_action(blocker: Option<Blocker>, mcp: bool, native: bool) -> &'static str {
    match blocker {
        Some(Blocker::Login) => "Sign in through the coding agent, then retry verification.",
        Some(Blocker::Trust) => {
            "Review and approve the configured integration, then start a fresh session."
        }
        Some(Blocker::Quota) => "Wait for the existing plan's usage limit to reset, then retry.",
        Some(Blocker::Unsupported) => "Review supported access for this coding agent and account.",
        Some(Blocker::Unknown) => "Inspect the coding agent's login, MCP, and hook status.",
        None if !mcp => "Call agentscm_setup_status through the configured native MCP connection.",
        None if !native => {
            "Native edit evidence appears after normal work; missing activity does not imply failure."
        }
        None => "Recent MCP and native edit-hook participation was observed.",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn connection(root: &str) -> Connection {
        Connection {
            root: PathBuf::from(root),
            origin_host: "github.com".into(),
            hub: "https://hub.example.test".into(),
            repo: "owner/repo".into(),
            token_file: PathBuf::from("/synthetic/token"),
            report_completion: false,
            owned_token_id: None,
            owned_token_expires_at: None,
            agents: Vec::new(),
        }
    }

    #[tokio::test]
    async fn aggregate_deadline_retains_completed_results() {
        let mut tasks = tokio::task::JoinSet::new();
        let fast = connection("/fast");
        tasks.spawn(async move {
            let root = fast.root.clone();
            let mut status = unresolved_status(fast, 1);
            status.hub_check = HubCheck::Valid {
                checked_at: 1,
                provider: "github".into(),
                host: "github.com".into(),
                login: "synthetic".into(),
                expires_at: 2,
                shared_conversation_retention_days: None,
                hub_build: None,
            };
            (root, status)
        });
        tasks.spawn(async move {
            std::future::pending::<()>().await;
            unreachable!()
        });

        let completed = collect_until(&mut tasks, Duration::from_millis(10)).await;
        assert_eq!(completed.len(), 1);
        assert!(matches!(
            completed.get(Path::new("/fast")).unwrap().hub_check,
            HubCheck::Valid { .. }
        ));
    }

    #[test]
    fn capture_mode_is_reported_only_when_a_binding_supplies_it() {
        let mut status = unresolved_status(connection("/synthetic/unbound-checkout"), 1);
        assert_eq!(status.capture_mode, None);
        assert!(
            serde_json::to_value(&status)
                .unwrap()
                .get("capture_mode")
                .is_none()
        );
        status.capture_mode = Some(CaptureModeV1::SharedConversation);
        assert_eq!(
            serde_json::to_value(&status).unwrap()["capture_mode"],
            "shared_conversation"
        );
        assert!(
            serde_json::to_value(&status)
                .unwrap()
                .get("capture_mismatch")
                .is_none()
        );
        status.capture_mismatch = Some(CaptureMismatch {
            repository: "owner/repo".into(),
            hub: "https://old.example.test".into(),
        });
        assert_eq!(
            serde_json::to_value(&status).unwrap()["capture_mismatch"]["hub"],
            "https://old.example.test"
        );
    }

    #[test]
    fn readable_external_credential_is_available_with_unknown_expiry() {
        let status = credential_status(&connection("/root"), true, 1);
        assert!(matches!(status.kind, CredentialKind::External));
        assert!(matches!(status.state, CredentialState::Available));
        assert_eq!(status.expires_at, None);
    }

    fn participation(expires_at: i64) -> Participation {
        Participation {
            agent: "codex".into(),
            state: "ready".into(),
            blocker: None,
            mcp_verified: true,
            native_verified: true,
            expires_at,
            next_action: "synthetic".into(),
        }
    }

    #[test]
    fn final_hub_check_rejects_crossed_expiry_and_refilters_all_participation() {
        let (expired, rows) = finalize_hub_check(
            10,
            10,
            "github",
            "github.com",
            "synthetic",
            None,
            vec![participation(11)],
        );
        assert!(matches!(expired, HubCheck::Rejected { checked_at: 10 }));
        assert!(rows.is_empty());

        let (valid, rows) = finalize_hub_check(
            20,
            10,
            "github",
            "github.com",
            "synthetic",
            Some(14),
            vec![participation(10), participation(11)],
        );
        assert!(matches!(
            valid,
            HubCheck::Valid {
                checked_at: 10,
                expires_at: 20,
                shared_conversation_retention_days: Some(14),
                ..
            }
        ));
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].expires_at, 11);
    }

    fn managed(state: CredentialState, expires_at: Option<i64>) -> CredentialStatus {
        CredentialStatus {
            kind: CredentialKind::Managed,
            state,
            expires_at,
        }
    }

    fn valid(expires_at: i64) -> HubCheck {
        HubCheck::Valid {
            checked_at: 0,
            provider: "github".into(),
            host: "github.com".into(),
            login: "synthetic".into(),
            expires_at,
            shared_conversation_retention_days: None,
            hub_build: None,
        }
    }

    #[test]
    fn eight_saved_connections_with_mixed_validity_are_each_described_honestly() {
        let now = 1_000_000;
        let day = 24 * 60 * 60;
        let down = HubCheck::Unavailable { checked_at: now };
        let refused = HubCheck::Rejected { checked_at: now };
        let failed = || Some("the Hub refused to renew".to_owned());
        let rows = [
            // Stored token, never asked: configured, not authenticated.
            (
                managed(CredentialState::Available, Some(now + day)),
                None,
                HubCheck::Skipped,
                Health::Configured,
            ),
            (
                managed(CredentialState::Available, Some(now + day)),
                None,
                valid(now + day),
                Health::Authenticated,
            ),
            (
                managed(CredentialState::Available, Some(now + 60)),
                None,
                valid(now + 60),
                Health::RenewalPending,
            ),
            (
                managed(CredentialState::Available, Some(now + 60)),
                failed(),
                valid(now + 60),
                Health::RenewalFailed {
                    message: "the Hub refused to renew".into(),
                },
            ),
            // The token ended and nothing renewed it; the Hub is not consulted.
            (
                managed(CredentialState::Expired, Some(now - 1)),
                None,
                HubCheck::Unavailable { checked_at: now },
                Health::Expired,
            ),
            (
                managed(CredentialState::Available, Some(now + day)),
                None,
                refused,
                Health::Revoked,
            ),
            (
                managed(CredentialState::Available, Some(now + day)),
                None,
                down,
                Health::Unreachable,
            ),
            (
                managed(CredentialState::Unknown, None),
                None,
                HubCheck::Skipped,
                Health::NoUsableToken,
            ),
        ];
        for (credential, attention, check, expected) in rows {
            assert_eq!(health(&credential, attention, &check, now), expected);
        }
    }

    #[test]
    fn a_supplied_token_never_reports_a_renewal_state() {
        let external = CredentialStatus {
            kind: CredentialKind::External,
            state: CredentialState::Available,
            expires_at: None,
        };
        // A stale renewal refusal recorded for the checkout does not describe a token this
        // computer does not renew.
        assert_eq!(
            health(&external, Some("old refusal".into()), &valid(10), 0),
            Health::Authenticated
        );
    }

    #[test]
    fn coverage_needs_the_hub_to_have_seen_both_the_mcp_and_a_native_hook() {
        let mut both = participation(10);
        let mut mcp_only = participation(10);
        mcp_only.native_verified = false;
        let mut none = participation(10);
        none.mcp_verified = false;
        none.native_verified = false;
        both.mcp_verified = true;
        assert_eq!(coverage(&valid(10), &[both]), Coverage::Verified);
        assert_eq!(coverage(&valid(10), &[mcp_only]), Coverage::Partial);
        assert_eq!(coverage(&valid(10), &[none]), Coverage::Unverified);
        // Rows from an unchecked or unreachable Hub are not evidence.
        assert_eq!(
            coverage(&HubCheck::Skipped, &[participation(10)]),
            Coverage::Unknown
        );
    }

    #[test]
    fn a_hub_build_is_taken_only_when_it_is_plain_version_text() {
        let build = HubBuild {
            version: "0.3.0".into(),
            commit: Some("0123456789abcdef".into()),
        };
        assert_eq!(build.label(), "0.3.0 (0123456789ab)");
        assert_eq!(
            HubBuild {
                version: "0.3.0".into(),
                commit: None
            }
            .label(),
            "0.3.0"
        );
        assert_eq!(
            plain_build_text("0.3.0-rc.1+abc").as_deref(),
            Some("0.3.0-rc.1+abc")
        );
        for hostile in ["", "1.0\u{1b}[31m", "a b", &"x".repeat(65)] {
            assert_eq!(plain_build_text(hostile), None, "{hostile:?}");
        }
    }
}
