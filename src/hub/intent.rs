//! Intents: an agent announces the files and task it is starting. Overlaps are
//! reported to the announcer and noticed to the holders of the overlapped
//! intents. Advisory only: an overlap never makes a request fail.
use std::collections::BTreeSet;
use std::time::Duration;

use axum::Router;
use axum::extract::rejection::QueryRejection;
use axum::extract::{Json, Path, Query, State};
use axum::routing::{delete, get, post};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::sync::{Notify, Semaphore};
use tokio::time::Instant;

use super::github::valid_ref_name;
use super::http::{ApiError, Authenticated, JsonBody, Reader, blocking};
use super::session::same_agent;
use super::store::{Event, HubEvent, Principal, User, new_id, unix_now, validate_repository};
use super::sync::overlapping_pulls;
use super::{HubError, HubState, Result};

pub const MAX_INTENT_TTL_SECONDS: i64 = 86_400;
pub(super) const MAX_PATHS: usize = 256;
const MAX_PATH_BYTES: usize = 1024;
pub(super) const MAX_TASK_BYTES: usize = 500;
const MAX_LABEL_BYTES: usize = 255;
pub(super) const MAX_LIVE_INTENTS: usize = 1000;
const NOTICES_PER_REPLY: usize = 100;
const MAX_ACTIVITY_SESSIONS: usize = 200;
const DEFAULT_ACTIVITY_EVENTS: usize = 50;
const MAX_ACTIVITY_EVENTS: usize = 100;
const MAX_WAIT_SECONDS: u64 = 25;
/// Other hub processes on the same database cannot notify this one.
const POLL_INTERVAL: Duration = Duration::from_secs(1);

/// Wakes long-polling notice requests in this process after an announce.
pub(super) static NOTICES: Notify = Notify::const_new();
/// Bounds long-polls held open per process; past it, a request answers at once.
static WAITERS: Semaphore = Semaphore::const_new(256);

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NewIntent {
    /// `int_` + 32 lowercase hex, chosen by the client so a retry is idempotent.
    pub id: String,
    pub task: String,
    pub branch: Option<String>,
    pub base: Option<String>,
    pub machine: Option<String>,
    pub paths: Vec<String>,
    pub ttl_seconds: i64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Intent {
    pub id: String,
    pub repository: String,
    pub task: String,
    pub branch: Option<String>,
    pub base: Option<String>,
    pub machine: Option<String>,
    /// Display specs, sorted.
    pub paths: Vec<String>,
    pub expires_at: i64,
    pub created_at: i64,
    /// Last write: an announce, a renewal or a release.
    pub updated_at: i64,
    pub token: String,
    /// The token's name.
    pub agent: String,
    pub owner: User,
    /// Connection modes of the session holding this intent (`mcp`, `hook:<agent>`); empty when
    /// it was announced through the REST API.
    pub via: Vec<String>,
}

/// A live intent of someone else that overlaps an announce; paths are display specs.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Overlap {
    pub intent: Intent,
    pub their_paths: Vec<String>,
    pub your_paths: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Announced {
    pub intent: Intent,
    pub overlaps: Vec<Overlap>,
}

impl NewIntent {
    /// Returns the normalised (sorted, deduplicated) index keys of `paths`.
    pub fn validate(&self) -> Result<Vec<String>> {
        let id_ok = self.id.strip_prefix("int_").is_some_and(|hex| {
            hex.len() == 32 && hex.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
        });
        if !id_ok {
            return Err(invalid("intent id must be int_ and 32 lowercase hex"));
        }
        text("task", &self.task, MAX_TASK_BYTES)?;
        for (name, value) in [
            ("branch", &self.branch),
            ("base", &self.base),
            ("machine", &self.machine),
        ] {
            if let Some(value) = value {
                text(name, value, MAX_LABEL_BYTES)?;
                // Branch names end up in forge URLs, so they must look like git refs.
                if name != "machine" && !valid_ref_name(value) {
                    return Err(HubError::Invalid(format!(
                        "{name} is not a valid git ref name"
                    )));
                }
            }
        }
        validate_ttl(self.ttl_seconds)?;
        if !(1..=MAX_PATHS).contains(&self.paths.len()) {
            return Err(invalid("an intent needs 1 to 256 paths"));
        }
        let mut keys = self
            .paths
            .iter()
            .map(|spec| spec_key(spec))
            .collect::<Result<Vec<_>>>()?;
        keys.sort();
        keys.dedup();
        Ok(keys)
    }
}

pub fn validate_ttl(ttl_seconds: i64) -> Result<()> {
    if !(1..=MAX_INTENT_TTL_SECONDS).contains(&ttl_seconds) {
        return Err(invalid("intent lifetime must be 1 second to 24 hours"));
    }
    Ok(())
}

fn invalid(message: &str) -> HubError {
    HubError::Invalid(message.to_owned())
}

pub(super) fn text(name: &str, value: &str, max: usize) -> Result<()> {
    if value.is_empty() || value.len() > max || value.chars().any(char::is_control) {
        return Err(HubError::Invalid(format!(
            "{name} must be 1-{max} bytes without control characters"
        )));
    }
    Ok(())
}

/// The index key of a path spec: an exact repo-relative path `a/b.rs`, or a
/// directory `a/b/**` (its descendants, not `a/b` itself) keyed `a/b/`.
pub fn spec_key(spec: &str) -> Result<String> {
    let bad = || invalid("path must be a repo-relative path, or a directory ending in /**");
    let (path, dir) = spec
        .strip_suffix("/**")
        .map_or((spec, false), |p| (p, true));
    let bytes = spec.as_bytes();
    let drive = bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':';
    if spec.is_empty()
        || spec.len() > MAX_PATH_BYTES
        || drive
        || spec.starts_with('/')
        || spec.contains('\\')
        || spec.chars().any(char::is_control)
        || path.contains('*')
        || path
            .split('/')
            .any(|segment| matches!(segment, "" | "." | ".."))
    {
        return Err(bad());
    }
    Ok(if dir {
        format!("{path}/")
    } else {
        path.to_owned()
    })
}

pub(super) fn display(key: &str) -> String {
    if key.ends_with('/') {
        format!("{key}**")
    } else {
        key.to_owned()
    }
}

pub(super) fn display_all<'a>(keys: impl IntoIterator<Item = &'a String>) -> Vec<String> {
    keys.into_iter().map(|key| display(key)).collect()
}

