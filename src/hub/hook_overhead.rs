//! What AgentSCM's hooks cost a session, as counts: the hook calls that reached the Hub and the
//! characters the Hub put into the agent's context, plus what the hook client measured itself
//! (calls, latency, timeouts and fail-open answers), which it sends with its next call because a
//! call that timed out never reached the Hub. Nothing here is converted to time saved or money.
//!
//! Measuring must not cause the failures it measures, so a hook call never touches the store for
//! this: its sample is added to an in-memory total, and [`FLUSH_EVERY`] after the first call since
//! the last write one background write stores the totals of every session that called since.
//!
//! Only tokens the store has verified get a total of their own; a token not yet verified waits in
//! a small table until the next write checks it, so made-up tokens cannot crowd out real sessions.
//! Calls that find no room are counted as dropped, and while any are dropped or not yet written
//! the overhead read says it is partial. A client report carries an id, and the Hub counts each
//! id once per session, so a report the client sends again after a timeout is not added twice.
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use axum::http::HeaderMap;
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// The hook client's report of its calls since its last delivered report.
pub const REPORT_HEADER: &str = "x-agentscm-hook-report";
const MAX_REPORT_BYTES: usize = 256;
/// One report never claims more than this many calls, nor more than this latency per call.
const MAX_REPORT_CALLS: u64 = 10_000;
const MAX_CALL_MS: u64 = 600_000;
/// Rows older than this are deleted when a row is written.
pub const RETAIN_SECONDS: i64 = 30 * 86_400;
/// Rows kept per token. Session IDs come from the client, so a cap per repository would let one
/// token's fresh session IDs push out every other token's rows.
pub const MAX_ROWS_PER_TOKEN: i64 = 1_000;
/// Sessions listed one by one in value metrics; the rest are only in the totals.
pub const MAX_SESSIONS_LISTED: usize = 50;
pub const MAX_READ: usize = 20_000;
/// How long after the first call since the last write the totals are written.
pub const FLUSH_EVERY: Duration = Duration::from_secs(30);
/// Sessions of verified tokens held in memory between writes.
const MAX_PENDING: usize = 10_000;
/// Sessions of tokens not yet verified held in memory between writes.
const MAX_UNVERIFIED: usize = 256;
/// How long a verified token is trusted before the next write checks it again.
const VERIFIED_FOR: Duration = Duration::from_secs(600);
/// Report ids remembered per session, to count a resent report once.
const SEEN_PER_SESSION: usize = 16;

/// The client's own measurement of hook calls, as sent in [`REPORT_HEADER`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ClientReport {
    /// Chosen by the client once per report and kept when it sends the report again.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<u64>,
    pub calls: u64,
    pub ms_total: u64,
    pub ms_max: u64,
    pub timeouts: u64,
    pub failed_open: u64,
}

impl ClientReport {
    /// A bounded, self-consistent report, or None. A malformed header is ignored, never refused.
    pub fn from_headers(headers: &HeaderMap) -> Option<Self> {
        let text = headers.get(REPORT_HEADER)?.to_str().ok()?;
        if text.len() > MAX_REPORT_BYTES {
            return None;
        }
        let report: Self = serde_json::from_str(text).ok()?;
        // Each bound is checked before any sum or product uses the field, so nothing overflows.
        (report.calls <= MAX_REPORT_CALLS
            && report.timeouts <= report.calls
            && report.failed_open <= report.calls
            && report.timeouts + report.failed_open <= report.calls
            && report.ms_max <= MAX_CALL_MS
            && report.ms_total <= report.calls * MAX_CALL_MS
            && report.ms_max <= report.ms_total)
            .then_some(report)
    }

    fn add(&mut self, other: Self) {
        self.id = None;
        self.calls = self.calls.saturating_add(other.calls);
        self.ms_total = self.ms_total.saturating_add(other.ms_total);
        self.ms_max = self.ms_max.max(other.ms_max);
        self.timeouts = self.timeouts.saturating_add(other.timeouts);
        self.failed_open = self.failed_open.saturating_add(other.failed_open);
    }
}

/// Hook calls of one session as the Hub saw them, added to its session's row.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Sample {
    pub session: String,
    pub agent: String,
    pub hub_calls: u64,
    pub injected_start: u64,
    pub injected_refusal: u64,
    pub injected_other: u64,
    pub client: Option<ClientReport>,
}

