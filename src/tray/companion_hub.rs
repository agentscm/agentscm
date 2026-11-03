//! Read-only, bounded Hub signals for the tray. The count follows the Hub page's Needs you
//! (`ui/src/needs.ts`, `ui/src/derive.ts`) from the same reads: the work overview, the
//! coordination list, whoami, and the repository's work and activity. A read that failed
//! leaves its rules uncounted and the snapshot partial, never an all-clear.

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
/// A handoff request unanswered this long can no longer be granted, as in `derive.ts`.
const HANDOFF_LAPSE_SECONDS: i64 = 1800;

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
    let whoami_url = format!("{hub}/v1/hub/whoami");
    let repo_url = format!(
        "{hub}/v1/hub/repos/{}",
        agentscm::forge::encode_path_segment(&connection.repository)
    );
    let work_url = format!("{repo_url}/work");
    let activity_url = format!("{repo_url}/activity?limit=50");
    let secret = Some(saved.token.secret.as_str());
    let (overview_reply, coordination_reply, whoami, work, activity) = tokio::join!(
        crate::hub_cli::call(client, Method::GET, &overview_url, secret, None),
        crate::hub_cli::call(client, Method::GET, &coordination_url, secret, None),
        crate::hub_cli::call(client, Method::GET, &whoami_url, secret, None),
        crate::hub_cli::call(client, Method::GET, &work_url, secret, None),
        crate::hub_cli::call(client, Method::GET, &activity_url, secret, None),
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
    // Anything but a 200 reads as null: its rules count nothing and the snapshot is partial.
    let body = |reply: agentscm::Result<(u16, Value)>| match reply {
        Ok((200, value)) => value,
        _ => Value::Null,
    };
    summarize(
        &connection.root,
        &connection.repository,
        &Reads {
            overview: &overview,
            coordination: &coordination,
            whoami: &body(whoami),
            work: &body(work),
            activity: &body(activity),
            now: agentscm::hub::store::unix_now(),
        },
    )
}

/// What one repository's Hub reads returned. A read that failed is `Value::Null`.
struct Reads<'a> {
    overview: &'a Value,
    coordination: &'a Value,
    whoami: &'a Value,
    work: &'a Value,
    activity: &'a Value,
    now: i64,
}

fn summarize(root: &std::path::Path, repository: &str, reads: &Reads) -> Signal {
    let Reads {
        overview,
        coordination,
        whoami,
        work,
        activity,
        now,
    } = *reads;
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
    let mut blocked = Vec::new();
    let mut work_rows = Vec::new();
    for item in current {
        if item["state"].as_str() == Some("blocked") {
            blocked.push(format!("work:{}", item["id"].as_str().unwrap_or("")));
        }
        work_rows.push(Work {
            agent: bounded_text(&item["lead_agent"], 64, "Unknown agent"),
            title: bounded_text(&item["title"], 160, "Untitled work"),
            state: bounded_text(&item["state"], 32, "unknown"),
            next_action: bounded_text(&item["next_action"], 160, "No next action reported"),
            claim_status: bounded_text(&item["claim_status"], 32, "unknown"),
        });
    }
    let asked = requests_for(whoami["owner"]["login"].as_str(), current, requests);
    // An overlap needs a person only while both tasks are active or blocked on an active claim.
    let live = |side: &Value| {
        side["claim_status"].as_str() == Some("active")
            && matches!(side["state"].as_str(), Some("active" | "blocked"))
    };
    let overlaps: Vec<_> = overlaps
        .iter()
        .filter(|o| live(&o["left"]) && live(&o["right"]))
        .map(|o| {
            format!(
                "overlap:{}:{}",
                o["left"]["id"].as_str().unwrap_or(""),
                o["right"]["id"].as_str().unwrap_or("")
            )
        })
        .collect();
    let agents = live_agents(work, activity);
    let events = activity["events"].as_array().map_or(&[][..], Vec::as_slice);
    let contested = contested_for_you(&agents, events, now);
    let prs = pull_request_overlaps(work, &agents);
    let kinds = [
        (&blocked, "blocked"),
        (&overlaps, "overlap"),
        (&asked, "Inbox request"),
        (&contested, "contested file"),
        (&prs, "PR overlap"),
    ];
    let count = kinds.iter().map(|(keys, _)| keys.len()).sum();
    let mut keys: Vec<String> = kinds.iter().flat_map(|(keys, _)| keys.to_vec()).collect();
    keys.sort();
    let complete = overview["current_complete"] == true
        && overview["overlaps_complete"] == true
        && coordination["complete"] == true
        && whoami["owner"]["login"].is_string()
        && work["intents"].is_array()
        && work["intents_truncated"] == false
        && activity["sessions"].is_array()
        && activity["events"].is_array();
    let parts: Vec<_> = kinds
        .iter()
        .filter(|(keys, _)| !keys.is_empty())
        .map(|(keys, label)| format!("{} {label}", keys.len()))
        .collect();
    let text = if parts.is_empty() && complete {
        "No Work or Inbox signals.".into()
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
        work: work_rows,
    }
}