pub(super) fn key_of(display: &str) -> &str {
    display.strip_suffix("**").unwrap_or(display)
}

/// Whether two index keys name a common path.
pub(super) fn overlaps(a: &str, b: &str) -> bool {
    a == b || (a.ends_with('/') && b.starts_with(a)) || (b.ends_with('/') && a.starts_with(b))
}

/// Index probes that find every stored key overlapping `key`: stored keys that
/// equal one of the point keys (the ancestor directories, and `key` itself if
/// exact), or that fall in the byte range (`key` and everything under it).
fn probes(key: &str) -> (Vec<String>, Option<(String, String)>) {
    let mut points: Vec<String> = key
        .match_indices('/')
        .filter(|(i, _)| i + 1 < key.len())
        .map(|(i, _)| key[..=i].to_owned())
        .collect();
    if let Some(dir) = key.strip_suffix('/') {
        // '0' is '/' + 1, so the range is exactly the keys starting with `key`.
        let end = format!("{dir}0");
        (points, Some((key.to_owned(), end)))
    } else {
        points.push(key.to_owned());
        (points, None)
    }
}

/// The points to look up for all `keys` at once, and one range per directory key.
pub(super) fn probe_set(keys: &[String]) -> (Vec<String>, Vec<(String, String)>) {
    let mut points = BTreeSet::new();
    let mut ranges = Vec::new();
    for key in keys {
        let (p, range) = probes(key);
        points.extend(p);
        ranges.extend(range);
    }
    (points.into_iter().collect(), ranges)
}

/// An intent row as stored, before the announce is compared with it.
pub(super) struct Existing {
    pub token: String,
    pub released: bool,
    pub expires_at: i64,
    pub task: String,
    pub branch: Option<String>,
    pub base: Option<String>,
    pub machine: Option<String>,
    /// Canonical JSON array of display specs.
    pub paths: String,
}

