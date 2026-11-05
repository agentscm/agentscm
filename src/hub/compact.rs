//! Compact agent-facing MCP replies. Agent hosts such as Claude Code save a tool result over
//! about 25,000 tokens to a file instead of showing it, so by default `agentscm_start` and
//! `agentscm_work_overview` return the decision, bounded summaries and the paged reads that hold
//! the rest. `detail: "full"` keeps the earlier shape for readers that depend on it. Nothing here
//! reads or writes a cursor: catch-up and briefing pages, high-waters and acknowledgements stay
//! with their own tools.

use serde::Deserialize;
use serde_json::{Value, json};

use super::catchup::CatchupPreview;
use super::coordination::CoordinationRequest;
use super::narrative::NarrativeView;
use super::work::{Briefing, WorkItem, WorkOverview, WorkStartConflict};
use super::work_http::actionable_status::ActionableStatus;
use super::work_preflight::WorkPreflightResult;
use super::work_transition::OperationalOverviewV1;

/// How much of a reply to return.
#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(super) enum Detail {
    #[default]
    Compact,
    /// The shape before compact replies: every field of every embedded read.
    Full,
}

/// Items in one compact overview page unless the caller asks for another size.
pub(super) const PAGE: usize = 10;
/// Entries of any other embedded list.
const LIST: usize = 5;
const TITLE_CHARS: usize = 120;
const TEXT_CHARS: usize = 200;

/// `text` cut to `max` characters, marked when cut.
fn clip(text: &str, max: usize) -> String {
    match text.char_indices().nth(max) {
        Some((end, _)) => format!("{}…", &text[..end]),
        None => text.to_owned(),
    }
}

/// One Work item for planning: enough to choose it, fence a call on it and read the rest.
pub(super) fn brief(item: &WorkItem) -> Value {
    json!({"id": item.id, "title": clip(&item.title, TITLE_CHARS), "state": item.state,
        "claim_status": item.claim_status, "owner_login": item.owner_login,
        "lead_agent": item.lead_agent, "lead_session": item.lead_session,
        "generation": item.generation, "updated_seq": item.updated_seq,
        "updated_at": item.updated_at, "branch": item.branch,
        "scope": item.scope.iter().take(LIST).collect::<Vec<_>>(), "scope_count": item.scope.len(),
        "next_action": item.next_action.as_deref().map(|text| clip(text, TEXT_CHARS)),
        "blocked": item.blocker.is_some()})
}

/// Where a compact overview page starts.
#[derive(Clone, Debug, Default)]
pub(super) struct Page {
    pub size: usize,
    /// Current Work after this id, in id order.
    pub after: Option<String>,
    /// Recent outcomes before this event sequence, newest first.
    pub recent_before: Option<i64>,
}

/// One page of the overview. `held` is the operational projection of reservation-holding Work;
/// each listed item says whether it holds one.
pub(super) fn overview(
    overview: &WorkOverview,
    held: &OperationalOverviewV1,
    page: &Page,
) -> Value {
    let reservation = |id: &str| {
        held.work
            .iter()
            .find(|work| work.work_id == id)
            .map_or_else(
                || json!({"status": if held.complete { "none" } else { "unknown" }}),
                |work| json!({"status": work.reservation.status, "basis": work.reservation.basis,
                    "activity": work.activity.status, "lease_expires_at": work.activity.lease_expires_at}),
            )
    };
    // The first page lists everything; a continuation lists only what its cursor continues.
    let first = page.after.is_none() && page.recent_before.is_none();
    let current: Vec<&WorkItem> = overview
        .current
        .iter()
        .filter(|_| first || page.after.is_some())
        .filter(|item| {
            page.after
                .as_deref()
                .is_none_or(|after| item.id.as_str() > after)
        })
        .collect();
    let next_after = (current.len() > page.size).then(|| current[page.size - 1].id.clone());
    let current: Vec<Value> = current
        .iter()
        .take(page.size)
        .map(|item| {
            let mut value = brief(item);
            value["reservation"] = reservation(&item.id);
            value
        })
        .collect();
    let recent: Vec<_> = overview
        .recent
        .iter()
        .filter(|_| first || page.recent_before.is_some())
        .filter(|outcome| page.recent_before.is_none_or(|before| outcome.seq < before))
        .collect();
    let next_recent_before = (recent.len() > page.size).then(|| recent[page.size - 1].seq);
    let recent: Vec<Value> = recent
        .iter()
        .take(page.size)
        .map(|outcome| {
            json!({"seq": outcome.seq, "at": outcome.at, "kind": outcome.kind,
                "work_id": outcome.item.id, "title": clip(&outcome.item.title, TITLE_CHARS),
                "state": outcome.item.state, "owner_login": outcome.item.owner_login,
                "summary": outcome.item.current.as_ref().map(|c| clip(&c.summary, TEXT_CHARS))})
        })
        .collect();
    let overlaps: Vec<Value> = overview
        .overlaps
        .iter()
        .take(if first { 2 * LIST } else { 0 })
        .map(|pair| {
            json!({"left": pair.left.id, "right": pair.right.id,
                "reasons": pair.reasons.iter().take(LIST).collect::<Vec<_>>(),
                "reasons_complete": pair.reasons_complete && pair.reasons.len() <= LIST})
        })
        .collect();
    json!({
        "detail": "compact",
        "recent_since": overview.recent_since, "until": overview.until,
        "current": current, "current_listed": overview.current.len(),
        "current_complete": overview.current_complete, "next_after": next_after,
        "recent": recent, "recent_listed": overview.recent.len(),
        "recent_complete": overview.recent_complete, "next_recent_before": next_recent_before,
        "overlaps": overlaps, "overlaps_listed": overview.overlaps.len(),
        "overlaps_complete": overview.overlaps_complete,
        "history_retention_gap": overview.history_retention_gap,
        "settled_count": overview.settled_count,
        "held_reservations": held.work.len(), "held_reservations_complete": held.complete,
        "pages": "The first page lists current Work, recent outcomes and overlaps. Pass next_after or next_recent_before, with the same limit, to continue that list alone. Pages are not one snapshot.",
    })
}

