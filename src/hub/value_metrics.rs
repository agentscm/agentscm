//! Deterministic projections of retained observations. Contribution is not causal savings.
use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::catchup::CatchupPage;
use super::insights::{AttemptPhase, CostKind, UsageBasis, UsageMeasurementScope, UsageRecord};
use super::work::WorkItem;
use super::{HubError, Result};

pub const MAX_FACTS: usize = 2_000;
const MAX_DETAILS: usize = 50;

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ValueMetricsQuery {
    pub since: Option<i64>,
    pub until: Option<i64>,
    pub work_id: Option<String>,
    pub repository: Option<String>,
    pub after: Option<String>,
    pub limit: Option<usize>,
}
impl ValueMetricsQuery {
    pub fn window(&self, now: i64) -> Result<(i64, i64)> {
        let until = self.until.unwrap_or(now);
        let until = if until > now && until.saturating_sub(now) <= 60 {
            now
        } else {
            until
        };
        let since = self
            .since
            .unwrap_or_else(|| until.saturating_sub(7 * 86_400).max(0));
        if since < 0
            || until > now
            || until < since
            || until - since > 90 * 86_400
            || self.limit.is_some_and(|n| !(1..=MAX_DETAILS).contains(&n))
            || self.after.as_ref().is_some_and(|s| s.len() > 256)
        {
            return Err(HubError::Invalid(
                "invalid value metrics window or limit".into(),
            ));
        }
        if let Some(id) = &self.work_id {
            super::work::validate_id("work", id, "wrk")?;
        }
        Ok((since, until))
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq, PartialOrd, Ord)]
pub struct ValueScope {
    pub provider: String,
    pub host: String,
    pub repository: String,
    pub repository_id: Option<i64>,
}
#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
pub struct ContextMetrics {
    pub availability: String,
    pub returned: usize,
    pub acknowledged: usize,
    pub applied: Option<usize>,
}
#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
pub struct IssueMetrics {
    pub availability: String,
    pub unresolved: usize,
    pub resolved: usize,
    pub dismissed: Option<usize>,
    pub false_trigger: Option<usize>,
}
#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
pub struct FollowupMetrics {
    pub availability: String,
    pub current: usize,
    pub pending: usize,
    pub failed: Option<usize>,
    pub stale: usize,
    pub cancelled: Option<usize>,
    pub unknown: usize,
}
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct ResourceCost {
    pub provider: String,
    pub model: String,
    pub kind: String,
    pub currency: String,
    pub basis: String,
    pub amount_micros: String,
    pub requests: usize,
}
#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
pub struct ResourceGroup {
    pub provider: String,
    pub model: String,
    pub category: String,
    pub usage_basis: String,
    pub input_tokens: Option<String>,
    pub output_tokens: Option<String>,
    pub cache_read_input_tokens: Option<String>,
    pub cache_creation_input_tokens: Option<String>,
    pub requests: usize,
}
#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
pub struct ResourceMetrics {
    pub input_tokens: Option<String>,
    pub output_tokens: Option<String>,
    pub costs: Vec<ResourceCost>,
    pub groups: Vec<ResourceGroup>,
    pub missing_cost_requests: usize,
    pub unallocated_overhead: Option<String>,
    pub unallocated_usage_records: usize,
}
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct ValueCoverage {
    pub state: String,
    pub eligible_works: Option<usize>,
    pub observed_works: usize,
    pub complete: bool,
    pub gaps: Vec<String>,
    pub omitted: usize,
}
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct ValueEvidence {
    pub id: String,
    pub kind: String,
    pub state: String,
    pub repository: String,
    pub work_id: Option<String>,
    pub source: Option<String>,
    pub recipe: Option<String>,
    pub policy: Option<String>,
    pub basis: String,
    pub assurance: String,
    pub availability: String,
    pub applicability: String,
    pub observed_at: i64,
    pub detail: String,
}
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct ValueMetrics {
    pub version: u32,
    pub method: String,
    pub assessment_id: String,
    pub as_of: i64,
    pub since: i64,
    pub until: i64,
    pub scope: Vec<ValueScope>,
    pub context: ContextMetrics,
    pub issues: IssueMetrics,
    pub followups: FollowupMetrics,
    pub resources: ResourceMetrics,
    pub coverage: ValueCoverage,
    pub evidence: Vec<ValueEvidence>,
    pub linked_merge: Option<ValueEvidence>,
    pub next_after: Option<String>,
    pub merge_observations: usize,
}

/// Produced only by existing admission/evidence paths, never accepted from a public metrics API.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct ValueObservation {
    pub work_id: Option<String>,
    pub generation: Option<i64>,
    pub identity: String,
    pub kind: String,
    pub state: String,
    pub branch: String,
    pub source: String,
    pub recipe: String,
    pub policy: String,
    pub gate: String,
    pub evidence_id: Option<String>,
    pub reason: String,
}
impl ValueObservation {
    pub fn validate(&self) -> Result<()> {
        if let Some(id) = &self.work_id {
            super::work::validate_id("work", id, "wrk")?;
        }
        if self.work_id.is_some() != self.generation.is_some()
            || self.generation.is_some_and(|n| n < 1)
        {
            return Err(HubError::Invalid("invalid value Work binding".into()));
        }
        for (name, value) in [
            ("identity", &self.identity),
            ("branch", &self.branch),
            ("source", &self.source),
            ("recipe", &self.recipe),
            ("policy", &self.policy),
            ("gate", &self.gate),
            ("reason", &self.reason),
        ] {
            super::intent::text(name, value, 255)?;
        }
        if !matches!(self.kind.as_str(), "issue" | "followup")
            || !matches!(
                self.state.as_str(),
                "unresolved"
                    | "resolved"
                    | "current"
                    | "pending"
                    | "failed"
                    | "stale"
                    | "cancelled"
                    | "unknown"
            )
        {
            return Err(HubError::Invalid("invalid value observation".into()));
        }
        Ok(())
    }
}
#[derive(Clone, Debug, Serialize)]
pub struct StoredValueObservation {
    pub sequence: i64,
    pub at: i64,
    pub observation: ValueObservation,
}
#[derive(Clone, Debug, Serialize)]
pub struct ValueUsage {
    pub owner: String,
    pub work: String,
    pub at: i64,
    pub request: UsageRecord,
}
#[derive(Clone, Debug, Serialize)]
pub struct ValueDelivery {
    pub token_namespace: String,
    pub id: String,
    pub at: i64,
    pub acknowledged: bool,
    pub page: CatchupPage,
}
#[derive(Clone, Debug, Serialize)]
pub struct ValueMerge {
    pub id: String,
    pub at: i64,
    pub head: String,
    pub merge_commit: String,
    pub source: String,
    pub pull_number: i64,
}
#[derive(Clone, Debug, Serialize)]
pub struct ValueFacts {
    pub scope: ValueScope,
    pub current_configuration: Vec<(String, String)>,
    pub works: Vec<WorkItem>,
    pub usage: Vec<ValueUsage>,
    pub deliveries: Vec<ValueDelivery>,
    pub observations: Vec<StoredValueObservation>,
    pub merges: Vec<ValueMerge>,
    pub capture_failures: bool,
    pub delivery_retained_from: Option<i64>,
    pub cap_rejections: usize,
    pub complete: bool,
    pub request_membership: BTreeMap<String, BTreeSet<String>>,
    pub request_membership_complete: bool,
    pub delivery_pruned_range: Option<(i64, i64)>,
    pub context_population: String,
}

pub fn identity(parts: &[&str]) -> String {
    let mut hash = Sha256::new();
    for part in parts {
        hash.update((part.len() as u64).to_be_bytes());
        hash.update(part.as_bytes());
    }
    format!("{:x}", hash.finalize())
}

