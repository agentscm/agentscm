//! Recovery for harness agents and owners: a bounded inspection of the Work a caller can act on,
//! typed remedies with what each would change, and transition batches with one outcome per item.
//!
//! A preview is advice read at one moment. Every apply goes through the existing store operation
//! (`work_transition`, a takeover start or a handover checkpoint), which re-checks identity,
//! permission, generation and `updated_seq` in its own transaction. The design is section 9 of
//! `docs/design/work-lifecycle.md`.
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use super::advice::agent_name;
use super::intent::display;
use super::store::Store;
use super::viewer::Shown;
use super::work::{
    ClaimStatus, WorkItem, WorkParticipant, WorkPathOverlap, WorkRole, WorkState, validate_id,
};
use super::work_lifecycle::{
    ActorVia, LEASE_LAPSED, MAX_OTHER_OWNER_PARTICIPANTS, MergeStatus, OperationalRow,
    OutcomeValue, ReservationStatus, WorkActorKind, WorkOperationalV1, WorkTransitionKind,
    decide_transition, operational_page,
};
use super::work_transition::{
    MAX_PARTICIPANTS, ParticipantRef, WorkLifecycleReader, WorkTransitionActor,
    WorkTransitionRequest, WorkTransitionResult,
};
use super::{HubError, Result};

pub const RECOVERY_SCHEMA_VERSION: u16 = 1;
/// The most Work items of the caller's owner one inspection page lists.
pub const MAX_RECOVERY_PAGE: usize = 50;
const DEFAULT_RECOVERY_PAGE: usize = 20;
/// The most paths one inspection checks for conflicts.
pub const MAX_RECOVERY_PATHS: usize = 32;
/// The most conflicts, resumable matches, requests and dependents one read lists for each kind.
pub const MAX_RECOVERY_LISTED: usize = 25;
/// The most transitions one batch applies.
pub const MAX_BATCH_TRANSITIONS: usize = 25;

/// What a caller asks to inspect.
#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct RecoveryQuery {
    /// One Work item of the repository, whoever owns it. Omit to page the caller owner's
    /// unfinished Work.
    #[serde(default)]
    pub work_id: Option<String>,
    /// Paths to check against live holds and the owner's resumable Work, before a start.
    #[serde(default)]
    pub paths: Vec<String>,
    /// The `next_after` of the previous page.
    #[serde(default)]
    pub after: Option<String>,
    #[serde(default)]
    pub limit: Option<usize>,
    /// The caller's own session, which tells its lead from its owner's other sessions.
    #[serde(default)]
    pub session: Option<String>,
    /// Page every owner's unfinished Work, not the caller's own. Only a Hub administrator signed
    /// in on the web may ask, and not together with `work_id`.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub all_owners: bool,
}

impl RecoveryQuery {
    fn validate(&self) -> Result<RecoveryRead> {
        if let Some(id) = &self.work_id {
            validate_id("work", id, "wrk")?;
            if self.after.is_some() {
                return Err(invalid(
                    "after pages the owner's Work; it cannot go with work_id",
                ));
            }
            if self.all_owners {
                return Err(invalid("all_owners pages Work; it cannot go with work_id"));
            }
        }
        if let Some(after) = &self.after {
            validate_id("after", after, "wrk")?;
        }
        if self.paths.len() > MAX_RECOVERY_PATHS {
            return Err(invalid("paths has more than 32 entries"));
        }
        let limit = self.limit.unwrap_or(DEFAULT_RECOVERY_PAGE);
        if !(1..=MAX_RECOVERY_PAGE).contains(&limit) {
            return Err(invalid("limit must be 1 to 50"));
        }
        if let Some(session) = &self.session {
            super::intent::text("session", session, 255)?;
        }
        Ok(RecoveryRead {
            work_id: self.work_id.clone(),
            after: self.after.clone(),
            limit,
            paths: super::work::normalized_scope(&self.paths)?,
            all_owners: self.all_owners,
        })
    }
}

/// A validated inspection as the store reads it. `paths` are scope keys.
#[derive(Clone, Debug)]
pub struct RecoveryRead {
    pub work_id: Option<String>,
    pub after: Option<String>,
    pub limit: usize,
    pub paths: Vec<String>,
    /// Page every owner's unfinished Work; `inspect` allows it only for a Hub administrator.
    pub all_owners: bool,
}

/// Who read, as the store resolved the reader. Never sent to a client.
#[derive(Clone, Debug)]
pub struct RecoveryViewer {
    pub owner: String,
    /// The agent token, empty for a person.
    pub token: String,
    pub read_only: bool,
}

/// One Work row with what ending or releasing it reaches: open requests that target it and
/// unfinished Work that depends on it, each bounded to `MAX_RECOVERY_LISTED` plus one.
pub struct RecoveryRow {
    pub row: OperationalRow,
    pub requests: Vec<String>,
    pub dependents: Vec<String>,
    /// When each participant joined and last changed, which picks whom a prune removes first.
    pub marks: Vec<ParticipantMark>,
}

/// The generation a participant joined at and when its row last changed.
#[derive(Clone, Debug)]
pub struct ParticipantMark {
    pub token: String,
    pub session: String,
    pub generation: i64,
    pub updated_at: i64,
}

/// What the store read in one snapshot.
pub struct RecoveryFacts {
    pub viewer: RecoveryViewer,
    /// The ids of the page: the owner's (or, with `all_owners`, every owner's) unfinished Work
    /// after the cursor, or the one `work_id`.
    pub page: Vec<String>,
    /// More of the owner's unfinished Work follows the page.
    pub more: bool,
    /// Live holds overlapping the paths, with the exact overlapping paths.
    pub conflicts: Vec<super::work::WorkScopeOverlap>,
    pub conflicts_complete: bool,
    /// The owner's unfinished Work whose declared scope overlaps the paths.
    pub matches: Vec<String>,
    pub matches_complete: bool,
    /// Every row the lists above name.
    pub rows: Vec<RecoveryRow>,
    pub forge_window_from: Option<i64>,
}

/// How the caller stands to one Work item.
#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Standing {
    /// The caller's token and session lead it.
    Lead,
    /// The caller is its owner, or another session of an agent of its owner.
    Owner,
    /// A Hub administrator acting from a web sign-in.
    Admin,
    /// Anyone else, who can see it but not change it.
    Other,
}

/// Where a Work item stands for the caller.
#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RecoveryBucket {
    /// Active, with a live hold.
    Active,
    /// A live hold the lead or owner must decide about: blocked, paused, handed over, or merged.
    NeedsYou,
    /// Unfinished and holding nothing live: released, or its hold lapsed. It can be resumed.
    Resumable,
    Finished,
    /// Another owner's Work the caller cannot change.
    Other,
}

/// A remedy: a lifecycle transition, a resume through fresh admission, or the lead's handover.
#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RemedyKind {
    Release,
    Retain,
    Complete,
    Abandon,
    Supersede,
    Reopen,
    /// Remove non-lead participants the Hub names, so a full participant list admits a new
    /// session again. Owner or Hub administrator only; it changes no state or reservation.
    PruneParticipants,
    /// Lead the Work again through `agentscm_start` with `takeover_generation`.
    Resume,
    /// The lead hands the Work over with a handover checkpoint, optionally narrowing its scope.
    Handover,
}

impl RemedyKind {
    pub const ALL: [Self; 9] = [
        Self::Abandon,
        Self::Complete,
        Self::Handover,
        Self::PruneParticipants,
        Self::Release,
        Self::Reopen,
        Self::Resume,
        Self::Retain,
        Self::Supersede,
    ];

    fn transition(self) -> Option<WorkTransitionKind> {
        Some(match self {
            Self::Release => WorkTransitionKind::Release,
            Self::Retain => WorkTransitionKind::Retain,
            Self::Complete => WorkTransitionKind::Complete,
            Self::Abandon => WorkTransitionKind::Abandon,
            Self::Supersede => WorkTransitionKind::Supersede,
            Self::Reopen => WorkTransitionKind::Reopen,
            Self::PruneParticipants => WorkTransitionKind::PruneParticipants,
            Self::Resume | Self::Handover => return None,
        })
    }
}

/// Why a remedy is not open to this caller now.
#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RefusalCode {
    /// A read-only token changes nothing.
    ReadOnly,
    /// This caller has no standing for this remedy.
    NotAuthorized,
    /// An agent must name its session; a person cannot lead.
    NeedsSession,
    WrongState,
    NoReservation,
    /// Another session's hold is live; it opens only once its lease lapses.
    HoldLive,
    /// The lease lapsed; only fresh admission holds the scope again.
    LeaseLapsed,
    /// Only the lead session can do this.
    NotLead,
    /// The caller already leads it and holds its scope.
    AlreadyLead,
    /// Work that holds nothing is open to another owner only once its claim is stale.
    ClaimCurrent,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct RemedyRefusalV1 {
    pub code: RefusalCode,
    pub message: String,
}

/// The call that applies a remedy, with the fences read at preview time filled in. The caller
/// adds the fields in `caller_supplies`; apply refuses if the fences moved.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ApplyV1 {
    /// The MCP tool; None for a person, who applies through the Hub route.
    pub tool: Option<String>,
    pub route: String,
    pub arguments: Value,
    pub caller_supplies: Vec<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct RemedyOptionV1 {
    pub remedy: RemedyKind,
    pub permitted: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refusal: Option<RemedyRefusalV1>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub apply: Option<ApplyV1>,
}

/// What stays to do after a release or an end: reports are history, never rewritten.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct FollowUpV1 {
    pub unfinished: bool,
    pub next_action: Option<String>,
    pub blocker: Option<String>,
}

/// An agent taking part in the Work, named the way the Hub's pages name it. Never a token or
/// another owner's session.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct PartyV1 {
    pub login: String,
    pub agent: String,
    pub role: WorkRole,
}

/// What an inventory notices about a Work item. Each is a reading of recorded facts, never a
/// decision: nothing here proposes `complete`, because age and silence do not show that work is done.
#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum FindingKind {
    /// Unfinished Work whose lease lapsed and that nothing ended.
    LapsedHold,
    /// Unfinished Work whose lead stopped checking in and that holds nothing live.
    AbandonedSession,
    /// Open coordination requests name Work that is finished or whose hold lapsed.
    OrphanedRequests,
    /// A recorded blocker on finished Work, or blocked Work that records none.
    ContradictoryBlocker,
    /// The participant list is full, or too much of it belongs to other owners.
    ParticipantCap,
    /// A held reservation that a merged pull request hides from the active views. Report only.
    HiddenHold,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct FindingV1 {
    pub kind: FindingKind,
    /// One plain sentence of the facts, with their times and counts.
    pub evidence: String,
    /// False when a fact this rests on is unknown or was cut short.
    pub complete: bool,
    /// The remedy this caller may apply to it now; absent when none fits or none is permitted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub proposed: Option<RemedyKind>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct RecoveryWorkV1 {
    pub work_id: String,
    pub title: String,
    pub source: super::work::WorkSource,
    pub owner_login: String,
    pub lead_agent: String,
    pub standing: Standing,
    pub bucket: RecoveryBucket,
    /// Why the Work still exists, in one plain sentence.
    pub why: String,
    pub follow_up: FollowUpV1,
    /// Open coordination requests that target it or propose it; ending or releasing it records
    /// the change on each.
    pub requests: Vec<String>,
    /// Unfinished Work that names it in `depends_on`.
    pub dependents: Vec<String>,
    /// False when either list was cut at `MAX_RECOVERY_LISTED`.
    pub affected_complete: bool,
    pub operational: WorkOperationalV1,
    pub remedies: Vec<RemedyOptionV1>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub findings: Vec<FindingV1>,
}