/// The overview text: counts and how to page, without repeating the structured content.
pub(super) fn overview_text(value: &Value) -> String {
    let count = |key: &str| value[key].as_array().map_or(0, Vec::len);
    format!(
        "AgentSCM overview page: {} current Work item(s) of {} listed (complete: {}), {} recent outcome(s) of {} listed (complete: {}), {} overlap pair(s) of {} listed (complete: {}). Next pages: next_after {}, next_recent_before {}. The items are in structuredContent; this read never acknowledges catch-up.",
        count("current"),
        value["current_listed"],
        value["current_complete"],
        count("recent"),
        value["recent_listed"],
        value["recent_complete"],
        count("overlaps"),
        value["overlaps_listed"],
        value["overlaps_complete"],
        value["next_after"],
        value["next_recent_before"],
    )
}

fn conflict(conflict: &WorkStartConflict) -> Value {
    json!({"kind": conflict.kind, "current_generation": conflict.current_generation,
        "overlaps": conflict.overlaps.iter().take(LIST).map(|overlap| json!({
            "item": brief(&overlap.item),
            "your_scope": overlap.your_scope.iter().take(LIST).collect::<Vec<_>>(),
            "their_scope": overlap.their_scope.iter().take(LIST).collect::<Vec<_>>(),
            "reasons": overlap.reasons.iter().take(LIST).collect::<Vec<_>>(),
        })).collect::<Vec<_>>(),
        "overlaps_listed": conflict.overlaps.len(),
        "operational": conflict.operational.iter().take(LIST).collect::<Vec<_>>()})
}

fn coordination(request: &CoordinationRequest) -> Value {
    json!({"id": request.id, "status": request.status, "updated_seq": request.updated_seq,
        "title": clip(&request.proposal.title, TITLE_CHARS),
        "targets": request.targets.iter().take(LIST).map(|target| json!({"work_id": target.work_id,
            "lead": {"login": target.lead.login, "agent": target.lead.agent,
                "session": target.lead.session}})).collect::<Vec<_>>(),
        "targets_listed": request.targets.len(),
        "read": "agentscm_coordination_inbox"})
}

/// Readiness, the reasons and the merges to check. Paths stay on the preflight read.
fn preflight(preflight: &WorkPreflightResult) -> Value {
    let merge = |merge: &super::catchup::CatchupMergeObservation| {
        json!({"source_id": merge.source_id, "pull_number": merge.pull_number,
            "merge_commit": merge.merge_commit, "head_commit": merge.head_commit,
            "observed_at": merge.observed_at})
    };
    json!({"status": preflight.status, "complete": preflight.complete,
        "source_revision": preflight.source_revision,
        "source_relation_checked": preflight.source_relation_checked,
        "merge_after": preflight.merge_after, "merge_through": preflight.merge_through,
        "current_merge_through": preflight.current_merge_through,
        "reasons": preflight.reasons,
        "subject_match": preflight.subject_match.as_ref().map(brief),
        "scope_overlaps": preflight.scope_overlaps.iter().take(LIST).map(|o| brief(&o.item)).collect::<Vec<_>>(),
        "scope_overlaps_listed": preflight.scope_overlaps.len(),
        "merges": preflight.merges.iter().take(2 * LIST).map(merge).collect::<Vec<_>>(),
        "merges_listed": preflight.merges.len(),
        "checked_observations": preflight.checked_observations.iter().take(2 * LIST).map(merge).collect::<Vec<_>>(),
        "checked_observations_listed": preflight.checked_observations.len(),
        "read": "agentscm_work_preflight"})
}