impl Sample {
    fn add(&mut self, other: &Self) {
        self.hub_calls = self.hub_calls.saturating_add(other.hub_calls);
        self.injected_start = self.injected_start.saturating_add(other.injected_start);
        self.injected_refusal = self.injected_refusal.saturating_add(other.injected_refusal);
        self.injected_other = self.injected_other.saturating_add(other.injected_other);
        match (&mut self.client, other.client) {
            (Some(mine), Some(theirs)) => mine.add(theirs),
            (None, theirs) => self.client = theirs,
            _ => {}
        }
    }
}

/// The text fields of a rendered hook reply that reach the agent or its person.
const TEXT_FIELDS: [&str; 6] = [
    "additionalContext",
    "additional_context",
    "systemMessage",
    "permissionDecisionReason",
    "reason",
    "text",
];

/// Characters of text in a rendered hook reply, each distinct text counted once (a refusal
/// can repeat its reason in several fields for one host).
pub fn injected(rendered: &Value) -> u64 {
    fn collect<'a>(value: &'a Value, out: &mut Vec<&'a str>) {
        if let Value::Object(map) = value {
            for (key, value) in map {
                match value {
                    Value::String(text) if TEXT_FIELDS.contains(&key.as_str()) => out.push(text),
                    Value::Object(_) => collect(value, out),
                    _ => {}
                }
            }
        }
    }
    let mut texts = Vec::new();
    collect(rendered, &mut texts);
    texts.sort_unstable();
    texts.dedup();
    texts.into_iter().map(characters).sum()
}

/// One answered hook call, from the reply exactly as it is sent. A call with no session id
/// counts under `unknown`.
pub fn sample(
    session: Option<&str>,
    agent: &str,
    start: bool,
    refused: bool,
    rendered: &Value,
    headers: &HeaderMap,
) -> Sample {
    let text = injected(rendered);
    let (injected_start, injected_refusal, injected_other) = match (start, refused) {
        (true, _) => (text, 0, 0),
        (false, true) => (0, text, 0),
        (false, false) => (0, 0, text),
    };
    Sample {
        session: session.unwrap_or("unknown").to_owned(),
        agent: agent.to_owned(),
        hub_calls: 1,
        injected_start,
        injected_refusal,
        injected_other,
        client: ClientReport::from_headers(headers),
    }
}

/// A session's totals waiting to be written: which store, which token (digest) and repository
/// the call named, and the session and agent.
type Key = (usize, String, Option<String>, String, String);
/// A token as the store verified it: store, digest and the repository the call named.
type Token = (usize, String, Option<String>);

/// A session's calls not written yet, and when the first of them came (Unix seconds).
#[derive(Clone)]
struct Waiting {
    sample: Sample,
    first_at: i64,
}

/// The report ids last counted for a session, and when it last sent one.
struct Ids {
    ids: VecDeque<u64>,
    at: Instant,
}

#[derive(Default)]
struct Pending {
    /// Totals of sessions whose token the store verified.
    totals: BTreeMap<Key, Waiting>,
    /// Totals of sessions whose token is not verified yet; the next write checks them.
    unverified: BTreeMap<Key, Waiting>,
    /// When the store last accepted each token, and the repository the token belongs to.
    verified: BTreeMap<Token, (Instant, String)>,
    /// Report ids already counted, per session, apart for verified and unverified tokens so
    /// made-up tokens cannot push out real sessions' ids.
    seen: BTreeMap<Key, Ids>,
    seen_unverified: BTreeMap<Key, Ids>,
    /// When a call was last dropped for lack of room, per repository ("" when not known).
    dropped: BTreeMap<String, i64>,
    /// Stores with a write scheduled.
    scheduled: BTreeSet<usize>,
    /// Totals a running write took out of the tables, by write: their repository (None when not
    /// known) and first call. They are in neither the tables nor the store until it ends.
    writing: BTreeMap<u64, Vec<(Option<String>, i64)>>,
    next_write: u64,
}

impl Pending {
    /// The repository a waiting total counts for: the token's own once verified, else the one
    /// the call named; None when neither is known.
    fn repository<'a>(&'a self, key: &'a Key) -> Option<&'a str> {
        self.verified
            .get(&(key.0, key.1.clone(), key.2.clone()))
            .map(|(_, repository)| repository.as_str())
            .or(key.2.as_deref())
    }