/// A live hold overlapping the inspected paths.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct RecoveryConflictV1 {
    pub work_id: String,
    pub owner_login: String,
    pub mine: bool,
    pub holder_agent: String,
    pub lease_expires_at: Option<i64>,
    pub paths: Vec<WorkPathOverlap>,
    /// What the caller can do about it.
    pub next: String,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct WorkRecoveryV1 {
    pub schema_version: u16,
    pub observed_at: i64,
    pub read_only: bool,
    pub work: Vec<RecoveryWorkV1>,
    /// Pass as `after` for the next page of the owner's Work.
    pub next_after: Option<String>,
    pub complete: bool,
    pub conflicts: Vec<RecoveryConflictV1>,
    pub conflicts_complete: bool,
    /// The owner's unfinished Work over the paths that holds nothing live: resume one of these
    /// instead of starting a duplicate. Each is also in `work`.
    pub resumable: Vec<String>,
    pub resumable_complete: bool,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct RemedyPreviewRequest {
    pub work_id: String,
    pub remedy: RemedyKind,
    #[serde(default)]
    pub session: Option<String>,
    /// For a handover: the scope the next lead takes; the rest is freed. Omit to keep it all.
    #[serde(default)]
    pub scope: Option<Vec<String>>,
    /// For a supersede: the Work that replaces this one.
    #[serde(default)]
    pub superseded_by: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct RemedyEffectsV1 {
    pub state: WorkState,
    pub reservation: ReservationStatus,
    pub outcome: OutcomeValue,
    pub generation_advances: bool,
    /// Apply runs a fresh admission over the scope and may return a conflict instead.
    pub admission: bool,
    /// Sessions that lose the Work: their generation goes stale and their file claims on it are
    /// released. For a prune, the participants removed from it.
    pub fenced: Vec<PartyV1>,
    pub freed_paths: Vec<String>,
    pub kept_paths: Vec<String>,
    pub requests: Vec<String>,
    pub dependents: Vec<String>,
    pub affected_complete: bool,
    pub follow_up: FollowUpV1,
    pub summary: String,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct RemedyPreviewV1 {
    pub schema_version: u16,
    pub observed_at: i64,
    pub work_id: String,
    pub remedy: RemedyKind,
    pub standing: Standing,
    pub permitted: bool,
    pub refusal: Option<RemedyRefusalV1>,
    /// The fences apply must send. A newer value refuses the apply with nothing changed.
    pub generation: i64,
    pub expected_updated_seq: i64,
    pub effects: RemedyEffectsV1,
    pub apply: Option<ApplyV1>,
    pub operational: WorkOperationalV1,
}

/// One transition of a batch, as `agentscm_work_transition` takes it.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct BatchTransitionItem {
    pub work_id: String,
    pub operation_id: String,
    pub generation: i64,
    pub expected_updated_seq: i64,
    pub transition: WorkTransitionKind,
    pub reason: String,
    #[serde(default)]
    pub superseded_by: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub participants: Vec<super::work_transition::ParticipantRef>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct WorkTransitionBatch {
    /// The acting agent session; absent for a person.
    #[serde(default)]
    pub session: Option<String>,
    pub items: Vec<BatchTransitionItem>,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum BatchOutcome {
    Applied,
    /// The same operation was applied before; its receipt is returned and nothing changed now.
    Replayed,
    /// A reopen met a live overlap and changed nothing.
    Conflict,
    Refused,
    /// The Hub could not finish this item; whether it applied is unknown until a retry.
    Unavailable,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct BatchItemV1 {
    pub work_id: String,
    pub operation_id: String,
    pub outcome: BatchOutcome,
    pub code: Option<String>,
    pub message: Option<String>,
    /// How to retry this item, or why not to.
    pub retry: String,
    pub result: Option<WorkTransitionResult>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct WorkTransitionBatchV1 {
    /// One bounded read-time context for the entire batch, never one copy per item.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub holder_intents: Option<super::work_intent::HolderIntents>,
    pub schema_version: u16,
    /// Each item ran in its own transaction, in order; earlier items stay applied when a later
    /// one is refused.
    pub items: Vec<BatchItemV1>,
    pub applied: usize,
    pub not_applied: usize,
}

fn invalid(message: &str) -> HubError {
    HubError::Invalid(message.to_owned())
}

/// A wire name, such as `paused` or `needs_you`, for plain text.
fn word<T: Serialize>(value: &T) -> String {
    serde_json::to_value(value)
        .ok()
        .and_then(|value| value.as_str().map(str::to_owned))
        .unwrap_or_default()
}

/// Who reads and how they may act.
#[derive(Clone, Copy, Debug)]
pub enum RecoveryCaller<'a> {
    Agent {
        token: &'a str,
        session: Option<&'a str>,
    },
    Person {
        user: &'a super::store::User,
        repository: &'a str,
        repository_id: i64,
        is_admin: bool,
    },
}

impl RecoveryCaller<'_> {
    fn reader(&self) -> WorkLifecycleReader<'_> {
        match *self {
            Self::Agent { token, .. } => WorkLifecycleReader::Token(token),
            Self::Person {
                user,
                repository,
                repository_id,
                ..
            } => WorkLifecycleReader::Person {
                owner: user,
                repository,
                repository_id,
            },
        }
    }

    fn session(&self) -> Option<&str> {
        match self {
            Self::Agent { session, .. } => *session,
            Self::Person { .. } => None,
        }
    }

    fn person(&self) -> bool {
        matches!(self, Self::Person { .. })
    }

    fn standing(&self, viewer: &RecoveryViewer, item: &WorkItem) -> Standing {
        match self {
            Self::Agent { session, .. } => {
                if item.lead_token == viewer.token && *session == Some(item.lead_session.as_str()) {
                    Standing::Lead
                } else if item.owner == viewer.owner {
                    Standing::Owner
                } else {
                    Standing::Other
                }
            }
            Self::Person { is_admin, .. } => {
                if item.owner == viewer.owner {
                    Standing::Owner
                } else if *is_admin {
                    Standing::Admin
                } else {
                    Standing::Other
                }
            }
        }
    }

    fn routes(&self) -> Routes {
        if self.person() {
            Routes {
                transition: "/v1/hub/session/work/transition",
                tool: None,
            }
        } else {
            Routes {
                transition: "/v1/hub/standup/work-transition",
                tool: Some("agentscm_work_transition"),
            }
        }
    }
}

struct Routes {
    transition: &'static str,
    tool: Option<&'static str>,
}

/// One Work item and what the caller sees of it.
struct Subject<'a> {
    item: &'a WorkItem,
    operational: &'a WorkOperationalV1,
    standing: Standing,
    /// The participants a prune would remove now; empty when none needs pruning.
    prune: Vec<&'a WorkParticipant>,
}

/// Join generation, then last change, then identity: the order a prune removes participants in.
fn age<'a>(
    marks: &[ParticipantMark],
    participant: &'a WorkParticipant,
) -> (i64, i64, &'a str, &'a str) {
    let mark = marks
        .iter()
        .find(|mark| mark.token == participant.token && mark.session == participant.session);
    (
        mark.map_or(0, |mark| mark.generation),
        mark.map_or(0, |mark| mark.updated_at),
        participant.token.as_str(),
        participant.session.as_str(),
    )
}

/// The participants a prune removes, chosen the same way every time: non-lead participants other
/// than the caller's own session, enough to leave room for one new session and to bring other
/// owners' share back under its limit. Other owners' participants go first when their share is
/// over, then the oldest by join generation and last change.
fn prune_targets<'a>(
    item: &'a WorkItem,
    caller: &RecoveryCaller<'_>,
    marks: &[ParticipantMark],
) -> Vec<&'a WorkParticipant> {
    let others = item
        .participants
        .iter()
        .filter(|participant| participant.owner != item.owner)
        .count();
    let from_others = if others > MAX_OTHER_OWNER_PARTICIPANTS {
        others - (MAX_OTHER_OWNER_PARTICIPANTS - 1)
    } else {
        0
    };
    let from_all = item.participants.len().saturating_sub(MAX_PARTICIPANTS - 1);
    if from_others == 0 && from_all == 0 {
        return Vec::new();
    }
    let own = match *caller {
        RecoveryCaller::Agent {
            token,
            session: Some(session),
        } => Some((token, session)),
        _ => None,
    };
    let mut candidates: Vec<&WorkParticipant> = item
        .participants
        .iter()
        .filter(|participant| {
            participant.role != WorkRole::Lead
                && !(participant.token == item.lead_token
                    && participant.session == item.lead_session)
                && own != Some((participant.token.as_str(), participant.session.as_str()))
        })
        .collect();
    candidates.sort_by_key(|participant| age(marks, participant));
    let mut chosen: Vec<&WorkParticipant> = candidates
        .iter()
        .copied()
        .filter(|participant| participant.owner != item.owner)
        .take(from_others)
        .collect();
    let more = from_all.saturating_sub(chosen.len());
    let rest = candidates
        .into_iter()
        .filter(|participant| !chosen.contains(participant))
        .take(more)
        .collect::<Vec<_>>();
    chosen.extend(rest);
    chosen.sort_by_key(|participant| age(marks, participant));
    chosen
}

impl Subject<'_> {
    fn held(&self) -> bool {
        self.operational.reservation.status != ReservationStatus::None
    }

    fn lapsed(&self) -> bool {
        self.operational.reservation.status == ReservationStatus::Lapsed
    }

    fn live(&self) -> bool {
        self.operational.reservation.status == ReservationStatus::Held
    }
}

fn refusal(code: RefusalCode, message: impl Into<String>) -> Option<RemedyRefusalV1> {
    Some(RemedyRefusalV1 {
        code,
        message: message.into(),
    })
}

