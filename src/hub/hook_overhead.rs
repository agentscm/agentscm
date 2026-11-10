//! What AgentSCM's hooks cost a session, as counts: the hook calls that reached the Hub and the
//! characters the Hub put into the agent's context, plus what the hook client measured itself
//! (calls, latency, timeouts and fail-open answers), which it sends with its next call because a
//! call that timed out never reached the Hub. Nothing here is converted to time saved or money.
use axum::http::HeaderMap;
use serde::{Deserialize, Serialize};

/// The hook client's report of its calls since its last delivered report.
pub const REPORT_HEADER: &str = "x-agentscm-hook-report";
const MAX_REPORT_BYTES: usize = 256;
/// One report never claims more than this many calls, nor more than this latency per call.
const MAX_REPORT_CALLS: u64 = 10_000;
const MAX_CALL_MS: u64 = 600_000;
/// Rows older than this are deleted when a row is written; rows kept per repository.
pub const RETAIN_SECONDS: i64 = 30 * 86_400;
pub const MAX_ROWS: i64 = 20_000;
/// Sessions listed one by one in value metrics; the rest are only in the totals.
pub const MAX_SESSIONS_LISTED: usize = 50;
pub const MAX_READ: usize = 20_000;

/// The client's own measurement of hook calls, as sent in [`REPORT_HEADER`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ClientReport {
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
        (report.calls <= MAX_REPORT_CALLS
            && report.timeouts + report.failed_open <= report.calls
            && report.ms_max <= MAX_CALL_MS
            && report.ms_total <= report.calls * MAX_CALL_MS
            && report.ms_max <= report.ms_total)
            .then_some(report)
    }
}

/// One hook call as the Hub saw it, added to its session's row.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Sample {
    pub session: String,
    pub agent: String,
    pub injected_start: u64,
    pub injected_refusal: u64,
    pub injected_other: u64,
    pub client: Option<ClientReport>,
}

/// One answered hook call: the text the Hub returned is counted by what it was for. A call with
/// no session id counts under `unknown`.
pub fn sample(
    session: Option<&str>,
    agent: &str,
    start: bool,
    text: &str,
    refusal: Option<&str>,
    headers: &HeaderMap,
) -> Sample {
    let text = characters(text);
    Sample {
        session: session.unwrap_or("unknown").to_owned(),
        agent: agent.to_owned(),
        injected_start: if start { text } else { 0 },
        injected_refusal: refusal.map_or(0, characters),
        injected_other: if start { 0 } else { text },
        client: ClientReport::from_headers(headers),
    }
}

/// Adds the call to its session's row after the reply was built, so it never delays the hook.
/// Best effort: a call that does not authenticate, or a failed write, records nothing; the
/// client's report then stays unrecorded, which value metrics shows as a missing report.
pub async fn record(
    state: super::HubState,
    token: Option<String>,
    repo: Option<String>,
    sample: Sample,
) {
    let Ok(principal) = super::hook::authenticate(&state, token, repo).await else {
        return;
    };
    let store = state.store.clone();
    let _ = super::http::blocking(&state, move || {
        store.hook_overhead(
            &principal.token_id,
            &principal.repository,
            &sample,
            super::store::unix_now(),
        )
    })
    .await;
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
            self.hub_calls += row.hub_calls;
            self.injected_characters.session_start += row.injected_start;
            self.injected_characters.refusal += row.injected_refusal;
            self.injected_characters.other += row.injected_other;
            if row.client_calls > 0 {
                self.client.reporting_sessions += 1;
                self.client.calls += row.client_calls;
                self.client.latency_ms_total += row.client_ms_total;
                self.client.latency_ms_max = self.client.latency_ms_max.max(row.client_ms_max);
                self.client.timed_out += row.client_timeouts;
                self.client.failed_open += row.client_failed_open;
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
            "not json",
        ] {
            assert_eq!(ClientReport::from_headers(&headers(bad)), None, "{bad}");
        }
        assert_eq!(ClientReport::from_headers(&HeaderMap::new()), None);
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