    /// Adds `sample` to the total at `key` in the verified or unverified table, or counts it as
    /// dropped when that table is full.
    fn add(&mut self, verified: bool, key: Key, sample: &Sample, first_at: i64) {
        let repository = self.repository(&key).unwrap_or_default().to_owned();
        let (table, limit) = if verified {
            (&mut self.totals, MAX_PENDING)
        } else {
            (&mut self.unverified, MAX_UNVERIFIED)
        };
        let held = table.len();
        match table.get_mut(&key) {
            Some(total) => {
                total.sample.add(sample);
                total.first_at = total.first_at.min(first_at);
            }
            None if held < limit => {
                table.insert(
                    key,
                    Waiting {
                        sample: sample.clone(),
                        first_at,
                    },
                );
            }
            None => {
                // Repository names of unverified calls come from the client, so they are bounded.
                let repository = if self.dropped.len() < MAX_UNVERIFIED || verified {
                    repository
                } else {
                    String::new()
                };
                self.dropped.insert(repository, super::store::unix_now());
            }
        }
    }
}

/// Whether `table` already counted report `id` for `key`; records it if not. A full table
/// forgets the session that sent a report longest ago, never every session at once.
fn counted_before(table: &mut BTreeMap<Key, Ids>, limit: usize, key: &Key, id: u64) -> bool {
    if !table.contains_key(key) && table.len() >= limit {
        let oldest = table
            .iter()
            .min_by_key(|(_, ids)| ids.at)
            .map(|(key, _)| key.clone());
        if let Some(oldest) = oldest {
            table.remove(&oldest);
        }
    }
    let entry = table.entry(key.clone()).or_insert_with(|| Ids {
        ids: VecDeque::new(),
        at: Instant::now(),
    });
    entry.at = Instant::now();
    if entry.ids.contains(&id) {
        return true;
    }
    if entry.ids.len() == SEEN_PER_SESSION {
        entry.ids.pop_front();
    }
    entry.ids.push_back(id);
    false
}

static PENDING: Mutex<Option<Pending>> = Mutex::new(None);

fn pending() -> std::sync::MutexGuard<'static, Option<Pending>> {
    PENDING.lock().unwrap_or_else(|p| p.into_inner())
}

fn store_key(state: &super::HubState) -> usize {
    std::sync::Arc::as_ptr(&state.store).cast::<()>() as usize
}

/// Whether the store holds every hook call this process saw for `repository` in the window from
/// `since` to `until`: none dropped in it, and none waiting longer than a write takes. Calls of
/// the last [`FLUSH_EVERY`] or two are on their way and do not make a read partial.
// shortcut: scans every waiting total under the lock hook calls take (microseconds at 10,000);
// keep an index by repository if it shows in hook latency.
pub fn all_written(repository: &str, since: i64, until: i64) -> bool {
    let overdue = super::store::unix_now() - 2 * FLUSH_EVERY.as_secs() as i64;
    let guard = pending();
    let Some(pending) = guard.as_ref() else {
        return true;
    };
    let dropped = [repository, ""]
        .iter()
        .filter_map(|name| pending.dropped.get(*name))
        .any(|at| *at >= since);
    let waiting = pending
        .totals
        .iter()
        .chain(&pending.unverified)
        .any(|(key, waiting)| {
            pending.repository(key).is_none_or(|r| r == repository)
                && waiting.first_at <= until
                && waiting.first_at < overdue
        });
    let writing =
        pending.writing.values().flatten().any(|(r, first_at)| {
            r.as_deref().is_none_or(|r| r == repository) && *first_at <= until
        });
    !dropped && !waiting && !writing
}