/// Whether the caller may apply `remedy` now, mirroring the checks the apply repeats.
fn decide(
    remedy: RemedyKind,
    subject: &Subject<'_>,
    caller: &RecoveryCaller<'_>,
    viewer: &RecoveryViewer,
) -> Option<RemedyRefusalV1> {
    let item = subject.item;
    if viewer.read_only {
        return refusal(
            RefusalCode::ReadOnly,
            "This token is read-only; a token that can write, or the owner in the Hub, applies it.",
        );
    }
    if let Some(kind) = remedy.transition() {
        if kind == WorkTransitionKind::PruneParticipants
            && !matches!(subject.standing, Standing::Owner | Standing::Admin)
        {
            return refusal(
                RefusalCode::NotAuthorized,
                format!(
                    "Only the owner {} or a Hub administrator prunes a Work's participants.",
                    item.owner_login
                ),
            );
        }
        let actor = match subject.standing {
            Standing::Lead => WorkActorKind::Lead,
            Standing::Owner => WorkActorKind::Owner,
            Standing::Admin => WorkActorKind::Admin,
            Standing::Other => {
                return refusal(
                    RefusalCode::NotAuthorized,
                    format!(
                        "Only the lead, the owner {} or a Hub administrator can change this Work; ask its lead or owner.",
                        item.owner_login
                    ),
                );
            }
        };
        if !caller.person() && caller.session().is_none() {
            return refusal(
                RefusalCode::NeedsSession,
                "Name your session; an agent changes Work only from a named session.",
            );
        }
        let via = if caller.person() {
            ActorVia::Person
        } else {
            ActorVia::Agent
        };
        use super::work_lifecycle::Refusal;
        return match decide_transition(kind, actor, via, item.state, subject.held()) {
            Err(Refusal::NotAuthorized) => refusal(
                if kind == WorkTransitionKind::Reopen && caller.person() {
                    RefusalCode::NeedsSession
                } else {
                    RefusalCode::NotAuthorized
                },
                match kind {
                    WorkTransitionKind::Reopen => {
                        "Only an agent session of the owner can reopen Work, because it becomes the lead."
                    }
                    _ => {
                        "A Hub administrator can release, abandon or supersede Work, not complete or retain it."
                    }
                },
            ),
            Err(Refusal::WrongState) => refusal(
                RefusalCode::WrongState,
                if item.state == WorkState::Finished {
                    "The Work is finished; only reopen applies."
                } else {
                    "Only finished Work can be reopened."
                },
            ),
            Err(Refusal::NoReservation) => refusal(
                RefusalCode::NoReservation,
                "The Work holds no reservation, so there is nothing to release or retain.",
            ),
            Ok(()) if kind == WorkTransitionKind::Retain && subject.lapsed() => refusal(
                RefusalCode::LeaseLapsed,
                "The hold lapsed; a retain cannot revive it. Resume the Work to hold its scope again.",
            ),
            Ok(()) if kind == WorkTransitionKind::PruneParticipants && subject.prune.is_empty() => {
                refusal(
                    RefusalCode::WrongState,
                    "No participant list needs pruning: it has room for a new session and no owner's share is over its limit.",
                )
            }
            Ok(()) => None,
        };
    }
    if caller.person() {
        return refusal(
            RefusalCode::NeedsSession,
            "Only an agent session can lead Work; ask the agent to do this.",
        );
    }
    if caller.session().is_none() {
        return refusal(
            RefusalCode::NeedsSession,
            "Name your session; it becomes the lead.",
        );
    }
    if item.state == WorkState::Finished {
        return refusal(
            RefusalCode::WrongState,
            "The Work is finished; reopen it instead.",
        );
    }
    match remedy {
        RemedyKind::Handover if subject.standing != Standing::Lead => refusal(
            RefusalCode::NotLead,
            "Only the lead session hands its Work over.",
        ),
        RemedyKind::Handover => None,
        _ => resume_refusal(subject, viewer),
    }
}

/// The start rules for a takeover with the current `takeover_generation`.
fn resume_refusal(subject: &Subject<'_>, viewer: &RecoveryViewer) -> Option<RemedyRefusalV1> {
    let item = subject.item;
    if subject.standing == Standing::Lead {
        let working = matches!(item.state, WorkState::Active | WorkState::Blocked);
        return (subject.live() && working).then(|| RemedyRefusalV1 {
            code: RefusalCode::AlreadyLead,
            message: "This session already leads the Work and holds its scope.".to_owned(),
        });
    }
    let own = item.owner == viewer.owner;
    let own_paused = item.lead_token == viewer.token && item.state == WorkState::Paused;
    let open = if subject.held() {
        subject.lapsed()
    } else {
        item.claim_status == ClaimStatus::Stale
    };
    if (own && !subject.held()) || own_paused || open || item.state == WorkState::Handover {
        return None;
    }
    if subject.live() {
        refusal(
            RefusalCode::HoldLive,
            format!(
                "{} holds the scope with a live lease{}; it opens only when that lapses. Ask its lead with agentscm_ask or a coordination request, or work elsewhere.",
                agent_name(&item.lead_agent),
                subject
                    .operational
                    .activity
                    .lease_expires_at
                    .map(|at| format!(" until Unix {at}"))
                    .unwrap_or_default()
            ),
        )
    } else {
        refusal(
            RefusalCode::ClaimCurrent,
            format!(
                "This Work holds nothing, but its lead checked in recently; only its owner {} resumes it now.",
                item.owner_login
            ),
        )
    }
}

fn apply_call(
    remedy: RemedyKind,
    subject: &Subject<'_>,
    caller: &RecoveryCaller<'_>,
    scope: Option<&[String]>,
    superseded_by: Option<&str>,
) -> ApplyV1 {
    let item = subject.item;
    let routes = caller.routes();
    let session = caller.session();
    match remedy.transition() {
        Some(kind) => {
            let mut arguments = json!({
                "work_id": item.id,
                "generation": item.generation,
                "expected_updated_seq": item.updated_seq,
                "transition": kind,
            });
            let mut supplies = vec!["operation_id".to_owned(), "reason".to_owned()];
            match caller {
                RecoveryCaller::Person { repository, .. } => {
                    arguments["repository"] = json!(repository);
                }
                RecoveryCaller::Agent { .. } => arguments["session"] = json!(session),
            }
            if kind == WorkTransitionKind::PruneParticipants {
                // A session is named by the digest a Work shows for another owner's session, so
                // the call never carries a native session id the caller could not already see.
                arguments["participants"] = json!(
                    subject
                        .prune
                        .iter()
                        .map(|participant| ParticipantRef {
                            token: participant.token.clone(),
                            session: super::store::session_key(
                                &participant.token,
                                &participant.session
                            ),
                        })
                        .collect::<Vec<_>>()
                );
            }
            if kind == WorkTransitionKind::Supersede {
                match superseded_by {
                    Some(by) => arguments["superseded_by"] = json!(by),
                    None => supplies.push("superseded_by".to_owned()),
                }
            }
            ApplyV1 {
                tool: routes.tool.map(str::to_owned),
                route: routes.transition.to_owned(),
                arguments,
                caller_supplies: supplies,
            }
        }
        None if remedy == RemedyKind::Handover => {
            let mut arguments = json!({
                "work_id": item.id,
                "generation": item.generation,
                "expected_updated_seq": item.updated_seq,
                "session": session,
                "kind": "handover",
            });
            if let Some(scope) = scope {
                arguments["scope"] = json!(super::intent::display_all(scope));
            }
            ApplyV1 {
                tool: Some("agentscm_work_checkpoint".to_owned()),
                route: "/v1/hub/standup/checkpoint".to_owned(),
                arguments,
                caller_supplies: vec!["operation_id".to_owned(), "summary".to_owned()],
            }
        }
        None => ApplyV1 {
            tool: Some("agentscm_start".to_owned()),
            route: "/v1/hub/standup/start".to_owned(),
            arguments: json!({
                "work_id": item.id,
                "task": item.title,
                "takeover_generation": item.generation,
                "session": session,
            }),
            caller_supplies: Vec::new(),
        },
    }
}

fn options(
    subject: &Subject<'_>,
    caller: &RecoveryCaller<'_>,
    viewer: &RecoveryViewer,
) -> Vec<RemedyOptionV1> {
    RemedyKind::ALL
        .into_iter()
        .map(|remedy| {
            let refusal = decide(remedy, subject, caller, viewer);
            RemedyOptionV1 {
                remedy,
                permitted: refusal.is_none(),
                apply: refusal
                    .is_none()
                    .then(|| apply_call(remedy, subject, caller, None, None)),
                refusal,
            }
        })
        .collect()
}

fn follow_up(item: &WorkItem, unfinished: bool) -> FollowUpV1 {
    FollowUpV1 {
        unfinished,
        next_action: item.next_action.clone(),
        blocker: item.blocker.as_ref().map(|blocker| blocker.summary.clone()),
    }
}

fn why(subject: &Subject<'_>) -> String {
    let item = subject.item;
    let operational = subject.operational;
    let lead = agent_name(&item.lead_agent);
    match operational.reservation.status {
        _ if item.state == WorkState::Finished => format!(
            "Finished as {}; it holds nothing and stays as history.",
            word(&operational.outcome.value)
        ),
        ReservationStatus::Held => match operational.activity.lease_expires_at {
            Some(at) => format!(
                "Unfinished and {}: {lead} holds its scope with a lease that lapses at Unix {at} unless renewed.",
                word(&item.state)
            ),
            None => format!(
                "Unfinished and {}: {lead} holds its scope; no lease is known.",
                word(&item.state)
            ),
        },
        ReservationStatus::Lapsed => format!(
            "Unfinished and {}: the hold of {lead} lapsed at Unix {}, so it blocks nobody, but nothing ended it.",
            word(&item.state),
            operational.activity.lease_expires_at.unwrap_or_default()
        ),
        ReservationStatus::None => format!(
            "Unfinished and {} with no reservation: it was released, or its lapsed hold was taken, and nobody ended it.",
            word(&item.state)
        ),
    }
}

fn bucket(subject: &Subject<'_>) -> RecoveryBucket {
    let state = subject.item.state;
    if state == WorkState::Finished {
        RecoveryBucket::Finished
    } else if matches!(subject.standing, Standing::Other | Standing::Admin) {
        RecoveryBucket::Other
    } else if !subject.live() {
        RecoveryBucket::Resumable
    } else if state == WorkState::Active && !subject.operational.merge.hidden_by_merged_pr {
        RecoveryBucket::Active
    } else {
        RecoveryBucket::NeedsYou
    }
}

fn bounded(mut ids: Vec<String>) -> (Vec<String>, bool) {
    let complete = ids.len() <= MAX_RECOVERY_LISTED;
    ids.truncate(MAX_RECOVERY_LISTED);
    (ids, complete)
}

/// The projection of every row the store read, in work id order.
fn projections(facts: &RecoveryFacts, now: i64) -> Vec<WorkOperationalV1> {
    let rows = facts
        .rows
        .iter()
        .map(|row| OperationalRow {
            item: row.row.item.clone(),
            outcome: row.row.outcome,
            basis: row.row.basis,
            hidden_by_merged_pr: row.row.hidden_by_merged_pr,
            lease: row.row.lease,
            coverage: row.row.coverage,
        })
        .collect();
    operational_page(
        rows,
        facts.forge_window_from,
        false,
        now,
        super::viewer::Viewer(&facts.viewer.owner),
    )
    .work
}

fn row_and_view<'a>(
    facts: &'a RecoveryFacts,
    projections: &'a [WorkOperationalV1],
    id: &str,
) -> Option<(&'a RecoveryRow, &'a WorkOperationalV1)> {
    let row = facts.rows.iter().find(|row| row.row.item.id == id)?;
    let view = projections.iter().find(|view| view.work_id == id)?;
    Some((row, view))
}

fn subject_of<'a>(
    row: &'a RecoveryRow,
    operational: &'a WorkOperationalV1,
    caller: &RecoveryCaller<'_>,
    viewer: &RecoveryViewer,
) -> Subject<'a> {
    let item = &row.row.item;
    Subject {
        item,
        operational,
        standing: caller.standing(viewer, item),
        prune: prune_targets(item, caller, &row.marks),
    }
}

