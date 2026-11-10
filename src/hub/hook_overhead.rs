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
use std::sync::atomic::{AtomicU64, Ordering};
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

#[derive(Default)]
struct Pending {
    /// Totals of sessions whose token the store verified.
    totals: BTreeMap<Key, Sample>,
    /// Totals of sessions whose token is not verified yet; the next write checks them.
    unverified: BTreeMap<Key, Sample>,
    verified: BTreeMap<Token, Instant>,
    /// Report ids already counted, per session.
    seen: BTreeMap<Key, VecDeque<u64>>,
    /// Stores with a write scheduled.
    scheduled: BTreeSet<usize>,
}

static PENDING: Mutex<Option<Pending>> = Mutex::new(None);
/// Hook calls not counted in this process because the memory tables were full.
static DROPPED: AtomicU64 = AtomicU64::new(0);

fn pending() -> std::sync::MutexGuard<'static, Option<Pending>> {
    PENDING.lock().unwrap_or_else(|p| p.into_inner())
}

fn store_key(state: &super::HubState) -> usize {
    std::sync::Arc::as_ptr(&state.store).cast::<()>() as usize
}

/// Adds `sample` to the total at `key` in `table`, or drops it when the table is full.
fn add_to(table: &mut BTreeMap<Key, Sample>, limit: usize, key: Key, sample: &Sample) {
    let held = table.len();
    match table.get_mut(&key) {
        Some(total) => total.add(sample),
        None if held < limit => {
            table.insert(key, sample.clone());
        }
        None => {
            DROPPED.fetch_add(sample.hub_calls, Ordering::Relaxed);
        }
    }
}

/// Whether every hook call this process saw is in the store: none dropped, none waiting.
pub fn all_written() -> bool {
    DROPPED.load(Ordering::Relaxed) == 0
        && pending()
            .as_ref()
            .is_none_or(|p| p.totals.is_empty() && p.unverified.is_empty())
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
        if let Some(id) = sample.client.and_then(|client| client.id) {
            if pending.seen.len() >= MAX_PENDING && !pending.seen.contains_key(&key) {
                // shortcut: forgetting every session's ids can count a resent report twice once
                // in this many sessions; a per-session expiry would fix it if that ever matters.
                pending.seen.clear();
            }
            let seen = pending.seen.entry(key.clone()).or_default();
            if seen.contains(&id) {
                sample.client = None;
            } else {
                if seen.len() == SEEN_PER_SESSION {
                    seen.pop_front();
                }
                seen.push_back(id);
            }
        }
        let verified = pending
            .verified
            .get(&token)
            .is_some_and(|at| at.elapsed() < VERIFIED_FOR);
        if verified {
            add_to(&mut pending.totals, MAX_PENDING, key, &sample);
        } else {
            add_to(&mut pending.unverified, MAX_UNVERIFIED, key, &sample);
        }
        pending.scheduled.insert(store)
    };
    if schedule {
        schedule_flush(state);
    }
}

/// Writes this store's totals [`FLUSH_EVERY`] from now. A plain function, so `flush` can schedule
/// itself without checking its own future's `Send` from inside its definition.
fn schedule_flush(state: &super::HubState) {
    if let Ok(runtime) = tokio::runtime::Handle::try_current() {
        let state = state.clone();
        runtime.spawn(async move {
            tokio::time::sleep(FLUSH_EVERY).await;
            flush(&state).await;
        });
    }
}

/// Writes every waiting total of this Hub's store, each session in its own store transaction,
/// in one background store call. A token the store does not accept loses its totals. When a
/// write fails, the totals from that one on are kept for the next write and those before it,
/// already committed, are not.
pub async fn flush(state: &super::HubState) {
    let store_id = store_key(state);
    let taken: Vec<(Key, Sample)> = {
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
        taken
    };
    if taken.is_empty() {
        return;
    }
    let store = state.store.clone();
    let writing = taken.clone();
    fn accepted(
        checked: BTreeMap<(String, Option<String>), Option<super::store::Principal>>,
    ) -> BTreeSet<(String, Option<String>)> {
        checked
            .into_iter()
            .filter_map(|(token, principal)| principal.map(|_| token))
            .collect()
    }
    // (tokens the store accepted, index of the first total not written)
    let written = super::http::blocking(state, move || {
        let now = super::store::unix_now();
        // Each token is checked once per write; None is a token the store does not accept.
        let mut checked: BTreeMap<(String, Option<String>), Option<super::store::Principal>> =
            BTreeMap::new();
        for (at, ((_, digest, repo, ..), sample)) in writing.iter().enumerate() {
            let token = (digest.clone(), repo.clone());
            if !checked.contains_key(&token) {
                let Ok(parsed) = super::store::TokenDigest::from_persisted(digest) else {
                    continue;
                };
                let Ok(found) = store.authenticate(&parsed, now) else {
                    return Ok((accepted(checked), Some(at)));
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
                return Ok((accepted(checked), Some(at)));
            }
        }
        Ok((accepted(checked), None))
    })
    .await;
    let (accepted, unwritten) = match written {
        Ok((accepted, failed_at)) => (accepted, failed_at.unwrap_or(taken.len())),
        // The store call never ran (no free permit): nothing was written.
        Err(_) => (BTreeSet::new(), 0),
    };
    let left = unwritten < taken.len();
    let again = {
        let mut guard = pending();
        let pending = guard.get_or_insert_with(Pending::default);
        let now = Instant::now();
        for (digest, repo) in accepted {
            pending.verified.insert((store_id, digest, repo), now);
        }
        for (key, sample) in taken.into_iter().skip(unwritten) {
            let token = (key.0, key.1.clone(), key.2.clone());
            if pending.verified.contains_key(&token) {
                add_to(&mut pending.totals, MAX_PENDING, key, &sample);
            } else {
                add_to(&mut pending.unverified, MAX_UNVERIFIED, key, &sample);
            }
        }
        // Totals a failed write put back are retried by the timer, not left for the next call.
        (left || !pending.unverified.is_empty()) && pending.scheduled.insert(store_id)
    };
    if again {
        schedule_flush(state);
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
            .map(|(_, sample)| sample.clone())
            .collect()
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
        assert!(!all_written());
    }

    #[test]
    fn unverified_tokens_cannot_crowd_out_verified_ones_and_drops_are_reported() {
        let (_dir, state) = hub();
        let real = "6".repeat(64);
        let digest = super::super::store::agent_token_digest(&real).unwrap();
        pending()
            .get_or_insert_with(Pending::default)
            .verified
            .insert(
                (store_key(&state), digest.as_str().to_owned(), None),
                Instant::now(),
            );
        // A flood of sessions under a token the store has not verified fills only its own table.
        for n in 0..MAX_UNVERIFIED + 10 {
            record(
                &state,
                Some(&"7".repeat(64)),
                None,
                one(&format!("fake{n}"), None),
            );
        }
        assert_eq!(DROPPED.load(Ordering::Relaxed), 10);
        // The verified token is still counted.
        record(&state, Some(&real), None, one("real", None));
        assert!(
            waiting(&state)
                .iter()
                .any(|sample| sample.session == "real")
        );
        assert!(!all_written(), "dropped calls make the read partial");
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
