//! Repository-scoped work measurements and conservative read-only projections.

#[cfg(test)]
use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::work::{ClaimStatus, WorkItem, WorkState};
use super::{HubError, Result};

pub const MAX_OBSERVATIONS: usize = 500;
pub const MAX_HISTORY: usize = 256;
pub const MAX_PUBLICATIONS: usize = 16;
pub const MAX_INSIGHT_PAGE: usize = 20;
/// Bounds both individual values and the sums of a bounded work report within JSON's exact integers.
pub const MAX_MEASUREMENT: i64 = 1_000_000_000_000;

/// Explicit, per-request opt-in to best-effort completion observations.
pub const COMPLETION_HEADER: &str = "x-agentscm-completion-reporting";

pub(crate) fn completion_enabled(headers: &axum::http::HeaderMap) -> bool {
    let mut values = headers.get_all(COMPLETION_HEADER).iter();
    values.next().is_some_and(|value| value == "true") && values.next().is_none()
}

/// Record at most one completion observation per participant and work generation.
/// A stop does not establish success, model usage, or a charge. The ordinary usage
/// writer rechecks membership and generation, and applies its existing bounds.
pub(crate) fn record_completion(
    store: &dyn super::store::Store,
    token: &str,
    item: &WorkItem,
    session: &str,
    now: i64,
) -> Result<UsageReceipt> {
    let participant = item
        .participants
        .iter()
        .find(|participant| participant.token == token && participant.session == session)
        .ok_or_else(|| invalid("completion session is not a work participant"))?;
    // Length-prefix variable fields so distinct tuples cannot share an encoding.
    let mut hash = Sha256::new();
    for value in [token, item.id.as_str(), session] {
        hash.update((value.len() as u64).to_be_bytes());
        hash.update(value.as_bytes());
    }
    hash.update(item.generation.to_be_bytes());
    let observation_id = format!("completion-{:x}", hash.finalize());
    store.work_usage(
        token,
        &UsageRecord {
            work_id: item.id.clone(),
            observation_id,
            generation: item.generation,
            session: session.to_owned(),
            phase: AttemptPhase::Other,
            outcome: AttemptOutcome::Unknown,
            agent: participant.agent.clone(),
            provider: "unknown".into(),
            model: "unknown".into(),
            usage_basis: UsageBasis::Reported,
            source_commit: None,
            input_tokens: None,
            output_tokens: None,
            cost: None,
        },
        now,
    )
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub enum UsageBasis {
    Reported,
    ProviderResult,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub enum CostKind {
    ReportedCharge,
    ListPriceEstimate,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AttemptPhase {
    Implementation,
    Review,
    Test,
    Repair,
    Other,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AttemptOutcome {
    Succeeded,
    Failed,
    Cancelled,
    Unknown,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ReportedCost {
    pub kind: CostKind,
    pub currency: String,
    pub amount_micros: i64,
    /// Caller-supplied billing or rate-version label, not a verified billing record.
    pub basis: String,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct UsageRecord {
    pub work_id: String,
    pub observation_id: String,
    pub generation: i64,
    pub session: String,
    pub phase: AttemptPhase,
    pub outcome: AttemptOutcome,
    /// Client/runtime identity, independent of the model provider.
    pub agent: String,
    pub provider: String,
    pub model: String,
    pub usage_basis: UsageBasis,
    pub source_commit: Option<String>,
    pub input_tokens: Option<i64>,
    pub output_tokens: Option<i64>,
    pub cost: Option<ReportedCost>,
}

impl UsageRecord {
    pub fn validate(&self) -> Result<()> {
        super::work::validate_id("work", &self.work_id, "wrk")?;
        for (name, value) in [
            ("observation_id", &self.observation_id),
            ("session", &self.session),
            ("agent", &self.agent),
            ("provider", &self.provider),
            ("model", &self.model),
        ] {
            super::intent::text(name, value, 255)?;
        }
        if self.generation < 1 {
            return Err(invalid("generation must be positive"));
        }
        if self
            .source_commit
            .as_ref()
            .is_some_and(|oid| !valid_oid(oid))
        {
            return Err(invalid(
                "source_commit must be a full lowercase Git object ID",
            ));
        }
        for count in [self.input_tokens, self.output_tokens]
            .into_iter()
            .flatten()
        {
            if !(0..=MAX_MEASUREMENT).contains(&count) {
                return Err(invalid("token count is out of bounds"));
            }
        }
        if let Some(cost) = &self.cost {
            if !(0..=MAX_MEASUREMENT).contains(&cost.amount_micros)
                || cost.currency.len() != 3
                || !cost.currency.bytes().all(|b| b.is_ascii_uppercase())
            {
                return Err(invalid("cost amount or currency is out of bounds"));
            }
            super::intent::text("cost basis", &cost.basis, 255)?;
        }
        Ok(())
    }
}

pub(super) fn valid_oid(value: &str) -> bool {
    matches!(value.len(), 40 | 64)
        && value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct UsageReceipt {
    pub work_id: String,
    pub observation_id: String,
    pub received_at: i64,
    pub replayed: bool,
}

/// Storage-only input. Tokens and native session identifiers never enter an insight response.
#[cfg(test)]
#[derive(Clone, Debug)]
pub struct UsageObservation {
    pub request: UsageRecord,
    pub received_at: i64,
}

#[derive(Clone, Debug)]
pub struct HistoryPoint {
    pub seq: i64,
    pub at: i64,
    pub state: WorkState,
    pub generation: i64,
    pub last_checkin_at: i64,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct RecordedCheck {
    pub head_sha: String,
    pub state: String,
    pub phase: String,
    pub binding_digest: Option<String>,
    pub observed_at: i64,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct PublicationEvidence {
    pub publication_id: String,
    pub commit: String,
    pub branch_state: String,
    pub pr_number: Option<i64>,
    pub merge_commit: Option<String>,
    /// Last stored publication update, not the time the forge merged it.
    pub observed_at: i64,
    pub checks: Vec<RecordedCheck>,
    pub association: String,
}

#[derive(Clone, Debug)]
pub struct InsightInput {
    pub item: WorkItem,
    pub history: Vec<HistoryPoint>,
    pub history_complete: bool,
    pub retention_gap: bool,
    pub usage: UsageSummary,
    pub publications: Vec<PublicationEvidence>,
    pub publications_complete: bool,
    pub commits_truncated: bool,
    pub as_of: i64,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct InsightQuery {
    pub since: Option<i64>,
    pub until: Option<i64>,
    pub after: Option<String>,
    pub through: Option<i64>,
    #[serde(default = "default_limit")]
    pub limit: usize,
}

impl Default for InsightQuery {
    fn default() -> Self {
        Self {
            since: None,
            until: None,
            after: None,
            through: None,
            limit: default_limit(),
        }
    }
}

const fn default_limit() -> usize {
    MAX_INSIGHT_PAGE
}

impl InsightQuery {
    pub fn window(&self, now: i64) -> Result<(i64, i64)> {
        let until = self.until.unwrap_or(now);
        let since = self
            .since
            .unwrap_or_else(|| until.saturating_sub(7 * 86_400));
        if !(1..=MAX_INSIGHT_PAGE).contains(&self.limit)
            || since < 0
            || until > now
            || until < since
            || until - since > 90 * 86_400
            || self.through.is_some_and(|seq| seq < 0)
            || (self.after.is_some()
                && (self.until.is_none() || self.since.is_none() || self.through.is_none()))
        {
            return Err(invalid("insight window or continuation is out of bounds"));
        }
        if let Some(cursor) = &self.after {
            InsightCursor::decode(cursor, since, until, self.through.unwrap_or_default())?;
        }
        Ok((since, until))
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq, Default)]
#[serde(deny_unknown_fields)]
pub struct InsightTiming {
    pub elapsed_seconds: Option<i64>,
    pub active_seconds: Option<i64>,
    pub blocked_seconds: Option<i64>,
    pub paused_seconds: Option<i64>,
    pub handover_seconds: Option<i64>,
    pub finished_seconds: Option<i64>,
    pub unknown_seconds: Option<i64>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq, Default)]
#[serde(deny_unknown_fields)]
pub struct InsightSignals {
    pub takeovers: Option<usize>,
    pub handovers: Option<usize>,
    pub reopened: Option<usize>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct CostGroup {
    pub kind: CostKind,
    pub currency: String,
    pub basis: String,
    pub usage_basis: UsageBasis,
    pub agent: String,
    pub provider: String,
    pub model: String,
    pub amount_micros: i64,
    pub observations: usize,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct UsageSummary {
    pub observations: usize,
    /// "unknown" without reports, otherwise "partial"; reporting does not prove exhaustive capture.
    pub coverage: String,
    pub input_tokens: Option<i64>,
    pub output_tokens: Option<i64>,
    pub input_known_observations: usize,
    pub output_known_observations: usize,
    pub missing_cost_observations: usize,
    pub reported_failed_attempts: usize,
    pub cost_groups: Vec<CostGroup>,
    pub token_groups: Vec<TokenGroup>,
    /// False when a cohort page omits groups; the detail endpoint returns all bounded groups.
    pub groups_complete: bool,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct WorkInsight {
    pub version: u32,
    pub as_of: i64,
    pub work_id: String,
    pub title: String,
    pub state: WorkState,
    pub claim_status: ClaimStatus,
    pub created_at: i64,
    pub updated_at: i64,
    pub reported_finished: bool,
    pub history_complete: bool,
    pub history_retention_gap: bool,
    pub scanned_events: usize,
    pub timing: InsightTiming,
    pub signals: InsightSignals,
    pub usage: UsageSummary,
    /// Claimed source associations with stored controller observations, never work acceptance.
    pub publications: Vec<PublicationEvidence>,
    pub publications_complete: bool,
    pub commits_truncated: bool,
    pub evidence_basis: String,
    pub approval: String,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct InsightPage {
    pub version: u32,
    pub as_of: i64,
    pub since: i64,
    pub until: i64,
    pub through: i64,
    pub next_after: Option<String>,
    pub complete: bool,
    pub items: Vec<WorkInsight>,
}

fn invalid(message: &str) -> HubError {
    HubError::Invalid(message.into())
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct TokenGroup {
    pub agent: String,
    pub provider: String,
    pub model: String,
    pub usage_basis: UsageBasis,
    pub observations: usize,
    pub input_tokens: Option<i64>,
    pub output_tokens: Option<i64>,
    pub input_known_observations: usize,
    pub output_known_observations: usize,
}

/// A bounded, self-contained continuation. Scope is always supplied by authentication.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct InsightCursor {
    pub since: i64,
    pub until: i64,
    pub through: i64,
    pub created_at: i64,
    pub id: String,
}
impl InsightCursor {
    pub fn encode(&self) -> String {
        serde_json::to_string(self).expect("serializable cursor")
    }
    pub fn decode(value: &str, since: i64, until: i64, through: i64) -> Result<Self> {
        if value.len() > 512 {
            return Err(invalid("invalid insight cursor"));
        }
        let cursor: Self =
            serde_json::from_str(value).map_err(|_| invalid("invalid insight cursor"))?;
        super::work::validate_id("work", &cursor.id, "wrk")?;
        if (cursor.since, cursor.until, cursor.through) != (since, until, through)
            || cursor.created_at < since
            || cursor.created_at > until
        {
            return Err(invalid("insight cursor does not match window"));
        }
        Ok(cursor)
    }
}

/// Summarize submitted measurements; absence remains unknown and provenance stays grouped.
/// Stores use equivalent SQL aggregates so cohort pages need not materialize every observation.
#[cfg(test)]
pub fn summarize_usage(observations: &[UsageObservation]) -> Result<UsageSummary> {
    if observations.len() > MAX_OBSERVATIONS {
        return Err(invalid("too many observations"));
    }
    for observation in observations {
        observation.request.validate()?;
    }
    type CostKey = (String, String, String, UsageBasis, CostKind, String, String);
    let mut tokens: BTreeMap<(String, String, String, UsageBasis), TokenGroup> = BTreeMap::new();
    let mut costs: BTreeMap<CostKey, CostGroup> = BTreeMap::new();
    let mut input = None;
    let mut output = None;
    let mut input_known = 0;
    let mut output_known = 0;
    let mut missing_cost = 0;
    let mut failed = 0;
    for observation in observations {
        let r = &observation.request;
        let key = (
            r.agent.clone(),
            r.provider.clone(),
            r.model.clone(),
            r.usage_basis,
        );
        let group = tokens.entry(key).or_insert_with(|| TokenGroup {
            agent: r.agent.clone(),
            provider: r.provider.clone(),
            model: r.model.clone(),
            usage_basis: r.usage_basis,
            observations: 0,
            input_tokens: None,
            output_tokens: None,
            input_known_observations: 0,
            output_known_observations: 0,
        });
        group.observations += 1;
        if let Some(value) = r.input_tokens {
            input = Some(input.unwrap_or(0) + value);
            group.input_tokens = Some(group.input_tokens.unwrap_or(0) + value);
            input_known += 1;
            group.input_known_observations += 1;
        }
        if let Some(value) = r.output_tokens {
            output = Some(output.unwrap_or(0) + value);
            group.output_tokens = Some(group.output_tokens.unwrap_or(0) + value);
            output_known += 1;
            group.output_known_observations += 1;
        }
        failed += usize::from(r.outcome == AttemptOutcome::Failed);
        if let Some(cost) = &r.cost {
            let key = (
                r.agent.clone(),
                r.provider.clone(),
                r.model.clone(),
                r.usage_basis,
                cost.kind,
                cost.currency.clone(),
                cost.basis.clone(),
            );
            let group = costs.entry(key).or_insert_with(|| CostGroup {
                agent: r.agent.clone(),
                provider: r.provider.clone(),
                model: r.model.clone(),
                usage_basis: r.usage_basis,
                kind: cost.kind,
                currency: cost.currency.clone(),
                basis: cost.basis.clone(),
                amount_micros: 0,
                observations: 0,
            });
            group.amount_micros += cost.amount_micros;
            group.observations += 1;
        } else {
            missing_cost += 1;
        }
    }
    Ok(UsageSummary {
        observations: observations.len(),
        coverage: if observations.is_empty() {
            "unknown"
        } else {
            "partial"
        }
        .into(),
        input_tokens: input,
        output_tokens: output,
        input_known_observations: input_known,
        output_known_observations: output_known,
        missing_cost_observations: missing_cost,
        reported_failed_attempts: failed,
        cost_groups: costs.into_values().collect(),
        token_groups: tokens.into_values().collect(),
        groups_complete: true,
    })
}

/// Derive reported-state timings, never CPU time, productivity or acceptance.
pub fn project(input: InsightInput) -> WorkInsight {
    let InsightInput {
        item,
        history,
        history_complete,
        retention_gap,
        usage,
        publications,
        publications_complete,
        commits_truncated,
        as_of,
    } = input;
    let complete = history_complete
        && !retention_gap
        && item.created_seq > 0
        && !history.is_empty()
        && history.len() <= MAX_HISTORY
        && history[0].seq == item.created_seq
        && history[0].at == item.created_at
        && history
            .last()
            .is_some_and(|point| point.seq == item.updated_seq)
        && history
            .iter()
            .all(|point| point.at >= item.created_at && point.at <= as_of)
        && history.windows(2).all(|pair| {
            pair[0].seq < pair[1].seq
                && pair[0].at <= pair[1].at
                && pair[0].generation <= pair[1].generation
        });
    let (timing, signals) = if complete {
        let mut timing = InsightTiming {
            elapsed_seconds: as_of.checked_sub(item.created_at),
            active_seconds: Some(0),
            blocked_seconds: Some(0),
            paused_seconds: Some(0),
            handover_seconds: Some(0),
            finished_seconds: Some(0),
            unknown_seconds: Some(0),
        };
        for (index, point) in history.iter().enumerate() {
            let end = history.get(index + 1).map_or(as_of, |next| next.at);
            let duration = end - point.at;
            let slot = match point.state {
                WorkState::Active => {
                    let observed_end = point
                        .last_checkin_at
                        .saturating_add(super::work::CLAIM_STALE_SECONDS)
                        .clamp(point.at, end);
                    *timing.unknown_seconds.as_mut().expect("initialized") += end - observed_end;
                    *timing.active_seconds.as_mut().expect("initialized") +=
                        observed_end - point.at;
                    continue;
                }
                WorkState::Blocked => &mut timing.blocked_seconds,
                WorkState::Paused => &mut timing.paused_seconds,
                WorkState::Handover => &mut timing.handover_seconds,
                WorkState::Finished => &mut timing.finished_seconds,
            };
            *slot.as_mut().expect("initialized") += duration;
        }
        let signals = InsightSignals {
            takeovers: Some(
                history
                    .windows(2)
                    .filter(|p| p[1].generation > p[0].generation)
                    .count(),
            ),
            handovers: Some(
                history
                    .iter()
                    .enumerate()
                    .filter(|(i, p)| {
                        p.state == WorkState::Handover
                            && (*i == 0 || history[*i - 1].state != WorkState::Handover)
                    })
                    .count(),
            ),
            reopened: Some(
                history
                    .windows(2)
                    .filter(|p| {
                        p[0].state == WorkState::Finished && p[1].state != WorkState::Finished
                    })
                    .count(),
            ),
        };
        (timing, signals)
    } else {
        (InsightTiming::default(), InsightSignals::default())
    };
    WorkInsight {
        version: 1,
        as_of,
        work_id: item.id,
        title: item.title,
        state: item.state,
        claim_status: item.claim_status,
        created_at: item.created_at,
        updated_at: item.updated_at,
        reported_finished: item.state == WorkState::Finished,
        history_complete: complete,
        history_retention_gap: retention_gap,
        scanned_events: history.len(),
        timing,
        signals,
        usage,
        publications,
        publications_complete,
        commits_truncated,
        evidence_basis: "client_claimed_commits_with_stored_controller_observations".into(),
        approval: "unknown".into(),
    }
}

#[cfg(test)]
mod tests {
    use super::super::work::WorkSource;
    use super::*;

    #[test]
    fn completion_requires_one_explicit_true_header() {
        let mut headers = axum::http::HeaderMap::new();
        assert!(!completion_enabled(&headers));
        for value in ["false", "1", "TRUE", "true, true", ""] {
            headers.insert(COMPLETION_HEADER, value.parse().unwrap());
            assert!(!completion_enabled(&headers));
        }
        headers.insert(COMPLETION_HEADER, "true".parse().unwrap());
        assert!(completion_enabled(&headers));
        headers.append(COMPLETION_HEADER, "true".parse().unwrap());
        assert!(!completion_enabled(&headers));
    }

    fn observation(agent: &str) -> UsageObservation {
        UsageObservation {
            received_at: 101,
            request: UsageRecord {
                work_id: "wrk_0123456789abcdef0123456789abcdef".into(),
                observation_id: "attempt-1".into(),
                generation: 1,
                session: "fixture-session".into(),
                phase: AttemptPhase::Implementation,
                outcome: AttemptOutcome::Unknown,
                agent: agent.into(),
                provider: "unknown".into(),
                model: "unknown".into(),
                usage_basis: UsageBasis::Reported,
                source_commit: None,
                input_tokens: None,
                output_tokens: None,
                cost: None,
            },
        }
    }
    fn input(states: &[(i64, WorkState, i64)], as_of: i64) -> InsightInput {
        let history: Vec<_> = states
            .iter()
            .enumerate()
            .map(|(i, (at, state, generation))| HistoryPoint {
                seq: i as i64 + 1,
                at: *at,
                state: *state,
                generation: *generation,
                last_checkin_at: *at,
            })
            .collect();
        let last = history.last().unwrap();
        let item = WorkItem {
            id: "wrk_0123456789abcdef0123456789abcdef".into(),
            repository: "sample/repo".into(),
            owner: "user".into(),
            owner_login: "user".into(),
            source: WorkSource::Native {
                request_key: "fixture".into(),
            },
            title: "Synthetic work".into(),
            state: last.state,
            claim_status: ClaimStatus::Active,
            last_checkin_at: last.at,
            generation: last.generation,
            lead_token: "private-token-id".into(),
            lead_agent: "generic".into(),
            lead_session: "private-session".into(),
            participants: vec![],
            scope: vec![],
            branch: None,
            worktree: None,
            next_action: None,
            current: None,
            blocker: None,
            depends_on: vec![],
            related_work: vec![],
            created_at: history[0].at,
            updated_at: last.at,
            created_seq: 1,
            updated_seq: last.seq,
        };
        InsightInput {
            item,
            history,
            history_complete: true,
            retention_gap: false,
            usage: summarize_usage(&[]).unwrap(),
            publications: vec![],
            publications_complete: true,
            commits_truncated: false,
            as_of,
        }
    }
    #[test]
    fn every_client_can_report_without_fabricating_measurements() {
        let reports: Vec<_> = [
            "claude",
            "codex",
            "grok",
            "gemini",
            "cursor",
            "copilot",
            "custom-mcp-client",
        ]
        .into_iter()
        .map(observation)
        .collect();
        let summary = summarize_usage(&reports).unwrap();
        assert_eq!(summary.observations, 7);
        assert_eq!(summary.token_groups.len(), 7);
        assert_eq!(summary.input_tokens, None);
        assert_eq!(summary.output_tokens, None);
        assert_eq!(summary.missing_cost_observations, 7);
        assert_eq!(summary.coverage, "partial");
    }
    #[test]
    fn zeros_are_known_and_incompatible_costs_are_separate() {
        let mut a = observation("generic");
        a.request.input_tokens = Some(0);
        a.request.cost = Some(ReportedCost {
            kind: CostKind::ListPriceEstimate,
            currency: "USD".into(),
            amount_micros: 0,
            basis: "rates-v1".into(),
        });
        let mut b = a.clone();
        b.request.usage_basis = UsageBasis::ProviderResult;
        b.request.cost.as_mut().unwrap().kind = CostKind::ReportedCharge;
        let summary = summarize_usage(&[a, b]).unwrap();
        assert_eq!(summary.input_tokens, Some(0));
        assert_eq!(summary.output_tokens, None);
        assert_eq!(summary.token_groups.len(), 2);
        assert_eq!(summary.cost_groups.len(), 2);
        assert_eq!(summary.input_known_observations, 2);
    }
    #[test]
    fn bounded_measurements_remain_exact_in_json_clients() {
        let mut a = observation("generic");
        a.request.input_tokens = Some(MAX_MEASUREMENT);
        a.request.output_tokens = Some(MAX_MEASUREMENT);
        let summary = summarize_usage(&vec![a.clone(); MAX_OBSERVATIONS]).unwrap();
        assert_eq!(summary.input_tokens, Some(500_000_000_000_000));
        assert!(summary.input_tokens.unwrap() < (1i64 << 53));
        a.request.input_tokens = Some(MAX_MEASUREMENT + 1);
        assert!(summarize_usage(&[a]).is_err());
    }
    #[test]
    fn reported_states_and_unobserved_active_time_are_distinct() {
        let report = project(input(
            &[
                (100, WorkState::Active, 1),
                (1100, WorkState::Blocked, 1),
                (1200, WorkState::Handover, 1),
                (1300, WorkState::Active, 2),
                (1400, WorkState::Finished, 2),
            ],
            1500,
        ));
        assert!(report.history_complete);
        assert_eq!(report.timing.active_seconds, Some(1000));
        assert_eq!(report.timing.unknown_seconds, Some(100));
        assert_eq!(report.timing.blocked_seconds, Some(100));
        assert_eq!(report.timing.handover_seconds, Some(100));
        assert_eq!(report.timing.finished_seconds, Some(100));
        assert_eq!(report.signals.takeovers, Some(1));
        assert_eq!(report.signals.handovers, Some(1));
        assert!(report.reported_finished);
        assert_eq!(report.approval, "unknown");
        let json = serde_json::to_string(&report).unwrap();
        assert!(!json.contains("private-token-id"));
        assert!(!json.contains("private-session"));
    }
    #[test]
    fn missing_or_nonmonotonic_history_never_invents_zero_wait() {
        let mut gap = input(&[(100, WorkState::Active, 1)], 200);
        gap.retention_gap = true;
        let report = project(gap);
        assert!(!report.history_complete);
        assert_eq!(report.timing.active_seconds, None);
        assert_eq!(report.signals.reopened, None);
        let report = project(input(
            &[(100, WorkState::Active, 1), (90, WorkState::Finished, 1)],
            200,
        ));
        assert!(!report.history_complete);
        assert_eq!(report.timing.elapsed_seconds, None);
    }
    #[test]
    fn continuation_is_bound_to_its_cohort_and_watermark() {
        let cursor = InsightCursor {
            since: 100,
            until: 200,
            through: 5,
            created_at: 150,
            id: "wrk_0123456789abcdef0123456789abcdef".into(),
        }
        .encode();
        assert!(InsightCursor::decode(&cursor, 100, 200, 5).is_ok());
        assert!(InsightCursor::decode(&cursor, 100, 200, 6).is_err());
        assert!(InsightCursor::decode(&cursor, 100, 201, 5).is_err());
    }
}