/// What an inventory notices about one Work item, from facts the read already holds. Nothing
/// here proposes `complete`; a proposal is only a remedy the caller may apply now.
fn findings(
    subject: &Subject<'_>,
    caller: &RecoveryCaller<'_>,
    viewer: &RecoveryViewer,
    requests: &[String],
    requests_complete: bool,
) -> Vec<FindingV1> {
    let item = subject.item;
    let operational = subject.operational;
    let finished = item.state == WorkState::Finished;
    let lead = agent_name(&item.lead_agent);
    let permitted = |remedy| {
        decide(remedy, subject, caller, viewer)
            .is_none()
            .then_some(remedy)
    };
    let mut found = Vec::new();
    if subject.lapsed() && !finished {
        let expires = operational.activity.lease_expires_at;
        found.push(FindingV1 {
            kind: FindingKind::LapsedHold,
            evidence: match expires {
                Some(at) => format!(
                    "The hold of {lead} lapsed at Unix {at} and nothing ended the {} Work.",
                    word(&item.state)
                ),
                None => format!(
                    "The hold of {lead} lapsed at a time the Hub did not record and nothing ended the {} Work.",
                    word(&item.state)
                ),
            },
            complete: expires.is_some()
                && !operational.completeness.unknown.iter().any(|name| {
                    name == "activity_coverage" || name == "activity"
                }),
            proposed: permitted(RemedyKind::Release),
        });
    }
    if !finished && subject.live() && operational.merge.hidden_by_merged_pr {
        found.push(FindingV1 {
            kind: FindingKind::HiddenHold,
            evidence: format!(
                "A merged pull request hides the {} Work, but the hold of {lead} is still live and blocks overlapping Work; the Hub does not name the pull request here.",
                word(&item.state)
            ),
            complete: operational.merge.status == MergeStatus::Observed,
            proposed: None,
        });
    }
    if !finished && item.claim_status == ClaimStatus::Stale && !subject.live() {
        found.push(FindingV1 {
            kind: FindingKind::AbandonedSession,
            evidence: format!(
                "The lead {lead} last checked in at Unix {} and its claim is stale; the Work holds no live reservation.",
                item.last_checkin_at
            ),
            complete: true,
            proposed: matches!(subject.standing, Standing::Lead | Standing::Owner)
                .then(|| permitted(RemedyKind::Resume))
                .flatten(),
        });
    }
    if !requests.is_empty() && (finished || subject.lapsed()) {
        let count = requests.len().min(MAX_RECOVERY_LISTED);
        found.push(FindingV1 {
            kind: FindingKind::OrphanedRequests,
            evidence: format!(
                "{}{count} open coordination {} this Work, which {}; withdrawing or resolving a request is the requester's or the target's action.",
                if requests_complete { "" } else { "At least " },
                if count == 1 {
                    "request names"
                } else {
                    "requests name"
                },
                if finished {
                    "is finished"
                } else {
                    "no longer holds its scope"
                }
            ),
            complete: requests_complete,
            proposed: None,
        });
    }
    let blocker = item.blocker.is_some();
    if finished && blocker {
        found.push(FindingV1 {
            kind: FindingKind::ContradictoryBlocker,
            evidence: "The Work is finished but still records a blocker.".to_owned(),
            complete: true,
            proposed: None,
        });
    } else if item.state == WorkState::Blocked && !blocker {
        found.push(FindingV1 {
            kind: FindingKind::ContradictoryBlocker,
            evidence: "The Work is blocked but records no blocker.".to_owned(),
            complete: true,
            proposed: None,
        });
    }
    let total = item.participants.len();
    let others = item
        .participants
        .iter()
        .filter(|participant| participant.owner != item.owner)
        .count();
    if total >= MAX_PARTICIPANTS || others > MAX_OTHER_OWNER_PARTICIPANTS {
        found.push(FindingV1 {
            kind: FindingKind::ParticipantCap,
            evidence: format!(
                "The Work lists {total} participants, {others} from other owners; a new session is refused at {MAX_PARTICIPANTS}, or at {MAX_OTHER_OWNER_PARTICIPANTS} from other owners."
            ),
            complete: true,
            proposed: permitted(RemedyKind::PruneParticipants),
        });
    }
    found
}

fn inspected(
    row: &RecoveryRow,
    operational: &WorkOperationalV1,
    caller: &RecoveryCaller<'_>,
    viewer: &RecoveryViewer,
) -> RecoveryWorkV1 {
    let item = &row.row.item;
    let subject = subject_of(row, operational, caller, viewer);
    let (requests, requests_complete) = bounded(row.requests.clone());
    let (dependents, dependents_complete) = bounded(row.dependents.clone());
    RecoveryWorkV1 {
        findings: findings(&subject, caller, viewer, &row.requests, requests_complete),
        work_id: item.id.clone(),
        title: item.title.clone(),
        source: item.source.clone(),
        owner_login: item.owner_login.clone(),
        lead_agent: agent_name(&item.lead_agent),
        standing: subject.standing,
        bucket: bucket(&subject),
        why: why(&subject),
        follow_up: follow_up(item, item.state != WorkState::Finished),
        requests,
        dependents,
        affected_complete: requests_complete && dependents_complete,
        operational: operational.clone(),
        remedies: options(&subject, caller, viewer),
    }
}

/// The inspection: the caller owner's unfinished Work page (or one Work item), live conflicts
/// over the paths with the exact overlapping paths, and the owner's resumable Work over them.
pub fn inspect(
    store: &dyn Store,
    caller: &RecoveryCaller<'_>,
    query: &RecoveryQuery,
    now: i64,
) -> Result<WorkRecoveryV1> {
    let read = query.validate()?;
    if query.all_owners && !matches!(caller, RecoveryCaller::Person { is_admin: true, .. }) {
        return Err(invalid(
            "only a Hub administrator can inspect every owner's Work",
        ));
    }
    let facts = store.work_recovery_read(&caller.reader(), &read, now)?;
    let projections = projections(&facts, now);
    let find = |id: &str| row_and_view(&facts, &projections, id);
    let work: Vec<RecoveryWorkV1> = facts
        .page
        .iter()
        .chain(&facts.matches)
        .filter_map(|id| find(id))
        .map(|(row, operational)| inspected(row, operational, caller, &facts.viewer))
        .fold(Vec::new(), |mut work, entry| {
            if !work
                .iter()
                .any(|seen: &RecoveryWorkV1| seen.work_id == entry.work_id)
            {
                work.push(entry);
            }
            work
        });
    let resumable: Vec<String> = facts
        .matches
        .iter()
        .filter(|id| {
            work.iter()
                .any(|entry| &&entry.work_id == id && entry.bucket == RecoveryBucket::Resumable)
        })
        .take(MAX_RECOVERY_LISTED)
        .cloned()
        .collect();
    let conflicts: Vec<RecoveryConflictV1> = facts
        .conflicts
        .iter()
        .take(MAX_RECOVERY_LISTED)
        .map(|overlap| {
            let item = &overlap.item;
            let mine = item.owner == facts.viewer.owner;
            let expires = find(&item.id).and_then(|(_, view)| view.activity.lease_expires_at);
            RecoveryConflictV1 {
                work_id: item.id.clone(),
                owner_login: item.owner_login.clone(),
                mine,
                holder_agent: agent_name(&item.lead_agent),
                lease_expires_at: expires,
                paths: overlap.reasons.clone(),
                next: if mine {
                    format!(
                        "Your owner's {} holds these paths live. Continue in that Work, join it as helper, or have its lead hand it over; release or end it only when it is obsolete.",
                        item.id
                    )
                } else {
                    format!(
                        "{}'s {} holds these paths live. Do not take it over: ask its lead with agentscm_ask or the coordination request a start creates, narrow your paths, or wait until the lease lapses.",
                        item.owner_login,
                        agent_name(&item.lead_agent)
                    )
                },
            }
        })
        .collect();
    Ok(WorkRecoveryV1 {
        schema_version: RECOVERY_SCHEMA_VERSION,
        observed_at: now,
        read_only: facts.viewer.read_only,
        next_after: facts.more.then(|| facts.page.last().cloned()).flatten(),
        complete: !facts.more,
        conflicts_complete: facts.conflicts_complete
            && facts.conflicts.len() <= MAX_RECOVERY_LISTED,
        conflicts,
        resumable_complete: facts.matches_complete && facts.matches.len() <= MAX_RECOVERY_LISTED,
        resumable,
        work,
    })
}

/// What one remedy would change, from what the store read now. Not authority: the apply
/// re-checks everything.
pub fn preview(
    store: &dyn Store,
    caller: &RecoveryCaller<'_>,
    request: &RemedyPreviewRequest,
    now: i64,
) -> Result<RemedyPreviewV1> {
    validate_id("work", &request.work_id, "wrk")?;
    let scope = match (&request.scope, request.remedy) {
        (Some(scope), RemedyKind::Handover) => Some(super::work::normalized_scope(scope)?),
        (Some(_), _) => return Err(invalid("scope is only valid for handover")),
        (None, _) => None,
    };
    match (&request.superseded_by, request.remedy) {
        (Some(by), RemedyKind::Supersede) => validate_id("superseded_by", by, "wrk")?,
        (Some(_), _) => return Err(invalid("superseded_by is only valid for supersede")),
        (None, _) => {}
    }
    let read = RecoveryQuery {
        work_id: Some(request.work_id.clone()),
        session: request.session.clone(),
        ..RecoveryQuery::default()
    }
    .validate()?;
    let facts = store.work_recovery_read(&caller.reader(), &read, now)?;
    let projections = projections(&facts, now);
    let (row, operational) = row_and_view(&facts, &projections, &request.work_id)
        .ok_or_else(|| invalid("unknown work item"))?;
    let item = &row.row.item;
    let subject = subject_of(row, operational, caller, &facts.viewer);
    if let Some(scope) = &scope
        && let Some(extra) = super::work::uncovered_scope(&item.scope, scope).first()
    {
        return Err(HubError::Invalid(format!(
            "a handover can only narrow the scope; {} is outside it",
            display(extra)
        )));
    }
    let refusal = decide(request.remedy, &subject, caller, &facts.viewer);
    let (requests, requests_complete) = bounded(row.requests.clone());
    let (dependents, dependents_complete) = bounded(row.dependents.clone());
    let effects = effects(
        request.remedy,
        &subject,
        scope.as_deref(),
        requests,
        dependents,
        requests_complete && dependents_complete,
    );
    Ok(RemedyPreviewV1 {
        schema_version: RECOVERY_SCHEMA_VERSION,
        observed_at: now,
        work_id: item.id.clone(),
        remedy: request.remedy,
        standing: subject.standing,
        permitted: refusal.is_none(),
        apply: refusal.is_none().then(|| {
            apply_call(
                request.remedy,
                &subject,
                caller,
                scope.as_deref(),
                request.superseded_by.as_deref(),
            )
        }),
        refusal,
        generation: item.generation,
        expected_updated_seq: item.updated_seq,
        effects,
        operational: operational.clone(),
    })
}

fn party(login: &str, agent: &str, role: WorkRole) -> PartyV1 {
    PartyV1 {
        login: login.to_owned(),
        agent: agent_name(agent),
        role,
    }
}

/// Every session that takes part, lead first, as the Hub names them.
fn parties(item: &WorkItem) -> Vec<PartyV1> {
    let mut parties = vec![party(&item.owner_login, &item.lead_agent, WorkRole::Lead)];
    for participant in &item.participants {
        if participant.token == item.lead_token && participant.session == item.lead_session {
            continue;
        }
        parties.push(party(
            &participant.login,
            &participant.agent,
            participant.role,
        ));
    }
    parties
}