/// Open requests aimed at the viewer's own idle work (paused, or its claim not active) that its
/// lead has not answered at the target's current generation: `asked` in `needs.ts`.
fn requests_for(me: Option<&str>, current: &[Value], requests: &[Value]) -> Vec<String> {
    let Some(me) = me else {
        return Vec::new();
    };
    let mut keys = Vec::new();
    for request in requests {
        if matches!(request["status"].as_str(), Some("resolved" | "withdrawn")) {
            continue;
        }
        for target in request["targets"].as_array().into_iter().flatten() {
            let work_id = &target["work_id"];
            let idle = current
                .iter()
                .find(|i| i["id"] == *work_id)
                .is_some_and(|i| {
                    i["state"].as_str() == Some("paused")
                        || i["claim_status"].as_str() != Some("active")
                });
            let answered = request["responses"]
                .as_array()
                .into_iter()
                .flatten()
                .any(|r| {
                    r["target_work_id"] == *work_id
                        && r["target_generation"] == target["generation"]
                });
            if target["lead"]["login"].as_str() == Some(me) && idle && !answered {
                keys.push(format!(
                    "request:{}:{}",
                    request["id"].as_str().unwrap_or(""),
                    work_id.as_str().unwrap_or("")
                ));
            }
        }
    }
    keys
}

/// A live agent: one per session (joined to its intent), plus one per intent no session holds.
struct Agent {
    owner: String,
    token: String,
    /// The intent's `via`; `None` when the session has no intent.
    via: Option<Vec<String>>,
    specs: Vec<String>,
}

/// The Hub's `same_agent`: two intents of one token are one agent only when one is MCP-only.
fn same_agent(a: &Agent, b: &Agent) -> bool {
    let mcp_only = |via: &[String]| via == ["mcp"];
    a.token == b.token
        && matches!((&a.via, &b.via), (Some(x), Some(y)) if mcp_only(x) || mcp_only(y))
}

fn live_agents(work: &Value, activity: &Value) -> Vec<Agent> {
    let strings = |value: &Value| -> Vec<String> {
        value
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|s| s.as_str().map(str::to_owned))
            .collect()
    };
    let intents = work["intents"].as_array().map_or(&[][..], Vec::as_slice);
    let agent = |owner: &Value, token: &Value, intent: Option<&Value>| Agent {
        owner: owner["login"].as_str().unwrap_or("").to_owned(),
        token: token["token"].as_str().unwrap_or("").to_owned(),
        via: intent.map(|i| strings(&i["via"])),
        specs: intent.map_or_else(Vec::new, |i| {
            let mut specs = strings(&i["paths"]);
            specs.sort();
            specs.dedup();
            specs
        }),
    };
    let mut used = HashSet::new();
    let mut agents = Vec::new();
    for session in activity["sessions"].as_array().into_iter().flatten() {
        let intent = session["intent"]
            .as_str()
            .and_then(|id| intents.iter().find(|i| i["id"].as_str() == Some(id)));
        if let Some(id) = intent.and_then(|i| i["id"].as_str()) {
            used.insert(id);
        }
        agents.push(agent(&session["owner"], &session["agent"], intent));
    }
    for intent in intents {
        if !intent["id"].as_str().is_some_and(|id| used.contains(id)) {
            agents.push(agent(&intent["owner"], &intent["agent"], Some(intent)));
        }
    }
    agents
}