/// Adds the call to its session's in-memory total; no store call. The first call since the last
/// write schedules the next one [`FLUSH_EVERY`] later. A call without a usable token is not
/// counted, and a client report whose id this session already sent is not counted again.
pub fn record(
    state: &super::HubState,
    token: Option<&str>,
    repo: Option<String>,
    mut sample: Sample,
) {
    let Some(digest) = token.and_then(|token| super::store::agent_token_digest(token).ok()) else {
        return;
    };
    let store = store_key(state);
    let digest = digest.as_str().to_owned();
    let token: Token = (store, digest.clone(), repo.clone());
    let key: Key = (
        store,
        digest,
        repo,
        sample.session.clone(),
        sample.agent.clone(),
    );
    let schedule = {
        let mut guard = pending();
        let pending = guard.get_or_insert_with(Pending::default);
        let verified = pending
            .verified
            .get(&token)
            .is_some_and(|(at, _)| at.elapsed() < VERIFIED_FOR);
        if let Some(id) = sample.client.and_then(|client| client.id) {
            let other = if verified {
                &pending.seen_unverified
            } else {
                &pending.seen
            };
            let before = other.get(&key).is_some_and(|ids| ids.ids.contains(&id))
                || if verified {
                    counted_before(&mut pending.seen, MAX_PENDING, &key, id)
                } else {
                    counted_before(&mut pending.seen_unverified, MAX_UNVERIFIED, &key, id)
                };
            if before {
                sample.client = None;
            }
        }
        pending.add(verified, key, &sample, super::store::unix_now());
        pending.scheduled.insert(store)
    };
    if schedule && !schedule_flush(state) {
        pending()
            .get_or_insert_with(Pending::default)
            .scheduled
            .remove(&store);
    }
}

/// Writes this store's totals [`FLUSH_EVERY`] from now; false when there is no runtime to run
/// the write. A plain function, so `flush` can schedule itself without checking its own
/// future's `Send` from inside its definition.
fn schedule_flush(state: &super::HubState) -> bool {
    let Ok(runtime) = tokio::runtime::Handle::try_current() else {
        return false;
    };
    let state = state.clone();
    runtime.spawn(async move {
        tokio::time::sleep(FLUSH_EVERY).await;
        flush(&state).await;
    });
    true
}