/// One provider request can receive complementary reports. Unknown components are enriched;
/// conflicting known components remain unknown without discarding compatible measurements.
struct RequestMeasurements {
    request: UsageRecord,
    token_conflicts: [bool; 4],
    costs: BTreeMap<(CostKind, String, String), Option<i64>>,
    model_conflict: bool,
}
impl RequestMeasurements {
    fn new(request: UsageRecord) -> Self {
        let mut costs = BTreeMap::new();
        if let Some(c) = &request.cost {
            costs.insert(
                (c.kind, c.currency.clone(), c.basis.clone()),
                Some(c.amount_micros),
            );
        }
        Self {
            request,
            token_conflicts: [false; 4],
            costs,
            model_conflict: false,
        }
    }
    fn merge(&mut self, other: &UsageRecord) {
        let r = &mut self.request;
        for (index, (target, incoming)) in [
            (&mut r.input_tokens, other.input_tokens),
            (&mut r.output_tokens, other.output_tokens),
            (
                &mut r.cache_read_input_tokens,
                other.cache_read_input_tokens,
            ),
            (
                &mut r.cache_creation_input_tokens,
                other.cache_creation_input_tokens,
            ),
        ]
        .into_iter()
        .enumerate()
        {
            if self.token_conflicts[index] {
                continue;
            }
            match (*target, incoming) {
                (None, Some(n)) => *target = Some(n),
                (Some(a), Some(b)) if a != b => {
                    *target = None;
                    self.token_conflicts[index] = true;
                }
                _ => {}
            }
        }
        if r.model == "unknown" && !self.model_conflict {
            r.model = other.model.clone();
        } else if other.model != "unknown" && r.model != other.model {
            r.model = "unknown".into();
            self.model_conflict = true;
        }
        if r.phase != other.phase {
            r.phase = AttemptPhase::Other;
        }
        if other.usage_basis == UsageBasis::ProviderResult {
            r.usage_basis = UsageBasis::ProviderResult;
        }
        if let Some(c) = &other.cost {
            let entry = self
                .costs
                .entry((c.kind, c.currency.clone(), c.basis.clone()))
                .or_insert(Some(c.amount_micros));
            if entry.is_some_and(|amount| amount != c.amount_micros) {
                *entry = None;
            }
        }
    }
}