/// `agentscm_start`'s `work`: the caller's own item in full, the rest bounded.
pub(super) fn start_work(work: &super::work::WorkStartResult) -> Value {
    json!({"item": work.item, "created": work.created, "joined": work.joined,
        "took_over": work.took_over,
        "conflict": work.conflict.as_ref().map(conflict),
        "coordination_request": work.coordination_request.as_ref().map(coordination),
        "preflight": work.preflight.as_ref().map(preflight),
        "reclaimed": work.reclaimed})
}

/// Counts of the startup overview; `agentscm_work_overview` pages it.
pub(super) fn context(overview: &WorkOverview) -> Value {
    json!({"current": overview.current.len(), "recent": overview.recent.len(),
        "overlaps": overview.overlaps.len(), "current_complete": overview.current_complete,
        "recent_complete": overview.recent_complete, "overlaps_complete": overview.overlaps_complete,
        "recent_since": overview.recent_since, "until": overview.until,
        "settled_count": overview.settled_count, "read": "agentscm_work_overview"})
}

/// The startup briefing preview, by counts; `agentscm_work_briefing` pages it.
pub(super) fn briefing(briefing: &Briefing) -> Value {
    json!({"subscriber": briefing.subscriber, "delivery": briefing.delivery,
        "from": briefing.from, "through": briefing.through, "items": briefing.items.len(),
        "events": briefing.events.len(), "retention_gap": briefing.retention_gap,
        "complete": briefing.complete, "read": "agentscm_work_briefing"})
}

/// The catch-up preview's fixed high-waters, coverage and counts. Read the delivery itself with
/// `agentscm_work_catchup`, passing these high-waters as `event_through` and `merge_through`.
pub(super) fn catchup(preview: &CatchupPreview) -> Value {
    json!({"subscriber": preview.subscriber, "event_ack": preview.event_ack,
        "event_through": preview.event_through, "merge_ack": preview.merge_ack,
        "merge_through": preview.merge_through, "work_events": preview.work_events.len(),
        "coordination_events": preview.coordination_events.len(),
        "merges": preview.merges.iter().take(LIST).map(|merge| json!({"source_id": merge.source_id,
            "pull_number": merge.pull_number, "merge_commit": merge.merge_commit,
            "title": merge.title.as_deref().map(|t| clip(t, TITLE_CHARS))})).collect::<Vec<_>>(),
        "merges_listed": preview.merges.len(), "retention": preview.retention,
        "source_coverage": preview.source_coverage, "complete": preview.complete,
        "read": "agentscm_work_catchup"})
}

/// Counts, the digest and what needs the caller, at most six; `agentscm_work_status` has all.
pub(super) fn status(status: &ActionableStatus) -> Value {
    let work = status.work.iter().filter(|w| w.needs_you).map(|w| {
        json!({"work_id": w.work_id, "title": clip(&w.title, TITLE_CHARS),
            "state": w.state, "action": clip(&w.action, TEXT_CHARS)})
    });
    let actions = status.actions.iter().filter(|a| a.needs_you).map(|a| {
        json!({"action_id": a.id, "kind": a.kind, "work_ids": a.work_ids,
            "action": clip(&a.action, TEXT_CHARS)})
    });
    let needs: Vec<Value> = work.chain(actions).collect();
    json!({"detail": "compact", "repository": status.repository, "counts": status.counts,
        "complete": status.complete, "observed_at": status.observed_at, "digest": status.digest,
        "needs_you": needs.iter().take(6).collect::<Vec<_>>(), "needs_you_listed": needs.len(),
        "read": "agentscm_work_status"})
}

/// The cached agent briefing, by its identity and headline; `agentscm_briefing_read` has it all.
pub(super) fn narrative(view: &NarrativeView) -> Value {
    json!({"status": "available", "briefing": {"id": view.id, "digest": view.digest,
        "status": view.status, "generated_at": view.generated_at,
        "headline": view.output.as_ref().map(|output| clip(&output.headline, 300)),
        "warnings": view.warnings.len()}, "read": "agentscm_briefing_read"})
}

#[cfg(test)]
mod tests {
    use super::clip;

    #[test]
    fn clip_cuts_on_characters_and_marks_the_cut() {
        assert_eq!(clip("abc", 3), "abc");
        assert_eq!(clip("abcd", 3), "abc…");
        assert_eq!(clip("日本語の説明", 2), "日本…");
    }
}