/// A path spec is an exact path or `dir/**`; its key drops the trailing `**`.
fn spec_key(spec: &str) -> &str {
    spec.strip_suffix("**").unwrap_or(spec)
}

fn overlaps(a: &str, b: &str) -> bool {
    let (a, b) = (spec_key(a), spec_key(b));
    a == b || (a.ends_with('/') && b.starts_with(a)) || (b.ends_with('/') && a.starts_with(b))
}

fn event_paths(event: &Value) -> impl Iterator<Item = &str> {
    event["data"]["paths"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
}

/// Contested files a person must settle (`contested` with `who: 'you'` in `needs.ts`): a file two
/// agents hold, or one an agent kept editing after a refusal, unless a handoff request for it is
/// still waiting for its answer, which the agents settle themselves.
fn contested_for_you(agents: &[Agent], events: &[Value], now: i64) -> Vec<String> {
    let mut held = std::collections::BTreeSet::new();
    for (i, a) in agents.iter().enumerate() {
        for b in &agents[i + 1..] {
            if same_agent(a, b) {
                continue;
            }
            for x in a.specs.iter().map(|s| spec_key(s)) {
                for y in b.specs.iter().map(|s| spec_key(s)) {
                    if overlaps(x, y) {
                        held.insert(if y.len() > x.len() { y } else { x }.to_owned());
                    }
                }
            }
        }
    }
    // Files with a refusal or handoff in the recent events count too, even when one agent holds them.
    let mut paths = held.clone();
    let mut story = std::collections::BTreeSet::new();
    for event in events {
        let kind = event["kind"].as_str().unwrap_or("");
        if kind != "edit_denied" && !kind.starts_with("handoff_") {
            continue;
        }
        for path in event_paths(event) {
            if !held.iter().chain(&story).any(|q| overlaps(path, q)) {
                story.insert(spec_key(path).to_owned());
            }
        }
    }
    paths.extend(story);
    let first = |kind: &str, path: &str| {
        events
            .iter()
            .find(|e| e["kind"].as_str() == Some(kind) && event_paths(e).any(|p| overlaps(p, path)))
    };
    paths
        .into_iter()
        .filter(|path| {
            let holders: HashSet<_> = agents
                .iter()
                .filter(|a| a.specs.iter().any(|s| overlaps(s, path)))
                .map(|a| (&a.owner, &a.token))
                .collect();
            let repeated = first("edit_denied", path).is_some_and(|refused| {
                events.iter().any(|e| {
                    e["kind"].as_str() == Some("edit_repeated")
                        && e["agent"]["token"] == refused["agent"]["token"]
                        && e["at"].as_i64() >= refused["at"].as_i64()
                        && event_paths(e).any(|p| overlaps(p, path))
                })
            });
            let waiting = first("handoff_request", path).is_some_and(|request| {
                let id = request["data"]["request"].as_str().unwrap_or("");
                let answered = !id.is_empty()
                    && events.iter().any(|e| {
                        matches!(
                            e["kind"].as_str(),
                            Some("handoff_grant" | "handoff_decline")
                        ) && e["data"]["request"].as_str() == Some(id)
                    });
                !answered && now - request["at"].as_i64().unwrap_or(0) <= HANDOFF_LAPSE_SECONDS
            });
            repeated || (holders.len() >= 2 && !waiting)
        })
        .map(|path| format!("contested:{path}"))
        .collect()
}

/// Open pull requests that touch files an agent holds now.
fn pull_request_overlaps(work: &Value, agents: &[Agent]) -> Vec<String> {
    work["pull_requests"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|pr| pr["state"].as_str() == Some("open"))
        .filter(|pr| {
            let files = pr["files"].as_array().into_iter().flatten();
            files.filter_map(Value::as_str).any(|file| {
                agents
                    .iter()
                    .any(|a| a.specs.iter().any(|spec| overlaps(file, spec)))
            })
        })
        .map(|pr| format!("pr:{}", pr["number"]))
        .collect()
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

    const NOW: i64 = 10_000;

    fn summarize_all(
        overview: &Value,
        coordination: &Value,
        whoami: &Value,
        work: &Value,
        activity: &Value,
    ) -> Signal {
        summarize(
            std::path::Path::new("/demo"),
            "example/repo",
            &Reads {
                overview,
                coordination,
                whoami,
                work,
                activity,
                now: NOW,
            },
        )
    }

    fn item(id: &str, state: &str, claim: &str) -> Value {
        json!({"id": id, "state": state, "claim_status": claim, "updated_at": 5})
    }

    fn lead(login: &str) -> Value {
        json!({"login": login})
    }

    fn request(id: &str, status: &str, work: &str, login: &str, generation: u64) -> Value {
        json!({
            "id": id, "status": status, "responses": [],
            "targets": [{"work_id": work, "lead": lead(login), "generation": generation}]
        })
    }

    fn intent(id: &str, owner: &str, token: &str, via: &[&str], paths: &[&str]) -> Value {
        json!({"id": id, "owner": lead(owner), "agent": {"token": token, "name": token},
               "via": via, "paths": paths})
    }

    fn event(kind: &str, token: &str, at: i64, path: &str, request: &str) -> Value {
        json!({"kind": kind, "at": at, "agent": {"token": token},
               "data": {"paths": [path], "request": request}})
    }

    /// One overview, read as the viewer `ana`, with the count `needsYou` in `ui/src/needs.ts` gives it.
    #[test]
    fn count_follows_the_hub_needs_you_rules() {
        let overview = json!({
            "current": [
                item("wrk_blocked", "blocked", "stale"),
                item("wrk_mine_paused", "paused", "stale"),
                item("wrk_mine_running", "active", "active"),
                item("wrk_theirs_paused", "paused", "active"),
                item("wrk_quiet", "active", "stale"),
                item("wrk_handover", "handover", "active")
            ],
            "overlaps": [
                {"left": item("wrk_a", "active", "active"), "right": item("wrk_b", "blocked", "active")},
                {"left": item("wrk_c", "active", "stale"), "right": item("wrk_d", "active", "active")},
                {"left": item("wrk_e", "paused", "active"), "right": item("wrk_f", "active", "active")}
            ],
            "current_complete": true, "overlaps_complete": true
        });
        let mut regenerated = request("crq_regenerated", "pending", "wrk_mine_paused", "ana", 2);
        regenerated["responses"] =
            json!([{"target_work_id": "wrk_mine_paused", "target_generation": 1}]);
        let mut answered = request("crq_answered", "responded", "wrk_mine_paused", "ana", 1);
        answered["responses"] =
            json!([{"target_work_id": "wrk_mine_paused", "target_generation": 1}]);
        let coordination = json!({"requests": [
            request("crq_waiting", "pending", "wrk_mine_paused", "ana", 1),
            regenerated,
            answered,
            request("crq_running", "pending", "wrk_mine_running", "ana", 1),
            request("crq_theirs", "pending", "wrk_theirs_paused", "bob", 1),
            request("crq_done", "resolved", "wrk_mine_paused", "ana", 1),
            request("crq_withdrawn", "withdrawn", "wrk_mine_paused", "ana", 1)
        ], "complete": true});
        let whoami = json!({"owner": lead("ana")});
        let work = json!({
            "intents": [
                intent("int_1", "ana", "tok_a", &["hook"], &["src/**", "docs/guide.md"]),
                intent("int_2", "bob", "tok_b", &["hook"], &["src/lib.rs"]),
                // One token, one side MCP-only: the same agent, so not contested.
                intent("int_3", "cy", "tok_c", &["mcp"], &["shared.rs"]),
                intent("int_4", "cy", "tok_c", &["hook"], &["shared.rs"]),
                // Two agents hold each of these; the handoff decides who settles it.
                intent("int_5", "dee", "tok_d", &["hook"], &["waiting/held.rs"]),
                intent("int_6", "eli", "tok_e", &["hook"], &["waiting/held.rs"]),
                intent("int_7", "dee", "tok_d", &["hook"], &["granted/held.rs"]),
                intent("int_8", "eli", "tok_e", &["hook"], &["granted/held.rs"]),
                intent("int_9", "dee", "tok_d", &["hook"], &["lapsed/held.rs"]),
                intent("int_10", "eli", "tok_e", &["hook"], &["lapsed/held.rs"])
            ],
            "intents_truncated": false,
            "pull_requests": [
                {"number": 7, "state": "open", "files": ["docs/guide.md"]},
                {"number": 8, "state": "open", "files": ["elsewhere.rs"]},
                {"number": 9, "state": "closed", "files": ["src/lib.rs"]}
            ]
        });
        let activity = json!({
            "sessions": [{"key": "s1", "agent": {"token": "tok_b"}, "owner": lead("bob"), "intent": "int_2"}],
            "events": [
                event("handoff_grant", "tok_e", NOW - 5, "granted/held.rs", "hnd_granted"),
                event("handoff_request", "tok_d", NOW - 10, "waiting/held.rs", "hnd_waiting"),
                event("handoff_request", "tok_d", NOW - 20, "granted/held.rs", "hnd_granted"),
                event("handoff_request", "tok_d", NOW - 3600, "lapsed/held.rs", "hnd_lapsed"),
                event("edit_repeated", "tok_x", 110, "story/kept.rs", ""),
                event("edit_denied", "tok_x", 100, "story/kept.rs", ""),
                event("edit_denied", "tok_y", 90, "story/once.rs", "")
            ]
        });
        let result = summarize_all(&overview, &coordination, &whoami, &work, &activity);
        let mut expected = vec![
            // Blocked work counts even when its claim is stale; paused, quiet and handed-over work does not.
            "work:wrk_blocked",
            // Live overlaps only: not one with a stale or paused side.
            "overlap:wrk_a:wrk_b",
            // Requests to the viewer's own idle work with no reply at its generation; not their running
            // task, someone else's, answered, resolved or withdrawn ones.
            "request:crq_waiting:wrk_mine_paused",
            "request:crq_regenerated:wrk_mine_paused",
            // Files two agents hold (once, by the more specific path) and one kept after a refusal,
            // not the same agent's two sessions, a refusal alone, or a handoff still waiting.
            "contested:src/lib.rs",
            "contested:granted/held.rs",
            "contested:lapsed/held.rs",
            "contested:story/kept.rs",
            // Open pull requests touching held files.
            "pr:7",
        ];
        expected.sort_unstable();
        assert_eq!(result.keys, expected);
        assert_eq!(result.count, 9);
        assert!(result.complete);
        assert_eq!(
            result.text,
            "example/repo · 1 blocked · 1 overlap · 2 Inbox request · 4 contested file · 1 PR overlap"
        );
        assert_eq!(result.work.len(), 6);
    }

    #[test]
    fn missing_reads_are_not_an_all_clear() {
        let overview = json!({"current": [item("wrk_blocked", "blocked", "active")],
            "overlaps": [], "current_complete": true, "overlaps_complete": true});
        let coordination = json!({"requests": [], "complete": true});
        let result = summarize_all(
            &overview,
            &coordination,
            &Value::Null,
            &Value::Null,
            &Value::Null,
        );
        // The overview's own rules still count; the rest need whoami, work and activity.
        assert_eq!(result.count, 1);
        assert!(!result.complete);
        assert!(result.text.contains("(partial)"));
        let quiet = json!({"current": [], "overlaps": [], "current_complete": true, "overlaps_complete": true});
        let result = summarize_all(
            &quiet,
            &coordination,
            &Value::Null,
            &Value::Null,
            &Value::Null,
        );
        assert_eq!(result.count, 0);
        assert!(result.text.contains("partial"));
    }

    #[test]
    fn incomplete_read_is_not_an_all_clear() {
        let overview =
            json!({"current":[],"overlaps":[],"current_complete":false,"overlaps_complete":true});
        let coordination = json!({"requests":[],"complete":true});
        let whoami = json!({"owner": lead("ana")});
        let work = json!({"intents": [], "intents_truncated": false, "pull_requests": []});
        let activity = json!({"sessions": [], "events": []});
        let result = summarize_all(&overview, &coordination, &whoami, &work, &activity);
        assert!(result.text.contains("partial"));
        assert_eq!(result.count, 0);
    }
}