/// Shared arithmetic for all surfaces, with namespaced request de-duplication before grouping.
pub fn project(
    facts: Vec<ValueFacts>,
    query: &ValueMetricsQuery,
    now: i64,
) -> Result<ValueMetrics> {
    let (since, until) = query.window(now)?;
    let mut scopes = Vec::new();
    let mut context = ContextMetrics::default();
    let mut issues = IssueMetrics::default();
    let mut followups = FollowupMetrics::default();
    let mut evidence = Vec::new();
    let mut observed = BTreeSet::new();
    let mut known_work_metadata = BTreeSet::new();
    let mut eligible_ids = BTreeSet::new();
    let mut existing_metadata_ids = BTreeSet::new();
    let mut referenced_bindings = BTreeSet::new();
    let mut complete = true;
    let mut gaps = BTreeSet::from([
        "capture is partial; zero observations do not establish zero problems".to_owned(),
        "AgentSCM overhead and total eligible provider requests are unknown".to_owned(),
        "Observation capture failure diagnostics are process-local and do not establish capture completeness across Hub restarts".to_owned(),
        "context returned means a response was prepared; transport receipt and application are not observed".to_owned(),
        "Issue dismissals/false triggers and failed/cancelled follow-up outcomes are not captured by supported producers".to_owned(),
    ]);
    let mut requests: BTreeMap<String, RequestMeasurements> = BTreeMap::new();
    let mut unallocated_usage_records = 0;
    let mut request_works: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    let mut request_reports: BTreeMap<String, BTreeMap<String, usize>> = BTreeMap::new();
    let mut watermark = Vec::new();
    let mut merges = BTreeSet::new();
    let mut membership_complete = true;
    for fact in facts {
        complete &= fact.complete;
        membership_complete &= fact.request_membership_complete;
        for (request, works) in fact.request_membership {
            request_works.entry(request).or_default().extend(works);
        }
        gaps.insert(format!("Context population: {}", fact.context_population));
        if fact
            .delivery_pruned_range
            .is_some_and(|(first, last)| first <= until && last >= since)
        {
            complete = false;
            gaps.insert("Context delivery records in this repository were pruned by count; prepared/acknowledged evidence for this window may be incomplete".into());
        }
        scopes.push(fact.scope.clone());
        eligible_ids.extend(
            fact.works
                .iter()
                .filter(|w| {
                    query.work_id.as_ref().is_none_or(|id| id == &w.id)
                        && w.created_at <= until
                        && ((w.updated_at >= since && w.updated_at <= until)
                            || w.state != super::work::WorkState::Finished
                            || fact
                                .usage
                                .iter()
                                .any(|u| u.work == w.id && u.at >= since && u.at <= until))
                })
                .map(|work| work.id.clone()),
        );
        existing_metadata_ids.extend(
            fact.works
                .iter()
                .filter(|work| work.created_at <= until)
                .map(|work| work.id.clone()),
        );
        if fact.capture_failures {
            complete = false;
            gaps.insert(
                "Observation capture failed in this Hub process; evidence may be missing".into(),
            );
        }
        if fact.cap_rejections > 0 {
            gaps.insert("usage observation cap rejected capture".into());
        }
        if fact
            .delivery_retained_from
            .is_some_and(|floor| since < floor)
        {
            complete = false;
            gaps.insert("Catch-up delivery records expire after 24 hours; earlier context returns and acknowledgements in this window may be missing".into());
        }
        let works: BTreeMap<_, _> = fact.works.iter().map(|w| (w.id.as_str(), w)).collect();
        known_work_metadata.extend(works.keys().map(|id| (*id).to_owned()));
        for work in works
            .values()
            .filter(|work| query.work_id.as_ref().is_none_or(|id| id == &work.id))
        {
            let mut commits = work
                .current
                .as_ref()
                .map(|c| {
                    c.source_refs
                        .iter()
                        .filter_map(|r| match r {
                            super::work::SourceRef::Commit { oid, .. } => Some(oid.as_str()),
                            _ => None,
                        })
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default();
            commits.sort();
            commits.dedup();
            watermark.push(format!(
                "work-source:{}:{}:{}",
                work.id,
                work.generation,
                commits.join(",")
            ));
        }
        for usage in fact.usage {
            if usage.at < since || usage.at > until {
                continue;
            }
            let r = &usage.request;
            let relevant = query.work_id.as_ref().is_none_or(|id| id == &usage.work);
            let numeric = [
                r.input_tokens,
                r.output_tokens,
                r.cache_read_input_tokens,
                r.cache_creation_input_tokens,
            ]
            .iter()
            .any(Option::is_some);
            if relevant && (numeric || r.cost.is_some()) {
                observed.insert(usage.work.clone());
            }
            if r.measurement_scope != Some(UsageMeasurementScope::Request)
                || r.provider_request_id.is_none()
            {
                if relevant {
                    unallocated_usage_records += 1;
                    let reason = if r.measurement_scope
                        == Some(UsageMeasurementScope::SessionCumulative)
                    {
                        "session cumulative usage is excluded from request totals"
                    } else if r.measurement_scope.is_none() {
                        "usage with unknown measurement scope is excluded from additive request totals"
                    } else {
                        "usage without a stable provider request identity is excluded from additive request totals"
                    };
                    gaps.insert(reason.into());
                    watermark.push(format!(
                        "unallocated:{}:{}:{}",
                        usage.work,
                        r.observation_id,
                        identity(&[&serde_json::to_string(r).expect("usage serializes")])
                    ));
                    evidence.push(ValueEvidence {
                        id: format!("usage-{}", identity(&[&usage.work, &r.observation_id])),
                        kind: "resource".into(),
                        state: "unallocated".into(),
                        repository: fact.scope.repository.clone(),
                        work_id: Some(usage.work.clone()),
                        source: r.source_commit.clone(),
                        recipe: None,
                        policy: None,
                        basis: "structured_usage_report".into(),
                        assurance: serde_json::to_value(r.usage_basis)
                            .expect("basis serializes")
                            .as_str()
                            .unwrap_or_default()
                            .into(),
                        availability: "record_available".into(),
                        applicability: "unknown_accounting_scope".into(),
                        observed_at: usage.at,
                        detail: reason.into(),
                    });
                }
                continue;
            }
            // Legacy collectors lack provider account/endpoint namespaces. Equal provider/
            // request identities are collapsed conservatively; compatibility is component-wise.
            let key = identity(&[
                &r.provider,
                r.provider_request_id
                    .as_deref()
                    .expect("request identity checked"),
            ]);
            request_works
                .entry(key.clone())
                .or_default()
                .insert(usage.work.clone());
            *request_reports
                .entry(key.clone())
                .or_default()
                .entry(usage.work.clone())
                .or_default() += 1;
            if let Some(old) = requests.get_mut(&key) {
                old.merge(r);
            } else {
                requests.insert(key, RequestMeasurements::new(usage.request));
            }
        }
        let mut latest = BTreeMap::new();
        let mut opened = BTreeMap::new();
        let mut current_subject = BTreeMap::new();
        let mut observations = fact.observations;
        observations.sort_by_key(|o| o.sequence);
        for stored in observations {
            if stored.at > until {
                continue;
            }
            let o = &stored.observation;
            // Keep the producer's binding even if a bounded aggregate omitted the Work.
            // Missing current metadata makes applicability unknown, never unlinked/current.
            let work_id = o.work_id.clone();
            if o.kind == "issue" && o.state == "unresolved" {
                opened
                    .entry(o.identity.clone())
                    .or_insert((work_id.clone(), o.generation));
            }
            if o.kind == "followup" {
                current_subject.insert(
                    (o.work_id.clone(), o.branch.clone(), o.gate.clone()),
                    (o.source.clone(), o.recipe.clone(), o.policy.clone()),
                );
            }
            latest.insert((o.kind.clone(), o.identity.clone()), (stored, work_id));
        }
        for ((kind, _), (stored, mut work_id)) in latest {
            let mut generation = stored.observation.generation;
            if kind == "issue"
                && let Some((original_work, original_generation)) =
                    opened.get(&stored.observation.identity)
            {
                work_id = original_work.clone();
                generation = *original_generation;
            }
            if query
                .work_id
                .as_ref()
                .is_some_and(|id| work_id.as_ref() != Some(id))
            {
                continue;
            }
            let mut o = stored.observation;
            o.work_id = work_id.clone();
            o.generation = generation;
            if kind == "issue" && !opened.contains_key(&o.identity) {
                continue;
            }
            if kind == "followup"
                && (current_subject.get(&(o.work_id.clone(), o.branch.clone(), o.gate.clone()))
                    != Some(&(o.source.clone(), o.recipe.clone(), o.policy.clone()))
                    || !fact
                        .current_configuration
                        .contains(&(o.recipe.clone(), o.policy.clone())))
            {
                o.state = "stale".into();
            }
            let applicability = if o.state == "stale" {
                "stale"
            } else if let Some(id) = &work_id {
                match works.get(id.as_str()) {
                    Some(work) if Some(work.generation) != o.generation => "stale",
                    Some(work) => {
                        let commits = work
                            .current
                            .as_ref()
                            .map(|c| {
                                c.source_refs
                                    .iter()
                                    .filter_map(|r| match r {
                                        super::work::SourceRef::Commit { oid, .. } => {
                                            Some(oid.as_str())
                                        }
                                        _ => None,
                                    })
                                    .collect::<Vec<_>>()
                            })
                            .unwrap_or_default();
                        if commits.is_empty() {
                            "unknown"
                        } else if commits.contains(&o.source.as_str()) {
                            "current"
                        } else {
                            "stale"
                        }
                    }
                    None => "unknown",
                }
            } else if o.work_id.is_some() {
                "unknown"
            } else {
                "reported_source"
            };
            if kind == "followup" && applicability == "stale" {
                o.state = "stale".into();
            }
            // Current actionable states remain visible outside the contribution window.
            if stored.at < since
                && !matches!(
                    o.state.as_str(),
                    "unresolved" | "pending" | "failed" | "stale" | "unknown"
                )
            {
                continue;
            }
            // Only genuine retained producer/staleness states keep old evidence. Missing
            // metadata cannot turn an aged satisfied result into a permanent Unknown.
            if kind == "followup" && applicability == "unknown" {
                o.state = "unknown".into();
            }
            watermark.push(format!(
                "observation:{}:{}",
                stored.sequence,
                identity(&[&serde_json::to_string(&o).expect("observation serializes")])
            ));
            if kind == "issue" {
                match o.state.as_str() {
                    "resolved" => issues.resolved += 1,
                    _ => issues.unresolved += 1,
                }
            }
            if kind == "followup" {
                match o.state.as_str() {
                    "current" => followups.current += 1,

                    "stale" => followups.stale += 1,

                    "pending" => followups.pending += 1,
                    _ => followups.unknown += 1,
                }
            }
            if let Some(id) = &work_id {
                referenced_bindings.insert(id.clone());
                if stored.at >= since {
                    observed.insert(id.clone());
                }
            }
            evidence.push(ValueEvidence {
                id: o.evidence_id.unwrap_or(o.identity),
                kind,
                state: o.state.clone(),
                repository: fact.scope.repository.clone(),
                work_id,
                source: Some(o.source),
                recipe: Some(o.recipe),
                policy: Some(o.policy),
                basis: "supported_skill_admission".into(),
                assurance: "self_reported".into(),
                availability: "record_available_artifact_unverified".into(),
                applicability: applicability.into(),
                observed_at: stored.at,
                detail: o.reason,
            });
        }
        let mut deliveries = BTreeSet::new();
        let mut acknowledged = BTreeSet::new();
        for delivery in &fact.deliveries {
            if delivery.acknowledged {
                for event in &delivery.page.work_events {
                    acknowledged.insert((
                        delivery.token_namespace.clone(),
                        delivery.page.subscriber.clone(),
                        event.seq,
                    ));
                }
            }
        }
        for delivery in fact.deliveries {
            if delivery.at < since || delivery.at > until {
                continue;
            }
            for event in &delivery.page.work_events {
                if query
                    .work_id
                    .as_ref()
                    .is_some_and(|id| id != &event.work_id)
                {
                    continue;
                }
                // Delivery retry and pagination identities do not multiply the same source
                // event returned to the same subscriber. No before-edit claim is inferred.
                let key = (
                    delivery.token_namespace.clone(),
                    delivery.page.subscriber.clone(),
                    event.seq,
                );
                if !deliveries.insert(key) {
                    continue;
                }
                context.returned += 1;
                context.acknowledged += usize::from(acknowledged.contains(&(
                    delivery.token_namespace.clone(),
                    delivery.page.subscriber.clone(),
                    event.seq,
                )));
                observed.insert(event.work_id.clone());
                watermark.push(format!(
                    "delivery:{}:{}:{}",
                    identity(&[&delivery.token_namespace, &delivery.page.subscriber]),
                    event.seq,
                    acknowledged.contains(&(
                        delivery.token_namespace.clone(),
                        delivery.page.subscriber.clone(),
                        event.seq
                    ))
                ));
                evidence.push(ValueEvidence { id: format!("context-{}",identity(&[&fact.scope.provider,&fact.scope.host,&fact.scope.repository,&fact.scope.repository_id.unwrap_or(-1).to_string(),&delivery.token_namespace,&delivery.page.subscriber,&event.seq.to_string()])), kind: "context".into(), state: if acknowledged.contains(&(delivery.token_namespace.clone(),delivery.page.subscriber.clone(),event.seq)) { "acknowledged" } else { "returned" }.into(), repository: fact.scope.repository.clone(), work_id: Some(event.work_id.clone()), source: Some(format!("work-event:{}", event.seq)), recipe: None, policy: None, basis:"retained_catchup_response".into(),assurance:"hub_observed".into(),availability:"available".into(),applicability:"source_event".into(),observed_at: delivery.at, detail: "Shared Work update in a retained catch-up response; application not observed".into() });
            }
            if delivery.page.retention.work_gap
                || delivery.page.retention.merge_gap
                || delivery.page.retention.coordination_gap
            {
                gaps.insert("catch-up retention gap".into());
            }
            if !delivery.page.complete {
                gaps.insert("catch-up response was bounded".into());
            }
        }
        for merge in fact.merges {
            if merge.at < since || merge.at > until {
                continue;
            }
            // Source equality to an explicitly recorded checkpoint commit is a declared
            // association; a branch name alone never proves a Work was merged.
            let linked = fact.works.iter().filter(|w| w.current.as_ref().is_some_and(|c| c.source_refs.iter().any(|r| matches!(r, super::work::SourceRef::Commit { oid, .. } if oid == &merge.head)))).collect::<Vec<_>>();
            // An incomplete aggregate candidate set cannot establish unique attribution.
            // Explicit Work scope retains its declared source-association semantics.
            let work_id = (linked.len() == 1 && (query.work_id.is_some() || fact.complete))
                .then(|| linked[0].id.clone());
            if query
                .work_id
                .as_ref()
                .is_some_and(|id| work_id.as_ref() != Some(id))
            {
                continue;
            }
            watermark.push(format!(
                "merge:{}:{}:{}:{}:{}",
                merge.id,
                merge.head,
                merge.merge_commit,
                merge.source,
                work_id.as_deref().unwrap_or_default()
            ));
            if merges.insert(merge.id.clone()) {
                evidence.push(ValueEvidence { id: merge.id, kind: "provider_merge".into(), state: "observed".into(), repository: fact.scope.repository.clone(), work_id, source: Some(merge.head), recipe: None, policy: None,basis:format!("provider_observed:{}",merge.source),assurance:"provider_observed".into(),availability:"available".into(),applicability:"declared_source_association".into(),observed_at: merge.at, detail: format!("Provider-observed pull request #{} merge {}; Work association is declared, not acceptance", merge.pull_number, merge.merge_commit) });
            }
        }
    }
    referenced_bindings.extend(observed.iter().cloned());
    eligible_ids.extend(observed.intersection(&existing_metadata_ids).cloned());
    let eligible = eligible_ids.len();
    if !referenced_bindings.is_subset(&known_work_metadata) {
        complete = false;
        gaps.insert("Current Work metadata is unavailable for some retained observations; the eligible Work population and linked applicability are unknown".into());
    }
    if query.work_id.is_some() && !membership_complete {
        complete = false;
        gaps.insert("Provider request membership across retained repository history is incomplete; selected Work resources remain unallocated".into());
    }
    let mut input: Option<i128> = None;
    let mut output: Option<i128> = None;
    type CostKey = (String, String, String, String, String);
    let mut costs: BTreeMap<CostKey, (i128, usize)> = BTreeMap::new();
    let mut missing_cost = 0;
    type GroupKey = (String, String, String, String);
    let mut resource_groups: BTreeMap<GroupKey, ([Option<i128>; 4], usize)> = BTreeMap::new();
    for (key, usage) in requests {
        if let Some(work) = &query.work_id {
            let works = request_works
                .get(&key)
                .expect("request Work identities retained");
            if !works.contains(work) {
                continue;
            }
            if works.len() > 1 || !membership_complete {
                // Count this Work's retained report rows, not another Work's reports or
                // additional additive requests. Exact observation retries never add rows.
                unallocated_usage_records += request_reports
                    .get(&key)
                    .and_then(|reports| reports.get(work))
                    .copied()
                    .unwrap_or(0);
                gaps.insert(if membership_complete {"Shared provider request resources are unallocated and excluded from individual Work totals"}else{"Provider request membership across retained repository history is incomplete; selected Work resources remain unallocated"}.into());
                continue;
            }
        }
        gaps.insert("Provider account/endpoint namespace is unavailable; equal provider/request IDs are conservatively collapsed".into());
        if request_works.get(&key).is_some_and(|works| works.len() > 1) {
            gaps.insert(
                "shared provider requests counted once; attribution remains unallocated".into(),
            );
        }
        let request_costs = usage.costs;
        let r = usage.request;
        watermark.push(format!("request:{}:{}:{}",key,request_works.get(&key).map_or(0,|w|w.len()),identity(&[&serde_json::to_string(&serde_json::json!({"provider":r.provider,"model":r.model,"phase":r.phase,"basis":r.usage_basis,"input":r.input_tokens,"output":r.output_tokens,"cache_read":r.cache_read_input_tokens,"cache_creation":r.cache_creation_input_tokens,"costs":request_costs.iter().collect::<Vec<_>>(),"conflicts":usage.token_conflicts,"model_conflict":usage.model_conflict})).expect("request serializes")])));
        if usage.token_conflicts.iter().any(|v| *v)
            || request_costs.values().any(Option::is_none)
            || usage.model_conflict
        {
            gaps.insert(
                "conflicting provider request components excluded; compatible components retained"
                    .into(),
            );
        }
        let category = serde_json::to_value(r.phase)
            .expect("phase serializes")
            .as_str()
            .unwrap_or_default()
            .to_owned();
        let basis = serde_json::to_value(r.usage_basis)
            .expect("basis serializes")
            .as_str()
            .unwrap_or_default()
            .to_owned();
        let group = resource_groups
            .entry((r.provider.clone(), r.model.clone(), category, basis))
            .or_default();
        group.1 += 1;
        for (index, n) in [
            r.input_tokens,
            r.output_tokens,
            r.cache_read_input_tokens,
            r.cache_creation_input_tokens,
        ]
        .into_iter()
        .enumerate()
        {
            if let Some(n) = n {
                group.0[index] = Some(
                    group.0[index]
                        .unwrap_or(0)
                        .checked_add(i128::from(n))
                        .ok_or_else(|| HubError::Invalid("usage group overflow".into()))?,
                );
            }
        }
        if let Some(n) = r.input_tokens {
            input = Some(
                input
                    .unwrap_or(0)
                    .checked_add(i128::from(n))
                    .ok_or_else(|| HubError::Invalid("usage overflow".into()))?,
            );
        }
        if let Some(n) = r.output_tokens {
            output = Some(
                output
                    .unwrap_or(0)
                    .checked_add(i128::from(n))
                    .ok_or_else(|| HubError::Invalid("usage overflow".into()))?,
            );
        }
        let mut known_cost = false;
        for ((kind, currency, basis), amount) in request_costs {
            let Some(amount) = amount else {
                continue;
            };
            known_cost = true;
            let kind = serde_json::to_value(kind)
                .expect("kind serializes")
                .as_str()
                .unwrap_or_default()
                .to_owned();
            let group = costs
                .entry((r.provider.clone(), r.model.clone(), kind, currency, basis))
                .or_default();
            group.0 = group
                .0
                .checked_add(i128::from(amount))
                .ok_or_else(|| HubError::Invalid("cost overflow".into()))?;
            group.1 += 1;
        }
        if !known_cost
            && [
                r.input_tokens,
                r.output_tokens,
                r.cache_read_input_tokens,
                r.cache_creation_input_tokens,
            ]
            .iter()
            .any(Option::is_some)
        {
            missing_cost += 1;
        }
    }
    if !complete {
        gaps.insert(
            "bounded or unavailable source facts; totals and allocation coverage are incomplete"
                .into(),
        );
    }
    scopes.sort();
    scopes.dedup();
    watermark.push(
        serde_json::to_string(&(
            &context,
            &issues,
            &followups,
            &gaps,
            complete,
            eligible,
            unallocated_usage_records,
        ))
        .expect("projection serializes"),
    );
    watermark.sort();
    let assessment_id = identity(&[
        "agentscm.observed.v1",
        &serde_json::to_string(&scopes).expect("scope serializes"),
        &query.work_id.clone().unwrap_or_default(),
        &watermark.join("\n"),
    ]);
    context.availability = if context.returned > 0 {
        "partial"
    } else {
        "unknown"
    }
    .into();
    issues.availability = if issues.unresolved + issues.resolved > 0 {
        "partial"
    } else {
        "unknown"
    }
    .into();
    followups.availability =
        if followups.current + followups.pending + followups.stale + followups.unknown > 0 {
            "partial"
        } else {
            "unknown"
        }
        .into();
    let linked_merge = if query.work_id.is_some() {
        evidence
            .iter()
            .filter(|e| e.kind == "provider_merge")
            .max_by_key(|e| e.observed_at)
            .cloned()
    } else {
        None
    };
    evidence.sort_by(|a, b| {
        b.observed_at
            .cmp(&a.observed_at)
            .then(a.id.cmp(&b.id))
            .then(a.kind.cmp(&b.kind))
    });
    let omitted = evidence.len().saturating_sub(query.limit.unwrap_or(20));
    let after = query.after.as_deref();
    let after = after
        .map(|cursor| {
            cursor
                .split_once(':')
                .filter(|(assessment, _)| *assessment == assessment_id)
                .map(|(_, detail)| detail)
                .ok_or_else(|| {
                    HubError::Invalid(
                        "value metrics evidence changed; restart detail pagination".into(),
                    )
                })
        })
        .transpose()?;
    let start = after
        .map(|cursor| {
            evidence
                .iter()
                .position(|e| identity(&[&e.id, &e.kind, &e.state]) == cursor)
                .map(|n| n + 1)
                .ok_or_else(|| HubError::Invalid("value metrics cursor is unavailable".into()))
        })
        .transpose()?
        .unwrap_or(0);
    let mut details = evidence
        .into_iter()
        .skip(start)
        .take(query.limit.unwrap_or(20) + 1)
        .collect::<Vec<_>>();
    let next_after = (details.len() > query.limit.unwrap_or(20)).then(|| {
        let e = &details[query.limit.unwrap_or(20) - 1];
        format!(
            "{}:{}",
            assessment_id,
            identity(&[&e.id, &e.kind, &e.state])
        )
    });
    details.truncate(query.limit.unwrap_or(20));
    Ok(ValueMetrics {
        version: 1,
        method: "agentscm.observed.v1".into(),
        assessment_id,
        as_of: now,
        since,
        until,
        scope: scopes,
        context,
        issues,
        followups,
        resources: ResourceMetrics {
            input_tokens: input.map(|v| v.to_string()),
            output_tokens: output.map(|v| v.to_string()),
            costs: costs
                .into_iter()
                .map(
                    |((provider, model, kind, currency, basis), (amount, requests))| ResourceCost {
                        provider,
                        model,
                        kind,
                        currency,
                        basis,
                        amount_micros: amount.to_string(),
                        requests,
                    },
                )
                .collect(),
            groups: resource_groups
                .into_iter()
                .map(
                    |((provider, model, category, usage_basis), (counts, requests))| {
                        ResourceGroup {
                            provider,
                            model,
                            category,
                            usage_basis,
                            input_tokens: counts[0].map(|v| v.to_string()),
                            output_tokens: counts[1].map(|v| v.to_string()),
                            cache_read_input_tokens: counts[2].map(|v| v.to_string()),
                            cache_creation_input_tokens: counts[3].map(|v| v.to_string()),
                            requests,
                        }
                    },
                )
                .collect(),
            missing_cost_requests: missing_cost,
            unallocated_overhead: None,
            unallocated_usage_records,
        },
        coverage: ValueCoverage {
            state: if observed.is_empty() {
                "unknown"
            } else {
                "partial"
            }
            .into(),
            eligible_works: complete.then_some(eligible),
            observed_works: observed.len(),
            complete,
            gaps: gaps.into_iter().collect(),
            omitted,
        },
        evidence: details,
        linked_merge,
        next_after,
        merge_observations: merges.len(),
    })
}

#[derive(Default)]
struct CaptureFailures {
    scopes: BTreeSet<String>,
    unscoped: bool,
}
static CAPTURE_FAILURES: std::sync::Mutex<Option<CaptureFailures>> = std::sync::Mutex::new(None);
fn failure_scope(scope: &ValueScope) -> String {
    identity(&[
        &scope.provider,
        &scope.host,
        &scope
            .repository_id
            .map(|id| id.to_string())
            .unwrap_or_else(|| scope.repository.clone()),
    ])
}
pub fn observe(
    store: &dyn super::store::Store,
    token: &str,
    observation: &ValueObservation,
    now: i64,
) {
    if store.value_observe(token, observation, now).is_err() {
        let scope = store.value_scope(token, now).ok();
        let mut failures = CAPTURE_FAILURES
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let failures = failures.get_or_insert_with(CaptureFailures::default);
        match scope {
            Some(scope) if failures.scopes.len() < 10_000 => {
                failures.scopes.insert(failure_scope(&scope));
            }
            _ => failures.unscoped = true,
        }
        eprintln!("hub: value observation capture failed");
    }
}
pub fn capture_failed(scope: &ValueScope) -> bool {
    CAPTURE_FAILURES
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .as_ref()
        .is_some_and(|failures| {
            failures.unscoped || failures.scopes.contains(&failure_scope(scope))
        })
}

/// Cache only deterministic projections of already-authorized, freshly read facts. Role and
/// credential validation happen before this function on every read, including a hit.
pub fn project_cached(
    facts: Vec<ValueFacts>,
    query: &ValueMetricsQuery,
    now: i64,
    authorization: &str,
) -> Result<ValueMetrics> {
    static CACHE: std::sync::Mutex<Option<BTreeMap<String, ValueMetrics>>> =
        std::sync::Mutex::new(None);
    let window = query.window(now)?;
    let key = identity(&[
        authorization,
        &serde_json::to_string(&facts)
            .map_err(|_| HubError::Unavailable("value facts serialization failed".into()))?,
        &serde_json::to_string(query)
            .map_err(|_| HubError::Invalid("value query serialization failed".into()))?,
        &format!("{}:{}", window.0, window.1),
    ]);
    if let Some(value) = CACHE
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .as_ref()
        .and_then(|cache| cache.get(&key))
        .cloned()
    {
        let mut value = value;
        value.as_of = now;
        return Ok(value);
    }
    let value = project(facts, query, now)?;
    let mut cache = CACHE
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let cache = cache.get_or_insert_with(BTreeMap::new);
    if cache.len() >= 32 {
        cache.clear();
    }
    cache.insert(key, value.clone());
    Ok(value)
}

pub fn configuration(lock: Option<&str>) -> Vec<(String, String)> {
    lock.and_then(|s| crate::skill_workflow::supported::SkillLock::parse(s.as_bytes()).ok())
        .map(|lock| {
            lock.skills
                .into_iter()
                .filter(|pin| {
                    crate::skill_workflow::supported::catalog_entry_by_sha256(&pin.sha256).is_some()
                })
                .map(|pin| {
                    (
                        pin.sha256,
                        identity(&[&serde_json::to_string(&pin.params).unwrap_or_default()]),
                    )
                })
                .collect()
        })
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::super::insights::{
        AttemptOutcome, AttemptPhase, CostKind, ReportedCost, UsageBasis,
    };
    use super::*;
    fn facts() -> ValueFacts {
        ValueFacts {
            current_configuration: vec![("recipe".into(), "policy".into())],
            scope: ValueScope {
                provider: "github".into(),
                host: "forge.example".into(),
                repository: "example/repo".into(),
                repository_id: Some(1),
            },
            works: vec![],
            usage: vec![],
            deliveries: vec![],
            observations: vec![],
            merges: vec![],
            capture_failures: false,
            delivery_retained_from: None,
            cap_rejections: 0,
            complete: true,
            request_membership: BTreeMap::new(),
            request_membership_complete: true,
            delivery_pruned_range: None,
            context_population: "calling repository token's structured catch-up deliveries".into(),
        }
    }
    fn usage(work: &str, owner: &str, id: &str) -> ValueUsage {
        ValueUsage {
            owner: owner.into(),
            work: work.into(),
            at: 100,
            request: UsageRecord {
                work_id: work.into(),
                observation_id: id.into(),
                generation: 1,
                session: "session".into(),
                phase: AttemptPhase::Test,
                outcome: AttemptOutcome::Succeeded,
                agent: "client".into(),
                provider: "provider".into(),
                model: "model".into(),
                usage_basis: UsageBasis::ProviderResult,
                source_commit: None,
                input_tokens: Some(1_000_000_000_000),
                output_tokens: Some(20),
                cache_read_input_tokens: Some(5),
                cache_creation_input_tokens: None,
                action_id: None,
                attempt_id: None,
                provider_request_id: Some("account/request-1".into()),
                provider_event_id: None,
                measurement_scope: Some(UsageMeasurementScope::Request),
                cost: Some(ReportedCost {
                    kind: CostKind::ReportedCharge,
                    currency: "USD".into(),
                    amount_micros: 2_000,
                    basis: "provider_statement".into(),
                }),
            },
        }
    }
    fn observation(
        seq: i64,
        identity: &str,
        kind: &str,
        state: &str,
        source: &str,
    ) -> StoredValueObservation {
        StoredValueObservation {
            sequence: seq,
            at: seq + 90,
            observation: ValueObservation {
                work_id: None,
                generation: None,
                identity: identity.into(),
                kind: kind.into(),
                state: state.into(),
                branch: "branch".into(),
                source: source.into(),
                recipe: "recipe".into(),
                policy: "policy".into(),
                gate: "gate".into(),
                evidence_id: Some(format!("evidence-{seq}")),
                reason: "retained observation".into(),
            },
        }
    }
    #[test]
    fn shared_requests_across_owners_and_works_are_counted_once_and_unallocated() {
        let mut f = facts();
        f.usage = vec![
            usage("wrk_a", "owner-a", "one"),
            usage("wrk_b", "owner-b", "two"),
        ];
        let all = project(vec![f.clone()], &ValueMetricsQuery::default(), 200).unwrap();
        assert_eq!(all.resources.input_tokens.as_deref(), Some("1000000000000"));
        assert_eq!(all.resources.costs[0].amount_micros, "2000");
        assert_eq!(
            all.resources.groups[0].cache_read_input_tokens.as_deref(),
            Some("5")
        );
        // No per-Work report can allocate the same entire request to each Work.
        let q = ValueMetricsQuery {
            work_id: Some("wrk_00000000000000000000000000000001".into()),
            ..Default::default()
        };
        f.usage[0].work = q.work_id.clone().unwrap();
        f.usage[0].request.work_id = f.usage[0].work.clone();
        f.usage[1].work = "wrk_00000000000000000000000000000002".into();
        f.usage[1].request.work_id = f.usage[1].work.clone();
        let mut repeated_report = f.usage[0].clone();
        repeated_report.request.observation_id = "second-report-for-a".into();
        f.usage.push(repeated_report);
        let one = project(vec![f.clone()], &q, 200).unwrap();
        assert_eq!(one.resources.input_tokens, None);
        assert!(one.resources.costs.is_empty());
        assert_eq!(one.resources.unallocated_usage_records, 2);
        let other = project(
            vec![f.clone()],
            &ValueMetricsQuery {
                work_id: Some(f.usage[1].work.clone()),
                ..Default::default()
            },
            200,
        )
        .unwrap();
        assert_eq!(other.resources.unallocated_usage_records, 1);
        let all = project(vec![f], &Default::default(), 200).unwrap();
        assert_eq!(all.resources.unallocated_usage_records, 0);
        assert_eq!(all.resources.input_tokens.as_deref(), Some("1000000000000"));
        assert_eq!(all.resources.costs[0].amount_micros, "2000");
    }
    #[test]
    fn duplicate_conflicts_are_excluded_and_cumulative_never_added() {
        let mut f = facts();
        let a = usage("a", "owner", "one");
        let mut b = a.clone();
        b.request.input_tokens = Some(1);
        b.request.observation_id = "two".into();
        let mut cumulative = a.clone();
        cumulative.request.measurement_scope = Some(UsageMeasurementScope::SessionCumulative);
        cumulative.request.provider_request_id = None;
        f.usage = vec![a, b, cumulative];
        let p = project(vec![f], &Default::default(), 200).unwrap();
        assert_eq!(p.resources.input_tokens, None);
        assert_eq!(p.resources.output_tokens.as_deref(), Some("20"));
        assert_eq!(p.resources.costs[0].amount_micros, "2000");
        assert!(p.coverage.gaps.iter().any(|g| g.contains("conflicting")));
    }
    #[test]
    fn resources_keep_money_bases_currencies_and_unknowns_separate() {
        let mut f = facts();
        let mut a = usage("a", "owner", "one");
        let mut b = a.clone();
        b.request.provider_request_id = Some("different".into());
        b.request.cost.as_mut().unwrap().currency = "EUR".into();
        b.request.cost.as_mut().unwrap().kind = CostKind::ListPriceEstimate;
        a.request.cost = None;
        f.usage = vec![a, b];
        let p = project(vec![f], &Default::default(), 200).unwrap();
        assert_eq!(p.resources.input_tokens.as_deref(), Some("2000000000000"));
        assert_eq!(p.resources.missing_cost_requests, 1);
        assert_eq!(p.resources.costs.len(), 1);
        assert_eq!(p.resources.costs[0].currency, "EUR");
        assert_eq!(p.resources.unallocated_overhead, None);
    }
    #[test]
    fn episodes_require_original_refusal_and_same_identity_resolution() {
        let mut f = facts();
        f.observations = vec![
            observation(2, "episode", "issue", "resolved", "source"),
            observation(1, "episode", "issue", "unresolved", "source"),
            observation(3, "unrelated", "issue", "resolved", "other"),
        ];
        let p = project(vec![f], &Default::default(), 200).unwrap();
        assert_eq!(p.issues.resolved, 1);
        assert_eq!(p.issues.unresolved, 0);
    }
    #[test]
    fn stale_followups_and_old_unresolved_episodes_stay_visible() {
        let mut f = facts();
        f.observations = vec![
            observation(1, "old", "followup", "current", "old-source"),
            observation(2, "new", "followup", "pending", "new-source"),
            observation(3, "episode", "issue", "unresolved", "source"),
        ];
        let q = ValueMetricsQuery {
            since: Some(95),
            until: Some(200),
            ..Default::default()
        };
        let p = project(vec![f], &q, 200).unwrap();
        assert_eq!(p.issues.unresolved, 1);
        assert_eq!(p.followups.pending, 1);
    }
    #[test]
    fn compatible_request_reports_enrich_unknowns_and_keep_charge_and_reference_bases() {
        let mut a = usage("work", "owner", "first");
        a.request.cost = None;
        a.request.cache_read_input_tokens = None;
        let mut b = a.clone();
        b.request.observation_id = "enriched".into();
        b.request.cost = Some(ReportedCost {
            kind: CostKind::ReportedCharge,
            currency: "USD".into(),
            amount_micros: 2000,
            basis: "statement".into(),
        });
        b.request.cache_read_input_tokens = Some(5);
        let mut estimate = b.clone();
        estimate.request.observation_id = "estimate".into();
        estimate.request.cost.as_mut().unwrap().kind = CostKind::ListPriceEstimate;
        estimate.request.cost.as_mut().unwrap().basis = "rate-card".into();
        estimate.request.cost.as_mut().unwrap().amount_micros = 3000;
        let mut output = Vec::new();
        for reports in [
            vec![a.clone(), b.clone(), estimate.clone()],
            vec![estimate, b, a],
        ] {
            let mut f = facts();
            f.usage = reports;
            let p = project(vec![f], &Default::default(), 200).unwrap();
            assert_eq!(p.resources.input_tokens.as_deref(), Some("1000000000000"));
            assert_eq!(
                p.resources.groups[0].cache_read_input_tokens.as_deref(),
                Some("5")
            );
            assert_eq!(p.resources.costs.len(), 2);
            assert_eq!(p.resources.missing_cost_requests, 0);
            output.push(p.resources);
        }
        assert_eq!(output[0], output[1]);
    }
    #[test]
    fn unknown_scope_or_request_identity_is_non_additive_and_cache_only_is_numeric_coverage() {
        let mut f = facts();
        let mut a = usage("work", "owner", "legacy");
        a.request.measurement_scope = None;
        let mut b = a.clone();
        b.request.observation_id = "cache-only".into();
        b.request.measurement_scope = Some(UsageMeasurementScope::Request);
        b.request.provider_request_id = None;
        b.request.input_tokens = None;
        b.request.output_tokens = None;
        b.request.cost = None;
        b.request.cache_read_input_tokens = Some(5);
        f.usage = vec![a, b];
        let p = project(vec![f], &Default::default(), 200).unwrap();
        assert_eq!(p.resources.input_tokens, None);
        assert!(p.resources.costs.is_empty());
        assert_eq!(p.resources.unallocated_usage_records, 2);
        assert_eq!(p.coverage.observed_works, 1);
        assert_eq!(p.coverage.state, "partial");
    }
    #[test]
    fn unrelated_work_observations_do_not_version_unchanged_work_receipt() {
        let mut f = facts();
        let a = linked_work("source-a");
        let mut b = a.clone();
        b.id = "wrk_00000000000000000000000000000002".into();
        let mut own = observation(1, "a-followup", "followup", "current", "source-a");
        own.observation.work_id = Some(a.id.clone());
        own.observation.generation = Some(1);
        f.works = vec![a.clone(), b.clone()];
        f.observations = vec![own];
        let q = ValueMetricsQuery {
            work_id: Some(a.id),
            ..Default::default()
        };
        let before = project(vec![f.clone()], &q, 200).unwrap();
        let mut unrelated = observation(2, "b-followup", "followup", "pending", "source-b");
        unrelated.observation.work_id = Some(b.id);
        unrelated.observation.generation = Some(1);
        f.observations.push(unrelated);
        let b_id = f.works[1].id.clone();
        let mut unrelated_usage = usage(&b_id, "other-owner", "unrelated-report");
        unrelated_usage.request.provider_request_id = Some("unrelated-request".into());
        f.request_membership.insert(
            identity(&["provider", "unrelated-request"]),
            BTreeSet::from([b_id]),
        );
        f.usage.push(unrelated_usage);
        let after = project(vec![f], &q, 200).unwrap();
        assert_eq!(before.assessment_id, after.assessment_id);
        assert_eq!(before.evidence, after.evidence);
    }
    #[test]
    fn explicit_metadata_does_not_make_a_work_eligible_before_its_creation() {
        let mut f = facts();
        let mut work = linked_work("source");
        work.created_at = 180;
        work.updated_at = 180;
        work.last_checkin_at = 180;
        work.current.as_mut().unwrap().at = 180;
        let id = work.id.clone();
        f.works = vec![work];
        let q = ValueMetricsQuery {
            since: Some(100),
            until: Some(150),
            work_id: Some(id),
            ..Default::default()
        };
        let p = project(vec![f], &q, 200).unwrap();
        assert_eq!(p.coverage.eligible_works, Some(0));
        assert!(p.coverage.complete);
        let mut f = facts();
        let mut finished = linked_work("source");
        finished.created_at = 80;
        finished.updated_at = 180;
        finished.state = super::super::work::WorkState::Finished;
        let id = finished.id.clone();
        f.works = vec![finished];
        let after_window = project(
            vec![f],
            &ValueMetricsQuery {
                since: Some(100),
                until: Some(150),
                work_id: Some(id),
                ..Default::default()
            },
            200,
        )
        .unwrap();
        assert_eq!(after_window.coverage.eligible_works, Some(0));
    }
    #[test]
    fn unbound_resolution_inherits_original_episode_work_and_generation() {
        let mut f = facts();
        let work = linked_work("source");
        let id = work.id.clone();
        f.works = vec![work];
        let mut refused = observation(1, "same-episode", "issue", "unresolved", "source");
        refused.observation.work_id = Some(id.clone());
        refused.observation.generation = Some(1);
        let resolved = observation(2, "same-episode", "issue", "resolved", "source");
        f.observations = vec![refused, resolved];
        let q = ValueMetricsQuery {
            work_id: Some(id),
            ..Default::default()
        };
        let current = project(vec![f.clone()], &q, 200).unwrap();
        assert_eq!(current.issues.resolved, 1);
        assert_eq!(current.evidence[0].applicability, "current");
        f.works[0].generation = 2;
        let moved = project(vec![f], &q, 200).unwrap();
        assert_eq!(moved.issues.resolved, 1);
        assert_eq!(moved.evidence[0].applicability, "stale");
    }
    #[test]
    fn old_satisfied_evidence_does_not_become_unknown_but_missing_pending_binding_stays_partial() {
        let mut f = facts();
        f.complete = false;
        let mut o = observation(1, "old", "followup", "current", "source");
        o.observation.work_id = Some("wrk_00000000000000000000000000000001".into());
        o.observation.generation = Some(1);
        f.observations = vec![o];
        let q = ValueMetricsQuery {
            since: Some(150),
            until: Some(200),
            ..Default::default()
        };
        let satisfied = project(vec![f.clone()], &q, 200).unwrap();
        assert_eq!(satisfied.followups.unknown, 0);
        assert!(satisfied.evidence.is_empty());
        assert!(!satisfied.coverage.complete);
        f.complete = true;
        f.observations[0].observation.state = "pending".into();
        let pending = project(vec![f], &q, 200).unwrap();
        assert_eq!(pending.followups.unknown, 1);
        assert_eq!(pending.coverage.observed_works, 0);
        assert_eq!(pending.coverage.eligible_works, None);
        assert!(!pending.coverage.complete);
        assert!(
            pending
                .coverage
                .gaps
                .iter()
                .any(|gap| gap.contains("Current Work metadata is unavailable"))
        );
    }
    #[test]
    fn fresh_admission_observation_brings_old_finished_work_into_period_eligible_cohort() {
        let mut f = facts();
        let mut work = linked_work("source");
        work.state = super::super::work::WorkState::Finished;
        work.created_at = 10;
        work.updated_at = 20;
        let mut o = observation(100, "fresh", "followup", "current", "source");
        o.observation.work_id = Some(work.id.clone());
        o.observation.generation = Some(1);
        f.works = vec![work];
        f.observations = vec![o];
        let p = project(
            vec![f],
            &ValueMetricsQuery {
                since: Some(150),
                until: Some(200),
                ..Default::default()
            },
            200,
        )
        .unwrap();
        assert_eq!(p.followups.current, 1);
        assert_eq!(p.coverage.observed_works, 1);
        assert_eq!(p.coverage.eligible_works, Some(1));
        assert!(p.coverage.complete);
    }
    #[test]
    fn missing_work_metadata_preserves_binding_and_makes_followup_unknown() {
        let mut f = facts();
        let id = "wrk_00000000000000000000000000000001".to_owned();
        let mut o = observation(1, "linked", "followup", "current", "source");
        o.observation.work_id = Some(id.clone());
        o.observation.generation = Some(1);
        f.observations = vec![o];
        let p = project(
            vec![f],
            &ValueMetricsQuery {
                work_id: Some(id.clone()),
                ..Default::default()
            },
            200,
        )
        .unwrap();
        assert_eq!(p.followups.current, 0);
        assert_eq!(p.followups.unknown, 1);
        assert_eq!(p.evidence[0].work_id.as_ref(), Some(&id));
        assert_eq!(p.evidence[0].applicability, "unknown");
        assert_eq!(p.coverage.eligible_works, None);
        assert_eq!(p.issues.dismissed, None);
        assert_eq!(p.issues.false_trigger, None);
        assert_eq!(p.followups.failed, None);
        assert_eq!(p.followups.cancelled, None);
    }
    #[test]
    fn different_incomplete_membership_subsets_keep_public_allocation_and_revision_identical() {
        let mut first = facts();
        let work = linked_work("source");
        let id = work.id.clone();
        first.works = vec![work];
        first.usage = vec![usage(&id, "owner", "own")];
        first.request_membership_complete = false;
        first.request_membership.insert(
            identity(&["provider", "account/request-1"]),
            BTreeSet::from(["first-overflow-subset".into()]),
        );
        let mut second = first.clone();
        second.request_membership.clear();
        second.request_membership.insert(
            identity(&["provider", "account/request-1"]),
            BTreeSet::from(["different-overflow-subset".into(), "another".into()]),
        );
        let q = ValueMetricsQuery {
            work_id: Some(id),
            ..Default::default()
        };
        let a = project(vec![first], &q, 200).unwrap();
        let b = project(vec![second], &q, 200).unwrap();
        assert_eq!(a, b);
        assert_eq!(a.resources.input_tokens, None);
        assert_eq!(a.resources.unallocated_usage_records, 1);
        assert_eq!(a.coverage.eligible_works, None);
    }
    #[test]
    fn incomplete_or_historically_shared_membership_excludes_work_allocation() {
        let mut f = facts();
        let work = linked_work("source");
        let id = work.id.clone();
        f.works = vec![work];
        f.usage = vec![usage(&id, "owner", "request")];
        let q = ValueMetricsQuery {
            work_id: Some(id),
            ..Default::default()
        };
        f.request_membership_complete = false;
        let capped = project(vec![f.clone()], &q, 200).unwrap();
        assert_eq!(capped.resources.input_tokens, None);
        assert!(capped.resources.costs.is_empty());
        assert_eq!(capped.resources.unallocated_usage_records, 1);
        assert!(!capped.coverage.complete);
        f.request_membership_complete = true;
        f.request_membership.insert(
            identity(&["provider", "account/request-1"]),
            BTreeSet::from(["old-other-work".into()]),
        );
        let historical = project(vec![f], &q, 200).unwrap();
        assert_eq!(historical.resources.input_tokens, None);
        assert_eq!(historical.resources.unallocated_usage_records, 1);
    }
    fn linked_work(source: &str) -> WorkItem {
        use super::super::work::{
            Checkpoint, CheckpointKind, ClaimStatus, SourceRef, WorkSource, WorkState,
        };
        WorkItem {
            id: "wrk_00000000000000000000000000000001".into(),
            repository: "example/repo".into(),
            owner: "owner".into(),
            owner_login: "synthetic".into(),
            source: WorkSource::Native {
                request_key: "synthetic".into(),
            },
            title: "Synthetic Work".into(),
            state: WorkState::Active,
            claim_status: ClaimStatus::Active,
            last_checkin_at: 90,
            generation: 1,
            lead_token: "token".into(),
            lead_agent: "client".into(),
            lead_session: "session".into(),
            participants: vec![],
            scope: vec![],
            branch: Some("branch".into()),
            worktree: None,
            next_action: None,
            current: Some(Checkpoint {
                kind: CheckpointKind::Progress,
                summary: "Synthetic source".into(),
                next_action: None,
                source_refs: vec![SourceRef::Commit {
                    oid: source.into(),
                    path: None,
                }],
                at: 90,
                token: "token".into(),
                agent: "client".into(),
                session: "session".into(),
                generation: 1,
            }),
            blocker: None,
            depends_on: vec![],
            related_work: vec![],
            created_at: 80,
            updated_at: 90,
            created_seq: 1,
            updated_seq: 2,
        }
    }
    #[test]
    fn linked_work_source_and_generation_changes_invalidate_retained_followup_without_new_events() {
        use super::super::work::SourceRef;
        let mut f = facts();
        f.works = vec![linked_work("source-a")];
        let id = f.works[0].id.clone();
        let mut o = observation(1, "followup", "followup", "current", "source-a");
        o.observation.work_id = Some(id.clone());
        o.observation.generation = Some(1);
        f.observations = vec![o];
        let q = ValueMetricsQuery {
            work_id: Some(id),
            ..Default::default()
        };
        let a = project(vec![f.clone()], &q, 200).unwrap();
        assert_eq!(a.followups.current, 1);
        assert_eq!(a.evidence[0].applicability, "current");
        f.works[0].current.as_mut().unwrap().source_refs = vec![SourceRef::Commit {
            oid: "source-b".into(),
            path: None,
        }];
        let b = project(vec![f.clone()], &q, 200).unwrap();
        assert_eq!(b.followups.current, 0);
        assert_eq!(b.followups.stale, 1);
        assert_eq!(b.evidence[0].applicability, "stale");
        assert_ne!(a.assessment_id, b.assessment_id);
        f.works[0].current.as_mut().unwrap().source_refs.clear();
        let unknown = project(vec![f.clone()], &q, 200).unwrap();
        assert_eq!(unknown.followups.unknown, 1);
        assert_eq!(unknown.evidence[0].applicability, "unknown");
        f.works[0].generation = 2;
        let next = project(vec![f], &q, 200).unwrap();
        assert_eq!(next.followups.stale, 1);
        assert_ne!(unknown.assessment_id, next.assessment_id);
    }
    #[test]
    fn context_detail_pages_advance_once_per_source_event_from_one_delivery() {
        use super::super::catchup::{CatchupRetention, CatchupWorkEvent};
        let mut f = facts();
        let work = linked_work("source-a");
        let page = CatchupPage {
            subscriber: "agent:synthetic".into(),
            delivery_client: "mcp".into(),
            delivery: "delivery".into(),
            event_from: 0,
            event_through: 25,
            event_after: 0,
            next_event_after: 25,
            merge_from: 0,
            merge_through: 0,
            merge_after: 0,
            next_merge_after: 0,
            work_events: (1..=25)
                .map(|seq| CatchupWorkEvent {
                    seq,
                    at: 100,
                    kind: "checkpoint".into(),
                    work_id: work.id.clone(),
                    item: work.clone(),
                })
                .collect(),
            coordination_events: vec![],
            merges: vec![],
            retention: CatchupRetention {
                work_gap: false,
                coordination_gap: false,
                merge_gap: false,
                event_available_from: 0,
                merge_available_from: 0,
            },
            source_coverage: vec![],
            complete: true,
        };
        f.deliveries = vec![ValueDelivery {
            token_namespace: "namespace".into(),
            id: "delivery".into(),
            at: 100,
            acknowledged: false,
            page,
        }];
        let mut q = ValueMetricsQuery {
            since: Some(0),
            until: Some(200),
            limit: Some(7),
            ..Default::default()
        };
        let mut seen = BTreeSet::new();
        let mut assessments = BTreeSet::new();
        let mut pages = 0;
        loop {
            let value = project(vec![f.clone()], &q, 200).unwrap();
            assessments.insert(value.assessment_id.clone());
            for e in value.evidence {
                assert!(seen.insert(e.id));
            }
            pages += 1;
            assert!(pages <= 4);
            match value.next_after {
                Some(cursor) => q.after = Some(cursor),
                None => break,
            }
        }
        assert_eq!(seen.len(), 25);
        assert_eq!(pages, 4);
        assert_eq!(assessments.len(), 1);
    }
    #[test]
    fn merge_attribution_changes_assessment_even_when_coverage_is_already_incomplete() {
        let mut f = facts();
        let work = linked_work("head");
        let id = work.id.clone();
        f.works = vec![work];
        f.merges = vec![ValueMerge {
            id: "merge".into(),
            at: 100,
            head: "head".into(),
            merge_commit: "merge-commit".into(),
            source: "provider".into(),
            pull_number: 1,
        }];
        // Retention already makes overall coverage incomplete in both projections.
        f.delivery_retained_from = Some(100);
        let q = ValueMetricsQuery {
            since: Some(0),
            until: Some(200),
            ..Default::default()
        };
        let attributed = project(vec![f.clone()], &q, 200).unwrap();
        let stable = project(vec![f.clone()], &q, 200).unwrap();
        assert_eq!(attributed, stable);
        assert!(!attributed.coverage.complete);
        assert_eq!(attributed.evidence[0].work_id.as_deref(), Some(id.as_str()));
        // Only per-fact completeness changes; the merge record and global coverage
        // stay the same, while aggregate attribution must be withheld and versioned.
        f.complete = false;
        let unattributed = project(vec![f.clone()], &q, 200).unwrap();
        let stable = project(vec![f], &q, 200).unwrap();
        assert_eq!(unattributed, stable);
        assert!(!unattributed.coverage.complete);
        assert_eq!(attributed.coverage.gaps, unattributed.coverage.gaps);
        assert!(unattributed.evidence[0].work_id.is_none());
        assert_ne!(attributed.assessment_id, unattributed.assessment_id);
    }
    #[test]
    fn unknown_resources_caps_and_merge_watermark_are_explicit() {
        let mut f = facts();
        f.cap_rejections = 1;
        f.complete = false;
        let a = project(vec![f.clone()], &Default::default(), 200).unwrap();
        assert_eq!(a.resources.input_tokens, None);
        assert_eq!(a.coverage.eligible_works, None);
        assert!(!a.coverage.complete);
        assert!(a.coverage.gaps.iter().any(|g| g.contains("cap rejected")));
        f.merges.push(ValueMerge {
            id: "merge".into(),
            at: 100,
            head: "head".into(),
            merge_commit: "merge-commit".into(),
            source: "provider".into(),
            pull_number: 1,
        });
        let b = project(vec![f.clone()], &Default::default(), 200).unwrap();
        let c = project(vec![f], &Default::default(), 201).unwrap();
        assert_ne!(a.assessment_id, b.assessment_id);
        assert_eq!(b.assessment_id, c.assessment_id);
        assert_eq!(b.merge_observations, 1);
    }
}
