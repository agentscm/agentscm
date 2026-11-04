//! Read-only bounded shared Hub status. Personal counts and recipients come from the same
//! projection as the Dashboard; a partial read never becomes an all-clear.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::time::Duration;

use reqwest::Method;
use serde_json::Value;

use crate::client_status::ConnectionStatus;
use crate::hub_cli::HubAuth;
use crate::hub_read_token::{
    MintedToken, TokenRequest, expected_provider, load_exact_session, mint_and_confirm,
    revoke_token,
};

use super::hub_open::OpenRequest;

const POLL_SECONDS: u64 = 60;
const CATALOG_POLL_SECONDS: u64 = 120;
const TOKEN_NAME: &str = "agentscm-tray-attention";

pub(super) struct Worker {
    command: std::sync::mpsc::Sender<WorkerCommand>,
    stopping: std::sync::Arc<std::sync::atomic::AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

enum WorkerCommand {
    Refresh,
    Stop,
}

impl Worker {
    /// Recheck Hub work when the panel opens, unless the last worker pass was very recent.
    pub(super) fn refresh(&self) {
        let _ = self.command.send(WorkerCommand::Refresh);
    }
}

impl Drop for Worker {
    fn drop(&mut self) {
        self.stopping
            .store(true, std::sync::atomic::Ordering::Release);
        let _ = self.command.send(WorkerCommand::Stop);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Signal {
    pub root: PathBuf,
    pub repository: String,
    pub text: String,
    pub keys: Vec<String>,
    pub available: bool,
    pub complete: bool,
    pub count: usize,
    pub work: Vec<Work>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Work {
    pub agent: String,
    pub title: String,
    pub state: String,
    pub next_action: String,
    pub claim_status: String,
    pub needs_you: bool,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(super) struct Catalog {
    pub repositories: Vec<CatalogRepository>,
    pub unavailable_hubs: usize,
    pub incomplete_hubs: usize,
    /// Keep previous notification baselines while these Hubs could not be checked completely.
    pub unverified_hubs: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct CatalogRepository {
    pub hub: String,
    pub repository: String,
    pub agents: usize,
    pub attention: usize,
    pub attention_revision: String,
    pub attention_complete: bool,
    pub complete: bool,
    pub work: Vec<Work>,
    pub live: Vec<LiveAgent>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct LiveAgent {
    pub agent: String,
    pub task: String,
    pub source: String,
}

struct CachedToken {
    hub: String,
    repository: String,
    generation: String,
    origin_host: String,
    provider: Option<String>,
    session: HubAuth,
    token: MintedToken,
}

pub(super) fn spawn(proxy: winit::event_loop::EventLoopProxy<super::UserEvent>) -> Worker {
    let (command, commands) = std::sync::mpsc::channel();
    let stopping = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let worker_stopping = stopping.clone();
    let thread = std::thread::spawn(move || {
        let Ok(runtime) = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
        else {
            return;
        };
        let mut cache = HashMap::<PathBuf, CachedToken>::new();
        let mut last_catalog_read: Option<std::time::Instant> = None;
        let Ok(client) = agentscm::hub::forge_http::client() else {
            return;
        };
        'worker: loop {
            if worker_stopping.load(std::sync::atomic::Ordering::Acquire) {
                break;
            }
            let signals = runtime.block_on(read(&client, &mut cache, &worker_stopping));
            if worker_stopping.load(std::sync::atomic::Ordering::Acquire) {
                break;
            }
            if proxy
                .send_event(super::UserEvent::CompanionHubSignals(signals))
                .is_err()
            {
                break;
            }
            if last_catalog_read.is_none_or(|at| at.elapsed().as_secs() >= CATALOG_POLL_SECONDS) {
                let catalog = runtime.block_on(read_catalog(&client, &worker_stopping));
                last_catalog_read = Some(std::time::Instant::now());
                if worker_stopping.load(std::sync::atomic::Ordering::Acquire)
                    || proxy
                        .send_event(super::UserEvent::CompanionHubCatalog(catalog))
                        .is_err()
                {
                    break;
                }
            }
            let last_pass = std::time::Instant::now();
            let next_poll = last_pass + Duration::from_secs(POLL_SECONDS);
            loop {
                let wait = next_poll.saturating_duration_since(std::time::Instant::now());
                match commands.recv_timeout(wait) {
                    Ok(WorkerCommand::Stop)
                    | Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break 'worker,
                    Ok(WorkerCommand::Refresh) => {
                        // Coalesce panel opens while a read is running; Stop always wins.
                        while let Ok(command) = commands.try_recv() {
                            if matches!(command, WorkerCommand::Stop) {
                                break 'worker;
                            }
                        }
                        if worker_stopping.load(std::sync::atomic::Ordering::Acquire) {
                            break 'worker;
                        }
                        if last_pass.elapsed() >= Duration::from_secs(20) {
                            last_catalog_read = None;
                            break;
                        }
                    }
                    Err(std::sync::mpsc::RecvTimeoutError::Timeout) => break,
                }
            }
        }
        let _ = runtime.block_on(async {
            tokio::time::timeout(Duration::from_secs(10), async {
                for (_, saved) in cache {
                    revoke_token(
                        &client,
                        &saved.hub,
                        Some(&saved.token.secret),
                        Some(&saved.token.id),
                        &saved.session,
                    )
                    .await;
                }
            })
            .await
        });
    });
    Worker {
        command,
        stopping,
        thread: Some(thread),
    }
}

async fn read_catalog(
    client: &reqwest::Client,
    stopping: &std::sync::atomic::AtomicBool,
) -> Catalog {
    let mut catalog = Catalog::default();
    for summary in crate::hub_cli::sign_in_summaries() {
        if stopping.load(std::sync::atomic::Ordering::Acquire) {
            break;
        }
        let Ok((hub, crate::hub_cli::HubAuth::Device(secret))) = load_exact_session(&summary.hub)
        else {
            catalog.unavailable_hubs += 1;
            catalog.unverified_hubs.push(summary.hub);
            continue;
        };
        let url = format!("{hub}/v1/hub/device/repositories");
        let Ok((200, reply)) =
            crate::hub_cli::call(client, Method::GET, &url, Some(&secret), None).await
        else {
            catalog.unavailable_hubs += 1;
            catalog.unverified_hubs.push(hub);
            continue;
        };
        let Some(repositories) = reply["repositories"].as_array() else {
            catalog.unavailable_hubs += 1;
            catalog.unverified_hubs.push(hub);
            continue;
        };
        let mut incomplete =
            reply["complete"] != true || reply["unchecked"].as_u64().unwrap_or(1) > 0;
        for row in repositories.iter().take(200) {
            let Some(repository) = row["repository"].as_str() else {
                continue;
            };
            let work = row["work"]
                .as_array()
                .map(|items| {
                    items
                        .iter()
                        .take(100)
                        .map(|item| Work {
                            agent: bounded_text(&item["agent"], 64, "Unknown agent"),
                            title: bounded_text(&item["title"], 160, "Untitled work"),
                            state: bounded_text(&item["state"], 32, "unknown"),
                            next_action: bounded_text(
                                &item["next_action"],
                                160,
                                "No next action reported",
                            ),
                            claim_status: bounded_text(&item["claim_status"], 32, "unknown"),
                            needs_you: item["needs_you"].as_bool().unwrap_or(false),
                        })
                        .collect()
                })
                .unwrap_or_default();
            let live = row["live"]
                .as_array()
                .map(|items| {
                    items
                        .iter()
                        .take(40)
                        .map(|item| LiveAgent {
                            agent: bounded_text(&item["agent"], 64, "Unknown agent"),
                            task: bounded_text(&item["task"], 160, "Task not reported"),
                            source: bounded_text(&item["source"], 16, "Hub"),
                        })
                        .collect()
                })
                .unwrap_or_default();
            let complete = row["complete"] == true
                && row["work_complete"] == true
                && row["live_complete"] == true;
            incomplete |= !complete;
            catalog.repositories.push(CatalogRepository {
                hub: hub.clone(),
                repository: repository
                    .chars()
                    .filter(|character| !character.is_control())
                    .take(255)
                    .collect(),
                agents: row["agents"].as_u64().unwrap_or(0).min(500) as usize,
                attention: row["needs_you"].as_u64().unwrap_or(0).min(500) as usize,
                attention_revision: bounded_text(&row["attention_revision"], 64, ""),
                attention_complete: row["complete"] == true,
                complete,
                work,
                live,
            });
        }
        if incomplete {
            catalog.incomplete_hubs += 1;
            catalog.unverified_hubs.push(hub);
        }
    }
    catalog
}

async fn read(
    client: &reqwest::Client,
    cache: &mut HashMap<PathBuf, CachedToken>,
    stopping: &std::sync::atomic::AtomicBool,
) -> Vec<Signal> {
    let Ok(snapshot) = crate::client_status::snapshot(false).await else {
        return Vec::new();
    };
    let connections = snapshot.connections;
    let current: HashSet<_> = connections
        .iter()
        .map(|connection| connection.root.clone())
        .collect();
    let stale: Vec<_> = cache
        .keys()
        .filter(|root| !current.contains(*root))
        .cloned()
        .collect();
    for root in stale {
        if let Some(saved) = cache.remove(&root) {
            revoke_token(
                client,
                &saved.hub,
                Some(&saved.token.secret),
                Some(&saved.token.id),
                &saved.session,
            )
            .await;
        }
    }
    let mut signals = Vec::new();
    for connection in connections {
        if stopping.load(std::sync::atomic::Ordering::Acquire) {
            break;
        }
        signals.push(read_one(client, cache, &connection).await);
    }
    signals
}

async fn read_one(
    client: &reqwest::Client,
    cache: &mut HashMap<PathBuf, CachedToken>,
    connection: &ConnectionStatus,
) -> Signal {
    let unavailable = || Signal {
        root: connection.root.clone(),
        repository: connection.repository.clone(),
        text: "Hub attention unavailable. Open Hub to check.".into(),
        keys: Vec::new(),
        available: false,
        complete: false,
        count: 0,
        work: Vec::new(),
    };
    if OpenRequest::from_status(connection).is_err() {
        if let Some(saved) = cache.remove(&connection.root) {
            revoke_token(
                client,
                &saved.hub,
                Some(&saved.token.secret),
                Some(&saved.token.id),
                &saved.session,
            )
            .await;
        }
        return unavailable();
    }
    let Ok((hub, session)) = load_exact_session(&connection.hub) else {
        if let Some(saved) = cache.remove(&connection.root) {
            revoke_token(
                client,
                &saved.hub,
                Some(&saved.token.secret),
                Some(&saved.token.id),
                &saved.session,
            )
            .await;
        }
        return unavailable();
    };
    let Ok(saved_connections) = crate::client_config::load() else {
        if let Some(saved) = cache.remove(&connection.root) {
            revoke_token(
                client,
                &saved.hub,
                Some(&saved.token.secret),
                Some(&saved.token.id),
                &saved.session,
            )
            .await;
        }
        return unavailable();
    };
    let Some(saved_connection) = saved_connections
        .into_iter()
        .find(|saved| saved.root == connection.root)
    else {
        if let Some(saved) = cache.remove(&connection.root) {
            revoke_token(
                client,
                &saved.hub,
                Some(&saved.token.secret),
                Some(&saved.token.id),
                &saved.session,
            )
            .await;
        }
        return unavailable();
    };
    let generation = connection
        .binding
        .activity_generation
        .as_deref()
        .unwrap_or("");
    let provider = expected_provider(&saved_connection.origin_host, &connection.hub_check);
    let now = agentscm::hub::store::unix_now();
    let reuse = cache.get(&connection.root).is_some_and(|saved| {
        saved.hub == hub
            && saved.repository == connection.repository
            && saved.generation == generation
            && saved.origin_host == saved_connection.origin_host
            && saved.provider == provider
            && saved.session == session
            && saved.token.expires_at > now + 90
    });
    if !reuse {
        if let Some(saved) = cache.remove(&connection.root) {
            revoke_token(
                client,
                &saved.hub,
                Some(&saved.token.secret),
                Some(&saved.token.id),
                &saved.session,
            )
            .await;
        }
        let Ok(token) = mint_and_confirm(
            client,
            &hub,
            &session,
            &TokenRequest {
                repository: &connection.repository,
                origin_host: &saved_connection.origin_host,
                provider: provider.as_deref(),
                name: TOKEN_NAME,
            },
            now,
        )
        .await
        else {
            return unavailable();
        };
        cache.insert(
            connection.root.clone(),
            CachedToken {
                hub: hub.clone(),
                repository: connection.repository.clone(),
                generation: generation.to_owned(),
                origin_host: saved_connection.origin_host,
                provider,
                session,
                token,
            },
        );
    }
    let Some(saved) = cache.get(&connection.root) else {
        return unavailable();
    };
    let status_url = format!("{hub}/v1/hub/standup/status");
    let reply = crate::hub_cli::call(
        client,
        Method::GET,
        &status_url,
        Some(&saved.token.secret),
        None,
    )
    .await;
    let unauthorized = matches!(&reply, Ok((401 | 403, _)));
    let Ok((200, status)) = reply else {
        if unauthorized && let Some(saved) = cache.remove(&connection.root) {
            revoke_token(
                client,
                &saved.hub,
                Some(&saved.token.secret),
                Some(&saved.token.id),
                &saved.session,
            )
            .await;
        }
        return unavailable();
    };
    summarize(&connection.root, &connection.repository, &status)
}

fn summarize(root: &std::path::Path, repository: &str, value: &Value) -> Signal {
    let unavailable = || Signal {
        root: root.to_path_buf(),
        repository: repository.into(),
        text: "Hub attention unavailable. Open Hub to check.".into(),
        keys: vec![],
        available: false,
        complete: false,
        count: 0,
        work: vec![],
    };
    let Ok(status) = serde_json::from_value::<
        agentscm::hub::work_http::actionable_status::ActionableStatus,
    >(value.clone()) else {
        return unavailable();
    };
    if status.repository != repository || status.work.len() > 100 || status.actions.len() > 1000 {
        return unavailable();
    }
    let count = status.work.iter().filter(|w| w.needs_you).count()
        + status.actions.iter().filter(|a| a.needs_you).count();
    if count != status.counts.needs_you {
        return unavailable();
    }
    let mut keys = status
        .work
        .iter()
        .filter(|w| w.needs_you)
        .map(|w| format!("work:{}", w.work_id))
        .chain(
            status
                .actions
                .iter()
                .filter(|a| a.needs_you)
                .map(|a| a.id.clone()),
        )
        .collect::<Vec<_>>();
    keys.sort();
    let work = status
        .work
        .iter()
        .map(|row| Work {
            agent: row
                .recipient
                .agent
                .clone()
                .or_else(|| row.operational.reservation.holder_agent.clone())
                .unwrap_or_else(|| "Unknown agent".into()),
            title: row
                .title
                .chars()
                .filter(|c| !c.is_control())
                .take(160)
                .collect(),
            state: serde_json::to_value(row.state)
                .ok()
                .and_then(|v| v.as_str().map(str::to_owned))
                .unwrap_or_else(|| "unknown".into()),
            next_action: row
                .action
                .chars()
                .filter(|c| !c.is_control())
                .take(160)
                .collect(),
            claim_status: format!("{:?}", row.freshness).to_ascii_lowercase(),
            needs_you: row.needs_you,
        })
        .collect();
    let text = if count == 0 {
        if status.complete {
            "No personal action observed.".into()
        } else {
            "No personal action observed in partial Hub coverage. Open Hub to check.".into()
        }
    } else {
        format!(
            "{repository} · {count} actions need you{}",
            if status.complete { "" } else { " (partial)" }
        )
    };
    Signal {
        root: root.to_path_buf(),
        repository: repository.into(),
        text,
        keys,
        available: true,
        complete: status.complete,
        count,
        work,
    }
}

fn bounded_text(value: &Value, max_chars: usize, fallback: &str) -> String {
    value
        .as_str()
        .map(|text| {
            text.chars()
                .filter(|character| !character.is_control())
                .take(max_chars)
                .collect::<String>()
        })
        .filter(|text| !text.trim().is_empty())
        .unwrap_or_else(|| fallback.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn shared_fixture_drives_counts_and_recipients() {
        let value =
            serde_json::from_str(include_str!("../hub/fixtures/actionable-status.json")).unwrap();
        let result = summarize(std::path::Path::new("/synthetic"), "acme/widgets", &value);
        assert!(result.available);
        assert_eq!(result.count, 3);
        assert_eq!(result.work.iter().filter(|w| w.needs_you).count(), 2);
        assert!(result.text.contains("partial"));
    }
    #[test]
    fn older_hub_response_stays_unavailable() {
        let result = summarize(
            std::path::Path::new("/synthetic"),
            "acme/widgets",
            &serde_json::json!({"current":[]}),
        );
        assert!(!result.available);
        assert!(!result.complete);
    }
}