pub(super) enum Plan {
    /// Identical and still live with the same expiry: nothing to write.
    Unchanged,
    /// Identical and still live: only the expiry moves.
    Renew,
    /// New or changed: write it, and notify overlaps with the keys not in `previous`.
    Write { previous: BTreeSet<String> },
}

pub(super) fn plan(
    existing: Option<&Existing>,
    token: &str,
    new: &NewIntent,
    paths_json: &str,
    expires_at: i64,
    now: i64,
) -> Result<Plan> {
    let Some(old) = existing else {
        return Ok(Plan::Write {
            previous: BTreeSet::new(),
        });
    };
    if old.token != token {
        return Err(invalid("intent id is in use"));
    }
    if old.released {
        return Err(invalid("intent was released"));
    }
    if old.expires_at <= now {
        return Ok(Plan::Write {
            previous: BTreeSet::new(),
        });
    }
    if old.task == new.task
        && old.branch == new.branch
        && old.base == new.base
        && old.machine == new.machine
        && old.paths == paths_json
    {
        return Ok(if old.expires_at == expires_at {
            Plan::Unchanged
        } else {
            Plan::Renew
        });
    }
    let previous: Vec<String> = serde_json::from_str(&old.paths)
        .map_err(|error| HubError::Unavailable(error.to_string()))?;
    Ok(Plan::Write {
        previous: previous
            .iter()
            .map(|spec| key_of(spec).to_owned())
            .collect(),
    })
}

/// The caller of a mutation, read in the same transaction that rechecks the token.
pub(super) struct Who {
    pub token: String,
    pub repository: String,
    pub agent: String,
    pub owner: User,
}

/// Notice data. Fields are in key order so the JSON stays canonical, like `json!`'s.
#[derive(Serialize)]
struct NoticeData<'a> {
    about: &'a str,
    agent: &'a str,
    branch: &'a Option<String>,
    machine: &'a Option<String>,
    owner: OwnerData<'a>,
    repository: &'a str,
    task: &'a str,
    their_paths: &'a [String],
    token: &'a str,
    your_paths: &'a [String],
}

#[derive(Serialize)]
struct OwnerData<'a> {
    host: &'a str,
    id: &'a str,
    login: &'a str,
    provider: &'a str,
    user_id: i64,
}

impl Event {
    pub(super) fn intent_announced(
        who: &Who,
        new: &NewIntent,
        paths: &[String],
        expires_at: i64,
    ) -> Result<Self> {
        let data = json!({"repository": who.repository, "token": who.token,
            "owner": who.owner.id, "agent": who.agent, "task": new.task, "branch": new.branch,
            "base": new.base, "machine": new.machine, "paths": paths, "expires_at": expires_at});
        Self::new("intent_announced", &new.id, data)
    }

    pub(super) fn intent_renewed(intent: &str, expires_at: i64) -> Result<Self> {
        Self::new("intent_renewed", intent, json!({"expires_at": expires_at}))
    }

    pub(super) fn intent_released(intent: &str) -> Result<Self> {
        Self::new("intent_released", intent, json!({}))
    }

    /// Serialized without a `Value` tree: a broad announce writes one per overlapped intent.
    fn intent_notice(who: &Who, new: &NewIntent, overlap: &Overlap) -> Result<Self> {
        let owner = &who.owner;
        let data = NoticeData {
            about: &new.id,
            agent: &who.agent,
            branch: &new.branch,
            machine: &new.machine,
            owner: OwnerData {
                host: &owner.identity.host,
                id: &owner.id,
                login: &owner.login,
                provider: &owner.identity.provider,
                user_id: owner.identity.user_id,
            },
            repository: &who.repository,
            task: &new.task,
            // The recipient's view: "their" is the announcer.
            their_paths: &overlap.your_paths,
            token: &who.token,
            your_paths: &overlap.their_paths,
        };
        Ok(Self {
            id: new_id("evt")?,
            at: unix_now(),
            kind: "intent_notice",
            subject: overlap.intent.id.clone(),
            data: serde_json::to_string(&data)
                .map_err(|error| HubError::Unavailable(error.to_string()))?,
        })
    }
}

