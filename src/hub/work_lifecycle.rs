//! Pure rules for durable Work outcomes and temporary scope reservations.
//!
//! This module holds no store access. It decides which transitions an actor may request from a
//! state and builds the `WorkOperationalV1` projection from facts the caller has already read.
//! The design is in `docs/design/work-lifecycle.md`.
use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};

use super::work::{CheckpointKind, ClaimStatus, ReclaimedHold, WorkItem, WorkState};

/// Version 1 of the reports that embed these types without `activity`, such as reconciliation.
pub(super) const SCHEMA_VERSION_V1: u16 = 1;
/// The operational projection's version: 2 added the reservation lease to `activity`.
pub(super) const OPERATIONAL_SCHEMA_VERSION: u16 = 2;
/// A reservation lapses this long after its lead session last renewed it.
pub const LEASE_SECONDS: i64 = 1200;
/// A `retain` keeps the reservation at most this long without lead activity.
pub const RETAIN_CAP_SECONDS: i64 = 7200;
/// The lead session renews its lease at most this often.
pub(super) const RENEW_INTERVAL_SECONDS: i64 = 60;
/// A shell call that is still running keeps its session's leases live at most this long.
pub const IN_FLIGHT_CAP_SECONDS: i64 = 3600;
/// The start of the refusal a lead meets after its reservation lapsed; every surface shows it.
pub const LEASE_LAPSED: &str = "LEASE_LAPSED";
/// Participants from owners other than the Work's own. The rest of the 100 participants stay for
/// the owner's sessions, so other owners cannot block a resume or slow cleanup without bound.
pub const MAX_OTHER_OWNER_PARTICIPANTS: usize = 25;

/// Whole minutes from a hold's last renewal to its lapse: 20 for an idle lease, longer after a
/// shell call or a retain.
fn held_minutes(renewed: i64, expired: i64) -> i64 {
    (expired - renewed).max(0) / 60
}

/// What met a lapsed hold that nothing took.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Unheld {
    Edit,
    Retain,
    /// A start whose session the Hub resolved because the call named none.
    UnnamedStart,
}

/// The refusal on Work `id` whose lease, last renewed at `renewed`, lapsed at `expired` while
/// nothing took its scope: it must be held again through fresh admission first.
pub(super) fn lapsed_unheld_text(id: &str, renewed: i64, expired: i64, what: Unheld) -> String {
    let what = match what {
        Unheld::Edit => {
            format!("Hold it again before editing: the lead runs agentscm_start with work_id {id}")
        }
        Unheld::Retain => format!(
            "A retain cannot revive it: hold it again with agentscm_start with work_id {id}"
        ),
        Unheld::UnnamedStart => format!(
            "This start names no session, so it cannot hold it again: the lead runs agentscm_start with work_id {id} and its session named"
        ),
    };
    format!(
        "{LEASE_LAPSED}: the hold on {id} lapsed at Unix {expired}, {} minutes after its lead session last renewed it. Nothing held the scope when this was refused, but other Work may take it at any time. {what}; a helper rejoins after that.",
        held_minutes(renewed, expired)
    )
}

/// The refusal of an edit or a start by the lead of Work `id` whose reservation lapsed, from the
/// `reservation_lapsed` event `data`: when it lapsed, which Work took the scope and how to resume.
pub(super) fn lapsed_refusal_text(
    id: &str,
    generation: i64,
    data: &serde_json::Value,
    holder: &str,
    holder_agent: Option<&str>,
) -> String {
    let expired = data["expires_at"].as_i64().unwrap_or_default();
    let renewed = data["renewed_at"].as_i64().unwrap_or(expired);
    let branch = data["branch"].as_str().map_or_else(
        || "Its branch".to_owned(),
        |branch| format!("Branch {branch}"),
    );
    let holder = match holder_agent {
        Some(agent) => format!(
            "{holder} (led by agent {} when this was refused)",
            super::advice::agent_name(agent)
        ),
        None => holder.to_owned(),
    };
    format!(
        "{LEASE_LAPSED}: your hold on {id} lapsed at Unix {expired}, {} minutes after this session last renewed it, and {holder} took the scope then. {branch} may hold unpushed work: push or set it aside before you continue. Resume with agentscm_start work_id {id} takeover_generation {generation}, a fresh admission that conflicts while the scope is held.",
        held_minutes(renewed, expired)
    )
}

/// What an admission tells its caller about the lapsed hold of `item`, which lapsed at `expired`,
/// whose scope it took.
pub(super) fn reclaimed_hold(item: &WorkItem, expired: i64) -> ReclaimedHold {
    let branch = item.branch.as_deref().map_or_else(
        || "its branch".to_owned(),
        |branch| format!("branch {branch}"),
    );
    ReclaimedHold {
        work_id: item.id.clone(),
        lead_agent: item.lead_agent.clone(),
        branch: item.branch.clone(),
        lapsed_at: expired,
        notice: format!(
            "AgentSCM: this admission took the scope of {id}, whose hold lapsed at Unix {expired}. Its lead agent {agent} may have edits on {branch} that it never pushed: check with that agent or its owner before you change the same files.",
            id = item.id,
            agent = super::advice::agent_name(&item.lead_agent),
        ),
    }
}

/// Where a hook saw a tool call: a shell call may run longer than a lease, so its start and end
/// are counted; any other call only renews.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CallPhase {
    Tick,
    Start,
    End,
}

/// What renews the lead session's lease, from what the Hub saw of that session.
#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RenewalCoverage {
    /// A hook reported the session's tool calls, so every tool call renews.
    ToolCalls,
    /// The only hook reports each tool call's end and none's start (Cursor's `postToolUse`), so a
    /// session inside one long call does not renew.
    ToolEnds,
    /// Hooks report only edits (an older `client setup`): edits, MCP calls and checkpoints renew.
    Edits,
    /// MCP only: named MCP calls and checkpoints renew.
    McpOnly,
    /// The Hub keeps no record of the lead session.
    Unknown,
}