/// Writes every waiting total of this Hub's store, each session in its own store transaction,
/// in one background store call. A token the store does not accept loses its totals. A total
/// whose write fails is kept for the next write and the rest are still written; after two
/// failures in a row the store is taken to be unavailable and every total not written is kept.
pub async fn flush(state: &super::HubState) {
    let store_id = store_key(state);
    let (write, taken): (u64, Vec<(Key, Waiting)>) = {
        let mut guard = pending();
        let Some(pending) = guard.as_mut() else {
            return;
        };
        pending.scheduled.remove(&store_id);
        let mut taken = Vec::new();
        for table in [&mut pending.totals, &mut pending.unverified] {
            let (mine, rest) = std::mem::take(table)
                .into_iter()
                .partition(|((store, ..), _)| *store == store_id);
            *table = rest;
            taken.extend::<Vec<_>>(mine.into_iter().collect());
        }
        if taken.is_empty() {
            return;
        }
        let write = pending.next_write;
        pending.next_write += 1;
        let marks = taken
            .iter()
            .map(|(key, waiting)| (pending.repository(key).map(str::to_owned), waiting.first_at))
            .collect();
        pending.writing.insert(write, marks);
        (write, taken)
    };
    let store = state.store.clone();
    let writing: Vec<(Key, Sample)> = taken
        .iter()
        .map(|(key, waiting)| (key.clone(), waiting.sample.clone()))
        .collect();
    type Checked = BTreeMap<(String, Option<String>), Option<super::store::Principal>>;
    fn accepted(checked: Checked) -> BTreeMap<(String, Option<String>), String> {
        checked
            .into_iter()
            .filter_map(|(token, principal)| principal.map(|p| (token, p.repository)))
            .collect()
    }
    // (tokens the store accepted with their repository, indexes of the totals not written)
    let written = super::http::blocking(state, move || {
        let now = super::store::unix_now();
        // Each token is checked once per write; None is a token the store does not accept.
        let mut checked = Checked::new();
        let mut failed = Vec::new();
        let mut in_a_row = 0;
        for (at, ((_, digest, repo, ..), sample)) in writing.iter().enumerate() {
            if in_a_row == 2 {
                failed.extend(at..writing.len());
                break;
            }
            let token = (digest.clone(), repo.clone());
            if !checked.contains_key(&token) {
                let Ok(parsed) = super::store::TokenDigest::from_persisted(digest) else {
                    continue;
                };
                let Ok(found) = store.authenticate(&parsed, now) else {
                    failed.push(at);
                    in_a_row += 1;
                    continue;
                };
                let found = found
                    .filter(|principal| !principal.read_only)
                    .filter(|principal| repo.as_ref().is_none_or(|r| *r == principal.repository));
                checked.insert(token.clone(), found);
            }
            let Some(Some(principal)) = checked.get(&token) else {
                continue;
            };
            if store
                .hook_overhead(&principal.token_id, &principal.repository, sample, now)
                .is_err()
            {
                failed.push(at);
                in_a_row += 1;
            } else {
                in_a_row = 0;
            }
        }
        Ok((accepted(checked), failed))
    })
    .await;
    // None: the write task failed part-way, so which totals it stored is not known.
    let outcome = match written {
        Ok(written) => Some(written),
        // The store call never ran (no free permit): nothing was written.
        Err(super::HubError::Busy) => Some((BTreeMap::new(), (0..taken.len()).collect())),
        Err(_) => None,
    };
    let again = {
        let mut guard = pending();
        let pending = guard.get_or_insert_with(Pending::default);
        pending.writing.remove(&write);
        let Some((accepted, failed)) = outcome else {
            // Putting them back could store some twice; count them as dropped instead, so
            // reads of their repositories say partial.
            let at = super::store::unix_now();
            for (key, _) in &taken {
                let repository = pending.repository(key).unwrap_or_default().to_owned();
                pending.dropped.insert(repository, at);
            }
            return;
        };
        let left = !failed.is_empty();
        let now = Instant::now();
        for ((digest, repo), repository) in accepted {
            // Report ids this token's sessions sent before it was verified move with them, so
            // unverified callers cannot push them out any more.
            let moved: Vec<Key> = pending
                .seen_unverified
                .keys()
                .filter(|key| key.0 == store_id && key.1 == digest && key.2 == repo)
                .cloned()
                .collect();
            for key in moved {
                if pending.seen.len() >= MAX_PENDING {
                    break;
                }
                if let Some(ids) = pending.seen_unverified.remove(&key) {
                    pending.seen.entry(key).or_insert(ids);
                }
            }
            pending
                .verified
                .insert((store_id, digest, repo), (now, repository));
        }
        let failed: BTreeSet<usize> = failed.into_iter().collect();
        for (at, (key, waiting)) in taken.into_iter().enumerate() {
            if failed.contains(&at) {
                let token = (key.0, key.1.clone(), key.2.clone());
                let verified = pending.verified.contains_key(&token);
                pending.add(verified, key, &waiting.sample, waiting.first_at);
            }
        }
        // Totals a failed write put back are retried by the timer, not left for the next call.
        (left || !pending.unverified.is_empty()) && pending.scheduled.insert(store_id)
    };
    if again && !schedule_flush(state) {
        pending()
            .get_or_insert_with(Pending::default)
            .scheduled
            .remove(&store_id);
    }
}

/// Characters as the agent's host counts them (Claude Code limits its context in UTF-16 units).
pub fn characters(text: &str) -> u64 {
    text.encode_utf16().count() as u64
}

/// Whether a window's session rows are all there (see `refusal_outcomes::complete`).
pub fn complete(matched: usize, since: i64, now: i64, at_cap: bool) -> bool {
    matched <= MAX_READ && since >= now - RETAIN_SECONDS && !at_cap
}

/// A session's stored row, as read for value metrics.
#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize, Serialize)]
pub struct SessionOverhead {
    /// Never serialized: value metrics return no session or token identities.
    #[serde(skip)]
    pub session: String,
    pub agent: String,
    pub hub_calls: u64,
    pub injected_start: u64,
    pub injected_refusal: u64,
    pub injected_other: u64,
    /// Calls the client measured and reported; 0 when this client never reported.
    pub client_calls: u64,
    pub client_ms_total: u64,
    pub client_ms_max: u64,
    pub client_timeouts: u64,
    pub client_failed_open: u64,
    pub first_at: i64,
    pub updated_at: i64,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize, Serialize)]
pub struct Injected {
    pub session_start: u64,
    pub refusal: u64,
    pub other: u64,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize, Serialize)]
pub struct ClientMeasured {
    /// Sessions whose hook client sent at least one report.
    pub reporting_sessions: usize,
    pub calls: u64,
    pub latency_ms_total: u64,
    pub latency_ms_max: u64,
    pub timed_out: u64,
    pub failed_open: u64,
}