/// One notice per overlapped intent that overlaps a key not in `previous`; `via` is the announcer's.
pub(super) fn notice_events(
    who: &Who,
    new: &NewIntent,
    via: &[String],
    previous: &BTreeSet<String>,
    announced: &Announced,
) -> Result<Vec<Event>> {
    announced
        .overlaps
        .iter()
        // Not a notice to one's own other connection.
        .filter(|overlap| !same_agent(&who.token, via, &overlap.intent))
        .filter(|overlap| {
            overlap
                .your_paths
                .iter()
                .any(|spec| !previous.contains(key_of(spec)))
        })
        .map(|overlap| Event::intent_notice(who, new, overlap))
        .collect()
}

/// Rows of `INTENT_SELECT` read by a backend, with the paths and via JSON still to parse.
pub(super) fn finish(rows: Vec<(Intent, String, Option<String>)>) -> Result<Vec<Intent>> {
    let parse = |json: &str| {
        serde_json::from_str(json).map_err(|error| HubError::Unavailable(error.to_string()))
    };
    rows.into_iter()
        .map(|(mut intent, paths, via)| {
            intent.paths = parse(&paths)?;
            intent.via = via.as_deref().map(parse).transpose()?.unwrap_or_default();
            Ok(intent)
        })
        .collect()
}

/// Column order read by both backends' row mappers; `paths` and `via` are last.
pub(super) const INTENT_SELECT: &str = "SELECT i.id, i.repository, i.task, i.branch, i.base,
        i.machine, i.expires_at, i.token, t.name, u.id, u.provider, u.host, u.forge_user_id,
        u.login, i.created_at, i.updated_at, i.paths, s.via
    FROM hub_intents i JOIN hub_agent_tokens t ON t.id = i.token JOIN hub_users u ON u.id = t.owner
    LEFT JOIN hub_sessions s ON s.intent = i.id";

/// Split the loaded intents into the announcer's own and the ones overlapping
/// `keys`, with the specs on each side that overlap, sorted by intent id.
pub(super) fn announced(own: &str, keys: &[String], intents: Vec<Intent>) -> Result<Announced> {
    let mut mine = None;
    let mut found = Vec::new();
    for intent in intents {
        if intent.id == own {
            mine = Some(intent);
            continue;
        }
        let their_paths: Vec<String> = intent
            .paths
            .iter()
            .filter(|spec| keys.iter().any(|key| overlaps(key, key_of(spec))))
            .cloned()
            .collect();
        let your_paths = keys
            .iter()
            .filter(|key| their_paths.iter().any(|spec| overlaps(key, key_of(spec))))
            .map(|key| display(key))
            .collect();
        found.push(Overlap {
            intent,
            their_paths,
            your_paths,
        });
    }
    found.sort_by(|a, b| a.intent.id.cmp(&b.intent.id));
    let intent = mine.ok_or_else(|| HubError::Unavailable("announced intent not found".into()))?;
    Ok(Announced {
        intent,
        overlaps: found,
    })
}

impl Intent {
    pub(super) fn to_json(&self) -> Value {
        json!({"id": self.id, "repository": self.repository, "task": self.task,
            "branch": self.branch, "base": self.base, "machine": self.machine,
            "paths": self.paths, "via": self.via, "expires_at": self.expires_at,
            "created_at": self.created_at, "updated_at": self.updated_at,
            "agent": {"token": self.token, "name": self.agent},
            "owner": {"id": self.owner.id, "provider": self.owner.identity.provider,
                "host": self.owner.identity.host, "user_id": self.owner.identity.user_id,
                "login": self.owner.login}})
    }
}

pub fn routes() -> Router<HubState> {
    Router::new()
        .route("/v1/hub/repos/{repo}/intents", post(announce).get(list))
        .route("/v1/hub/repos/{repo}/intents/{id}/renew", post(renew))
        .route("/v1/hub/repos/{repo}/intents/{id}", delete(release))
        .route("/v1/hub/repos/{repo}/notices", get(notices))
        .route("/v1/hub/repos/{repo}/activity", get(activity))
}

type ApiResult = std::result::Result<Json<Value>, ApiError>;

