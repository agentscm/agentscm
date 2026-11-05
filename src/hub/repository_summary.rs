//! The web pages' repository list: for each connected repository a person can read now, counts of
//! unfinished work, what needs a person, live agents and the last activity. Counts only: titles,
//! paths and people remain on the role-checked detail read. Shared Rust status owns all attention
//! counts; live-agent and last-activity fields retain their independent provenance.
//! The separate device catalog adds bounded role-checked Work and live-agent details.
use std::collections::BTreeSet;

use serde::Serialize;

use super::intent::Intent;
use super::store::SessionView;
use super::work::WorkOverview;
use super::work::{ClaimStatus, WorkItem, WorkOverlapPair, WorkState};
use super::work_http::actionable_status::{ActionableStatus, StatusState};

#[derive(Debug, Default, PartialEq, Eq, Serialize)]
pub struct Unfinished {
    pub total: usize,
    /// Active, with a lead that is checking in.
    pub moving: usize,
    /// Compatibility count: shared blocked and reassessment-needed states.
    pub stuck: usize,
}

#[derive(Debug, PartialEq, Eq, Serialize)]
pub struct RepositorySummary {
    pub repository: String,
    pub repository_id: i64,
    pub provider: String,
    pub unfinished: Unfinished,
    /// Shared actions addressed to this stable person identity.
    pub needs_you: usize,
    pub agents: usize,
    pub last_activity_at: Option<i64>,
    /// False when a list was cut short, so the counts are lower bounds.
    pub complete: bool,
    pub status_counts: super::work_http::actionable_status::StatusCounts,
    pub status_digest: String,
    pub coverage: Vec<String>,
}

/// What the Hub read for one repository.
pub(super) struct Facts {
    pub overview: WorkOverview,
    pub status: ActionableStatus,
    pub intents: Vec<Intent>,
    pub sessions: Vec<SessionView>,
    /// Whether `intents` and `sessions` are every live one.
    pub live_complete: bool,
    pub last_event_at: Option<i64>,
}

pub(super) fn summarize(
    repository: String,
    repository_id: i64,
    provider: String,
    facts: &Facts,
) -> RepositorySummary {
    let current = &facts.overview.current;
    let c = &facts.status.counts;
    let unfinished = Unfinished {
        total: facts
            .status
            .work
            .iter()
            .filter(|w| w.state != StatusState::Retired)
            .count(),
        moving: c.active,
        stuck: c.blocked + c.reassessment_needed,
    };
    let needs_you = c.needs_you;
    let last_activity_at = current
        .iter()
        .map(|item| item.updated_at)
        .chain(facts.intents.iter().map(|intent| intent.updated_at))
        .chain(facts.sessions.iter().map(|session| session.updated_at))
        .chain(facts.last_event_at)
        .max();
    RepositorySummary {
        repository,
        repository_id,
        provider,
        unfinished,
        needs_you,
        agents: agent_count(&facts.sessions, &facts.intents),
        last_activity_at,
        complete: facts.status.complete
            && facts.overview.current_complete
            && facts.overview.overlaps_complete
            && facts.live_complete,
        status_counts: c.clone(),
        status_digest: facts.status.digest.clone(),
        coverage: facts.status.limits.clone(),
    }
}

/// An overlap where both tasks hold an active claim and are active or blocked.
pub(super) fn is_live_overlap(pair: &WorkOverlapPair) -> bool {
    let live = |item: &WorkItem| {
        item.claim_status == ClaimStatus::Active
            && matches!(item.state, WorkState::Active | WorkState::Blocked)
    };
    live(&pair.left) && live(&pair.right)
}

/// One per live session, plus one per live intent no session holds, as the status page lists them.
fn agent_count(sessions: &[SessionView], intents: &[Intent]) -> usize {
    let held: BTreeSet<&str> = sessions
        .iter()
        .filter_map(|s| s.intent.as_deref())
        .collect();
    sessions.len()
        + intents
            .iter()
            .filter(|intent| !held.contains(intent.id.as_str()))
            .count()
}

#[cfg(test)]
mod tests {
    use super::super::store::{ForgeIdentity, User};
    use super::*;
    use crate::hub::work::WorkSource;

    fn intent(id: &str, token: &str, via: &[&str], paths: &[&str]) -> Intent {
        Intent {
            id: id.into(),
            repository: "example-org/api".into(),
            task: String::new(),
            branch: None,
            base: None,
            machine: None,
            paths: paths.iter().map(|p| (*p).into()).collect(),
            expires_at: 2_000,
            created_at: 1_000,
            updated_at: 1_000,
            token: token.into(),
            agent: "agent".into(),
            owner: User {
                id: "usr_1".into(),
                identity: ForgeIdentity {
                    provider: "github".into(),
                    host: "github.com".into(),
                    user_id: 1,
                },
                login: "example-user".into(),
                name: None,
                disabled: false,
            },
            via: via.iter().map(|v| (*v).into()).collect(),
            session: None,
        }
    }

    fn session(intent: Option<&str>) -> SessionView {
        SessionView {
            key: "key".into(),
            token: "tok".into(),
            agent: "agent".into(),
            owner: "example-user".into(),
            intent: intent.map(Into::into),
            task: None,
            via: "[]".into(),
            created_at: 1_000,
            updated_at: 1_500,
        }
    }

    fn work_item(state: WorkState, claim_status: ClaimStatus) -> WorkItem {
        WorkItem {
            id: "wrk_0123456789abcdef0123456789abcdef".into(),
            repository: "example-org/api".into(),
            owner: "usr_1".into(),
            owner_login: "example-user".into(),
            source: WorkSource::Native {
                request_key: "fixture".into(),
            },
            title: "Synthetic work".into(),
            state,
            claim_status,
            last_checkin_at: 1_000,
            reported_at: None,
            generation: 1,
            lead_token: "tok".into(),
            lead_agent: "agent".into(),
            lead_session: "session".into(),
            participants: vec![],
            scope: vec![],
            branch: None,
            worktree: None,
            next_action: None,
            current: None,
            blocker: None,
            depends_on: vec![],
            related_work: vec![],
            created_at: 1_000,
            updated_at: 1_000,
            created_seq: 1,
            updated_seq: 1,
        }
    }

    fn pair(left: (WorkState, ClaimStatus), right: (WorkState, ClaimStatus)) -> WorkOverlapPair {
        WorkOverlapPair {
            left: work_item(left.0, left.1),
            right: work_item(right.0, right.1),
            reasons: vec![],
            reasons_complete: true,
        }
    }

    #[test]
    fn only_overlaps_between_live_tasks_count() {
        use ClaimStatus::{Active, Stale, Unknown};
        use WorkState::{Active as Working, Blocked, Handover, Paused};
        let pairs = [
            pair((Working, Active), (Blocked, Active)),
            pair((Working, Active), (Working, Stale)),
            pair((Working, Unknown), (Working, Active)),
            pair((Paused, Active), (Working, Active)),
            pair((Working, Active), (Handover, Active)),
        ];
        assert_eq!(pairs.iter().filter(|p| is_live_overlap(p)).count(), 1);
        assert!(is_live_overlap(&pairs[0]));
        assert!(pairs[1..].iter().all(|p| !is_live_overlap(p)));
    }

    #[test]
    fn agents_are_sessions_plus_unheld_intents() {
        let intents = [
            intent("int_a", "tok_a", &["mcp"], &["a"]),
            intent("int_b", "tok_b", &[], &["b"]),
        ];
        assert_eq!(
            agent_count(&[session(Some("int_a")), session(None)], &intents),
            3
        );
        assert_eq!(agent_count(&[], &[]), 0);
    }
}