fn effects(
    remedy: RemedyKind,
    subject: &Subject<'_>,
    scope: Option<&[String]>,
    requests: Vec<String>,
    dependents: Vec<String>,
    affected_complete: bool,
) -> RemedyEffectsV1 {
    let item = subject.item;
    let current = subject.operational.reservation.status;
    let all = || super::intent::display_all(&item.scope);
    let held_paths = || if subject.held() { all() } else { Vec::new() };
    let unfinished = follow_up(item, true);
    let lead = subject.standing == Standing::Lead;
    let mut effects = RemedyEffectsV1 {
        state: item.state,
        reservation: current,
        outcome: subject.operational.outcome.value,
        generation_advances: false,
        admission: false,
        fenced: Vec::new(),
        freed_paths: Vec::new(),
        kept_paths: held_paths(),
        requests,
        dependents,
        affected_complete,
        follow_up: unfinished,
        summary: String::new(),
    };
    match remedy {
        RemedyKind::Release => {
            if item.state == WorkState::Active {
                effects.state = WorkState::Paused;
            }
            effects.reservation = ReservationStatus::None;
            effects.generation_advances = true;
            effects.fenced = parties(item);
            effects.freed_paths = held_paths();
            effects.kept_paths.clear();
            effects.summary = "Frees the scope and keeps the Work unfinished with its reports, next action and blocker; every current session is fenced and its file claims are released. Requests that target it record the change and stay open.".to_owned();
        }
        RemedyKind::Retain => {
            effects.reservation = ReservationStatus::Held;
            effects.summary = "Keeps the reservation on purpose for up to two hours without lead activity; nothing else changes.".to_owned();
        }
        RemedyKind::Complete | RemedyKind::Abandon | RemedyKind::Supersede => {
            effects.state = WorkState::Finished;
            effects.reservation = ReservationStatus::None;
            effects.outcome = match remedy {
                RemedyKind::Complete => OutcomeValue::Completed,
                RemedyKind::Abandon => OutcomeValue::Abandoned,
                _ => OutcomeValue::Superseded,
            };
            effects.generation_advances = !lead;
            effects.fenced = if lead { Vec::new() } else { parties(item) };
            effects.freed_paths = held_paths();
            effects.kept_paths.clear();
            effects.follow_up.unfinished = false;
            effects.summary = format!(
                "Ends the Work as {} and frees its scope. Its reports stay as history; open requests record the change and stay open; dependents keep naming it.",
                word(&effects.outcome)
            );
        }
        RemedyKind::PruneParticipants => {
            effects.fenced = subject
                .prune
                .iter()
                .map(|participant| party(&participant.login, &participant.agent, participant.role))
                .collect();
            effects.summary = format!(
                "Removes the {} listed non-lead participants, {} from other owners; the lead, state, reservation and generation stay as they are, and each removed session's intent on this Work ends. A removed session can join again if there is room.",
                subject.prune.len(),
                subject
                    .prune
                    .iter()
                    .filter(|participant| participant.owner != item.owner)
                    .count()
            );
        }
        RemedyKind::Reopen | RemedyKind::Resume => {
            effects.state = WorkState::Active;
            effects.reservation = ReservationStatus::Held;
            effects.outcome = OutcomeValue::None;
            effects.generation_advances = true;
            effects.admission = true;
            effects.fenced = if lead {
                Vec::new()
            } else {
                vec![party(&item.owner_login, &item.lead_agent, WorkRole::Lead)]
            };
            effects.kept_paths = all();
            effects.follow_up.unfinished = true;
            effects.summary = "Makes this session the lead through a fresh admission: apply checks the scope against live holds and returns a conflict with nothing changed if one overlaps. The previous lead is fenced; its branch may hold edits it never pushed.".to_owned();
        }
        RemedyKind::Handover => {
            effects.state = WorkState::Handover;
            if let Some(scope) = scope {
                effects.kept_paths = super::intent::display_all(scope);
                effects.freed_paths =
                    super::intent::display_all(&super::work::uncovered_scope(scope, &item.scope));
            }
            effects.summary = "Hands the Work over: it stays unfinished, open to the next lead's takeover start, and keeps only the kept paths; the freed paths are open to others at once. A live lease restarts; a lapsed one stays lapsed.".to_owned();
        }
    }
    effects
}

/// A stable code and retry advice for one refused or failed batch item.
fn classify_failure(error: &HubError) -> (BatchOutcome, &'static str, String, &'static str) {
    const REREAD: &str = "Read the Work again (agentscm_work_recovery with its work_id), then retry with its current generation and updated_seq and a new operation_id if the change still applies.";
    match error {
        HubError::Unauthorized => (
            BatchOutcome::Refused,
            "unauthorized",
            "This caller may not change this Work.".to_owned(),
            "Do not retry: ask its lead or owner, or a Hub administrator for a release.",
        ),
        HubError::Busy | HubError::Unavailable(_) => (
            BatchOutcome::Unavailable,
            "unavailable",
            "The Hub could not finish this item.".to_owned(),
            "Retry this item with the same operation_id: if it applied, the retry returns its receipt and changes nothing.",
        ),
        HubError::Invalid(message) => {
            let code = if message.starts_with(LEASE_LAPSED) {
                "lease_lapsed"
            } else if message.contains("generation is fenced") {
                "generation_changed"
            } else if message.contains("updated_seq") {
                "work_changed"
            } else if message.contains("operation_id was already used") {
                "operation_reused"
            } else if message.contains("may not request") {
                "not_authorized"
            } else if message.contains("does not allow") {
                "wrong_state"
            } else if message.contains("holds no reservation") {
                "no_reservation"
            } else if message.contains("unknown work item") {
                "not_found"
            } else {
                "invalid"
            };
            let retry = match code {
                "generation_changed" | "work_changed" | "lease_lapsed" => REREAD,
                "operation_reused" => {
                    "Use a new operation_id for a different request; the earlier one stays as it was."
                }
                _ => "Do not retry unchanged: the request itself was refused.",
            };
            // Only fixed sentences and the lapse refusal, which every surface passes through,
            // leave the Hub; other refusal texts stay behind the shared error rules.
            let message = match code {
                "lease_lapsed" => message.clone(),
                "generation_changed" => {
                    "Another change advanced the Work's generation first; nothing changed."
                        .to_owned()
                }
                "work_changed" => "The Work changed after it was read; nothing changed.".to_owned(),
                "operation_reused" => {
                    "This operation_id was already used for a different request.".to_owned()
                }
                "not_authorized" => "This caller may not request this transition.".to_owned(),
                "wrong_state" => "The Work's state does not allow this transition.".to_owned(),
                "no_reservation" => "The Work holds no reservation.".to_owned(),
                "not_found" => "No such Work in this repository.".to_owned(),
                _ => "The request was refused.".to_owned(),
            };
            (BatchOutcome::Refused, code, message, retry)
        }
        _ => (
            BatchOutcome::Refused,
            "refused",
            "The Hub refused this transition.".to_owned(),
            "Do not retry unchanged.",
        ),
    }
}

/// Applies each transition in order, each in its own store transaction, and reports one outcome
/// per item. A refused item never stops the rest; an exact retry of the whole batch replays the
/// applied items and retries the others.
pub fn apply_batch(
    store: &dyn Store,
    actor: &WorkTransitionActor<'_>,
    batch: &WorkTransitionBatch,
    now: i64,
) -> Result<WorkTransitionBatchV1> {
    if batch.items.is_empty() || batch.items.len() > MAX_BATCH_TRANSITIONS {
        return Err(invalid("a batch takes 1 to 25 transitions"));
    }
    let mut items = Vec::with_capacity(batch.items.len());
    for entry in &batch.items {
        let request = WorkTransitionRequest {
            work_id: entry.work_id.clone(),
            operation_id: entry.operation_id.clone(),
            generation: entry.generation,
            expected_updated_seq: entry.expected_updated_seq,
            session: batch.session.clone(),
            transition: entry.transition,
            reason: entry.reason.clone(),
            superseded_by: entry.superseded_by.clone(),
            participants: entry.participants.clone(),
        };
        let item = match store.work_transition(actor, &request, now) {
            Ok(result) => {
                let (outcome, retry) = match (&result.receipt, &result.conflict) {
                    (Some(receipt), _) if receipt.replayed => (
                        BatchOutcome::Replayed,
                        "Nothing to retry: this operation was already applied.",
                    ),
                    (Some(_), _) => (BatchOutcome::Applied, "Nothing to retry."),
                    (None, _) => (
                        BatchOutcome::Conflict,
                        "A live hold overlaps the scope; nothing changed. Inspect result.conflict, resolve the overlap, then retry with a new operation_id.",
                    ),
                };
                BatchItemV1 {
                    work_id: entry.work_id.clone(),
                    operation_id: entry.operation_id.clone(),
                    outcome,
                    code: None,
                    message: None,
                    retry: retry.to_owned(),
                    result: Some(result),
                }
            }
            Err(error) => {
                let (outcome, code, message, retry) = classify_failure(&error);
                BatchItemV1 {
                    work_id: entry.work_id.clone(),
                    operation_id: entry.operation_id.clone(),
                    outcome,
                    code: Some(code.to_owned()),
                    message: Some(message),
                    retry: retry.to_owned(),
                    result: None,
                }
            }
        };
        items.push(item);
    }
    let applied = items
        .iter()
        .filter(|item| matches!(item.outcome, BatchOutcome::Applied | BatchOutcome::Replayed))
        .count();
    Ok(WorkTransitionBatchV1 {
        holder_intents: None,
        schema_version: RECOVERY_SCHEMA_VERSION,
        not_applied: items.len() - applied,
        applied,
        items,
    })
}

/// Each applied result names agents; another owner's are shown by session digest.
impl Shown for WorkTransitionBatchV1 {
    fn show(&mut self, viewer: super::viewer::Viewer<'_>) {
        for item in &mut self.items {
            item.result.show(viewer);
        }
    }
}

/// One repair a plan would apply: the finding that proposed it and the batch item that does it.
#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct RepairItemV1 {
    pub work_id: String,
    pub finding: FindingKind,
    pub remedy: RemedyKind,
    pub item: BatchTransitionItem,
}

/// A finding a plan leaves alone, and why.
#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct RepairSkipV1 {
    pub work_id: String,
    pub finding: FindingKind,
    pub reason: String,
}

/// What `agentscm work repair` would apply and what it leaves for a person.
#[derive(Clone, Debug, Default, Serialize, PartialEq, Eq)]
pub struct RepairPlanV1 {
    pub planned: Vec<RepairItemV1>,
    pub skipped: Vec<RepairSkipV1>,
}

/// One item's outcome in a repair report.
#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct RepairResultV1 {
    pub work_id: String,
    pub operation_id: String,
    pub outcome: BatchOutcome,
    pub code: Option<String>,
    pub message: Option<String>,
}

impl From<&BatchItemV1> for RepairResultV1 {
    fn from(item: &BatchItemV1) -> Self {
        Self {
            work_id: item.work_id.clone(),
            operation_id: item.operation_id.clone(),
            outcome: item.outcome,
            code: item.code.clone(),
            message: item.message.clone(),
        }
    }
}

/// The document `agentscm work repair --json` prints.
#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct RepairReportV1 {
    /// True when the plan was sent; false for a dry run.
    pub applied: bool,
    pub planned: Vec<RepairItemV1>,
    pub skipped: Vec<RepairSkipV1>,
    pub results: Vec<RepairResultV1>,
    /// False when the inventory was cut short, so the plan may miss Work.
    pub complete: bool,
}

impl RepairReportV1 {
    /// Items the Hub did not apply or replay.
    pub fn failed(&self) -> usize {
        self.results
            .iter()
            .filter(|r| !matches!(r.outcome, BatchOutcome::Applied | BatchOutcome::Replayed))
            .count()
    }
}