/// Hook overhead for the sessions active in the window. `availability` is `observed`,
/// `none_observed`, or `not_scoped_to_work` (rows are per session, not per Work).
#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
pub struct OverheadMetrics {
    pub availability: String,
    pub sessions: usize,
    pub hub_calls: u64,
    pub injected_characters: Injected,
    pub client: ClientMeasured,
    /// Sessions with Hub calls but no client report: an older client, or one that never got a
    /// call through after a measured one. Their latency, timeouts and fail-open are unknown.
    pub sessions_without_client_report: usize,
    pub per_session: Vec<SessionOverhead>,
    pub per_session_omitted: usize,
    /// More rows matched than were read.
    pub partial: bool,
}

impl Default for OverheadMetrics {
    fn default() -> Self {
        Self {
            availability: "none_observed".into(),
            sessions: 0,
            hub_calls: 0,
            injected_characters: Injected::default(),
            client: ClientMeasured::default(),
            sessions_without_client_report: 0,
            per_session: Vec::new(),
            per_session_omitted: 0,
            partial: false,
        }
    }
}

impl OverheadMetrics {
    pub fn add(&mut self, rows: &[SessionOverhead]) {
        for row in rows {
            self.sessions += 1;
            self.hub_calls = self.hub_calls.saturating_add(row.hub_calls);
            let injected = &mut self.injected_characters;
            injected.session_start = injected.session_start.saturating_add(row.injected_start);
            injected.refusal = injected.refusal.saturating_add(row.injected_refusal);
            injected.other = injected.other.saturating_add(row.injected_other);
            if row.client_calls > 0 {
                let client = &mut self.client;
                client.reporting_sessions += 1;
                client.calls = client.calls.saturating_add(row.client_calls);
                client.latency_ms_total =
                    client.latency_ms_total.saturating_add(row.client_ms_total);
                client.latency_ms_max = client.latency_ms_max.max(row.client_ms_max);
                client.timed_out = client.timed_out.saturating_add(row.client_timeouts);
                client.failed_open = client.failed_open.saturating_add(row.client_failed_open);
            } else {
                self.sessions_without_client_report += 1;
            }
            if self.per_session.len() < MAX_SESSIONS_LISTED {
                self.per_session.push(row.clone());
            } else {
                self.per_session_omitted += 1;
            }
        }
        if self.sessions > 0 {
            self.availability = "observed".into();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn headers(value: &str) -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert(REPORT_HEADER, value.parse().unwrap());
        headers
    }

    #[test]
    fn a_report_is_read_only_when_bounded_and_consistent() {
        let ok = r#"{"calls":61,"ms_total":250000,"ms_max":5000,"timeouts":48,"failed_open":2}"#;
        assert_eq!(
            ClientReport::from_headers(&headers(ok)),
            Some(ClientReport {
                id: None,
                calls: 61,
                ms_total: 250_000,
                ms_max: 5000,
                timeouts: 48,
                failed_open: 2
            })
        );
        for bad in [
            r#"{"calls":1,"ms_total":5,"ms_max":5,"timeouts":1,"failed_open":1}"#,
            r#"{"calls":1,"ms_total":5,"ms_max":6,"timeouts":0,"failed_open":0}"#,
            r#"{"calls":1,"ms_total":5,"ms_max":5,"timeouts":0,"failed_open":0,"x":1}"#,
            r#"{"calls":20000,"ms_total":0,"ms_max":0,"timeouts":0,"failed_open":0}"#,
            // Values that would wrap if added before being bounded.
            r#"{"calls":1,"ms_total":0,"ms_max":0,"timeouts":18446744073709551615,"failed_open":2}"#,
            r#"{"calls":18446744073709551615,"ms_total":18446744073709551615,"ms_max":0,"timeouts":0,"failed_open":0}"#,
            "not json",
        ] {
            assert_eq!(ClientReport::from_headers(&headers(bad)), None, "{bad}");
        }
        assert_eq!(ClientReport::from_headers(&HeaderMap::new()), None);
    }

    #[test]
    fn injected_text_is_counted_from_the_reply_as_sent_each_text_once() {
        // A start reply carries the rendered context, not the Hub's raw text.
        let start = serde_json::json!({"hookSpecificOutput": {"hookEventName": "SessionStart",
            "additionalContext": "rules\nsession"}});
        assert_eq!(injected(&start), 13);
        // Copilot repeats a refusal reason in three fields; the agent reads it once.
        let deny = serde_json::json!({"hookSpecificOutput": {"permissionDecision": "deny",
            "permissionDecisionReason": "held", "additionalContext": "held"},
            "permissionDecision": "deny", "permissionDecisionReason": "held"});
        assert_eq!(injected(&deny), 4);
        // An allow answer sends no text.
        assert_eq!(injected(&serde_json::json!({"permission": "allow"})), 0);
        let call = sample(Some("s"), "copilot", false, true, &deny, &HeaderMap::new());
        assert_eq!((call.injected_refusal, call.injected_other), (4, 0));
    }

    #[test]
    fn sums_saturate_instead_of_wrapping() {
        let mut total = Sample {
            hub_calls: u64::MAX,
            client: Some(ClientReport {
                calls: u64::MAX,
                ..Default::default()
            }),
            ..Default::default()
        };
        total.add(&Sample {
            hub_calls: 1,
            client: Some(ClientReport {
                calls: 1,
                ..Default::default()
            }),
            ..Default::default()
        });
        assert_eq!(total.hub_calls, u64::MAX);
        assert_eq!(total.client.unwrap().calls, u64::MAX);
        let mut metrics = OverheadMetrics::default();
        let huge = SessionOverhead {
            hub_calls: u64::MAX,
            ..Default::default()
        };
        metrics.add(&[huge.clone(), huge]);
        assert_eq!(metrics.hub_calls, u64::MAX);
    }

    fn hub() -> (tempfile::TempDir, super::super::HubState) {
        let dir = tempfile::tempdir().unwrap();
        let store = super::super::store::open(&format!(
            "sqlite://{}",
            dir.path().join("hub.sqlite").display()
        ))
        .unwrap();
        let objects = crate::backend::LocalBackend::open(dir.path().join("objects")).unwrap();
        let state = super::super::HubState::new(store, std::sync::Arc::new(objects));
        (dir, state)
    }

    fn one(session: &str, client: Option<ClientReport>) -> Sample {
        Sample {
            session: session.into(),
            agent: "claude".into(),
            hub_calls: 1,
            injected_other: 10,
            client,
            ..Default::default()
        }
    }

    /// This store's waiting totals, verified and not.
    fn waiting(state: &super::super::HubState) -> Vec<Sample> {
        let guard = pending();
        let pending = guard.as_ref().unwrap();
        pending
            .totals
            .iter()
            .chain(&pending.unverified)
            .filter(|((store, ..), _)| *store == store_key(state))
            .map(|(_, waiting)| waiting.sample.clone())
            .collect()
    }

    /// Makes every waiting total of this store look older than a write takes.
    fn age(state: &super::super::HubState) {
        let mut guard = pending();
        let pending = guard.as_mut().unwrap();
        for (key, waiting) in pending
            .totals
            .iter_mut()
            .chain(pending.unverified.iter_mut())
        {
            if key.0 == store_key(state) {
                waiting.first_at -= 3600;
            }
        }
    }

    fn verify(state: &super::super::HubState, token: &str, repository: &str) {
        let digest = super::super::store::agent_token_digest(token).unwrap();
        pending()
            .get_or_insert_with(Pending::default)
            .verified
            .insert(
                (store_key(state), digest.as_str().to_owned(), None),
                (Instant::now(), repository.into()),
            );
    }

    #[test]
    fn calls_add_up_in_memory_and_a_resent_report_is_counted_once() {
        // No runtime here, so no write is scheduled: recording reads and writes no store.
        let (_dir, state) = hub();
        let token = &"5".repeat(64);
        let report = |id| {
            Some(ClientReport {
                id: Some(id),
                calls: 3,
                ms_total: 9000,
                ms_max: 5000,
                timeouts: 1,
                failed_open: 0,
            })
        };
        record(&state, Some(token), None, one("s", report(7)));
        // The client sends report 7 again after a timeout the Hub did not see: counted once.
        record(&state, Some(token), None, one("s", report(7)));
        record(&state, Some(token), None, one("s", report(8)));
        let totals = waiting(&state);
        assert_eq!(totals.len(), 1);
        assert_eq!((totals[0].hub_calls, totals[0].injected_other), (3, 30));
        let client = totals[0].client.unwrap();
        assert_eq!((client.calls, client.timeouts), (6, 2));
        // Calls of the last write period are on their way; once overdue the read is partial.
        assert!(all_written("r", 0, i64::MAX));
        age(&state);
        assert!(!all_written("r", 0, i64::MAX));
    }

    #[test]
    fn unverified_tokens_cannot_crowd_out_verified_ones_and_drops_are_reported() {
        let (_dir, state) = hub();
        let real = "6".repeat(64);
        verify(&state, &real, "r");
        let report = Some(ClientReport {
            id: Some(1),
            calls: 2,
            ms_total: 10,
            ms_max: 5,
            ..Default::default()
        });
        record(&state, Some(&real), None, one("real", report));
        // A flood of sessions under a token the store has not verified fills only its own table.
        for n in 0..MAX_UNVERIFIED + 10 {
            record(
                &state,
                Some(&"7".repeat(64)),
                None,
                one(
                    &format!("fake{n}"),
                    Some(ClientReport {
                        id: Some(n as u64),
                        ..Default::default()
                    }),
                ),
            );
        }
        // The verified token is still counted, and its report ids were not pushed out: the
        // resent report 1 is not counted again.
        record(&state, Some(&real), None, one("real", report));
        let real = waiting(&state)
            .into_iter()
            .find(|sample| sample.session == "real")
            .unwrap();
        assert_eq!((real.hub_calls, real.client.unwrap().calls), (2, 2));
        // Drops of calls naming no repository make every read partial from then on, and no
        // read of an earlier window.
        let now = super::super::store::unix_now();
        assert!(!all_written("r", now - 60, i64::MAX));
        assert!(all_written("r", now + 60, i64::MAX));
    }

    #[test]
    fn totals_a_write_has_taken_count_as_missing_until_it_ends() {
        let (_dir, state) = hub();
        let now = super::super::store::unix_now();
        // A write in progress holds a total of repository "a" from just now.
        pending()
            .get_or_insert_with(Pending::default)
            .writing
            .insert(u64::MAX, vec![(Some("a".into()), now)]);
        assert!(!all_written("a", 0, i64::MAX), "taken but not stored yet");
        assert!(all_written("b", 0, i64::MAX));
        pending().as_mut().unwrap().writing.remove(&u64::MAX);
        assert!(all_written("a", 0, i64::MAX));
        // A real write clears its own mark when it ends, whatever the store answered.
        let token = "9".repeat(64);
        record(&state, Some(&token), None, one("s", None));
        tokio::runtime::Runtime::new()
            .unwrap()
            .block_on(flush(&state));
        assert!(pending().as_ref().unwrap().writing.is_empty());
    }

    #[test]
    fn a_waiting_total_makes_only_its_own_repository_partial() {
        let (_dir, state) = hub();
        let token = "8".repeat(64);
        verify(&state, &token, "a");
        record(&state, Some(&token), None, one("s", None));
        age(&state);
        assert!(!all_written("a", 0, i64::MAX));
        assert!(all_written("b", 0, i64::MAX));
        // A window that ended before the calls came is not missing them.
        assert!(all_written("a", 0, 0));
    }

    #[test]
    fn totals_keep_sessions_without_a_client_report_apart() {
        let reported = SessionOverhead {
            session: "a".into(),
            hub_calls: 13,
            injected_start: 9000,
            injected_refusal: 300,
            client_calls: 61,
            client_ms_total: 250_000,
            client_ms_max: 5000,
            client_timeouts: 48,
            ..Default::default()
        };
        let silent = SessionOverhead {
            session: "b".into(),
            hub_calls: 4,
            injected_other: 120,
            ..Default::default()
        };
        let mut metrics = OverheadMetrics::default();
        metrics.add(&[reported, silent]);
        assert_eq!(metrics.availability, "observed");
        assert_eq!((metrics.sessions, metrics.hub_calls), (2, 17));
        assert_eq!(metrics.injected_characters.session_start, 9000);
        assert_eq!(metrics.injected_characters.other, 120);
        assert_eq!(metrics.client.reporting_sessions, 1);
        assert_eq!(metrics.client.timed_out, 48);
        assert_eq!(metrics.sessions_without_client_report, 1);
        assert_eq!(characters("a😀"), 3);
    }
}
