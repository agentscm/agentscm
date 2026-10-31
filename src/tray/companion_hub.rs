//! Read-only, bounded Hub signals for the tray. This uses the Hub's work overview and
//! coordination list, the same sources as the Dashboard and Inbox. Live intent and PR
//! crossings need the Hub page; a partial read is never presented as an all-clear.

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
    let overview_url = format!("{hub}/v1/hub/standup/overview?limit=100");
    let coordination_url = format!("{hub}/v1/hub/standup/coordination?active_only=true&limit=100");
    let (overview_reply, coordination_reply) = tokio::join!(
        crate::hub_cli::call(
            client,
            Method::GET,
            &overview_url,
            Some(&saved.token.secret),
            None
        ),
        crate::hub_cli::call(
            client,
            Method::GET,
            &coordination_url,
            Some(&saved.token.secret),
            None
        ),
    );
    let unauthorized = matches!(&overview_reply, Ok((401 | 403, _)))
        || matches!(&coordination_reply, Ok((401 | 403, _)));
    let (Ok((200, overview)), Ok((200, coordination))) = (overview_reply, coordination_reply)
    else {
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
    summarize(
        &connection.root,
        &connection.repository,
        &overview,
        &coordination,
    )
}

fn summarize(
    root: &std::path::Path,
    repository: &str,
    overview: &Value,
    coordination: &Value,
) -> Signal {
    let current = overview["current"].as_array();
    let overlaps = overview["overlaps"].as_array();
    let requests = coordination["requests"].as_array();
    let Some((current, overlaps, requests)) = current
        .zip(overlaps)
        .zip(requests)
        .map(|((a, b), c)| (a, b, c))
    else {
        return Signal {
            root: root.to_path_buf(),
            repository: repository.to_owned(),
            text: "Hub attention unavailable. Open Hub to check.".into(),
            keys: Vec::new(),
            available: false,
            complete: false,
            count: 0,
            work: Vec::new(),
        };
    };
    let mut blocked = 0;
    let mut quiet = 0;
    let mut handovers = 0;
    let mut keys = Vec::new();
    let mut work = Vec::new();
    for item in current {
        let id = item["id"].as_str().unwrap_or("");
        match item["state"].as_str() {
            Some("blocked") => {
                blocked += 1;
                keys.push(format!("work:{id}"));
            }
            _ if item["claim_status"].as_str() != Some("active") => {
                quiet += 1;
                keys.push(format!("work:{id}"));
            }
            Some("handover") => {
                handovers += 1;
                keys.push(format!("work:{id}"));
            }
            _ => {}
        }
        work.push(Work {
            agent: bounded_text(&item["lead_agent"], 64, "Unknown agent"),
            title: bounded_text(&item["title"], 160, "Untitled work"),
            state: bounded_text(&item["state"], 32, "unknown"),
            next_action: bounded_text(&item["next_action"], 160, "No next action reported"),
            claim_status: bounded_text(&item["claim_status"], 32, "unknown"),
        });
    }
    let open_requests: Vec<_> = requests
        .iter()
        .filter(|r| !matches!(r["status"].as_str(), Some("resolved" | "withdrawn")))
        .collect();
    for request in &open_requests {
        keys.push(format!("request:{}", request["id"].as_str().unwrap_or("")));
    }
    for overlap in overlaps {
        keys.push(format!(
            "overlap:{}:{}",
            overlap["left"]["id"].as_str().unwrap_or(""),
            overlap["right"]["id"].as_str().unwrap_or("")
        ));
    }
    keys.sort();
    let count = blocked + quiet + handovers + open_requests.len() + overlaps.len();
    let complete = overview["current_complete"] == true
        && overview["overlaps_complete"] == true
        && coordination["complete"] == true;
    let mut parts = Vec::new();
    if blocked > 0 {
        parts.push(format!("{blocked} blocked"));
    }
    if quiet > 0 {
        parts.push(format!("{quiet} quiet"));
    }
    if handovers > 0 {
        parts.push(format!("{handovers} handover"));
    }
    if !overlaps.is_empty() {
        parts.push(format!("{} overlap", overlaps.len()));
    }
    if !open_requests.is_empty() {
        parts.push(format!("{} Inbox request", open_requests.len()));
    }
    let text = if parts.is_empty() && complete {
        "No Work or Inbox signals. Open Hub for live file and PR crossings.".into()
    } else if parts.is_empty() {
        "No signal in this partial Hub snapshot. Open Hub to check.".into()
    } else if complete {
        format!("{} · {}", repository, parts.join(" · "))
    } else {
        format!("{} · {} (partial)", repository, parts.join(" · "))
    };
    Signal {
        root: root.to_path_buf(),
        repository: repository.to_owned(),
        text,
        keys,
        available: true,
        complete,
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
    use serde_json::json;

    #[test]
    fn hub_signals_match_work_and_inbox_categories() {
        let overview = json!({
            "current": [
                {"id":"wrk_blocked","state":"blocked","claim_status":"active","updated_at":4},
                {"id":"wrk_quiet","state":"active","claim_status":"stale","updated_at":5},
                {"id":"wrk_handover","state":"handover","claim_status":"active","updated_at":6}
            ],
            "overlaps": [{"left":{"id":"wrk_a"},"right":{"id":"wrk_b"}}],
            "current_complete": true, "overlaps_complete": true
        });
        let coordination = json!({"requests": [
            {"id":"crq_open","status":"responded","updated_at":7},
            {"id":"crq_done","status":"resolved","updated_at":8}
        ], "complete": true});
        let result = summarize(
            std::path::Path::new("/demo"),
            "example/repo",
            &overview,
            &coordination,
        );
        assert_eq!(result.count, 5);
        assert!(result.text.contains("1 blocked"));
        assert!(result.text.contains("1 quiet"));
        assert!(result.text.contains("1 handover"));
        assert!(result.text.contains("1 overlap"));
        assert!(result.text.contains("1 Inbox request"));
        assert_eq!(result.keys.len(), 5);
    }

    #[test]
    fn incomplete_read_is_not_an_all_clear() {
        let overview =
            json!({"current":[],"overlaps":[],"current_complete":false,"overlaps_complete":true});
        let coordination = json!({"requests":[],"complete":true});
        let result = summarize(
            std::path::Path::new("/demo"),
            "example/repo",
            &overview,
            &coordination,
        );
        assert!(result.text.contains("partial"));
        assert_eq!(result.count, 0);
    }
}