/// Builds the repairs a caller's own inventory proposes. It never plans `complete`, a resume or
/// a handover, anything the caller must supply input for, or more than one repair per Work (a
/// second would run against a fence the first moves). Each item's `operation_id` is fixed by
/// the fences it carries, so replaying a plan replays what applied.
pub fn repair_plan(recovery: &WorkRecoveryV1) -> RepairPlanV1 {
    let mut plan = RepairPlanV1::default();
    for work in &recovery.work {
        for finding in &work.findings {
            match repair_item(work, finding) {
                Ok(_) if plan.planned.iter().any(|p| p.work_id == work.work_id) => {
                    plan.skipped.push(RepairSkipV1 {
                        work_id: work.work_id.clone(),
                        finding: finding.kind,
                        reason: "another repair of this Work is planned first; its fence moves, so rerun after it applies".to_owned(),
                    });
                }
                Ok(item) => plan.planned.push(item),
                Err(reason) => plan.skipped.push(RepairSkipV1 {
                    work_id: work.work_id.clone(),
                    finding: finding.kind,
                    reason,
                }),
            }
        }
    }
    plan
}

fn repair_item(
    work: &RecoveryWorkV1,
    finding: &FindingV1,
) -> std::result::Result<RepairItemV1, String> {
    if work.standing == Standing::Other {
        return Err("another owner's Work; the caller cannot change it".to_owned());
    }
    let Some(remedy) = finding.proposed else {
        return Err(match finding.kind {
            FindingKind::OrphanedRequests
            | FindingKind::ContradictoryBlocker
            | FindingKind::HiddenHold => "report only: this finding proposes no remedy".to_owned(),
            _ => "no remedy is proposed: none is permitted to this caller".to_owned(),
        });
    };
    if remedy == RemedyKind::Complete {
        return Err(
            "complete is never planned: age and silence do not show that work is done".to_owned(),
        );
    }
    let Some(transition) = remedy.transition() else {
        return Err(match remedy {
            RemedyKind::Resume => {
                "needs a fresh start: resume through agentscm_start with takeover_generation"
                    .to_owned()
            }
            _ => "needs the lead's handover checkpoint".to_owned(),
        });
    };
    let Some(option) = work.remedies.iter().find(|o| o.remedy == remedy) else {
        return Err(format!("the inventory lists no {} remedy", word(&remedy)));
    };
    if !option.permitted {
        return Err(match &option.refusal {
            Some(r) => format!("not permitted ({}): {}", word(&r.code), r.message),
            None => "not permitted".to_owned(),
        });
    }
    let Some(apply) = &option.apply else {
        return Err("the inventory gives no apply call".to_owned());
    };
    let extra: Vec<&str> = apply
        .caller_supplies
        .iter()
        .map(String::as_str)
        .filter(|name| !matches!(*name, "operation_id" | "reason"))
        .collect();
    if !extra.is_empty() {
        return Err(format!("needs input from the caller: {}", extra.join(", ")));
    }
    if !finding.complete {
        return Err(match finding.kind {
            FindingKind::LapsedHold => "its facts are incomplete: lease renewal is not hook-covered, so the lapse may not mean the session stopped; inspect it with agentscm work recovery, and release it with agentscm work transitions if it is obsolete".to_owned(),
            _ => "its facts are incomplete; inspect it with agentscm work recovery".to_owned(),
        });
    }
    let args = &apply.arguments;
    let (Some(generation), Some(seq)) = (
        args.get("generation").and_then(Value::as_i64),
        args.get("expected_updated_seq").and_then(Value::as_i64),
    ) else {
        return Err("the apply call carries no fences".to_owned());
    };
    if args.get("work_id").and_then(Value::as_str) != Some(work.work_id.as_str()) {
        return Err("the apply call names another Work".to_owned());
    }
    let participants = match args.get("participants") {
        None => Vec::new(),
        Some(list) => serde_json::from_value(list.clone())
            .map_err(|_| "the apply call lists participants this version cannot read".to_owned())?,
    };
    Ok(RepairItemV1 {
        work_id: work.work_id.clone(),
        finding: finding.kind,
        remedy,
        item: BatchTransitionItem {
            work_id: work.work_id.clone(),
            operation_id: format!(
                "repair:{}:{generation}:{seq}:{}",
                work.work_id,
                word(&remedy)
            ),
            generation,
            expected_updated_seq: seq,
            transition,
            reason: format!("repair: {}", word(&finding.kind)),
            superseded_by: None,
            participants,
        },
    })
}

/// A short reading of a repair report.
pub fn render_repair(report: &RepairReportV1) -> String {
    let mut lines = Vec::new();
    if report.results.is_empty() {
        for p in &report.planned {
            lines.push(format!(
                "would {} {} ({})",
                word(&p.remedy),
                p.work_id,
                word(&p.finding)
            ));
        }
    } else {
        for r in &report.results {
            lines.push(format!(
                "{} {}{}",
                word(&r.outcome),
                r.work_id,
                r.code
                    .as_deref()
                    .map(|c| format!(" ({c})"))
                    .unwrap_or_default()
            ));
        }
    }
    for s in &report.skipped {
        lines.push(format!(
            "skipped {} ({}): {}",
            s.work_id,
            word(&s.finding),
            s.reason
        ));
    }
    if !report.complete {
        lines.push(
            "The listing is incomplete: more Work exists than was read; rerun to continue."
                .to_owned(),
        );
    }
    lines.push(if report.applied {
        format!(
            "{} planned, {} applied or replayed, {} not applied, {} skipped.",
            report.planned.len(),
            report.results.len() - report.failed(),
            report.failed(),
            report.skipped.len()
        )
    } else {
        format!(
            "Dry run: {} planned, {} skipped. Nothing changed; add --apply to apply the plan.",
            report.planned.len(),
            report.skipped.len()
        )
    });
    lines.join("\n")
}

