//! Shared deterministic attention facts. Reports, provider evidence and ownership stay separate.
//!
//! This bounded read is advisory, never completion or edit authority. Its digest identifies the
//! captured facts, not a transactional snapshot or current provider evidence omitted by V2.
use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use crate::hub::coordination::{CoordinationInbox, CoordinationListQuery, CoordinationStatus};
use crate::hub::store::{Principal, Store};
use crate::hub::value_metrics::identity;
use crate::hub::work::{ClaimStatus, WorkItem, WorkOverviewQuery, WorkRole, WorkState};
use crate::hub::work_lifecycle::{ReservationStatus, WorkOperationalV1};
use crate::hub::work_reconciliation::ReconciliationView;
use crate::hub::work_transition::{OperationalSelect, WorkLifecycleReader};
use crate::hub::{HubError, Result};

#[path = "actionable_conflicts.rs"]
mod conflicts;

pub const MAX_STATUS_WORK: usize = 100;

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum StatusState {
    Active,
    AwaitingParty,
    Blocked,
    ReassessmentNeeded,
    Retired,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RecipientKind {
    Person,
    LeadAgent,
    Unknown,
    None,
}

/// The stable Hub owner identity is authoritative; login and agent are display labels only.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct StatusRecipient {
    pub kind: RecipientKind,
    pub owner: Option<String>,
    pub login: Option<String>,
    pub agent: Option<String>,
}

