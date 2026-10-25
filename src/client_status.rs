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
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) activity_generation: Option<String>,
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

/// Produce a local-only snapshot unless `check` explicitly enables bounded Hub reads.
#[cfg(target_os = "macos")]
pub(crate) async fn snapshot(check: bool) -> Result<Snapshot> {
    snapshot_for(check, None).await
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
            activity_generation: None,
        },
        credential,
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
    let activity_generation = token
        .as_deref()
        .map(|token| crate::client_activity::activity_binding(&connection, token));
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
            activity_generation,
        },
        credential,
        hub_check,
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
    finalize_hub_check(expires_at, unix_now(), provider, host, login, participation)
}

fn finalize_hub_check(
    expires_at: i64,
    checked_at: i64,
    provider: &str,
    host: &str,
    login: &str,
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
        let Ok(rows) = serde_json::from_value::<Vec<OnboardingRow>>(reply) else {
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
            vec![participation(10), participation(11)],
        );
        assert!(matches!(
            valid,
            HubCheck::Valid {
                checked_at: 10,
                expires_at: 20,
                ..
            }
        ));
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].expires_at, 11);
    }
}