impl RenewalCoverage {
    /// From the lead session's stored `via` modes and whether a hook reported a tool call.
    pub(super) fn of(via: Option<&str>, tool_activity: bool) -> Self {
        let Some(via) = via else {
            return Self::Unknown;
        };
        let modes: Vec<String> = serde_json::from_str(via).unwrap_or_default();
        let mut hooks = modes
            .iter()
            .filter(|mode| mode.starts_with("hook:"))
            .peekable();
        let hooked = hooks.peek().is_some();
        if tool_activity && hooked && hooks.all(|mode| mode == "hook:cursor") {
            Self::ToolEnds
        } else if tool_activity {
            Self::ToolCalls
        } else if hooked {
            Self::Edits
        } else {
            Self::McpOnly
        }
    }

    fn hint(self) -> &'static str {
        match self {
            Self::ToolCalls => "It renews on every tool call.",
            Self::ToolEnds => {
                "Its agent reports only the end of each tool call, so a session inside one call longer than the lease lapses meanwhile."
            }
            Self::Edits => {
                "Its agent reports only edits, so a busy session that edits nothing lapses; run agentscm client setup in its checkout to renew on every tool call."
            }
            Self::McpOnly => {
                "It renews only on named MCP calls and checkpoints; a busy session that makes neither lapses."
            }
            Self::Unknown => {
                "The Hub keeps no record of the lead session, so nothing is known to renew it."
            }
        }
    }
}
/// Largest number of reservation-holding Work in one operational overview.
pub(super) const MAX_OPERATIONAL_WORK: usize = 100;

/// Why a finished Work item ended. Stored beside `WorkState::Finished`, never as a state.
#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TerminalOutcome {
    Completed,
    Abandoned,
    Superseded,
}

impl TerminalOutcome {
    /// The `hub_work_items.terminal_outcome` column value.
    pub(super) fn column(self) -> &'static str {
        match self {
            Self::Completed => "completed",
            Self::Abandoned => "abandoned",
            Self::Superseded => "superseded",
        }
    }

    pub fn from_column(value: &str) -> Option<Self> {
        match value {
            "completed" => Some(Self::Completed),
            "abandoned" => Some(Self::Abandoned),
            "superseded" => Some(Self::Superseded),
            _ => None,
        }
    }
}

/// How a reservation was acquired. No reservation is represented by the absence of a basis.
#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ReservationBasis {
    /// Backfilled or left behind by a pause or handover without an explicit decision.
    #[serde(rename = "implicit")]
    LegacyImplicit,
    Admitted,
    Retained,
}

/// How long a reservation with the `reservation_basis` column value `basis` lasts from the
/// admission, decision or renewal that set it.
pub(super) fn lease_seconds(basis: &str) -> i64 {
    if basis == ReservationBasis::Retained.column() {
        RETAIN_CAP_SECONDS
    } else {
        LEASE_SECONDS
    }
}

/// A held reservation's last renewal and its expiry, in server Unix seconds.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Lease {
    pub(super) renewed_at: i64,
    pub(super) expires_at: i64,
}