/// A short reading of an inspection for an agent's reply.
pub fn render_text(recovery: &WorkRecoveryV1) -> String {
    let mut lines = Vec::new();
    let count = |bucket| {
        recovery
            .work
            .iter()
            .filter(|work| work.bucket == bucket)
            .count()
    };
    lines.push(format!(
        "AgentSCM recovery: {} active, {} need you, {} can resume, {} finished, {} of other owners in this page{}.",
        count(RecoveryBucket::Active),
        count(RecoveryBucket::NeedsYou),
        count(RecoveryBucket::Resumable),
        count(RecoveryBucket::Finished),
        count(RecoveryBucket::Other),
        if recovery.complete {
            String::new()
        } else {
            format!(
                "; more follows: repeat with after {}",
                recovery.next_after.as_deref().unwrap_or_default()
            )
        }
    ));
    for work in &recovery.work {
        let permitted: Vec<String> = work
            .remedies
            .iter()
            .filter(|remedy| remedy.permitted)
            .map(|remedy| word(&remedy.remedy))
            .collect();
        lines.push(format!(
            "- {} \"{}\" [{}]: {} You may: {}. (third-party text)",
            work.work_id,
            super::advice::sanitize(&work.title, 120),
            word(&work.bucket),
            work.why,
            if permitted.is_empty() {
                "nothing; ask its lead or owner".to_owned()
            } else {
                permitted.join(", ")
            }
        ));
    }
    for work in &recovery.work {
        for finding in &work.findings {
            lines.push(format!(
                "  finding {} {}: {}{}{}",
                work.work_id,
                word(&finding.kind),
                finding.evidence,
                if finding.complete {
                    ""
                } else {
                    " (facts incomplete)"
                },
                finding
                    .proposed
                    .map(|remedy| format!(" Proposed: {}.", word(&remedy)))
                    .unwrap_or_default()
            ));
        }
    }
    for conflict in &recovery.conflicts {
        let paths: Vec<&str> = conflict
            .paths
            .iter()
            .map(|pair| pair.their_path.as_str())
            .collect();
        lines.push(format!(
            "- conflict {} on {}: {}",
            conflict.work_id,
            paths.join(", "),
            conflict.next
        ));
    }
    if !recovery.resumable.is_empty() {
        lines.push(format!(
            "Resume one of {} instead of starting new Work over these paths: agentscm_work_recovery_preview with remedy resume names the call.",
            recovery.resumable.join(", ")
        ));
    }
    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hub::work::{WorkParticipant, WorkSource};
    use crate::hub::work_lifecycle::{
        Lease, OutcomeFact, RenewalCoverage, ReservationBasis, work_operational,
    };

    const OWNER: &str = "user-owner";

    fn item(state: WorkState) -> WorkItem {
        WorkItem {
            initial_issue_choice: Default::default(),
            issue_link: None,
            id: "wrk_00000000000000000000000000000001".to_owned(),
            repository: "example/repo".to_owned(),
            owner: OWNER.to_owned(),
            owner_login: "owner".to_owned(),
            source: WorkSource::Native {
                request_key: "key".to_owned(),
            },
            title: "Synthetic task".to_owned(),
            description: None,
            state,
            claim_status: ClaimStatus::Active,
            last_checkin_at: 10,
            reported_at: None,
            generation: 4,
            lead_token: "tok-lead".to_owned(),
            lead_agent: "claude".to_owned(),
            lead_session: "s-lead".to_owned(),
            participants: vec![WorkParticipant {
                owner: "user-other".to_owned(),
                login: "helper".to_owned(),
                token: "tok-helper".to_owned(),
                agent: "codex".to_owned(),
                session: "s-helper".to_owned(),
                role: WorkRole::Helper,
            }],
            scope: vec!["src/a.rs".to_owned(), "src/b/".to_owned()],
            branch: None,
            worktree: None,
            next_action: Some("write the tests".to_owned()),
            current: None,
            blocker: None,
            depends_on: Vec::new(),
            related_work: Vec::new(),
            created_at: 1,
            updated_at: 10,
            created_seq: 1,
            updated_seq: 9,
        }
    }

    fn view(item: &WorkItem, basis: Option<ReservationBasis>, expires: i64) -> WorkOperationalV1 {
        work_operational(&super::super::work_lifecycle::OperationalFacts {
            item,
            outcome: None::<OutcomeFact>,
            basis,
            hidden_by_merged_pr: false,
            merge: super::super::work_lifecycle::MergeStatus::Unknown,
            lease: basis.map(|_| Lease {
                renewed_at: 50,
                expires_at: expires,
                grace_at: None,
            }),
            coverage: RenewalCoverage::ToolCalls,
            now: 100,
        })
    }

    fn viewer(owner: &str, token: &str, read_only: bool) -> RecoveryViewer {
        RecoveryViewer {
            owner: owner.to_owned(),
            token: token.to_owned(),
            read_only,
        }
    }

    fn permitted(
        item: &WorkItem,
        operational: &WorkOperationalV1,
        caller: &RecoveryCaller<'_>,
        viewer: &RecoveryViewer,
    ) -> Vec<RemedyKind> {
        let subject = Subject {
            item,
            operational,
            standing: caller.standing(viewer, item),
            prune: Vec::new(),
        };
        options(&subject, caller, viewer)
            .into_iter()
            .filter(|option| option.permitted)
            .map(|option| option.remedy)
            .collect()
    }

    fn agent<'a>(token: &'a str, session: &'a str) -> RecoveryCaller<'a> {
        RecoveryCaller::Agent {
            token,
            session: Some(session),
        }
    }

    #[test]
    fn the_owner_retires_a_dead_sessions_hold_but_another_owner_only_reads() {
        let item = item(WorkState::Active);
        let live = view(&item, Some(ReservationBasis::Admitted), 200);
        let owner = viewer(OWNER, "tok-owner-2", false);
        let mine = permitted(&item, &live, &agent("tok-owner-2", "s-new"), &owner);
        assert!(mine.contains(&RemedyKind::Release), "{mine:?}");
        assert!(mine.contains(&RemedyKind::Abandon));
        // A live hold of the owner's other session is not open to a takeover start.
        assert!(!mine.contains(&RemedyKind::Resume));
        assert!(!mine.contains(&RemedyKind::Handover));

        let other = viewer("user-other", "tok-helper", false);
        let theirs = permitted(&item, &live, &agent("tok-helper", "s-helper"), &other);
        assert!(theirs.is_empty(), "{theirs:?}");
        let subject = Subject {
            item: &item,
            operational: &live,
            standing: Standing::Other,
            prune: Vec::new(),
        };
        let resume = decide(
            RemedyKind::Resume,
            &subject,
            &agent("tok-helper", "s-helper"),
            &other,
        )
        .unwrap();
        assert_eq!(resume.code, RefusalCode::HoldLive);
        assert!(
            resume.message.contains("until Unix 200"),
            "{}",
            resume.message
        );
    }

    #[test]
    fn a_lapsed_hold_can_be_resumed_but_not_retained() {
        let item = item(WorkState::Paused);
        let lapsed = view(&item, Some(ReservationBasis::LegacyImplicit), 100);
        assert_eq!(lapsed.reservation.status, ReservationStatus::Lapsed);
        let owner = viewer(OWNER, "tok-owner-2", false);
        let caller = agent("tok-owner-2", "s-new");
        let open = permitted(&item, &lapsed, &caller, &owner);
        assert!(open.contains(&RemedyKind::Resume), "{open:?}");
        assert!(open.contains(&RemedyKind::Release));
        assert!(!open.contains(&RemedyKind::Retain));
        let subject = Subject {
            item: &item,
            operational: &lapsed,
            standing: Standing::Owner,
            prune: Vec::new(),
        };
        assert_eq!(bucket(&subject), RecoveryBucket::Resumable);
        let apply = apply_call(RemedyKind::Resume, &subject, &caller, None, None);
        assert_eq!(apply.tool.as_deref(), Some("agentscm_start"));
        assert_eq!(apply.arguments["takeover_generation"], 4);
        assert_eq!(apply.arguments["session"], "s-new");
    }

    #[test]
    fn read_only_and_unnamed_callers_cannot_escalate() {
        let item = item(WorkState::Active);
        let live = view(&item, Some(ReservationBasis::Admitted), 200);
        let reader = viewer(OWNER, "tok-lead", true);
        assert!(permitted(&item, &live, &agent("tok-lead", "s-lead"), &reader).is_empty());
        let owner = viewer(OWNER, "tok-owner-2", false);
        let unnamed = RecoveryCaller::Agent {
            token: "tok-owner-2",
            session: None,
        };
        assert!(permitted(&item, &live, &unnamed, &owner).is_empty());
    }

    #[test]
    fn a_person_owner_and_an_admin_get_the_person_matrix() {
        let item = item(WorkState::Active);
        let live = view(&item, Some(ReservationBasis::Admitted), 200);
        let user = |id: &str| crate::hub::store::User {
            id: id.to_owned(),
            identity: crate::hub::store::ForgeIdentity {
                provider: "github".to_owned(),
                host: "github.com".to_owned(),
                user_id: 1,
            },
            login: "person".to_owned(),
            name: None,
            disabled: false,
        };
        let owner_user = user(OWNER);
        let person = RecoveryCaller::Person {
            user: &owner_user,
            repository: "example/repo",
            repository_id: 7,
            is_admin: false,
        };
        let owner = viewer(OWNER, "", false);
        let open = permitted(&item, &live, &person, &owner);
        assert_eq!(
            open,
            [
                RemedyKind::Abandon,
                RemedyKind::Complete,
                RemedyKind::Release,
                RemedyKind::Retain,
                RemedyKind::Supersede
            ]
        );
        let subject = Subject {
            item: &item,
            operational: &live,
            standing: Standing::Owner,
            prune: Vec::new(),
        };
        let apply = apply_call(RemedyKind::Release, &subject, &person, None, None);
        assert_eq!(apply.route, "/v1/hub/session/work/transition");
        assert_eq!(apply.tool, None);
        assert_eq!(apply.arguments["repository"], "example/repo");
        assert!(apply.arguments.get("session").is_none());

        let admin_user = user("user-admin");
        let admin = RecoveryCaller::Person {
            user: &admin_user,
            repository: "example/repo",
            repository_id: 7,
            is_admin: true,
        };
        let open = permitted(&item, &live, &admin, &viewer("user-admin", "", false));
        assert_eq!(
            open,
            [
                RemedyKind::Abandon,
                RemedyKind::Release,
                RemedyKind::Supersede
            ]
        );
    }

    #[test]
    fn release_preserves_follow_up_and_ending_names_what_it_reaches() {
        let item = item(WorkState::Active);
        let live = view(&item, Some(ReservationBasis::Admitted), 200);
        let subject = Subject {
            item: &item,
            operational: &live,
            standing: Standing::Owner,
            prune: Vec::new(),
        };
        let release = effects(
            RemedyKind::Release,
            &subject,
            None,
            vec!["req-1".to_owned()],
            vec!["wrk-dependent".to_owned()],
            true,
        );
        assert_eq!(release.state, WorkState::Paused);
        assert_eq!(release.reservation, ReservationStatus::None);
        assert!(release.follow_up.unfinished);
        assert_eq!(
            release.follow_up.next_action.as_deref(),
            Some("write the tests")
        );
        assert_eq!(release.freed_paths, ["src/a.rs", "src/b/**"]);
        assert_eq!(release.fenced.len(), 2);
        assert_eq!(release.fenced[1].agent, "Codex");
        let json = serde_json::to_string(&release).unwrap();
        assert!(
            !json.contains("tok-") && !json.contains("s-helper"),
            "{json}"
        );

        let abandon = effects(RemedyKind::Abandon, &subject, None, vec![], vec![], true);
        assert_eq!(abandon.state, WorkState::Finished);
        assert_eq!(abandon.outcome, OutcomeValue::Abandoned);
        assert!(abandon.generation_advances);
        assert!(!abandon.follow_up.unfinished);
    }

    #[test]
    fn a_scoped_handover_frees_only_what_it_drops() {
        let item = item(WorkState::Active);
        let live = view(&item, Some(ReservationBasis::Admitted), 200);
        let subject = Subject {
            item: &item,
            operational: &live,
            standing: Standing::Lead,
            prune: Vec::new(),
        };
        let keep = vec!["src/a.rs".to_owned()];
        let handover = effects(
            RemedyKind::Handover,
            &subject,
            Some(&keep),
            vec![],
            vec![],
            true,
        );
        assert_eq!(handover.state, WorkState::Handover);
        assert_eq!(handover.kept_paths, ["src/a.rs"]);
        assert_eq!(handover.freed_paths, ["src/b/**"]);
        let caller = agent("tok-lead", "s-lead");
        let lead = viewer(OWNER, "tok-lead", false);
        assert!(permitted(&item, &live, &caller, &lead).contains(&RemedyKind::Handover));
        let apply = apply_call(RemedyKind::Handover, &subject, &caller, Some(&keep), None);
        assert_eq!(apply.arguments["scope"], json!(["src/a.rs"]));
        assert_eq!(apply.arguments["expected_updated_seq"], 9);
    }

    #[test]
    fn batch_failures_say_what_happened_and_how_to_retry() {
        let (outcome, code, _, retry) = classify_failure(&HubError::Invalid(
            "transition generation is fenced; current generation is 5".to_owned(),
        ));
        assert_eq!(
            (outcome, code),
            (BatchOutcome::Refused, "generation_changed")
        );
        assert!(retry.contains("current generation"));
        let (outcome, code, _, retry) = classify_failure(&HubError::Busy);
        assert_eq!((outcome, code), (BatchOutcome::Unavailable, "unavailable"));
        assert!(retry.contains("same operation_id"));
        let (_, code, message, _) = classify_failure(&HubError::Unauthorized);
        assert_eq!(code, "unauthorized");
        assert!(!message.is_empty());
    }

    #[test]
    fn queries_are_bounded() {
        let query = RecoveryQuery {
            paths: vec!["a".to_owned(); MAX_RECOVERY_PATHS + 1],
            ..RecoveryQuery::default()
        };
        assert!(query.validate().is_err());
        let query = RecoveryQuery {
            limit: Some(MAX_RECOVERY_PAGE + 1),
            ..RecoveryQuery::default()
        };
        assert!(query.validate().is_err());
        let query = RecoveryQuery {
            work_id: Some("wrk_00000000000000000000000000000001".to_owned()),
            after: Some("wrk_00000000000000000000000000000002".to_owned()),
            ..RecoveryQuery::default()
        };
        assert!(query.validate().is_err());
        assert_eq!(
            RecoveryQuery::default().validate().unwrap().limit,
            DEFAULT_RECOVERY_PAGE
        );
    }

    fn crowd(item: &mut WorkItem, owner: &str, count: usize, prefix: &str) -> Vec<ParticipantMark> {
        (0..count)
            .map(|n| {
                item.participants.push(WorkParticipant {
                    owner: owner.to_owned(),
                    login: owner.to_owned(),
                    token: format!("tok-{prefix}"),
                    agent: "codex".to_owned(),
                    session: format!("{prefix}-{n:03}"),
                    role: WorkRole::Helper,
                });
                ParticipantMark {
                    token: format!("tok-{prefix}"),
                    session: format!("{prefix}-{n:03}"),
                    generation: 4,
                    // Later numbers changed more recently.
                    updated_at: 1000 + n as i64,
                }
            })
            .collect()
    }

    #[test]
    fn a_prune_takes_other_owners_first_then_the_oldest_and_never_the_lead_or_caller() {
        let mut item = item(WorkState::Active);
        item.participants.clear();
        item.participants.push(WorkParticipant {
            owner: OWNER.to_owned(),
            login: "owner".to_owned(),
            token: "tok-lead".to_owned(),
            agent: "claude".to_owned(),
            session: "s-lead".to_owned(),
            role: WorkRole::Lead,
        });
        let mut marks = crowd(&mut item, OWNER, 60, "mine");
        marks.extend(crowd(&mut item, "user-other", 39, "theirs"));
        assert_eq!(item.participants.len(), 100);
        let caller = agent("tok-mine", "mine-000");
        // Full, but the other owners' share (39) is over its limit too: 15 of theirs go first and
        // the total only needs one removed, so exactly 15 are chosen.
        let chosen = prune_targets(&item, &caller, &marks);
        assert_eq!(chosen.len(), 15);
        assert!(chosen.iter().all(|p| p.owner == "user-other"));
        assert_eq!(chosen[0].session, "theirs-000", "oldest first");
        assert!(
            chosen
                .iter()
                .all(|p| p.session != "s-lead" && p.session != "mine-000")
        );
        assert_eq!(
            chosen,
            prune_targets(&item, &caller, &marks),
            "deterministic"
        );

        // Full with only the owner's own: one goes, and never the caller's own session.
        item.participants.retain(|p| p.owner == OWNER);
        item.participants.push(WorkParticipant {
            owner: OWNER.to_owned(),
            login: "owner".to_owned(),
            token: "tok-x".to_owned(),
            agent: "codex".to_owned(),
            session: "x".to_owned(),
            role: WorkRole::Helper,
        });
        marks.extend(crowd(&mut item, OWNER, 38, "more"));
        assert_eq!(item.participants.len(), 100);
        let chosen = prune_targets(&item, &caller, &marks);
        assert_eq!(chosen.len(), 1);
        assert_eq!(chosen[0].session, "x", "no mark sorts oldest");
        // Room for a new session and no share over: nothing to prune.
        item.participants.pop();
        assert!(prune_targets(&item, &caller, &marks).is_empty());
    }

    #[test]
    fn findings_report_facts_and_never_propose_complete() {
        let owner = viewer(OWNER, "tok-owner-2", false);
        let caller = agent("tok-owner-2", "s-new");
        let mut stale = item(WorkState::Blocked);
        stale.claim_status = ClaimStatus::Stale;
        let lapsed = view(&stale, Some(ReservationBasis::Admitted), 100);
        let subject = Subject {
            item: &stale,
            operational: &lapsed,
            standing: Standing::Owner,
            prune: Vec::new(),
        };
        let requests = vec!["req-1".to_owned()];
        let found = findings(&subject, &caller, &owner, &requests, true);
        let kinds: Vec<_> = found.iter().map(|finding| finding.kind).collect();
        assert_eq!(
            kinds,
            [
                FindingKind::LapsedHold,
                FindingKind::AbandonedSession,
                FindingKind::OrphanedRequests,
                FindingKind::ContradictoryBlocker
            ]
        );
        assert_eq!(found[0].proposed, Some(RemedyKind::Release));
        assert!(found[0].complete);
        assert!(
            found[0].evidence.contains("Unix 100"),
            "{}",
            found[0].evidence
        );
        assert_eq!(found[1].proposed, Some(RemedyKind::Resume));
        assert_eq!(found[2].proposed, None);
        assert!(found[3].evidence.contains("records no blocker"));
        assert!(
            found
                .iter()
                .all(|f| f.proposed != Some(RemedyKind::Complete))
        );
        // A read-only token is proposed nothing, and a cut-short list says so.
        let reader = viewer(OWNER, "tok-owner-2", true);
        let found = findings(&subject, &caller, &reader, &requests, false);
        assert!(found.iter().all(|f| f.proposed.is_none()));
        assert!(!found[2].complete && found[2].evidence.starts_with("At least"));
        // A lease with no coverage claim is not proof the session is gone.
        let mut unknown = lapsed.clone();
        unknown.completeness.unknown = vec!["activity_coverage".to_owned()];
        let subject = Subject {
            operational: &unknown,
            ..subject
        };
        assert!(!findings(&subject, &caller, &owner, &[], true)[0].complete);
    }

    #[test]
    fn a_live_hold_hidden_by_a_merged_pr_is_a_report_only_finding() {
        let owner = viewer(OWNER, "tok-owner-2", false);
        let caller = agent("tok-owner-2", "s-new");
        let item = item(WorkState::Active);
        let mut hidden = view(&item, Some(ReservationBasis::Admitted), 200);
        hidden.merge.hidden_by_merged_pr = true;
        let subject = |operational| Subject {
            item: &item,
            operational,
            standing: Standing::Owner,
            prune: Vec::new(),
        };
        let mut observed = hidden.clone();
        observed.merge.status = MergeStatus::Observed;
        let found = findings(&subject(&hidden), &caller, &owner, &[], true);
        assert_eq!(found.len(), 1, "{found:?}");
        assert_eq!(found[0].kind, FindingKind::HiddenHold);
        assert_eq!(found[0].proposed, None);
        assert!(!found[0].complete, "the merge fact is unknown here");
        let found = findings(&subject(&observed), &caller, &owner, &[], true);
        assert!(found[0].complete);
        let visible = view(&item, Some(ReservationBasis::Admitted), 200);
        assert!(findings(&subject(&visible), &caller, &owner, &[], true).is_empty());
    }

    #[test]
    fn a_batch_item_without_participants_omits_the_key() {
        let plain = BatchTransitionItem {
            work_id: "wrk_a".to_owned(),
            operation_id: "op".to_owned(),
            generation: 1,
            expected_updated_seq: 2,
            transition: WorkTransitionKind::Release,
            reason: "synthetic".to_owned(),
            superseded_by: None,
            participants: Vec::new(),
        };
        let json = serde_json::to_value(&plain).unwrap();
        assert!(json.get("participants").is_none(), "{json}");
    }

    fn finding(kind: FindingKind, proposed: Option<RemedyKind>) -> FindingV1 {
        FindingV1 {
            kind,
            evidence: "synthetic".to_owned(),
            complete: true,
            proposed,
        }
    }

    fn option(remedy: RemedyKind, id: &str, seq: i64, permitted: bool) -> RemedyOptionV1 {
        RemedyOptionV1 {
            remedy,
            permitted,
            refusal: (!permitted).then(|| RemedyRefusalV1 {
                code: RefusalCode::HoldLive,
                message: "a live hold".to_owned(),
            }),
            apply: permitted.then(|| ApplyV1 {
                tool: None,
                route: "/v1/hub/standup/work-transitions".to_owned(),
                arguments: json!({"work_id": id, "generation": 4, "expected_updated_seq": seq}),
                caller_supplies: vec!["operation_id".to_owned(), "reason".to_owned()],
            }),
        }
    }

    fn recovered(
        id: &str,
        standing: Standing,
        findings: Vec<FindingV1>,
        remedies: Vec<RemedyOptionV1>,
    ) -> RecoveryWorkV1 {
        let it = item(WorkState::Active);
        RecoveryWorkV1 {
            work_id: id.to_owned(),
            title: "t".to_owned(),
            source: it.source.clone(),
            owner_login: "owner".to_owned(),
            lead_agent: "claude".to_owned(),
            standing,
            bucket: RecoveryBucket::Resumable,
            why: "w".to_owned(),
            follow_up: follow_up(&it, true),
            requests: Vec::new(),
            dependents: Vec::new(),
            affected_complete: true,
            operational: view(&it, None, 0),
            remedies,
            findings,
        }
    }

    fn inventory(work: Vec<RecoveryWorkV1>) -> WorkRecoveryV1 {
        WorkRecoveryV1 {
            schema_version: RECOVERY_SCHEMA_VERSION,
            observed_at: 100,
            read_only: false,
            work,
            next_after: None,
            complete: true,
            conflicts: Vec::new(),
            conflicts_complete: true,
            resumable: Vec::new(),
            resumable_complete: true,
        }
    }

    #[test]
    fn a_repair_plan_is_deterministic_and_skips_everything_it_must_not_apply() {
        let (a, b, c, d, e, f) = ("wrk_a", "wrk_b", "wrk_c", "wrk_d", "wrk_e", "wrk_f");
        let mut incomplete = finding(FindingKind::LapsedHold, Some(RemedyKind::Release));
        incomplete.complete = false;
        let all = inventory(vec![
            recovered(
                a,
                Standing::Owner,
                vec![finding(FindingKind::LapsedHold, Some(RemedyKind::Release))],
                vec![option(RemedyKind::Release, a, 9, true)],
            ),
            recovered(
                b,
                Standing::Owner,
                vec![
                    finding(FindingKind::ParticipantCap, Some(RemedyKind::Complete)),
                    finding(FindingKind::AbandonedSession, Some(RemedyKind::Resume)),
                    finding(FindingKind::OrphanedRequests, None),
                    finding(FindingKind::ContradictoryBlocker, None),
                ],
                vec![option(RemedyKind::Complete, b, 1, true)],
            ),
            recovered(
                c,
                Standing::Other,
                vec![finding(FindingKind::LapsedHold, Some(RemedyKind::Release))],
                vec![option(RemedyKind::Release, c, 1, true)],
            ),
            recovered(
                d,
                Standing::Owner,
                vec![finding(FindingKind::LapsedHold, Some(RemedyKind::Release))],
                vec![option(RemedyKind::Release, d, 1, false)],
            ),
            recovered(
                e,
                Standing::Owner,
                vec![incomplete],
                vec![option(RemedyKind::Release, e, 1, true)],
            ),
            recovered(
                f,
                Standing::Owner,
                vec![
                    finding(FindingKind::LapsedHold, Some(RemedyKind::Release)),
                    finding(
                        FindingKind::ParticipantCap,
                        Some(RemedyKind::PruneParticipants),
                    ),
                ],
                vec![
                    option(RemedyKind::Release, f, 3, true),
                    option(RemedyKind::PruneParticipants, f, 3, true),
                ],
            ),
        ]);
        let plan = repair_plan(&all);
        assert_eq!(plan, repair_plan(&all));
        let ids: Vec<&str> = plan
            .planned
            .iter()
            .map(|p| p.item.operation_id.as_str())
            .collect();
        assert_eq!(
            ids,
            ["repair:wrk_a:4:9:release", "repair:wrk_f:4:3:release"]
        );
        assert_eq!(plan.planned[0].item.reason, "repair: lapsed_hold");
        assert!(
            plan.planned
                .iter()
                .all(|p| p.remedy != RemedyKind::Complete)
        );
        let reason = |work: &str, kind| {
            plan.skipped
                .iter()
                .find(|s| s.work_id == work && s.finding == kind)
                .unwrap_or_else(|| panic!("{work} {kind:?}"))
                .reason
                .as_str()
        };
        assert!(reason(b, FindingKind::ParticipantCap).starts_with("complete is never planned"));
        assert!(reason(b, FindingKind::AbandonedSession).starts_with("needs a fresh start"));
        assert!(reason(b, FindingKind::OrphanedRequests).starts_with("report only"));
        assert!(reason(b, FindingKind::ContradictoryBlocker).starts_with("report only"));
        assert!(reason(c, FindingKind::LapsedHold).starts_with("another owner's Work"));
        assert!(reason(d, FindingKind::LapsedHold).contains("hold_live"));
        assert!(reason(e, FindingKind::LapsedHold).contains("not hook-covered"));
        assert!(reason(e, FindingKind::LapsedHold).contains("agentscm work transitions"));
        assert!(reason(f, FindingKind::ParticipantCap).contains("planned first"));
        assert_eq!(plan.skipped.len(), 8);
    }

    #[test]
    fn a_repair_needing_caller_input_is_skipped() {
        let mut supersede = option(RemedyKind::Supersede, "wrk_a", 1, true);
        supersede
            .apply
            .as_mut()
            .unwrap()
            .caller_supplies
            .push("superseded_by".to_owned());
        let plan = repair_plan(&inventory(vec![recovered(
            "wrk_a",
            Standing::Owner,
            vec![finding(
                FindingKind::LapsedHold,
                Some(RemedyKind::Supersede),
            )],
            vec![supersede],
        )]));
        assert!(plan.planned.is_empty());
        assert!(plan.skipped[0].reason.contains("superseded_by"));
    }

    #[test]
    fn only_a_hub_administrator_may_ask_for_every_owner() {
        let query = RecoveryQuery {
            all_owners: true,
            work_id: Some("wrk_00000000000000000000000000000001".to_owned()),
            ..RecoveryQuery::default()
        };
        assert!(query.validate().is_err());
        let query = RecoveryQuery {
            all_owners: true,
            ..RecoveryQuery::default()
        };
        assert!(query.validate().unwrap().all_owners);
        assert!(
            serde_json::to_value(RecoveryQuery::default())
                .unwrap()
                .get("all_owners")
                .is_none(),
            "an older Hub rejects an unknown field, so the default is never sent"
        );
    }
}