/// A token only sees its own repository; any other is a 404, so others are not revealed.
pub(super) fn scope(principal: &Principal, repo: &str) -> std::result::Result<(), ApiError> {
    if validate_repository(repo).is_err() || repo != principal.repository {
        return Err(ApiError::not_found());
    }
    Ok(())
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct AnnounceRequest {
    intent_id: String,
    task: String,
    branch: Option<String>,
    base: Option<String>,
    machine: Option<String>,
    paths: Vec<String>,
    ttl_s: i64,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RenewRequest {
    ttl_s: i64,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct NoticesQuery {
    after: Option<i64>,
    wait: Option<u64>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ActivityQuery {
    limit: Option<usize>,
}

async fn announce(
    State(state): State<HubState>,
    Authenticated(principal): Authenticated,
    Path(repo): Path<String>,
    JsonBody(request): JsonBody<AnnounceRequest>,
) -> ApiResult {
    scope(&principal, &repo)?;
    let intent = NewIntent {
        id: request.intent_id,
        task: request.task,
        branch: request.branch,
        base: request.base,
        machine: request.machine,
        paths: request.paths,
        ttl_seconds: request.ttl_s,
    };
    let store = state.store.clone();
    let announced = blocking(&state, move || {
        store.announce_intent(&principal.token_id, &intent, unix_now())
    })
    .await?;
    NOTICES.notify_waiters();
    let host = principal.owner.identity.host.clone();
    // The announce is committed; a failed look at pull requests must not fail it.
    let pull_requests = overlapping_pulls(
        &state,
        &host,
        &principal.repository,
        principal.repository_id,
        &announced.intent.paths,
    )
    .await
    .unwrap_or_else(|error| {
        eprintln!("hub: pull requests for an announce: {error}");
        Vec::new()
    });
    let overlaps: Vec<Value> = announced
        .overlaps
        .iter()
        .map(|o| {
            json!({"intent": o.intent.to_json(), "their_paths": o.their_paths,
                "your_paths": o.your_paths})
        })
        .collect();
    Ok(Json(
        json!({"intent": announced.intent.to_json(), "overlaps": overlaps,
            "pull_requests": pull_requests}),
    ))
}

async fn list(
    State(state): State<HubState>,
    Reader(principal): Reader,
    Path(repo): Path<String>,
) -> ApiResult {
    scope(&principal, &repo)?;
    let store = state.store.clone();
    let mut intents = blocking(&state, move || {
        store.live_intents(&repo, unix_now(), MAX_LIVE_INTENTS + 1)
    })
    .await?;
    let truncated = intents.len() > MAX_LIVE_INTENTS;
    intents.truncate(MAX_LIVE_INTENTS);
    let intents: Vec<Value> = intents.iter().map(Intent::to_json).collect();
    Ok(Json(json!({"intents": intents, "truncated": truncated})))
}

/// Live sessions and recent events of the repository, for the status page.
async fn activity(
    State(state): State<HubState>,
    Reader(principal): Reader,
    Path(repo): Path<String>,
    query: std::result::Result<Query<ActivityQuery>, QueryRejection>,
) -> ApiResult {
    scope(&principal, &repo)?;
    let Query(query) = query.map_err(|_| ApiError::invalid())?;
    let limit = query.limit.unwrap_or(DEFAULT_ACTIVITY_EVENTS);
    if !(1..=MAX_ACTIVITY_EVENTS).contains(&limit) {
        return Err(ApiError::invalid());
    }
    let store = state.store.clone();
    let (sessions, events) = blocking(&state, move || {
        let sessions = store.live_sessions(&principal, unix_now(), MAX_ACTIVITY_SESSIONS)?;
        Ok((sessions, store.repository_events(&principal, limit)?))
    })
    .await?;
    let parse = |json: &str| {
        serde_json::from_str::<Value>(json)
            .map_err(|error| HubError::Unavailable(error.to_string()))
    };
    let sessions = sessions
        .iter()
        .map(|s| {
            Ok(
                json!({"key": s.key, "agent": {"token": s.token, "name": s.agent},
                "owner": {"login": s.owner}, "intent": s.intent, "task": s.task,
                "via": parse(&s.via)?, "created_at": s.created_at, "updated_at": s.updated_at}),
            )
        })
        .collect::<Result<Vec<Value>>>()?;
    let events = events
        .iter()
        .map(|e| {
            let mut data = parse(&e.event.data)?;
            // Session ids stay out of the page, as in `sessions`.
            if let Some(fields) = data.as_object_mut() {
                fields.remove("session");
            }
            Ok(
                json!({"seq": e.event.seq, "id": e.event.id, "at": e.event.at,
                "kind": e.event.kind, "agent": {"token": e.token, "name": e.agent},
                "owner": {"login": e.owner}, "data": data}),
            )
        })
        .collect::<Result<Vec<Value>>>()?;
    Ok(Json(json!({"sessions": sessions, "events": events})))
}

async fn renew(
    State(state): State<HubState>,
    Authenticated(principal): Authenticated,
    Path((repo, id)): Path<(String, String)>,
    JsonBody(request): JsonBody<RenewRequest>,
) -> ApiResult {
    scope(&principal, &repo)?;
    let store = state.store.clone();
    let intent = blocking(&state, move || {
        store.renew_intent(&principal.token_id, &id, request.ttl_s, unix_now())
    })
    .await?
    .ok_or_else(ApiError::not_found)?;
    Ok(Json(json!({"intent": intent.to_json()})))
}

async fn release(
    State(state): State<HubState>,
    Authenticated(principal): Authenticated,
    Path((repo, id)): Path<(String, String)>,
) -> ApiResult {
    scope(&principal, &repo)?;
    let store = state.store.clone();
    let released = blocking(&state, move || {
        store.release_intent(&principal.token_id, &id, unix_now())
    })
    .await?;
    Ok(Json(json!({"released": released})))
}

async fn notices(
    State(state): State<HubState>,
    Authenticated(principal): Authenticated,
    Path(repo): Path<String>,
    query: std::result::Result<Query<NoticesQuery>, QueryRejection>,
) -> ApiResult {
    scope(&principal, &repo)?;
    let Query(query) = query.map_err(|_| ApiError::invalid())?;
    let after = query.after.unwrap_or(0);
    let wait = query.wait.unwrap_or(0);
    if after < 0 || wait > MAX_WAIT_SECONDS {
        return Err(ApiError::invalid());
    }
    let waiter = WAITERS.try_acquire().ok().filter(|_| wait > 0);
    let wait = if waiter.is_some() { wait } else { 0 };
    let deadline = Instant::now() + Duration::from_secs(wait);
    let events = loop {
        // Registered before the query so an announce in between is not missed.
        let notified = NOTICES.notified();
        tokio::pin!(notified);
        notified.as_mut().enable();
        let store = state.store.clone();
        let token = principal.token_id.clone();
        let found = blocking(&state, move || {
            store.notices_after(&token, after, NOTICES_PER_REPLY)
        })
        .await;
        let left = deadline.saturating_duration_since(Instant::now());
        // One announce wakes every waiter at once; a waiter that finds no free permit waits on.
        let events: Vec<HubEvent> = match found {
            Err(HubError::Busy) if !left.is_zero() => Vec::new(),
            found => found?,
        };
        if !events.is_empty() || left.is_zero() {
            break events;
        }
        let _ = tokio::time::timeout(left.min(POLL_INTERVAL), notified).await;
    };
    let cursor = events.last().map_or(after, |event| event.seq);
    let notices = events
        .iter()
        .map(|event| {
            let notice: Value = serde_json::from_str(&event.data)
                .map_err(|error| HubError::Unavailable(error.to_string()))?;
            Ok(
                json!({"seq": event.seq, "id": event.id, "at": event.at, "kind": event.kind,
                "intent": event.subject, "notice": notice}),
            )
        })
        .collect::<Result<Vec<Value>>>()?;
    Ok(Json(json!({"notices": notices, "cursor": cursor})))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spec_validation() {
        for (spec, key) in [
            ("a/b/c.rs", "a/b/c.rs"),
            ("a", "a"),
            ("a/b/**", "a/b/"),
            ("src/**", "src/"),
            ("a/.hidden/**", "a/.hidden/"),
            ("a/..b", "a/..b"),
        ] {
            assert_eq!(spec_key(spec).unwrap(), key, "{spec}");
            assert_eq!(display(&spec_key(spec).unwrap()), spec);
        }
        let long = "a".repeat(MAX_PATH_BYTES + 1);
        for spec in [
            "",
            "/",
            "/**",
            "/etc/passwd",
            "../x",
            "a/../b",
            "a/./b",
            ".",
            "a//b",
            "a/",
            "a\\b",
            "C:/x",
            "c:x",
            "a/**/b",
            "**",
            "*.rs",
            "a/*",
            "a/b**",
            "a/**/",
            "a/**/**",
            "a\0b",
            "a\nb",
            "a\u{7f}b",
            &long,
        ] {
            assert!(spec_key(spec).is_err(), "{spec:?}");
        }
        assert!(spec_key(&"a".repeat(MAX_PATH_BYTES)).is_ok());
        // Only a drive letter prefix is rejected.
        assert!(spec_key("1:/x").is_ok());
    }

    #[test]
    fn overlap_relation_table() {
        for (a, b, expected) in [
            ("src/a.rs", "src/a.rs", true),
            ("src/a.rs", "src/a.rs.bak", false),
            ("src/", "srcx/a", false),
            ("src/", "src/x/y", true),
            ("src/a/", "src/", true),
            ("src", "src/", false),
            ("a/b/", "a/bc/d", false),
        ] {
            assert_eq!(overlaps(a, b), expected, "{a} {b}");
            assert_eq!(overlaps(b, a), expected, "{b} {a}");
        }
    }

    #[test]
    fn probes_equal_the_relation() {
        let keys = [
            "a", "a/", "a/b", "a/b/", "a/b/c", "a/b/c/", "a/bc", "a/bc/", "a/b0", "a/b.", "a/b-",
            "ab", "ab/", "b/a", "b/", "a/b/c/d", "a/b/c0",
        ];
        for k in keys {
            let (points, range) = probes(k);
            for r in keys {
                let in_range = range
                    .as_ref()
                    .is_some_and(|(start, end)| r >= start.as_str() && r < end.as_str());
                let found = points.iter().any(|p| p == r) || in_range;
                assert_eq!(found, overlaps(k, r), "probe {k} vs stored {r}");
            }
        }
    }

    #[test]
    fn new_intent_validation() {
        let good = NewIntent {
            id: format!("int_{}", "a".repeat(32)),
            task: "t".into(),
            branch: None,
            base: None,
            machine: None,
            paths: vec!["b/**".into(), "a.rs".into(), "a.rs".into()],
            ttl_seconds: 60,
        };
        assert_eq!(good.validate().unwrap(), vec!["a.rs", "b/"]);
        let mut cases = Vec::new();
        for id in [
            "",
            "int_",
            "int_ABCDEF",
            &format!("int_{}", "a".repeat(31)),
            &format!("x_{}", "a".repeat(32)),
        ] {
            cases.push(NewIntent {
                id: id.to_owned(),
                ..good.clone()
            });
        }
        cases.push(NewIntent {
            task: String::new(),
            ..good.clone()
        });
        cases.push(NewIntent {
            task: "a".repeat(501),
            ..good.clone()
        });
        cases.push(NewIntent {
            task: "a\nb".into(),
            ..good.clone()
        });
        cases.push(NewIntent {
            branch: Some(String::new()),
            ..good.clone()
        });
        cases.push(NewIntent {
            base: Some("a".repeat(256)),
            ..good.clone()
        });
        cases.push(NewIntent {
            machine: Some("a\0".into()),
            ..good.clone()
        });
        cases.push(NewIntent {
            paths: vec![],
            ..good.clone()
        });
        cases.push(NewIntent {
            paths: (0..257).map(|i| format!("p{i}")).collect(),
            ..good.clone()
        });
        cases.push(NewIntent {
            ttl_seconds: 0,
            ..good.clone()
        });
        cases.push(NewIntent {
            ttl_seconds: MAX_INTENT_TTL_SECONDS + 1,
            ..good.clone()
        });
        for case in cases {
            assert!(case.validate().is_err(), "{case:?}");
        }
        let max = NewIntent {
            paths: (0..256).map(|i| format!("p{i}")).collect(),
            ..good
        };
        assert_eq!(max.validate().unwrap().len(), 256);
    }
}
