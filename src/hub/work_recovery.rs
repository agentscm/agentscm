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
use super::work::{ClaimStatus, WorkItem, WorkPathOverlap, WorkRole, WorkState, validate_id};
use super::work_lifecycle::{
    ActorVia, LEASE_LAPSED, OperationalRow, OutcomeValue, ReservationStatus, WorkActorKind,
    WorkOperationalV1, WorkTransitionKind, decide_transition, operational_page,
};
use super::work_transition::{
    WorkLifecycleReader, WorkTransitionActor, WorkTransitionRequest, WorkTransitionResult,
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
}

/// What the store read in one snapshot.
pub struct RecoveryFacts {
    pub viewer: RecoveryViewer,
    /// The ids of the page: the owner's unfinished Work after the cursor, or the one `work_id`.
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
    /// Lead the Work again through `agentscm_start` with `takeover_generation`.
    Resume,
    /// The lead hands the Work over with a handover checkpoint, optionally narrowing its scope.
    Handover,
}

impl RemedyKind {
    pub const ALL: [Self; 8] = [
        Self::Abandon,
        Self::Complete,
        Self::Handover,
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
    /// released.
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

fn inspected(
    row: &RecoveryRow,
    operational: &WorkOperationalV1,
    caller: &RecoveryCaller<'_>,
    viewer: &RecoveryViewer,
) -> RecoveryWorkV1 {
    let item = &row.row.item;
    let subject = Subject {
        item,
        operational,
        standing: caller.standing(viewer, item),
    };
    let (requests, requests_complete) = bounded(row.requests.clone());
    let (dependents, dependents_complete) = bounded(row.dependents.clone());
    RecoveryWorkV1 {
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
    let subject = Subject {
        item,
        operational,
        standing: caller.standing(&facts.viewer, item),
    };
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
}