impl StatusRecipient {
    fn none() -> Self {
        Self {
            kind: RecipientKind::None,
            owner: None,
            login: None,
            agent: None,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct StatusOperational {
    pub outcome: crate::hub::work_lifecycle::OutcomeV1,
    pub report: Option<crate::hub::work_lifecycle::ReportV1>,
    pub reservation: StatusReservation,
    pub merge: crate::hub::work_lifecycle::MergeV1,
    /// Identity of the complete operational facts, including omitted scope paths.
    pub digest: String,
}
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct StatusReservation {
    pub status: ReservationStatus,
    pub basis: Option<crate::hub::work_lifecycle::ReservationBasis>,
    pub holder_agent: Option<String>,
    pub scope_count: usize,
}
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct StatusReconciliation {
    pub current: bool,
    pub declaration_seq: i64,
    pub current_evidence_digest: Option<String>,
    pub current_source_coverage: Vec<crate::hub::work_reconciliation::SourceEvidence>,
    pub stale_reasons: Vec<String>,
    /// Identity of the full view; snapshots remain on the Work detail endpoint.
    pub digest: String,
}
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct StatusWork {
    pub work_id: String,
    pub title: String,
    pub generation: i64,
    pub updated_seq: i64,
    pub state: StatusState,
    pub recipient: StatusRecipient,
    pub needs_you: bool,
    /// Deterministic instruction, distinct from the untrusted reported next action below.
    pub action: String,
    pub reported_next_action: Option<String>,
    pub freshness: ClaimStatus,
    pub operational: StatusOperational,
    /// Historical retained results are never promoted to current by this projection.
    pub reconciliation: Option<StatusReconciliation>,
    pub limits: Vec<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct StatusAction {
    pub id: String,
    pub kind: String,
    pub work_ids: Vec<String>,
    pub recipients: Vec<StatusRecipient>,
    pub needs_you: bool,
    pub action: String,
    pub updated_seq: i64,
    /// Semantic version for actions without a Work event sequence (for example forge reads).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub semantic_version: Option<String>,
    pub limits: Vec<String>,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct StatusCounts {
    pub active: usize,
    pub awaiting_party: usize,
    pub blocked: usize,
    pub reassessment_needed: usize,
    pub retired: usize,
    pub held_reservations: usize,
    pub needs_you: usize,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ActionableStatus {
    pub schema_version: u16,
    pub repository: String,
    pub repository_id: Option<i64>,
    pub provider: String,
    pub host: String,
    pub viewer_owner: String,
    pub work: Vec<StatusWork>,
    pub actions: Vec<StatusAction>,
    pub counts: StatusCounts,
    /// Read time, excluded from `digest`. Individual provider/report times stay in their facts.
    pub observed_at: i64,
    pub complete: bool,
    pub atomic_snapshot: bool,
    pub limits: Vec<String>,
    /// All captured status/evidence fields except read time and this digest itself.
    pub digest: String,
}

fn lead(item: &WorkItem) -> StatusRecipient {
    match item.participants.iter().find(|p| {
        p.role == WorkRole::Lead
            && p.token == item.lead_token
            && p.session == item.lead_session
            && p.agent == item.lead_agent
    }) {
        Some(p) => StatusRecipient {
            kind: RecipientKind::LeadAgent,
            owner: Some(p.owner.clone()),
            login: Some(p.login.clone()),
            agent: Some(p.agent.clone()),
        },
        None => StatusRecipient {
            kind: RecipientKind::Unknown,
            owner: None,
            login: None,
            agent: Some(item.lead_agent.clone()),
        },
    }
}

fn owner(item: &WorkItem) -> StatusRecipient {
    StatusRecipient {
        kind: RecipientKind::Person,
        owner: Some(item.owner.clone()),
        login: Some(item.owner_login.clone()),
        agent: None,
    }
}

/// A report is routed to its named lead/owner, never to every reader of the repository.
pub fn project_work(
    item: &WorkItem,
    operational: WorkOperationalV1,
    reconciliation: Option<ReconciliationView>,
    viewer: &str,
) -> StatusWork {
    let mut limits = BTreeSet::new();
    let held = operational.reservation.status == ReservationStatus::Held;
    let consistent = item.generation == operational.generation
        && item.updated_seq == operational.updated_seq
        && reconciliation
            .as_ref()
            .is_none_or(|r| r.generation == item.generation && r.updated_seq == item.updated_seq);
    if !consistent {
        limits.insert("work_changed_during_read".to_owned());
    }
    let current = reconciliation
        .as_ref()
        .filter(|r| r.generation == item.generation && r.updated_seq == item.updated_seq)
        .and_then(|r| r.current.as_ref());
    if let Some(r) = &reconciliation {
        if r.generation != item.generation || r.updated_seq != item.updated_seq {
            limits.insert("reconciliation_work_changed".to_owned());
        }
        limits.extend(
            r.stale_reasons
                .iter()
                .map(|s| format!("reconciliation:{s}")),
        );
    } else {
        limits.insert("reconciliation_unavailable".to_owned());
    }
    if let Some(r) = current {
        limits.extend(r.limits.iter().cloned());
    }
    // Older V2 responses may lack the fresh evidence identity. Never infer it from history.
    if reconciliation
        .as_ref()
        .is_none_or(|r| r.current_evidence_digest.is_none())
    {
        limits.insert("current_provider_evidence_not_exposed".to_owned());
    }
    if let Some(r) = &reconciliation {
        for source in ["forge_pull", "tracked_publication"] {
            match r
                .current_source_coverage
                .iter()
                .find(|s| s.source == source)
                .map(|s| s.state.as_str())
            {
                Some("observed_only") => {}
                Some("unavailable") => {
                    limits.insert("provider_coverage_unavailable".into());
                }
                Some("incomplete") => {
                    limits.insert("provider_coverage_incomplete".into());
                }
                _ => {
                    limits.insert("provider_coverage_unknown".into());
                }
            }
        }
    }
    let (state, recipient, action) = if !consistent {
        (
            StatusState::ReassessmentNeeded,
            StatusRecipient::none(),
            "Refresh changed Work before acting.",
        )
    } else if item.state == WorkState::Finished {
        (
            StatusState::Retired,
            StatusRecipient::none(),
            "Inspect the recorded outcome and its provenance.",
        )
    } else if current.is_some_and(|r| !r.reassessments.is_empty()) {
        (
            StatusState::ReassessmentNeeded,
            lead(item),
            "Reassess the reported blocker against current prerequisite evidence.",
        )
    } else if operational.merge.hidden_by_merged_pr && held {
        (
            StatusState::ReassessmentNeeded,
            owner(item),
            "Decide whether to retain, release or finish the held reservation; merge alone does not complete Work.",
        )
    } else if item.state == WorkState::Handover {
        (
            StatusState::AwaitingParty,
            owner(item),
            "Review the handover and choose the next responsible party.",
        )
    } else if item.state == WorkState::Blocked {
        (
            StatusState::Blocked,
            lead(item),
            "Review the reported blocker and identify its next action.",
        )
    } else if item.state == WorkState::Paused || item.claim_status != ClaimStatus::Active {
        (
            StatusState::ReassessmentNeeded,
            if held {
                owner(item)
            } else {
                StatusRecipient::none()
            },
            "Review the unfinished Work; a quiet claim does not prove failure or completion.",
        )
    } else {
        (
            StatusState::Active,
            lead(item),
            "Continue the reported work; activity is not independently observed.",
        )
    };
    if recipient.kind == RecipientKind::Unknown {
        limits.insert("recipient_unknown".into());
    }
    let needs_you = consistent
        && state != StatusState::Active
        && state != StatusState::Retired
        && recipient.owner.as_deref() == Some(viewer);
    StatusWork {
        work_id: item.id.clone(),
        title: item.title.clone(),
        generation: item.generation,
        updated_seq: item.updated_seq,
        state,
        recipient,
        needs_you,
        action: action.into(),
        reported_next_action: item.next_action.clone(),
        freshness: item.claim_status,
        operational: StatusOperational {
            digest: identity(&[
                &serde_json::to_string(&operational).expect("operational serializes")
            ]),
            outcome: operational.outcome,
            report: operational.report,
            merge: operational.merge,
            reservation: StatusReservation {
                status: operational.reservation.status,
                basis: operational.reservation.basis,
                holder_agent: operational.reservation.holder_agent,
                scope_count: operational.reservation.scope.len(),
            },
        },
        reconciliation: reconciliation.map(|r| StatusReconciliation {
            digest: identity(&[&serde_json::to_string(&r).expect("reconciliation serializes")]),
            current: r.current.is_some(),
            declaration_seq: r.declaration_seq,
            current_evidence_digest: r.current_evidence_digest,
            current_source_coverage: r.current_source_coverage,
            stale_reasons: r.stale_reasons,
        }),
        limits: limits.into_iter().collect(),
    }
}

/// Read through the caller's authenticated repository scope. Counts are lower bounds when any
/// collection is truncated. Reservation holders are selected first, including legacy-hidden Work.
pub fn read(store: &dyn Store, principal: &Principal, now: i64) -> Result<ActionableStatus> {
    read_scoped(
        store,
        principal,
        &WorkLifecycleReader::Token(&principal.token_id),
        now,
    )
}

/// Internal role-checked person read. Never infer person authority from an empty token.
pub(crate) fn read_person(
    store: &dyn Store,
    owner: &crate::hub::store::User,
    repository: &str,
    repository_id: i64,
    now: i64,
) -> Result<ActionableStatus> {
    if owner.disabled {
        return Err(HubError::Unauthorized);
    }
    let principal = Principal {
        token_id: String::new(),
        name: String::new(),
        repository: repository.into(),
        repository_id: Some(repository_id),
        expires_at: now,
        read_only: true,
        owner: owner.clone(),
    };
    read_scoped(
        store,
        &principal,
        &WorkLifecycleReader::Person {
            owner,
            repository,
            repository_id,
        },
        now,
    )
}

fn read_scoped(
    store: &dyn Store,
    principal: &Principal,
    reader: &WorkLifecycleReader<'_>,
    now: i64,
) -> Result<ActionableStatus> {
    let query = WorkOverviewQuery {
        recent_since: None,
        limit: MAX_STATUS_WORK,
    };
    let overview = match reader {
        WorkLifecycleReader::Token(token) => store.work_overview(token, &query, now)?,
        WorkLifecycleReader::Person {
            owner,
            repository,
            repository_id,
        } => store.repository_work_overview(owner, repository, *repository_id, &query, now)?,
    };
    let holders = store.work_operational(reader, &OperationalSelect::Holders, now)?;
    let requests = store.coordination_list_scoped(
        reader,
        &CoordinationListQuery {
            active_only: true,
            ..Default::default()
        },
        now,
    )?;
    let mut limits = BTreeSet::new();
    if !overview.current_complete {
        limits.insert("work_truncated".to_owned());
    }
    if !overview.overlaps_complete {
        limits.insert("overlaps_truncated".to_owned());
    }
    if !holders.complete {
        limits.insert("reservation_holders_truncated".to_owned());
    }
    if !requests.complete {
        limits.insert("requests_truncated".to_owned());
    }
    if requests.retention_gap {
        limits.insert("request_history_gap".to_owned());
    }
    // These producers remain separate issue slices, never inferred from Work checkpoint prose.
    limits.insert("failed_check_attention_not_integrated".to_owned());
    limits.insert("capacity_attention_not_integrated".to_owned());

    let mut selected: BTreeMap<String, Option<WorkItem>> = holders
        .work
        .iter()
        .map(|w| (w.work_id.clone(), None))
        .collect();
    for item in &overview.current {
        if selected.contains_key(&item.id) || selected.len() < MAX_STATUS_WORK {
            selected.insert(item.id.clone(), Some(item.clone()));
        } else {
            limits.insert("work_truncated".to_owned());
        }
    }
    let ids = selected.keys().cloned().collect::<Vec<_>>();
    let operational = store.work_operational(reader, &OperationalSelect::Many(&ids), now)?;
    let mut work = Vec::new();
    let mut items = BTreeMap::new();
    for (id, item) in selected {
        let item = match item {
            Some(i) => Some(i),
            None => store.work_get_scoped(reader, &id, now)?,
        };
        let Some(item) = item else {
            limits.insert("work_changed_during_read".into());
            continue;
        };
        let Some(op) = operational.work.iter().find(|op| op.work_id == id) else {
            limits.insert("work_changed_during_read".into());
            continue;
        };
        let reconciliation = match store.work_reconciliation_get_scoped(reader, &id, now) {
            Ok(r) => r,
            Err(HubError::Unauthorized) => return Err(HubError::Unauthorized),
            Err(_) => {
                limits.insert("reconciliation_unavailable".into());
                None
            }
        };
        let mut row = project_work(&item, op.clone(), reconciliation, &principal.owner.id);
        // Fence the row with the final Work version. This does not claim an atomic snapshot.
        if store
            .work_get_scoped(reader, &id, now)?
            .is_none_or(|i| i.updated_seq != item.updated_seq || i.generation != item.generation)
        {
            row.limits.push("work_changed_during_read".into());
            row.needs_you = false;
            row.state = StatusState::ReassessmentNeeded;
            row.recipient = StatusRecipient::none();
            row.action = "Refresh changed Work before acting.".into();
        }
        if !row.limits.iter().any(|limit| {
            limit == "work_changed_during_read" || limit == "reconciliation_work_changed"
        }) {
            items.insert(id, item);
        }
        work.push(row);
    }
    for request in &requests.requests {
        if !request.responses_complete {
            limits.insert("request_responses_truncated".into());
        }
        if request
            .targets
            .iter()
            .any(|target| !items.contains_key(&target.work_id))
        {
            limits.insert("request_target_work_not_read".into());
        }
    }
    let mut actions = request_actions(&requests, &items, &principal.owner.id);
    for overlap in &overview.overlaps {
        let ids = [&overlap.left.id, &overlap.right.id];
        let pair = ids
            .iter()
            .filter_map(|id| items.get(*id))
            .collect::<Vec<_>>();
        if pair.len() != 2
            || pair.iter().any(|i| {
                i.claim_status != ClaimStatus::Active
                    || !matches!(i.state, WorkState::Active | WorkState::Blocked)
            })
        {
            continue;
        }
        let recipients = pair.iter().map(|i| owner(i)).collect::<Vec<_>>();
        let mut work_ids = ids.iter().map(|id| (*id).clone()).collect::<Vec<_>>();
        work_ids.sort();
        actions.push(StatusAction {
            id: format!("overlap:{}:{}", work_ids[0], work_ids[1]),
            kind: "scope_overlap".into(),
            work_ids,
            needs_you: recipients
                .iter()
                .any(|r| r.owner.as_deref() == Some(&principal.owner.id)),
            recipients,
            action: "Resolve the overlapping declared scopes with the named owners.".into(),
            semantic_version: None,
            updated_seq: pair.iter().map(|i| i.updated_seq).max().unwrap_or(0),
            limits: vec![],
        });
    }
    let requests_after = store.coordination_list_scoped(
        reader,
        &CoordinationListQuery {
            active_only: true,
            ..Default::default()
        },
        now,
    )?;
    if requests != requests_after {
        limits.insert("requests_changed_during_read".into());
        actions.retain(|a| a.kind != "directed_request");
    }
    let (conflicts, conflict_limits, conflict_evidence_digest) =
        conflicts::read(store, principal, now)?;
    actions.extend(conflicts);
    limits.extend(conflict_limits);
    work.sort_by(|a, b| a.work_id.cmp(&b.work_id));
    actions.sort_by(|a, b| a.id.cmp(&b.id));
    let complete_action_digest =
        identity(&[&serde_json::to_string(&actions).expect("actions serialize")]);
    actions.dedup_by(|a, b| a.id == b.id);
    if actions.len() > 200 {
        limits.insert("status_actions_truncated".into());
        actions.truncate(200);
    }
    for action in &mut actions {
        if action.recipients.len() > 8 {
            action
                .recipients
                .sort_by_key(|r| r.owner.as_deref() != Some(&principal.owner.id));
            action.recipients.truncate(8);
            action.limits.push("recipients_truncated".into());
            limits.insert("recipients_truncated".into());
        }
    }
    let mut counts = StatusCounts::default();
    for row in &work {
        match row.state {
            StatusState::Active => counts.active += 1,
            StatusState::AwaitingParty => counts.awaiting_party += 1,
            StatusState::Blocked => counts.blocked += 1,
            StatusState::ReassessmentNeeded => counts.reassessment_needed += 1,
            StatusState::Retired => counts.retired += 1,
        }
        counts.held_reservations +=
            usize::from(row.operational.reservation.status == ReservationStatus::Held);
        counts.needs_you += usize::from(row.needs_you);
    }
    counts.needs_you += actions.iter().filter(|a| a.needs_you).count();
    let complete = limits.is_empty() && work.iter().all(|w| w.limits.is_empty());
    let mut result = ActionableStatus {
        schema_version: 1,
        repository: principal.repository.clone(),
        repository_id: principal.repository_id,
        provider: principal.owner.identity.provider.clone(),
        host: principal.owner.identity.host.clone(),
        viewer_owner: principal.owner.id.clone(),
        work,
        actions,
        counts,
        observed_at: now,
        complete,
        atomic_snapshot: false,
        limits: limits.into_iter().collect(),
        digest: String::new(),
    };
    // The existing agent CLI accepts at most 1 MiB. Preserve every selected Work/reservation
    // row and bound displayed actions below that transport limit. Their full identity remains
    // in complete_action_digest, so omission never makes changed captured facts look current.
    let mut response_bytes = serde_json::to_vec(&result)
        .expect("status serializes")
        .len();
    if response_bytes > 768 * 1024 {
        while response_bytes > 767 * 1024 {
            let Some(action) = result.actions.pop() else {
                return Err(HubError::Unavailable(
                    "shared status exceeds its response budget".into(),
                ));
            };
            response_bytes = response_bytes.saturating_sub(
                serde_json::to_vec(&action)
                    .expect("action serializes")
                    .len(),
            );
            result.counts.needs_you = result
                .counts
                .needs_you
                .saturating_sub(usize::from(action.needs_you));
        }
        result.complete = false;
        result
            .limits
            .push("status_response_budget_truncated".into());
        result.limits.sort();
    }
    // Include complete request versions, even omitted/no-longer-actionable request bodies, so a
    // changed recipient/response invalidates the captured evidence. No credential is included.
    let mut hashed = result.clone();
    hashed.observed_at = 0;
    result.digest = identity(&[&serde_json::to_string(&(
        hashed,
        requests,
        complete_action_digest,
        conflict_evidence_digest,
    ))
    .expect("status serializes")]);
    Ok(result)
}

/// Deterministic text used by agent-facing clients; generated prose is optional.
pub fn recipient_label(recipient: &StatusRecipient) -> String {
    match (&recipient.login, recipient.kind) {
        (Some(login), RecipientKind::Person) => login.clone(),
        (Some(login), RecipientKind::LeadAgent) => format!(
            "{login}’s {}",
            recipient.agent.as_deref().unwrap_or("agent")
        ),
        (_, RecipientKind::None) => "No action assigned".into(),
        _ => "Recipient unknown".into(),
    }
}

/// Short startup/query copy; the structured facts retain full captured counts and evidence.
pub fn render_compact_text(status: &ActionableStatus) -> String {
    let mut bounded = status.clone();
    bounded.work.sort_by_key(|w| !w.needs_you);
    bounded.work.truncate(6);
    bounded.actions.retain(|a| a.needs_you);
    bounded.actions.truncate(6);
    let mut text = render_text(&bounded);
    if status.work.len() > 6 || status.actions.iter().filter(|a| a.needs_you).count() > 6 {
        text.push_str("\nShowing at most six tasks and six personal actions; full captured facts and coverage remain in the structured status read.");
    }
    text
}

pub fn render_text(status: &ActionableStatus) -> String {
    let c = &status.counts;
    let mut lines = vec![format!(
        "{}: {} active, {} awaiting a named party, {} blocked, {} reassessment needed; {} actions need you.",
        status.repository,
        c.active,
        c.awaiting_party,
        c.blocked,
        c.reassessment_needed,
        c.needs_you
    )];
    for row in &status.work {
        let state = match row.state {
            StatusState::Active => "active",
            StatusState::AwaitingParty => "awaiting a named party",
            StatusState::Blocked => "blocked",
            StatusState::ReassessmentNeeded => "reassessment needed",
            StatusState::Retired => "retired",
        };
        let recipient = recipient_label(&row.recipient);
        lines.push(format!(
            "{} [{}; {}; reservation {:?}]: {}",
            row.title, state, recipient, row.operational.reservation.status, row.action
        ));
        lines.push(format!(
            "  Claim {:?}; outcome {:?} ({:?}). {}",
            row.freshness,
            row.operational.outcome.value,
            row.operational.outcome.provenance,
            row.operational
                .report
                .as_ref()
                .map(|r| format!("Agent report {:?} at {}.", r.kind, r.at))
                .unwrap_or_else(|| "No retained agent report.".into())
        ));
        if let Some(r) = &row.reconciliation {
            for source in &r.current_source_coverage {
                lines.push(format!(
                    "  Provider source {}: {}; checked {} ({} unread pages, {} deferred).",
                    source.source,
                    source.state,
                    source.checked_at_seconds,
                    source.unread_pages,
                    source.deferred
                ));
            }
            if !r.current {
                lines.push(
                    "  No current reconciliation synthesis; retained reports are history.".into(),
                );
            }
        } else {
            lines.push("  Provider coverage unavailable.".into());
        }
    }
    for action in &status.actions {
        if action.needs_you {
            lines.push(action.action.clone());
        }
    }
    if !status.complete {
        lines.push("Coverage incomplete; missing signals remain unknown.".into());
    }
    lines.push(format!(
        "Evidence revision {} (read as of {}; non-atomic; provider times shown separately).",
        status.digest, status.observed_at
    ));
    lines.join("\n")
}

fn request_actions(
    inbox: &CoordinationInbox,
    items: &BTreeMap<String, WorkItem>,
    viewer: &str,
) -> Vec<StatusAction> {
    let mut actions = Vec::new();
    for request in &inbox.requests {
        if matches!(
            request.status,
            CoordinationStatus::Resolved | CoordinationStatus::Withdrawn
        ) {
            continue;
        }
        for target in &request.targets {
            let Some(item) = items.get(&target.work_id) else {
                continue;
            };
            // A request against an old lead/generation is not a current obligation. Never use a
            // stale response or incomplete response page as proof an answer is still missing.
            if !request.responses_complete
                || target.generation != item.generation
                || target.lead.token != item.lead_token
                || target.lead.session != item.lead_session
                || target.lead.agent != item.lead_agent
            {
                continue;
            }
            if request.responses.iter().any(|r| {
                r.target_work_id == target.work_id
                    && r.target_generation == target.generation
                    && r.responder == target.lead
            }) {
                continue;
            }
            let mut recipient = lead(item);
            let idle = item.state == WorkState::Paused || item.claim_status != ClaimStatus::Active;
            if idle && recipient.kind == RecipientKind::LeadAgent {
                recipient.kind = RecipientKind::Person;
                recipient.agent = None;
            }
            actions.push(StatusAction {
                id: format!("request:{}:{}", request.id, target.work_id),
                kind: "directed_request".into(),
                work_ids: vec![target.work_id.clone()],
                needs_you: idle && recipient.owner.as_deref() == Some(viewer),
                recipients: vec![recipient],
                action: "Answer the outstanding directed coordination request.".into(),
                updated_seq: request.updated_seq,
                semantic_version: None,
                limits: if target.reasons_complete {
                    vec![]
                } else {
                    vec!["request_reasons_truncated".into()]
                },
            });
        }
    }
    actions
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hub::store::{self, ForgeIdentity, agent_token_digest, mint_bound_agent_token};
    use crate::hub::work::{CheckpointKind, WorkCheckpoint, WorkStart};
    use crate::hub::work_lifecycle::{
        MergeStatus, OperationalFacts, ReservationBasis, work_operational,
    };
    use crate::hub::work_reconciliation::{
        DurableReconciliation, ReassessmentRequest, ReconciliationEvidence,
    };
    use serde_json::json;

    fn item(state: WorkState) -> WorkItem {
        serde_json::from_value(json!({
            "id":"wrk_aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa", "repository":"acme/widgets",
            "owner":"usr_owner", "owner_login":"owner", "source":{"kind":"native","request_key":"synthetic"},
            "title":"Synthetic task", "state":state, "claim_status":"active", "last_checkin_at":100,
            "generation":1, "lead_token":"tok_lead", "lead_agent":"agent", "lead_session":"session",
            "participants":[{"owner":"usr_lead","login":"lead","token":"tok_lead","agent":"agent","session":"session","role":"lead"}],
            "scope":["src/example.rs"], "branch":null,"worktree":null,"next_action":null,"current":null,"blocker":null,
            "depends_on":[],"related_work":[],"created_at":100,"updated_at":100,"created_seq":1,"updated_seq":1
        })).unwrap()
    }
    fn op(item: &WorkItem, held: bool, merged: bool) -> WorkOperationalV1 {
        work_operational(&OperationalFacts {
            item,
            outcome: None,
            basis: held.then_some(ReservationBasis::Admitted),
            hidden_by_merged_pr: merged,
            merge: if merged {
                MergeStatus::Observed
            } else {
                MergeStatus::Unknown
            },
        })
    }
    #[test]
    fn other_persons_blocker_is_not_a_viewer_obligation() {
        let item = item(WorkState::Blocked);
        let row = project_work(&item, op(&item, true, false), None, "usr_viewer");
        assert_eq!(row.state, StatusState::Blocked);
        assert_eq!(row.recipient.owner.as_deref(), Some("usr_lead"));
        assert!(!row.needs_you);
        assert!(project_work(&item, op(&item, true, false), None, "usr_lead").needs_you);
        assert!(
            row.limits
                .contains(&"current_provider_evidence_not_exposed".into())
        );
    }
    #[test]
    fn handover_names_a_person_and_preserves_reservation() {
        let item = item(WorkState::Handover);
        let row = project_work(&item, op(&item, true, false), None, "usr_owner");
        assert_eq!(row.state, StatusState::AwaitingParty);
        assert_eq!(row.recipient.kind, RecipientKind::Person);
        assert_eq!(row.recipient.login.as_deref(), Some("owner"));
        assert!(row.needs_you);
        assert_eq!(row.operational.reservation.status, ReservationStatus::Held);
    }
    #[test]
    fn quiet_or_released_work_is_not_failed_or_completed() {
        let mut item = item(WorkState::Active);
        item.claim_status = ClaimStatus::Stale;
        let row = project_work(&item, op(&item, false, false), None, "usr_owner");
        assert_eq!(row.state, StatusState::ReassessmentNeeded);
        assert_eq!(row.recipient.kind, RecipientKind::None);
        assert!(!row.needs_you);
        assert_eq!(
            row.operational.outcome.value,
            crate::hub::work_lifecycle::OutcomeValue::None
        );
    }
    #[test]
    fn merge_does_not_retire_a_held_reservation_and_finished_does() {
        let item = item(WorkState::Active);
        let row = project_work(&item, op(&item, true, true), None, "usr_owner");
        assert_eq!(row.state, StatusState::ReassessmentNeeded);
        assert!(row.needs_you);
        let mut finished = item;
        finished.state = WorkState::Finished;
        let row = project_work(&finished, op(&finished, true, true), None, "usr_owner");
        assert_eq!(row.state, StatusState::Retired);
        assert_eq!(row.operational.reservation.status, ReservationStatus::None);
        assert!(!row.needs_you);
    }
    fn reassessment(item: &WorkItem) -> ReconciliationView {
        ReconciliationView {
            schema_version: 2,
            work_id: item.id.clone(),
            generation: item.generation,
            updated_seq: item.updated_seq,
            declaration_seq: 1,
            current_evidence_digest: Some("synthetic-evidence".into()),
            current_source_coverage: vec![],
            retained: None,
            current: Some(DurableReconciliation {
                work_id: item.id.clone(),
                generation: item.generation,
                work_seq: item.updated_seq,
                declaration_seq: 1,
                evidence_digest: "synthetic-evidence".into(),
                conditions: vec![],
                links: vec![],
                reassessments: vec![ReassessmentRequest {
                    id: "reassess".into(),
                    condition_id: "condition".into(),
                    blocker_report_digest: "report".into(),
                    reason: "Prerequisite changed".into(),
                }],
                reservation_disposition: "held".into(),
                reservation_remedies: vec![],
                next_action: "Reassess".into(),
                limits: vec![],
                evidence: ReconciliationEvidence {
                    provider: "github".into(),
                    host: "github.com".into(),
                    repository_id: Some(101),
                    merge_through: 1,
                    merge_floor: 0,
                    sources: vec![],
                    pulls: vec![],
                    prerequisites: vec![],
                },
            }),
            stale_reasons: vec![],
        }
    }
    #[test]
    fn only_current_matching_reconciliation_can_request_reassessment() {
        let item = item(WorkState::Blocked);
        let view = reassessment(&item);
        let row = project_work(
            &item,
            op(&item, true, false),
            Some(view.clone()),
            "usr_lead",
        );
        assert_eq!(row.state, StatusState::ReassessmentNeeded);
        let mut stale = view;
        stale.updated_seq += 1;
        let row = project_work(&item, op(&item, true, false), Some(stale), "usr_lead");
        assert_eq!(row.state, StatusState::ReassessmentNeeded);
        assert!(!row.needs_you);
        assert!(row.limits.contains(&"reconciliation_work_changed".into()));
    }
    #[test]
    fn fresh_evidence_identity_is_exposed_without_promoting_stale_evaluation() {
        use crate::hub::work_reconciliation::{
            WorkDeclarations, WorkDeclarationsView, reconciliation_view,
        };
        let item = item(WorkState::Blocked);
        let declarations = WorkDeclarationsView {
            schema_version: 2,
            work_id: item.id.clone(),
            generation: item.generation,
            updated_seq: item.updated_seq,
            declaration_seq: 1,
            compatibility_link: None,
            declarations: WorkDeclarations::default(),
        };
        let mut evidence: ReconciliationEvidence = serde_json::from_value(json!({
            "provider":"github", "host":"github.com", "repository_id":1,
            "merge_through":0,"merge_floor":0,"sources":[],"pulls":[],"prerequisites":[]
        }))
        .unwrap();
        let never = reconciliation_view(&item, &declarations, &evidence, None);
        assert!(never.current.is_none());
        assert!(never.current_evidence_digest.is_some());
        let old_digest = never.current_evidence_digest.clone();
        evidence.sources = ["forge_pull", "tracked_publication"]
            .into_iter()
            .map(|source| crate::hub::work_reconciliation::SourceEvidence {
                source: source.into(),
                state: "observed_only".into(),
                checked_at_seconds: 1,
                unread_pages: 0,
                deferred: 0,
            })
            .collect();
        let observed = reconciliation_view(&item, &declarations, &evidence, None);
        assert!(observed.current.is_none());
        let row = project_work(&item, op(&item, true, false), Some(observed), "usr_lead");
        assert!(
            !row.limits
                .iter()
                .any(|l| l.starts_with("provider_coverage_"))
        );
        evidence.sources[0].state = "unavailable".into();
        let unavailable = reconciliation_view(&item, &declarations, &evidence, None);
        assert_eq!(unavailable.current_source_coverage[0].state, "unavailable");
        assert!(unavailable.current.is_none());
        let row = project_work(&item, op(&item, true, false), Some(unavailable), "usr_lead");
        assert!(row.limits.contains(&"provider_coverage_unavailable".into()));

        let retained = reassessment(&item).current;
        evidence.merge_through += 1;
        let stale = reconciliation_view(&item, &declarations, &evidence, retained);
        assert!(stale.current.is_none());
        assert!(stale.retained.is_some());
        assert_ne!(stale.current_evidence_digest, old_digest);
        let row = project_work(&item, op(&item, true, false), Some(stale), "usr_lead");
        assert_eq!(row.state, StatusState::Blocked);
        assert!(
            !row.limits
                .contains(&"current_provider_evidence_not_exposed".into())
        );
    }

    #[test]
    fn mismatched_work_version_suppresses_actionable_instruction() {
        let item = item(WorkState::Blocked);
        let mut operational = op(&item, true, false);
        operational.updated_seq += 1;
        let row = project_work(&item, operational, None, "usr_lead");
        assert_eq!(row.state, StatusState::ReassessmentNeeded);
        assert!(!row.needs_you);
        assert_eq!(row.recipient.kind, RecipientKind::None);
    }

    fn store_contract(store: &dyn Store) {
        let now = crate::hub::store::unix_now();
        let user = store
            .upsert_user(
                &ForgeIdentity {
                    provider: "github".into(),
                    host: "github.com".into(),
                    user_id: 101,
                },
                "synthetic-owner",
            )
            .unwrap();
        let (_, secret, _) = mint_bound_agent_token(
            store,
            &user.id,
            "synthetic-agent",
            "acme/widgets",
            101,
            3600,
            false,
        )
        .unwrap();
        let principal = store
            .authenticate(&agent_token_digest(secret.expose()).unwrap(), now)
            .unwrap()
            .unwrap();
        let request: WorkStart = serde_json::from_value(json!({"request_key":"synthetic-status", "title":"Synthetic blocked work", "session":"synthetic-session","scope":["src/example.rs"]})).unwrap();
        let item = store
            .work_start(&principal.token_id, &request, now)
            .unwrap()
            .item;
        let checkpoint = WorkCheckpoint {
            work_id: item.id.clone(),
            operation_id: "synthetic-block".into(),
            generation: item.generation,
            session: item.lead_session.clone(),
            expected_updated_seq: Some(item.updated_seq),
            kind: CheckpointKind::Blocker,
            summary: "Synthetic blocker".into(),
            next_action: Some("Reported next step".into()),
            scope: None,
            branch: None,
            worktree: None,
            resolve_blocker: false,
            source_refs: vec![],
            depends_on: vec![],
            related_work: vec![],
        };
        store
            .work_checkpoint(&principal.token_id, &checkpoint, now)
            .unwrap();
        let a = read(store, &principal, now).unwrap();
        let b = read(store, &principal, now + 1).unwrap();
        assert_eq!(a.digest, b.digest, "read time is outside evidence version");
        let person = read_person(
            store,
            &principal.owner,
            &principal.repository,
            principal.repository_id.unwrap(),
            now,
        )
        .unwrap();
        assert_eq!(
            person, a,
            "role-checked person and token read the same facts without minting a token"
        );
        let mut disabled = principal.owner.clone();
        disabled.disabled = true;
        assert!(matches!(
            read_person(
                store,
                &disabled,
                &principal.repository,
                principal.repository_id.unwrap(),
                now
            ),
            Err(HubError::Unauthorized)
        ));
        assert_eq!(a.counts.blocked, 1);
        assert_eq!(a.counts.needs_you, 1);
        assert_eq!(a.counts.held_reservations, 1);
        assert!(!a.atomic_snapshot);
        assert!(
            !a.complete,
            "unsupported producers cannot be presented as clear"
        );
        assert!(!a.work[0].reconciliation.as_ref().unwrap().current);
        assert_eq!(
            a.work[0].reported_next_action.as_deref(),
            Some("Reported next step")
        );
        let other = store
            .upsert_user(
                &ForgeIdentity {
                    provider: "github".into(),
                    host: "github.com".into(),
                    user_id: 102,
                },
                "synthetic-reader",
            )
            .unwrap();
        let (_, secret, _) = mint_bound_agent_token(
            store,
            &other.id,
            "synthetic-reader",
            "acme/widgets",
            101,
            3600,
            true,
        )
        .unwrap();
        let reader = store
            .authenticate(&agent_token_digest(secret.expose()).unwrap(), now)
            .unwrap()
            .unwrap();
        let other_status = read(store, &reader, now).unwrap();
        assert_eq!(other_status.counts.blocked, 1);
        assert_eq!(other_status.counts.needs_you, 0);
        assert_ne!(a.digest, other_status.digest);
        let (_, secret, _) = mint_bound_agent_token(
            store,
            &other.id,
            "synthetic-reader",
            "acme/private",
            102,
            3600,
            true,
        )
        .unwrap();
        let foreign = store
            .authenticate(&agent_token_digest(secret.expose()).unwrap(), now)
            .unwrap()
            .unwrap();
        assert!(read(store, &foreign, now).unwrap().work.is_empty());
        assert!(
            read_person(
                store,
                &foreign.owner,
                &foreign.repository,
                foreign.repository_id.unwrap(),
                now
            )
            .unwrap()
            .work
            .is_empty()
        );
        store.revoke_agent_token(&reader.token_id).unwrap();
        assert!(matches!(
            read(store, &reader, now),
            Err(HubError::Unauthorized)
        ));
        assert!(
            read_person(
                store,
                &reader.owner,
                &reader.repository,
                reader.repository_id.unwrap(),
                now
            )
            .is_ok(),
            "person authority is independent of the revoked agent credential"
        );
        let mut empty_token = principal.clone();
        empty_token.token_id.clear();
        assert!(
            matches!(read(store, &empty_token, now), Err(HubError::Unauthorized)),
            "an arbitrary empty credential never confers person authority"
        );
    }
    #[test]
    fn sqlite_status_reader_is_scoped_deterministic_and_reports_limits() {
        let dir = tempfile::tempdir().unwrap();
        let store = store::open(&format!(
            "sqlite://{}",
            dir.path().join("hub.sqlite").display()
        ))
        .unwrap();
        store_contract(&*store);
    }
    #[test]
    fn directed_requests_require_current_recipient_and_no_current_response() {
        let mut item = item(WorkState::Paused);
        item.claim_status = ClaimStatus::Stale;
        let mut request: crate::hub::coordination::CoordinationRequest = serde_json::from_value(json!({
            "id":"crq_synthetic", "requester":{"token":"tok_requester","agent":"requester","session":"requester-session","owner":"usr_requester","login":"requester"},
            "proposal":{"work_id":null,"source":null,"request_key":"synthetic","title":"Synthetic request","scope":["src/example.rs"]},
            "targets":[{"work_id":item.id,"lead":{"token":"tok_lead","agent":"agent","session":"session","owner":"usr_lead","login":"lead"},"generation":1,"reasons":[],"reasons_complete":true}],
            "status":"pending","acknowledgements":[],"acknowledgements_complete":true,"responses":[],"responses_complete":true,"created_at":100,"updated_at":100,"created_seq":1,"updated_seq":1
        })).unwrap();
        let items = BTreeMap::from([(item.id.clone(), item)]);
        let inbox = |request| CoordinationInbox {
            from: 0,
            through: 1,
            next_after: 1,
            requests: vec![request],
            complete: true,
            retention_gap: false,
        };
        let actions = request_actions(&inbox(request.clone()), &items, "usr_lead");
        assert_eq!(actions.len(), 1);
        assert!(actions[0].needs_you);
        assert_eq!(actions[0].recipients[0].kind, RecipientKind::Person);
        assert!(!request_actions(&inbox(request.clone()), &items, "usr_requester")[0].needs_you);
        request.targets[0].generation = 2;
        assert!(request_actions(&inbox(request.clone()), &items, "usr_lead").is_empty());
        request.targets[0].generation = 1;
        request
            .responses
            .push(crate::hub::coordination::CoordinationResponse {
                id: "response".into(),
                operation_id: "response".into(),
                target_work_id: request.targets[0].work_id.clone(),
                responder: request.targets[0].lead.clone(),
                target_generation: 1,
                kind: crate::hub::coordination::CoordinationResponseKind::Decline,
                note: None,
                via: None,
                created_at: 100,
                seq: 2,
            });
        assert!(request_actions(&inbox(request), &items, "usr_lead").is_empty());
    }

    #[tokio::test]
    async fn status_http_requires_repository_reader_and_accepts_read_only() {
        let directory = tempfile::tempdir().unwrap();
        let store = store::open(&format!(
            "sqlite://{}",
            directory.path().join("hub.sqlite").display()
        ))
        .unwrap();
        let user = store
            .upsert_user(
                &ForgeIdentity {
                    provider: "github".into(),
                    host: "github.com".into(),
                    user_id: 1001,
                },
                "synthetic-reader",
            )
            .unwrap();
        let (_, secret, _) =
            mint_bound_agent_token(&*store, &user.id, "reader", "acme/widgets", 101, 3600, true)
                .unwrap();
        let objects = crate::backend::LocalBackend::open(directory.path().join("objects")).unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!(
            "http://{}/v1/hub/standup/status",
            listener.local_addr().unwrap()
        );
        let router = crate::hub::router(crate::hub::HubState::new(
            store,
            std::sync::Arc::new(objects),
        ));
        let server = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        let client = reqwest::Client::new();
        assert_eq!(
            client.get(&url).send().await.unwrap().status(),
            reqwest::StatusCode::UNAUTHORIZED
        );
        let response = client
            .get(&url)
            .bearer_auth(secret.expose())
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), reqwest::StatusCode::OK);
        let status: ActionableStatus =
            serde_json::from_slice(&response.bytes().await.unwrap()).unwrap();
        assert!(status.work.is_empty());
        assert_eq!(status.viewer_owner, user.id);
        assert!(!status.complete);
        server.abort();
    }
    #[test]
    fn postgres_status_reader_conformance() {
        let Ok(base) = std::env::var("AGENTSCM_TEST_DATABASE_URL") else {
            eprintln!("skipping PostgreSQL status conformance: AGENTSCM_TEST_DATABASE_URL unset");
            return;
        };
        let mut random = [0_u8; 8];
        getrandom::fill(&mut random).unwrap();
        let schema = format!(
            "status_t_{}",
            random
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect::<String>()
        );
        let mut admin = postgres::Client::connect(&base, postgres::NoTls).unwrap();
        admin
            .batch_execute(&format!("CREATE SCHEMA {schema}"))
            .unwrap();
        struct Schema {
            admin: postgres::Client,
            name: String,
        }
        impl Drop for Schema {
            fn drop(&mut self) {
                let _ = self
                    .admin
                    .batch_execute(&format!("DROP SCHEMA {} CASCADE", self.name));
            }
        }
        let _schema = Schema {
            admin,
            name: schema.clone(),
        };
        let separator = if base.contains('?') { '&' } else { '?' };
        let url = format!(
            "{base}{separator}options=-c%20search_path%3D{schema}&application_name={schema}"
        );
        let store = store::open(&url).unwrap();
        store_contract(&*store);
    }
    #[test]
    fn sqlite_status_keeps_reservations_visible_at_the_work_limit() {
        let directory = tempfile::tempdir().unwrap();
        let store = store::open(&format!(
            "sqlite://{}",
            directory.path().join("hub.sqlite").display()
        ))
        .unwrap();
        let now = crate::hub::store::unix_now();
        let user = store
            .upsert_user(
                &ForgeIdentity {
                    provider: "github".into(),
                    host: "github.com".into(),
                    user_id: 201,
                },
                "synthetic-owner",
            )
            .unwrap();
        let (_, secret, _) =
            mint_bound_agent_token(&*store, &user.id, "agent", "acme/widgets", 101, 3600, false)
                .unwrap();
        let principal = store
            .authenticate(&agent_token_digest(secret.expose()).unwrap(), now)
            .unwrap()
            .unwrap();
        for n in 0..=MAX_STATUS_WORK {
            let request:WorkStart=serde_json::from_value(json!({"request_key":format!("status-{n}"),"title":format!("Synthetic {n}"),"session":format!("session-{n}"),"scope":[format!("src/file-{n}.rs")]})).unwrap();
            store
                .work_start(&principal.token_id, &request, now)
                .unwrap();
        }
        let status = read(&*store, &principal, now).unwrap();
        assert_eq!(status.work.len(), MAX_STATUS_WORK);
        assert_eq!(status.counts.held_reservations, MAX_STATUS_WORK);
        assert!(!status.complete);
        assert!(
            status
                .limits
                .contains(&"reservation_holders_truncated".into())
        );
        assert!(
            status
                .work
                .windows(2)
                .all(|rows| rows[0].work_id < rows[1].work_id)
        );
    }
    #[test]
    fn frozen_surface_fixture_matches_work_projection_and_text() {
        let fixture: ActionableStatus =
            serde_json::from_str(include_str!("fixtures/actionable-status.json")).unwrap();
        assert_eq!(fixture.counts.needs_you, 3);
        for expected in &fixture.work {
            let mut source = item(match expected.state {
                StatusState::Blocked => WorkState::Blocked,
                StatusState::AwaitingParty => WorkState::Handover,
                _ => WorkState::Active,
            });
            source.id = expected.work_id.clone();
            source.title = expected.title.clone();
            let recipient = &expected.recipient;
            source.owner = recipient.owner.clone().unwrap();
            source.owner_login = recipient.login.clone().unwrap();
            source.participants[0].owner = source.owner.clone();
            source.participants[0].login = source.owner_login.clone();
            source.participants[0].agent = "codex".into();
            source.lead_agent = "codex".into();
            let actual = project_work(
                &source,
                op(&source, true, false),
                None,
                &fixture.viewer_owner,
            );
            assert_eq!(actual.state, expected.state);
            assert_eq!(actual.recipient, expected.recipient);
            assert_eq!(actual.needs_you, expected.needs_you);
        }
        let text = render_text(&fixture);
        assert!(text.contains("3 actions need you"));
        assert!(text.contains("awaiting a named party"));
        assert!(text.contains("Coverage incomplete"));
    }
    #[test]
    fn compact_status_keeps_full_scope_identity_without_serializing_paths() {
        let mut source = item(WorkState::Active);
        source.scope = (0..256)
            .map(|n| format!("src/{n}-{}", "a".repeat(900)))
            .collect();
        let row = project_work(&source, op(&source, true, false), None, "usr_owner");
        assert_eq!(row.operational.reservation.scope_count, 256);
        assert!(serde_json::to_vec(&row).unwrap().len() < 4096);
        let digest = row.operational.digest;
        source.scope[0].push('x');
        assert_ne!(
            project_work(&source, op(&source, true, false), None, "usr_owner")
                .operational
                .digest,
            digest
        );
    }
    #[test]
    fn conflict_reader_scopes_personal_recipients_and_hashes_quiet_paths() {
        use crate::hub::intent::NewIntent;
        let dir = tempfile::tempdir().unwrap();
        let store = store::open(&format!(
            "sqlite://{}",
            dir.path().join("hub.sqlite").display()
        ))
        .unwrap();
        let now = crate::hub::store::unix_now();
        let principal = |provider: &str, host: &str, id: i64| {
            let user = store
                .upsert_user(
                    &ForgeIdentity {
                        provider: provider.into(),
                        host: host.into(),
                        user_id: id,
                    },
                    &format!("synthetic-{id}"),
                )
                .unwrap();
            let (_, secret, _) = mint_bound_agent_token(
                &*store,
                &user.id,
                "synthetic",
                "acme/widgets",
                101,
                3600,
                false,
            )
            .unwrap();
            store
                .authenticate(&agent_token_digest(secret.expose()).unwrap(), now)
                .unwrap()
                .unwrap()
        };
        let owner = principal("github", "github.com", 901);
        let other = principal("github", "github.com", 902);
        let outsider = principal("github", "github.com", 903);
        let foreign = principal("gitlab", "gitlab.example", 901);
        let request = |id: char, path: &str| NewIntent {
            id: format!("int_{}", id.to_string().repeat(32)),
            task: "Synthetic task".into(),
            branch: None,
            base: None,
            machine: None,
            paths: vec![path.into()],
            ttl_seconds: 3600,
        };
        store
            .announce_intent(&owner.token_id, &request('a', "src/a.rs"), now)
            .unwrap();
        store
            .announce_intent(&other.token_id, &request('b', "src/a.rs"), now)
            .unwrap();
        let own = read(&*store, &owner, now).unwrap();
        assert_eq!(
            own.actions
                .iter()
                .filter(|a| a.kind == "file_conflict" && a.needs_you)
                .count(),
            1
        );
        assert_eq!(read(&*store, &outsider, now).unwrap().counts.needs_you, 0);
        assert!(read(&*store, &foreign, now).unwrap().actions.is_empty());
        let before = read(&*store, &outsider, now).unwrap().digest;
        store
            .announce_intent(&owner.token_id, &request('c', "docs/quiet.md"), now)
            .unwrap();
        let after = read(&*store, &outsider, now).unwrap();
        assert_eq!(after.counts.needs_you, 0);
        assert_ne!(before, after.digest);
    }
}