impl ReservationBasis {
    /// The `hub_work_items.reservation_basis` column value.
    pub(super) fn column(self) -> &'static str {
        match self {
            Self::LegacyImplicit => "legacy_implicit",
            Self::Admitted => "admitted",
            Self::Retained => "retained",
        }
    }

    pub fn from_column(value: &str) -> Option<Self> {
        match value {
            "legacy_implicit" => Some(Self::LegacyImplicit),
            "admitted" => Some(Self::Admitted),
            "retained" => Some(Self::Retained),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum WorkTransitionKind {
    Release,
    Retain,
    Complete,
    Abandon,
    Supersede,
    Reopen,
}

impl WorkTransitionKind {
    pub(super) const ALL: [Self; 6] = [
        Self::Release,
        Self::Retain,
        Self::Complete,
        Self::Abandon,
        Self::Supersede,
        Self::Reopen,
    ];

    pub(super) fn as_str(self) -> &'static str {
        match self {
            Self::Release => "release",
            Self::Retain => "retain",
            Self::Complete => "complete",
            Self::Abandon => "abandon",
            Self::Supersede => "supersede",
            Self::Reopen => "reopen",
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum WorkActorKind {
    Lead,
    Owner,
    Admin,
}

/// How the actor authenticated. Only an agent session can become the lead of reopened Work.
#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ActorVia {
    Agent,
    Person,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Refusal {
    /// The actor kind may not request this transition at all.
    NotAuthorized,
    /// The state does not allow the transition.
    WrongState,
    /// Release and retain need a held reservation.
    NoReservation,
}

pub(super) type Decision = std::result::Result<(), Refusal>;

/// Decides one transition. Authorization is checked first, then state, then the reservation.
pub(super) fn decide_transition(
    kind: WorkTransitionKind,
    actor: WorkActorKind,
    via: ActorVia,
    state: WorkState,
    reservation_held: bool,
) -> Decision {
    use WorkActorKind::{Admin, Lead, Owner};
    use WorkTransitionKind::{Abandon, Complete, Release, Reopen, Retain, Supersede};
    let authorized = match kind {
        Release | Abandon | Supersede => true,
        Retain | Complete => actor != Admin,
        Reopen => actor != Admin && via == ActorVia::Agent,
    };
    // A lead is an agent session by definition; an administrator acts through a web sign-in.
    let consistent = match actor {
        Lead => via == ActorVia::Agent,
        Admin => via == ActorVia::Person,
        Owner => true,
    };
    if !authorized || !consistent {
        return Err(Refusal::NotAuthorized);
    }
    let finished = state == WorkState::Finished;
    match kind {
        Reopen if !finished => Err(Refusal::WrongState),
        Reopen => Ok(()),
        _ if finished => Err(Refusal::WrongState),
        Release | Retain if !reservation_held => Err(Refusal::NoReservation),
        _ => Ok(()),
    }
}

/// Transitions the state allows, sorted by wire name, without regard to who may request them.
pub(super) fn remedies(state: WorkState, reservation_held: bool) -> Vec<WorkTransitionKind> {
    let mut found: Vec<_> = WorkTransitionKind::ALL
        .into_iter()
        .filter(|kind| {
            // Each kind is allowed to at least one actor in the matrix; probe with the actor
            // that is allowed most often so the answer depends only on state and reservation.
            [
                (WorkActorKind::Lead, ActorVia::Agent),
                (WorkActorKind::Owner, ActorVia::Agent),
                (WorkActorKind::Admin, ActorVia::Person),
            ]
            .into_iter()
            .any(|(actor, via)| {
                decide_transition(*kind, actor, via, state, reservation_held).is_ok()
            })
        })
        .collect();
    found.sort_by_key(|kind| kind.as_str());
    found
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum OutcomeValue {
    Completed,
    Abandoned,
    Superseded,
    /// Finished before outcomes were recorded.
    Unstated,
    /// Not finished.
    None,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum OutcomeProvenance {
    AgentReport,
    Person,
    Admin,
    Unknown,
}

/// A stored outcome with who stated it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct OutcomeFact {
    pub(super) outcome: TerminalOutcome,
    pub(super) provenance: OutcomeProvenance,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct OutcomeV1 {
    pub value: OutcomeValue,
    pub provenance: OutcomeProvenance,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ReservationStatus {
    Held,
    None,
}

/// The holder is the lead agent and session only. No token identifier is ever exposed.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ReservationV1 {
    pub status: ReservationStatus,
    pub basis: Option<ReservationBasis>,
    pub scope: Vec<String>,
    pub holder_agent: Option<String>,
    pub holder_session: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ReportV1 {
    pub at: i64,
    pub kind: CheckpointKind,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ActivityStatus {
    Unknown,
    /// The lead session renewed the reservation's lease and it has not expired.
    Live,
    /// The lease expired: the reservation blocks nobody and the next admission over it clears it.
    Lapsed,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ActivityV1 {
    pub status: ActivityStatus,
    pub reason: String,
    /// The lead session's last renewal of the lease, None while no lease is known.
    pub lease_renewed_at: Option<i64>,
    /// When the lease lapses without another renewal, None while no lease is known.
    pub lease_expires_at: Option<i64>,
    /// What renews the lease; anything but `tool_calls` lists `activity_coverage` as unknown.
    pub coverage: RenewalCoverage,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum MergeStatus {
    Observed,
    None,
    Unknown,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct MergeV1 {
    pub hidden_by_merged_pr: bool,
    pub status: MergeStatus,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Recipient {
    Lead,
    Owner,
    Admin,
    None,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ActionableV1 {
    pub recipient: Recipient,
    pub reason: String,
}

/// A transition the state allows. Whether this caller may request it is decided on request.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct RemedyV1 {
    pub transition: WorkTransitionKind,
    pub requires_authorization: bool,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct CompletenessV1 {
    pub complete: bool,
    pub unknown: Vec<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct WorkOperationalV1 {
    pub schema_version: u16,
    pub work_id: String,
    pub state: WorkState,
    pub generation: i64,
    pub updated_seq: i64,
    pub outcome: OutcomeV1,
    pub reservation: ReservationV1,
    pub report: Option<ReportV1>,
    /// The existing request-time claim projection.
    pub freshness: ClaimStatus,
    pub activity: ActivityV1,
    pub merge: MergeV1,
    pub actionable: ActionableV1,
    pub remedies: Vec<RemedyV1>,
    pub completeness: CompletenessV1,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct OperationalOverviewV1 {
    pub schema_version: u16,
    pub work: Vec<WorkOperationalV1>,
    /// False when more reservation-holding Work exists than `work` lists.
    pub complete: bool,
}

/// Facts the caller read for one Work item.
pub(super) struct OperationalFacts<'a> {
    pub(super) item: &'a WorkItem,
    pub(super) outcome: Option<OutcomeFact>,
    pub(super) basis: Option<ReservationBasis>,
    pub(super) hidden_by_merged_pr: bool,
    pub(super) merge: MergeStatus,
    pub(super) lease: Option<Lease>,
    pub(super) coverage: RenewalCoverage,
    /// The request time that decides whether the lease is live, like `freshness`.
    pub(super) now: i64,
}

const ACTIVITY_UNKNOWN_REASON: &str = "Activity is not observed; the Work holds no lease.";
const ACTIVITY_LIVE_REASON: &str =
    "The lead session renewed the lease; it lapses at lease_expires_at unless renewed again.";
const ACTIVITY_LAPSED_REASON: &str = "The lead session did not renew the lease in time. The reservation blocks nobody and the next admission over its scope clears it; the lead resumes with agentscm_start, the work_id and the current takeover_generation.";

pub(super) fn work_operational(facts: &OperationalFacts<'_>) -> WorkOperationalV1 {
    let item = facts.item;
    let finished = item.state == WorkState::Finished;
    // Finished Work holds nothing, whatever a caller read.
    let basis = if finished { None } else { facts.basis };
    let held = basis.is_some();
    let outcome = match (finished, facts.outcome) {
        (true, Some(fact)) => OutcomeV1 {
            value: match fact.outcome {
                TerminalOutcome::Completed => OutcomeValue::Completed,
                TerminalOutcome::Abandoned => OutcomeValue::Abandoned,
                TerminalOutcome::Superseded => OutcomeValue::Superseded,
            },
            provenance: fact.provenance,
        },
        (true, None) => OutcomeV1 {
            value: OutcomeValue::Unstated,
            provenance: OutcomeProvenance::Unknown,
        },
        (false, _) => OutcomeV1 {
            value: OutcomeValue::None,
            provenance: OutcomeProvenance::Unknown,
        },
    };
    let reservation = ReservationV1 {
        status: if held {
            ReservationStatus::Held
        } else {
            ReservationStatus::None
        },
        basis,
        scope: if held { item.scope.clone() } else { Vec::new() },
        holder_agent: held.then(|| item.lead_agent.clone()),
        holder_session: held.then(|| item.lead_session.clone()),
    };
    let activity = match facts.lease.filter(|_| held) {
        Some(lease) => ActivityV1 {
            status: if lease.expires_at > facts.now {
                ActivityStatus::Live
            } else {
                ActivityStatus::Lapsed
            },
            reason: format!(
                "{} {}",
                if lease.expires_at > facts.now {
                    ACTIVITY_LIVE_REASON
                } else {
                    ACTIVITY_LAPSED_REASON
                },
                facts.coverage.hint()
            ),
            lease_renewed_at: Some(lease.renewed_at),
            lease_expires_at: Some(lease.expires_at),
            coverage: facts.coverage,
        },
        None => ActivityV1 {
            status: ActivityStatus::Unknown,
            reason: ACTIVITY_UNKNOWN_REASON.to_owned(),
            lease_renewed_at: None,
            lease_expires_at: None,
            coverage: facts.coverage,
        },
    };
    let mut unknown = BTreeSet::new();
    if activity.status == ActivityStatus::Unknown {
        unknown.insert("activity".to_owned());
    } else if activity.coverage != RenewalCoverage::ToolCalls {
        unknown.insert("activity_coverage".to_owned());
    }
    if facts.merge == MergeStatus::Unknown {
        unknown.insert("merge".to_owned());
    }
    if outcome.value == OutcomeValue::Unstated {
        unknown.insert("outcome".to_owned());
    } else if finished && outcome.provenance == OutcomeProvenance::Unknown {
        // The outcome is stored but the event that stated it is gone, so nobody can say who did.
        unknown.insert("outcome_provenance".to_owned());
    }
    if item.claim_status == ClaimStatus::Unknown {
        unknown.insert("freshness".to_owned());
    }
    let unknown: Vec<String> = unknown.into_iter().collect();
    WorkOperationalV1 {
        schema_version: OPERATIONAL_SCHEMA_VERSION,
        work_id: item.id.clone(),
        state: item.state,
        generation: item.generation,
        updated_seq: item.updated_seq,
        outcome,
        reservation,
        report: item.current.as_ref().map(|checkpoint| ReportV1 {
            at: checkpoint.at,
            kind: checkpoint.kind,
        }),
        freshness: item.claim_status,
        activity,
        merge: MergeV1 {
            hidden_by_merged_pr: facts.hidden_by_merged_pr,
            status: facts.merge,
        },
        actionable: actionable(item.state, held, facts.hidden_by_merged_pr),
        remedies: remedies(item.state, held)
            .into_iter()
            .map(|transition| RemedyV1 {
                transition,
                requires_authorization: true,
            })
            .collect(),
        completeness: CompletenessV1 {
            complete: unknown.is_empty(),
            unknown,
        },
    }
}

/// Who should look at the Work, from stored facts only. Claim currency is request-time and is
/// reported as `freshness`; mixing it in here would make this field change with the clock.
fn actionable(state: WorkState, held: bool, hidden_by_merged_pr: bool) -> ActionableV1 {
    let (recipient, reason) = if state == WorkState::Finished {
        (Recipient::None, "Work is finished.")
    } else if !held {
        (
            Recipient::None,
            "No reservation is held, so this Work blocks nobody.",
        )
    } else if hidden_by_merged_pr {
        (
            Recipient::Owner,
            "A merged pull request was observed; the owner decides whether to release or finish.",
        )
    } else if matches!(state, WorkState::Paused | WorkState::Handover) {
        (
            Recipient::Owner,
            "The lead paused or handed over the Work but its reservation is still held; the owner decides whether to retain, release or finish it.",
        )
    } else {
        (
            Recipient::Lead,
            "The lead holds the reservation; freshness says whether its claim is current.",
        )
    };
    ActionableV1 {
        recipient,
        reason: reason.to_owned(),
    }
}

/// Orders by Work id and bounds the list. `more` says the caller already dropped some Work.
pub(super) fn operational_overview(
    mut work: Vec<WorkOperationalV1>,
    more: bool,
) -> OperationalOverviewV1 {
    work.sort_by(|left, right| left.work_id.cmp(&right.work_id));
    let truncated = work.len() > MAX_OPERATIONAL_WORK;
    work.truncate(MAX_OPERATIONAL_WORK);
    OperationalOverviewV1 {
        schema_version: OPERATIONAL_SCHEMA_VERSION,
        work,
        complete: !more && !truncated,
    }
}

/// One Work row as a store read it: the snapshot and the facts kept beside it.
pub(super) struct OperationalRow {
    pub(super) item: WorkItem,
    pub(super) outcome: Option<OutcomeFact>,
    pub(super) basis: Option<ReservationBasis>,
    /// Computed by the stores' shared settled rule, never by this module.
    pub(super) hidden_by_merged_pr: bool,
    pub(super) lease: Option<Lease>,
    pub(super) coverage: RenewalCoverage,
}

/// What a merge observation says about one Work item.
///
/// A hidden item has an observed merge. Otherwise `none` needs two things: the settled rule
/// applies to the item (it has a branch, is not blocked or finished, and carries no blocker), and
/// the forge pull-request source was read for a window that reaches back to the item's creation.
/// Anything else is unknown, so a merge the rule would not hide is never reported as absent.
pub(super) fn merge_status(
    item: &WorkItem,
    hidden_by_merged_pr: bool,
    forge_window_from: Option<i64>,
) -> MergeStatus {
    if hidden_by_merged_pr {
        return MergeStatus::Observed;
    }
    let judged = item.branch.is_some()
        && item.blocker.is_none()
        && !matches!(item.state, WorkState::Finished | WorkState::Blocked);
    if judged && forge_window_from.is_some_and(|from| from <= item.created_at) {
        MergeStatus::None
    } else {
        MergeStatus::Unknown
    }
}

/// Builds the overview from rows ordered or not, as `viewer` may see their holders. `more` says
/// the reader found more rows than it returned. `forge_window_from` is the start of the last
/// complete forge pull-request read, if any.
pub(super) fn operational_page(
    rows: Vec<OperationalRow>,
    forge_window_from: Option<i64>,
    more: bool,
    now: i64,
    viewer: super::viewer::Viewer<'_>,
) -> OperationalOverviewV1 {
    let work = rows
        .into_iter()
        .map(|mut row| {
            viewer.work(&mut row.item);
            work_operational(&OperationalFacts {
                item: &row.item,
                outcome: row.outcome,
                basis: row.basis,
                hidden_by_merged_pr: row.hidden_by_merged_pr,
                merge: merge_status(&row.item, row.hidden_by_merged_pr, forge_window_from),
                lease: row.lease,
                coverage: row.coverage,
                now,
            })
        })
        .collect();
    operational_overview(work, more)
}

/// Room kept for the JSON around the projection.
const OPERATIONAL_ENVELOPE_BYTES: usize = 1024;

/// The projection with trailing entries shed until the `work` array fits `budget` bytes.
fn fit_operational(operational: &OperationalOverviewV1, budget: usize) -> OperationalOverviewV1 {
    let mut fitted = operational.clone();
    match super::work::fit_json_budget(&mut fitted.work, budget) {
        Ok(false) => {}
        Ok(true) => fitted.complete = false,
        Err(_) => {
            fitted.work.clear();
            fitted.complete = false;
        }
    }
    fitted
}

/// A `WorkOverview` with the operational projection beside it, as the HTTP and MCP surfaces
/// emit it when asked. The projection stays out of `WorkOverview` itself, which older clients
/// decode strictly. It gets what the overview left of `MAX_BRIEFING_BYTES`; entries that do not
/// fit are shed and `complete` turns false, so the response never grows past the briefing bound.
pub(super) fn overview_with_operational(
    overview: &super::work::WorkOverview,
    operational: &OperationalOverviewV1,
) -> serde_json::Value {
    let mut value = serde_json::to_value(overview).unwrap_or(serde_json::Value::Null);
    let used = value.to_string().len();
    let budget = super::work::MAX_BRIEFING_BYTES
        .saturating_sub(used)
        .saturating_sub(OPERATIONAL_ENVELOPE_BYTES);
    let fitted = fit_operational(operational, budget);
    if let (Some(object), Ok(operational)) = (value.as_object_mut(), serde_json::to_value(&fitted))
    {
        object.insert("operational".to_owned(), operational);
    }
    value
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hub::work::{Checkpoint, WorkSource};

    const STATES: [WorkState; 5] = [
        WorkState::Active,
        WorkState::Blocked,
        WorkState::Paused,
        WorkState::Handover,
        WorkState::Finished,
    ];
    const ACTORS: [(WorkActorKind, ActorVia); 4] = [
        (WorkActorKind::Lead, ActorVia::Agent),
        (WorkActorKind::Owner, ActorVia::Agent),
        (WorkActorKind::Owner, ActorVia::Person),
        (WorkActorKind::Admin, ActorVia::Person),
    ];

    /// The matrix written out independently of `decide_transition`. Columns: lead, owner agent,
    /// owner person, admin, each for an unfinished state, then for Finished.
    /// `Y` allowed, `.` not authorized, `s` wrong state, `r` no reservation.
    fn expected(
        kind: WorkTransitionKind,
        actor: usize,
        finished: bool,
        held: bool,
    ) -> Result<(), Refusal> {
        let row = match kind {
            WorkTransitionKind::Release => "YYYY",
            WorkTransitionKind::Retain => "YYY.",
            WorkTransitionKind::Complete => "YYY.",
            WorkTransitionKind::Abandon => "YYYY",
            WorkTransitionKind::Supersede => "YYYY",
            WorkTransitionKind::Reopen => "YY..",
        };
        if row.as_bytes()[actor] == b'.' {
            return Err(Refusal::NotAuthorized);
        }
        match (kind, finished) {
            (WorkTransitionKind::Reopen, true) => Ok(()),
            (WorkTransitionKind::Reopen, false) => Err(Refusal::WrongState),
            (_, true) => Err(Refusal::WrongState),
            (WorkTransitionKind::Release | WorkTransitionKind::Retain, false) if !held => {
                Err(Refusal::NoReservation)
            }
            _ => Ok(()),
        }
    }

    #[test]
    fn matrix_matches_the_table_exhaustively() {
        let mut checked = 0;
        for kind in WorkTransitionKind::ALL {
            for (index, (actor, via)) in ACTORS.into_iter().enumerate() {
                for state in STATES {
                    for held in [false, true] {
                        let want = expected(kind, index, state == WorkState::Finished, held);
                        assert_eq!(
                            decide_transition(kind, actor, via, state, held),
                            want,
                            "{kind:?} {actor:?} {via:?} {state:?} held={held}"
                        );
                        checked += 1;
                    }
                }
            }
        }
        assert_eq!(checked, 6 * 4 * 5 * 2);
    }

    #[test]
    fn inconsistent_actor_and_via_is_refused() {
        for kind in WorkTransitionKind::ALL {
            assert_eq!(
                decide_transition(
                    kind,
                    WorkActorKind::Lead,
                    ActorVia::Person,
                    WorkState::Active,
                    true
                ),
                Err(Refusal::NotAuthorized)
            );
            assert_eq!(
                decide_transition(
                    kind,
                    WorkActorKind::Admin,
                    ActorVia::Agent,
                    WorkState::Active,
                    true
                ),
                Err(Refusal::NotAuthorized)
            );
        }
    }

    #[test]
    fn admin_may_not_complete_or_retain() {
        for kind in [WorkTransitionKind::Complete, WorkTransitionKind::Retain] {
            for state in STATES {
                for held in [false, true] {
                    assert_eq!(
                        decide_transition(
                            kind,
                            WorkActorKind::Admin,
                            ActorVia::Person,
                            state,
                            held
                        ),
                        Err(Refusal::NotAuthorized)
                    );
                }
            }
        }
    }

    #[test]
    fn a_person_cannot_reopen() {
        for actor in [WorkActorKind::Owner, WorkActorKind::Admin] {
            assert_eq!(
                decide_transition(
                    WorkTransitionKind::Reopen,
                    actor,
                    ActorVia::Person,
                    WorkState::Finished,
                    false
                ),
                Err(Refusal::NotAuthorized)
            );
        }
    }

    #[test]
    fn finished_work_only_has_reopen_as_a_remedy() {
        for held in [false, true] {
            assert_eq!(
                remedies(WorkState::Finished, held),
                vec![WorkTransitionKind::Reopen]
            );
        }
    }

    #[test]
    fn remedies_are_sorted_and_depend_on_the_reservation() {
        let names = |state, held| {
            remedies(state, held)
                .into_iter()
                .map(WorkTransitionKind::as_str)
                .collect::<Vec<_>>()
        };
        for state in [
            WorkState::Active,
            WorkState::Blocked,
            WorkState::Paused,
            WorkState::Handover,
        ] {
            assert_eq!(
                names(state, true),
                ["abandon", "complete", "release", "retain", "supersede"]
            );
            assert_eq!(names(state, false), ["abandon", "complete", "supersede"]);
        }
    }

    #[test]
    fn wire_names_are_stable() {
        let json = |value: &dyn erased::Json| value.json();
        assert_eq!(json(&ReservationBasis::LegacyImplicit), "\"implicit\"");
        assert_eq!(json(&ReservationBasis::Admitted), "\"admitted\"");
        assert_eq!(json(&ReservationBasis::Retained), "\"retained\"");
        assert_eq!(json(&TerminalOutcome::Superseded), "\"superseded\"");
        assert_eq!(json(&WorkTransitionKind::Supersede), "\"supersede\"");
        assert_eq!(json(&WorkActorKind::Admin), "\"admin\"");
        assert_eq!(json(&OutcomeProvenance::AgentReport), "\"agent_report\"");
        assert_eq!(json(&OutcomeValue::Unstated), "\"unstated\"");
        assert!(serde_json::from_str::<ReservationBasis>("\"legacy_implicit\"").is_err());
        assert!(serde_json::from_str::<TerminalOutcome>("\"finished\"").is_err());
    }

    mod erased {
        pub(super) trait Json {
            fn json(&self) -> String;
        }
        impl<T: serde::Serialize> Json for T {
            fn json(&self) -> String {
                serde_json::to_string(self).unwrap()
            }
        }
    }

    fn item(state: WorkState) -> WorkItem {
        WorkItem {
            id: "work-1".to_owned(),
            repository: "example/repo".to_owned(),
            owner: "user-1".to_owned(),
            owner_login: "owner".to_owned(),
            source: WorkSource::Native {
                request_key: "key".to_owned(),
            },
            title: "Synthetic".to_owned(),
            state,
            claim_status: ClaimStatus::Active,
            last_checkin_at: 10,
            reported_at: None,
            generation: 3,
            lead_token: "secret-token-id".to_owned(),
            lead_agent: "agent-a".to_owned(),
            lead_session: "session-a".to_owned(),
            participants: Vec::new(),
            scope: vec!["src/a".to_owned()],
            branch: None,
            worktree: None,
            next_action: None,
            current: Some(Checkpoint {
                kind: CheckpointKind::Progress,
                summary: "s".to_owned(),
                next_action: None,
                source_refs: Vec::new(),
                at: 9,
                token: "secret-token-id".to_owned(),
                agent: "agent-a".to_owned(),
                session: "session-a".to_owned(),
                generation: 3,
            }),
            blocker: None,
            depends_on: Vec::new(),
            related_work: Vec::new(),
            created_at: 1,
            updated_at: 10,
            created_seq: 1,
            updated_seq: 7,
        }
    }

    fn facts(item: &WorkItem, basis: Option<ReservationBasis>) -> OperationalFacts<'_> {
        OperationalFacts {
            item,
            outcome: None,
            basis,
            hidden_by_merged_pr: false,
            merge: MergeStatus::None,
            lease: None,
            coverage: RenewalCoverage::ToolCalls,
            now: 100,
        }
    }

    #[test]
    fn a_held_lease_is_live_until_it_expires_then_lapsed() {
        let item = item(WorkState::Active);
        let mut input = facts(&item, Some(ReservationBasis::Admitted));
        input.lease = Some(Lease {
            renewed_at: 40,
            expires_at: 100 + 1,
        });
        let live = work_operational(&input);
        assert_eq!(live.activity.status, ActivityStatus::Live);
        assert_eq!(live.activity.lease_expires_at, Some(101));
        assert!(!live.completeness.unknown.contains(&"activity".to_owned()));
        input.now = 101;
        let lapsed = work_operational(&input);
        assert_eq!(lapsed.activity.status, ActivityStatus::Lapsed);
        assert_eq!(lapsed.activity.lease_renewed_at, Some(40));
        // A lease without a reservation is never reported.
        input.basis = None;
        assert_eq!(
            work_operational(&input).activity.status,
            ActivityStatus::Unknown
        );
        assert_eq!(lease_seconds("retained"), RETAIN_CAP_SECONDS);
        assert_eq!(lease_seconds("legacy_implicit"), LEASE_SECONDS);
    }

    #[test]
    fn the_reclaimed_hold_notice_masks_a_stored_host_label() {
        let mut held = item(WorkState::Active);
        held.lead_agent = "client-synthetic-host-abcdef12".into();
        let notice = reclaimed_hold(&held, 3600).notice;
        assert!(notice.contains("Its lead agent Agent abcd may"), "{notice}");
        assert!(!notice.contains("synthetic-host"), "{notice}");
    }

    #[test]
    fn the_lapse_refusal_says_when_who_and_how_to_resume() {
        // A shell call kept this hold for an hour, so the text gives that, not the idle lease.
        let data = serde_json::json!({"renewed_at": 0, "expires_at": 3600, "branch": "feature/a"});
        let text = lapsed_refusal_text("wrk_a", 2, &data, "wrk_b", Some("agent-b"));
        assert!(text.starts_with(LEASE_LAPSED));
        for part in [
            "lapsed at Unix 3600, 60 minutes after this session last renewed it",
            "wrk_b (led by agent agent-b when this was refused) took the scope then",
            "Branch feature/a may hold unpushed work",
            "agentscm_start work_id wrk_a takeover_generation 2",
        ] {
            assert!(text.contains(part), "{part}: {text}");
        }
    }

    #[test]
    fn coverage_follows_what_the_hub_saw_and_is_listed_unless_every_call_renews() {
        assert_eq!(
            RenewalCoverage::of(Some(r#"["hook:claude","mcp"]"#), true),
            RenewalCoverage::ToolCalls
        );
        assert_eq!(
            RenewalCoverage::of(Some(r#"["hook:cursor","mcp"]"#), true),
            RenewalCoverage::ToolEnds
        );
        assert_eq!(
            RenewalCoverage::of(Some(r#"["hook:codex"]"#), false),
            RenewalCoverage::Edits
        );
        assert_eq!(
            RenewalCoverage::of(Some(r#"["mcp"]"#), false),
            RenewalCoverage::McpOnly
        );
        assert_eq!(RenewalCoverage::of(None, false), RenewalCoverage::Unknown);
        let item = item(WorkState::Active);
        let mut input = facts(&item, Some(ReservationBasis::Admitted));
        input.lease = Some(Lease {
            renewed_at: 90,
            expires_at: 200,
        });
        input.coverage = RenewalCoverage::McpOnly;
        let view = work_operational(&input);
        assert_eq!(view.activity.coverage, RenewalCoverage::McpOnly);
        assert!(view.activity.reason.contains("only on named MCP calls"));
        assert!(
            view.completeness
                .unknown
                .contains(&"activity_coverage".to_owned())
        );
        input.coverage = RenewalCoverage::ToolCalls;
        assert!(
            !work_operational(&input)
                .completeness
                .unknown
                .contains(&"activity_coverage".to_owned())
        );
    }

    #[test]
    fn projection_never_exposes_a_token() {
        let item = item(WorkState::Active);
        let view = work_operational(&facts(&item, Some(ReservationBasis::Admitted)));
        let json = serde_json::to_string(&view).unwrap();
        assert!(!json.contains("secret-token-id"));
        assert!(!json.contains("token"));
        assert_eq!(view.reservation.holder_agent.as_deref(), Some("agent-a"));
        assert_eq!(
            view.reservation.holder_session.as_deref(),
            Some("session-a")
        );
    }

    #[test]
    fn activity_is_always_unknown_and_listed() {
        for state in STATES {
            let item = item(state);
            let view = work_operational(&facts(&item, Some(ReservationBasis::Retained)));
            assert_eq!(view.activity.status, ActivityStatus::Unknown);
            assert!(view.completeness.unknown.contains(&"activity".to_owned()));
            assert!(!view.completeness.complete);
        }
    }

    #[test]
    fn unfinished_work_without_a_reservation_holds_nothing() {
        let item = item(WorkState::Paused);
        let view = work_operational(&facts(&item, None));
        assert_eq!(view.reservation.status, ReservationStatus::None);
        assert!(view.reservation.scope.is_empty());
        assert_eq!(view.reservation.holder_agent, None);
        assert_eq!(view.outcome.value, OutcomeValue::None);
        assert_eq!(view.actionable.recipient, Recipient::None);
        assert!(
            view.remedies
                .iter()
                .all(|r| r.transition != WorkTransitionKind::Release)
        );
    }

    #[test]
    fn finished_work_drops_a_stray_basis_and_reports_reopen_only() {
        let item = item(WorkState::Finished);
        let mut input = facts(&item, Some(ReservationBasis::Admitted));
        input.outcome = Some(OutcomeFact {
            outcome: TerminalOutcome::Abandoned,
            provenance: OutcomeProvenance::Admin,
        });
        let view = work_operational(&input);
        assert_eq!(view.reservation.status, ReservationStatus::None);
        assert_eq!(view.reservation.basis, None);
        assert_eq!(view.outcome.value, OutcomeValue::Abandoned);
        assert_eq!(view.outcome.provenance, OutcomeProvenance::Admin);
        let transitions: Vec<_> = view.remedies.iter().map(|r| r.transition).collect();
        assert_eq!(transitions, [WorkTransitionKind::Reopen]);
        assert!(view.remedies.iter().all(|r| r.requires_authorization));
        assert!(!view.completeness.unknown.contains(&"outcome".to_owned()));
    }

    #[test]
    fn finished_without_an_outcome_is_unstated_and_unknown() {
        let item = item(WorkState::Finished);
        let view = work_operational(&facts(&item, None));
        assert_eq!(view.outcome.value, OutcomeValue::Unstated);
        assert_eq!(view.outcome.provenance, OutcomeProvenance::Unknown);
        assert_eq!(view.completeness.unknown, ["activity", "outcome"]);
    }

    #[test]
    fn merge_unknown_is_listed_and_a_merge_changes_nothing_else() {
        let item = item(WorkState::Paused);
        let mut input = facts(&item, Some(ReservationBasis::LegacyImplicit));
        input.hidden_by_merged_pr = true;
        input.merge = MergeStatus::Observed;
        let view = work_operational(&input);
        assert_eq!(view.state, WorkState::Paused);
        assert_eq!(view.reservation.status, ReservationStatus::Held);
        assert_eq!(view.outcome.value, OutcomeValue::None);
        assert_eq!(view.actionable.recipient, Recipient::Owner);
        input.merge = MergeStatus::Unknown;
        let view = work_operational(&input);
        assert_eq!(view.completeness.unknown, ["activity", "merge"]);
    }

    #[test]
    fn only_freshness_follows_the_claim_clock() {
        let mut item = item(WorkState::Active);
        let current = work_operational(&facts(&item, Some(ReservationBasis::Admitted)));
        item.claim_status = ClaimStatus::Stale;
        let mut stale = work_operational(&facts(&item, Some(ReservationBasis::Admitted)));
        assert_eq!(stale.freshness, ClaimStatus::Stale);
        stale.freshness = current.freshness;
        assert_eq!(stale, current);
    }

    #[test]
    fn projection_round_trips_and_decode_is_strict() {
        let item = item(WorkState::Active);
        let view = work_operational(&facts(&item, Some(ReservationBasis::Admitted)));
        let json = serde_json::to_value(&view).unwrap();
        assert_eq!(json["schema_version"], 2);
        assert_eq!(json["reservation"]["basis"], "admitted");
        assert_eq!(json["activity"]["status"], "unknown");
        let back: WorkOperationalV1 = serde_json::from_value(json.clone()).unwrap();
        assert_eq!(back, view);

        let mut extra = json.clone();
        extra["surprise"] = true.into();
        assert!(serde_json::from_value::<WorkOperationalV1>(extra).is_err());
        let mut nested = json;
        nested["reservation"]["token"] = "x".into();
        assert!(serde_json::from_value::<WorkOperationalV1>(nested).is_err());
    }

    #[test]
    fn overview_is_ordered_bounded_and_flags_incompleteness() {
        let make = |id: &str| {
            let mut item = item(WorkState::Active);
            item.id = id.to_owned();
            work_operational(&facts(&item, Some(ReservationBasis::Admitted)))
        };
        let view = operational_overview(vec![make("b"), make("a"), make("c")], false);
        let ids: Vec<_> = view.work.iter().map(|w| w.work_id.as_str()).collect();
        assert_eq!(ids, ["a", "b", "c"]);
        assert!(view.complete);
        assert!(!operational_overview(vec![make("a")], true).complete);

        let many: Vec<_> = (0..=MAX_OPERATIONAL_WORK)
            .map(|n| make(&format!("w{n:04}")))
            .collect();
        let view = operational_overview(many, false);
        assert_eq!(view.work.len(), MAX_OPERATIONAL_WORK);
        assert!(!view.complete);
        let json = serde_json::to_value(&view).unwrap();
        let mut extra = json;
        extra["extra"] = 1.into();
        assert!(serde_json::from_value::<OperationalOverviewV1>(extra).is_err());
    }

    #[test]
    fn actionable_follows_state_reservation_and_merge_only() {
        let recipient = |state, basis, hidden| {
            let item = item(state);
            let mut input = facts(&item, basis);
            input.hidden_by_merged_pr = hidden;
            work_operational(&input).actionable
        };
        let held = Some(ReservationBasis::Admitted);
        for state in STATES {
            for hidden in [false, true] {
                let none = recipient(state, None, hidden);
                assert_eq!(none.recipient, Recipient::None, "{state:?} unheld");
                assert!(!none.reason.is_empty());
            }
        }
        assert_eq!(
            recipient(WorkState::Finished, held, true).recipient,
            Recipient::None
        );
        for state in [WorkState::Active, WorkState::Blocked] {
            assert_eq!(recipient(state, held, false).recipient, Recipient::Lead);
            assert_eq!(recipient(state, held, true).recipient, Recipient::Owner);
        }
        for state in [WorkState::Paused, WorkState::Handover] {
            assert_eq!(recipient(state, held, false).recipient, Recipient::Owner);
            assert_eq!(recipient(state, held, true).recipient, Recipient::Owner);
        }
    }

    #[test]
    fn unknown_freshness_and_provenance_are_listed() {
        let mut active = item(WorkState::Active);
        active.claim_status = ClaimStatus::Unknown;
        let view = work_operational(&facts(&active, Some(ReservationBasis::Admitted)));
        assert_eq!(view.completeness.unknown, ["activity", "freshness"]);

        let finished = item(WorkState::Finished);
        let mut input = facts(&finished, None);
        input.outcome = Some(OutcomeFact {
            outcome: TerminalOutcome::Completed,
            provenance: OutcomeProvenance::Unknown,
        });
        let view = work_operational(&input);
        assert_eq!(view.outcome.value, OutcomeValue::Completed);
        assert_eq!(
            view.completeness.unknown,
            ["activity", "outcome_provenance"]
        );
        input.outcome = Some(OutcomeFact {
            outcome: TerminalOutcome::Completed,
            provenance: OutcomeProvenance::AgentReport,
        });
        assert_eq!(work_operational(&input).completeness.unknown, ["activity"]);
    }

    #[test]
    fn the_operational_overview_sheds_entries_to_fit_a_budget() {
        let make = |n: usize| {
            let mut item = item(WorkState::Active);
            item.id = format!("w{n:04}");
            work_operational(&facts(&item, Some(ReservationBasis::Admitted)))
        };
        let all = operational_overview((0..10).map(make).collect(), false);
        assert!(fit_operational(&all, usize::MAX).complete);
        assert_eq!(fit_operational(&all, usize::MAX).work.len(), 10);
        let one = serde_json::to_string(&all.work[0]).unwrap().len();
        let fitted = fit_operational(&all, 2 * one + 3);
        assert_eq!(fitted.work.len(), 2);
        assert_eq!(fitted.work[0].work_id, "w0000");
        assert!(!fitted.complete);
        let none = fit_operational(&all, 0);
        assert!(none.work.is_empty() && !none.complete);
    }
}
