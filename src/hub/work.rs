//! Durable shared engineering work for stand-ups and agent hand-offs.

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};

use super::coordination::CoordinationRequest;
use super::github::valid_ref_name;
use super::intent::{display, overlaps, spec_key, text};
use super::store::{User, validate_repository};
use super::work_preflight::{WorkPreflightRequest, WorkPreflightResult};
use super::{HubError, Result};

pub const MAX_WORK_PAGE: usize = 100;
pub const MAX_BRIEFING_BYTES: usize = 2 * 1024 * 1024;
pub const CLAIM_STALE_SECONDS: i64 = 15 * 60;
pub const DEFAULT_RECENT_WORK_SECONDS: i64 = 24 * 60 * 60;
pub const WORK_PAYLOAD_CAPACITY_BYTES: i64 = 128 * 1024 * 1024;
pub const WORK_DELIVERY_RESERVATION_BYTES: i64 = 4 * 1024 * 1024;
pub(super) const WORK_PAYLOAD_ROW_BYTES: i64 = 256;
const MAX_RELATIONS: usize = 100;
const MAX_REFS: usize = 100;
const MAX_SCOPE: usize = 256;
const MAX_LABEL_BYTES: usize = 255;
const MAX_TEXT_BYTES: usize = 500;

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ForgeWorkRef {
    pub provider: String,
    pub host: String,
    pub repository: String,
    pub issue: String,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum WorkRole {
    #[default]
    Lead,
    Helper,
    Reviewer,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct WorkStart {
    /// Explicit caller choice; omission remains unassessed compatibility advice.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub issue_choice: Option<super::work_issues::WorkIssueChoice>,
    pub work_id: Option<String>,
    pub source: Option<ForgeWorkRef>,
    pub request_key: Option<String>,
    pub title: String,
    /// What the new Work changes and why, for a teammate; set only when Work is created.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub session: String,
    #[serde(default)]
    pub role: WorkRole,
    #[serde(default)]
    pub scope: Vec<String>,
    pub branch: Option<String>,
    pub worktree: Option<String>,
    pub next_action: Option<String>,
    #[serde(default)]
    pub depends_on: Vec<String>,
    #[serde(default)]
    pub related_work: Vec<String>,
    pub takeover_generation: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preflight: Option<WorkPreflightRequest>,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CheckpointKind {
    Progress,
    Decision,
    Blocker,
    Pause,
    Finish,
    Handover,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum SourceRef {
    RepositoryPath {
        path: String,
        line: Option<u32>,
    },
    Commit {
        oid: String,
        path: Option<String>,
    },
    Work {
        work_id: String,
    },
    CapturedEvent {
        session_id: String,
        event_id: String,
    },
    PriorWorkVersion {
        work_id: String,
        event_seq: i64,
    },
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct WorkCheckpoint {
    pub work_id: String,
    /// Stable caller-chosen idempotency key for this transition.
    pub operation_id: String,
    pub generation: i64,
    pub session: String,
    /// Reject a new transition if the item changed after the caller read it.
    #[serde(default)]
    pub expected_updated_seq: Option<i64>,
    pub kind: CheckpointKind,
    pub summary: String,
    pub next_action: Option<String>,
    pub scope: Option<Vec<String>>,
    pub branch: Option<String>,
    pub worktree: Option<String>,
    #[serde(default)]
    pub resolve_blocker: bool,
    #[serde(default)]
    pub source_refs: Vec<SourceRef>,
    #[serde(default)]
    pub depends_on: Vec<String>,
    #[serde(default)]
    pub related_work: Vec<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum WorkSource {
    Native {
        request_key: String,
    },
    Forge {
        provider: String,
        host: String,
        repository: String,
        issue: String,
    },
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum WorkState {
    Active,
    Blocked,
    Paused,
    Finished,
    Handover,
}

impl WorkState {
    /// Change capture continues while the Work is still the session's own. A session end
    /// pauses the Work and a resumed session keeps capturing, so Paused and Blocked count;
    /// Finished and Handover end the session's claim.
    pub(super) fn accepts_capture(self) -> bool {
        matches!(self, Self::Active | Self::Paused | Self::Blocked)
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ClaimStatus {
    Active,
    Stale,
    Unknown,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct WorkParticipant {
    pub owner: String,
    pub login: String,
    pub token: String,
    pub agent: String,
    pub session: String,
    pub role: WorkRole,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Checkpoint {
    pub kind: CheckpointKind,
    pub summary: String,
    pub next_action: Option<String>,
    pub source_refs: Vec<SourceRef>,
    pub at: i64,
    pub token: String,
    pub agent: String,
    pub session: String,
    pub generation: i64,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct CheckpointReceipt {
    pub work_id: String,
    pub operation_id: String,
    pub generation: i64,
    pub event_seq: i64,
    pub replayed: bool,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct WorkItem {
    /// Immutable initial caller choice, independent of subsequent issue links.
    #[serde(default)]
    pub initial_issue_choice: super::work_issues::WorkIssueChoice,
    /// Current caller assertion, never provider-confirmed assignment.
    #[serde(default)]
    pub issue_link: Option<super::planned_issues::IssueIdentity>,
    pub id: String,
    pub repository: String,
    pub owner: String,
    pub owner_login: String,
    pub source: WorkSource,
    pub title: String,
    /// What this Work changes and why, from the start that created it; absent when it had none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub state: WorkState,
    /// A request-time projection. Stale and unknown claims remain claimed.
    pub claim_status: ClaimStatus,
    pub last_checkin_at: i64,
    /// Set only on the automatic pause written at session end: the last check-in time before it.
    /// Later auto-pauses keep it, so the pause does not make the session look newly checked in.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reported_at: Option<i64>,
    pub generation: i64,
    pub lead_token: String,
    pub lead_agent: String,
    pub lead_session: String,
    pub participants: Vec<WorkParticipant>,
    pub scope: Vec<String>,
    pub branch: Option<String>,
    pub worktree: Option<String>,
    pub next_action: Option<String>,
    pub current: Option<Checkpoint>,
    pub blocker: Option<Checkpoint>,
    pub depends_on: Vec<String>,
    pub related_work: Vec<String>,
    pub created_at: i64,
    pub updated_at: i64,
    pub created_seq: i64,
    pub updated_seq: i64,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct WorkCheckpointResult {
    /// The current authorized projection, which can be newer than a replayed operation.
    pub item: WorkItem,
    /// The immutable commit identity for the requested operation.
    pub receipt: CheckpointReceipt,
    /// Lapsed holds a scope change took the scope of.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub reclaimed: Vec<ReclaimedHold>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct WorkRepositoryScope {
    pub provider: String,
    pub host: String,
    pub repository: String,
    pub repository_id: Option<i64>,
}

/// Local operator repair; the destination must match a connected stable repository ID.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct WorkRepositoryRename {
    pub scope: WorkRepositoryScope,
    pub destination: String,
    pub apply: bool,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct WorkRepositoryRenameResult {
    pub work_items: usize,
    pub coordination_requests: usize,
    pub applied: bool,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct WorkCompaction {
    pub scope: WorkRepositoryScope,
    pub before: i64,
    pub applied: bool,
    pub deleted_operations: u64,
    pub deleted_versions: u64,
    pub retention_floor: i64,
    pub deleted_coordination_requests: u64,
    pub coordination_retention_floor: i64,
    #[serde(default)]
    pub deleted_merge_observations: u64,
    #[serde(default)]
    pub merge_retention_floor: i64,
}

/// Which open deliveries a compaction ends: those whose event window starts below the first
/// returned seq, or whose merge window starts below the second. Each floor is compared with its
/// own old value and the highest raised one cuts; a pass that raised none returns 0, so a
/// delivery that reads nothing removed keeps paging.
pub fn delivery_cuts(
    old_event_floors: [i64; 3],
    new_event_floors: [i64; 3],
    old_merge_floor: i64,
    new_merge_floor: i64,
) -> (i64, i64) {
    let raised = |old: i64, new: i64| if new > old { new } else { 0 };
    let event = old_event_floors
        .into_iter()
        .zip(new_event_floors)
        .map(|(old, new)| raised(old, new))
        .max()
        .unwrap_or(0);
    (event, raised(old_merge_floor, new_merge_floor))
}

/// A lapsed hold whose scope an admission took. Its lead's branch may hold edits it never pushed;
/// the Hub cannot see them.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ReclaimedHold {
    pub work_id: String,
    pub lead_agent: String,
    pub branch: Option<String>,
    pub lapsed_at: i64,
    pub notice: String,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct WorkStartResult {
    pub item: WorkItem,
    pub created: bool,
    pub joined: bool,
    pub took_over: bool,
    pub conflict: Option<WorkStartConflict>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context: Option<WorkOverview>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub coordination_request: Option<CoordinationRequest>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preflight: Option<WorkPreflightResult>,
    /// Lapsed holds this admission took the scope of.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub reclaimed: Vec<ReclaimedHold>,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum WorkConflictKind {
    ClaimHeld,
    StaleTakeoverRequired,
    GenerationChanged,
    SessionGoalChanged,
    MetadataChanged,
    ScopeOverlap,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct WorkStartConflict {
    pub kind: WorkConflictKind,
    pub current_generation: i64,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub overlaps: Vec<WorkScopeOverlap>,
    /// The operational projection of each overlapping Work. Filled only by a surface that was
    /// asked for it; the stores leave it empty so an older strict client never sees the key.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub operational: Vec<super::work_lifecycle::WorkOperationalV1>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct WorkPathOverlap {
    pub your_path: String,
    pub their_path: String,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct WorkScopeOverlap {
    pub item: WorkItem,
    pub your_scope: Vec<String>,
    pub their_scope: Vec<String>,
    pub reasons: Vec<WorkPathOverlap>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct WorkOverviewQuery {
    pub recent_since: Option<i64>,
    #[serde(default = "default_limit")]
    pub limit: usize,
}

impl Default for WorkOverviewQuery {
    fn default() -> Self {
        Self {
            recent_since: None,
            limit: default_limit(),
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum WorkOutcomeKind {
    Decision,
    Completion,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct WorkOutcome {
    pub seq: i64,
    pub at: i64,
    pub kind: WorkOutcomeKind,
    pub item: WorkItem,
    /// Set only on a completion row built from a `work_transition` event.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ended: Option<WorkEnded>,
}

/// Who ended Work through a transition and why, as the `work_transition` event records it. The
/// event stores the actor's kind, user login and how they authenticated, not an agent label.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct WorkEnded {
    pub transition: String,
    pub reason: String,
    pub by: WorkEndedBy,
    pub at: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub superseded_by: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct WorkEndedBy {
    pub kind: super::work_lifecycle::WorkActorKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub login: Option<String>,
    pub via: super::work_lifecycle::ActorVia,
}

impl WorkEnded {
    /// Reads a `work_transition` event's data; None for any other event kind or unreadable data.
    pub(super) fn from_event(kind: &str, data: &str, at: i64) -> Option<Self> {
        if kind != "work_transition" {
            return None;
        }
        let v: serde_json::Value = serde_json::from_str(data).ok()?;
        let actor = v.get("actor")?;
        Some(Self {
            transition: v.get("transition")?.as_str()?.to_owned(),
            reason: v.get("reason")?.as_str()?.to_owned(),
            by: WorkEndedBy {
                kind: serde_json::from_value(actor.get("kind")?.clone()).ok()?,
                login: actor
                    .get("login")
                    .and_then(|l| l.as_str())
                    .map(str::to_owned),
                via: serde_json::from_value(actor.get("via")?.clone()).ok()?,
            },
            at,
            superseded_by: v
                .get("superseded_by")
                .and_then(|l| l.as_str())
                .map(str::to_owned),
        })
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct WorkPathPair {
    pub left_path: String,
    pub right_path: String,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct WorkOverlapPair {
    pub left: WorkItem,
    pub right: WorkItem,
    pub reasons: Vec<WorkPathPair>,
    pub reasons_complete: bool,
}

/// One work item found by [`Store::work_search`]; enough to pick it, not to read it.
#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct WorkSearchItem {
    pub id: String,
    pub title: String,
    pub state: String,
    pub lead_agent: String,
    pub owner: Option<String>,
    pub branch: Option<String>,
    pub source_issue: Option<String>,
    pub updated_at: i64,
    /// Creation time: the immutable position the paging cursor is built from.
    pub created_at: i64,
}

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct WorkSearchPage {
    pub items: Vec<WorkSearchItem>,
    pub next_cursor: Option<String>,
    pub complete: bool,
}

/// Reads a [`WorkSearchPage`] cursor, `<created_at>:<id>`, as the position to continue after.
pub(super) fn parse_search_cursor(cursor: &str) -> Result<(i64, String)> {
    let (at, id) = cursor
        .split_once(':')
        .filter(|(at, _)| {
            !at.is_empty() && at.len() <= 18 && at.bytes().all(|b| b.is_ascii_digit())
        })
        .ok_or_else(|| HubError::Invalid("malformed cursor".into()))?;
    validate_id("cursor", id, "wrk")?;
    let at = at
        .parse()
        .map_err(|_| HubError::Invalid("malformed cursor".into()))?;
    Ok((at, id.to_owned()))
}

impl WorkSearchPage {
    /// `items` read with `limit + 1`: the extra row only says more exist.
    pub(super) fn of(mut items: Vec<WorkSearchItem>, limit: usize) -> Self {
        let more = items.len() > limit;
        items.truncate(limit);
        let next_cursor = more
            .then(|| items.last().map(|i| format!("{}:{}", i.created_at, i.id)))
            .flatten();
        Self {
            items,
            next_cursor,
            complete: !more,
        }
    }
}

/// Work search page size by default and at most.
pub const DEFAULT_WORK_SEARCH: usize = 20;
pub const MAX_WORK_SEARCH: usize = 50;

/// Summary of the pause checkpoint written when a session ends without a reported outcome. Work
/// holding it is not settled by a merge that predates the session's last check-in.
pub(super) const AUTO_PAUSE_SUMMARY: &str = "Session ended without a reported final work outcome.";

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct WorkOverview {
    #[serde(default)]
    pub planned_issues: super::planned_issues::PlannedIssuesView,

    pub recent_since: i64,
    pub until: i64,
    pub current: Vec<WorkItem>,
    pub recent: Vec<WorkOutcome>,
    pub overlaps: Vec<WorkOverlapPair>,
    pub current_complete: bool,
    pub recent_complete: bool,
    pub overlaps_complete: bool,
    pub history_retention_gap: bool,
    /// Unfinished Work not listed in `current` because its branch has a merged pull request.
    /// "Settled" never means finished. It does not affect `current_complete`.
    #[serde(default)]
    pub settled_count: u64,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct WorkScopeCheck {
    pub item: Option<WorkItem>,
    pub requested_scope: Vec<String>,
    pub uncovered_scope: Vec<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct BriefingQuery {
    /// Optional session/subscriber identity. Omitted calls retain token-level catch-up.
    #[serde(default)]
    pub subscriber: Option<String>,
    pub delivery: Option<String>,
    pub after: Option<i64>,
    pub work_after: Option<String>,
    #[serde(default)]
    pub work_done: bool,
    pub through: Option<i64>,
    #[serde(default = "default_limit")]
    pub limit: usize,
}

impl Default for BriefingQuery {
    fn default() -> Self {
        Self {
            subscriber: None,
            delivery: None,
            after: None,
            work_after: None,
            work_done: false,
            through: None,
            limit: default_limit(),
        }
    }
}

const fn default_limit() -> usize {
    MAX_WORK_PAGE
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct WorkEvent {
    pub seq: i64,
    pub at: i64,
    pub kind: String,
    pub work_id: String,
    pub item: WorkItem,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Briefing {
    #[serde(default)]
    pub planned_issues: super::planned_issues::PlannedIssuesView,

    /// The explicit subscriber identity, or `None` for the token-level compatibility cursor.
    #[serde(default)]
    pub subscriber: Option<String>,
    pub delivery: Option<String>,
    pub from: i64,
    pub through: i64,
    pub after: i64,
    pub work_after: Option<String>,
    pub next_after: i64,
    pub next_work_after: Option<String>,
    pub next_work_done: bool,
    pub items: Vec<WorkItem>,
    pub events: Vec<WorkEvent>,
    pub retention_gap: bool,
    pub complete: bool,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct BriefingAck {
    pub delivery: String,
}

pub(super) struct WorkWho {
    pub token: String,
    pub repository: String,
    pub repository_id: Option<i64>,
    pub agent: String,
    pub read_only: bool,
    pub owner: User,
}

impl WorkWho {
    /// `limit` refused a write to this caller's repository.
    pub(super) fn capacity(&self, limit: super::capacity::CapacityLimit) -> HubError {
        HubError::capacity(
            limit,
            Some(super::capacity::CapacityRepository {
                provider: self.owner.identity.provider.clone(),
                host: self.owner.identity.host.clone(),
                repository: self.repository.clone(),
                repository_id: self.repository_id,
            }),
        )
    }
}

/// Whose view a work read is: an agent token's, or a person's on one connected repository whose
/// role the caller has just checked (the web pages' repository list, which holds no token).
pub(super) enum WorkReader<'a> {
    Token(&'a str),
    Person {
        owner: &'a User,
        repository: &'a str,
        repository_id: i64,
    },
}

impl WorkReader<'_> {
    /// The reader's scope, resolving a token through `token`.
    pub(super) fn who(&self, token: impl FnOnce(&str) -> Result<WorkWho>) -> Result<WorkWho> {
        match *self {
            Self::Token(id) => token(id),
            Self::Person {
                owner,
                repository,
                repository_id,
            } => Ok(WorkWho {
                token: String::new(),
                repository: repository.to_owned(),
                repository_id: Some(repository_id),
                agent: String::new(),
                read_only: true,
                owner: owner.clone(),
            }),
        }
    }
}

impl WorkStart {
    pub(super) fn validate(&self, who: &WorkWho) -> Result<Vec<String>> {
        if usize::from(self.work_id.is_some())
            + usize::from(self.source.is_some())
            + usize::from(self.request_key.is_some())
            != 1
        {
            let problem =
                if self.work_id.is_none() && self.source.is_none() && self.request_key.is_none() {
                    super::error_envelope::FieldProblem::Missing
                } else {
                    super::error_envelope::FieldProblem::Conflict
                };
            return Err(HubError::Input(
                ["work_id", "source", "request_key"]
                    .into_iter()
                    .filter_map(|name| {
                        super::error_envelope::InputProblem::new(
                            name,
                            problem,
                            "Provide exactly one of work_id, source or request_key.",
                        )
                    })
                    .collect(),
            ));
        }
        if self.work_id.is_none() && self.role != WorkRole::Lead {
            return Err(invalid("new work must start with a lead"));
        }
        if let Some(id) = &self.work_id {
            validate_id("work", id, "wrk")?;
        }
        if let Some(source) = &self.source {
            source.validate(who)?;
        }
        if let Some(key) = &self.request_key {
            label("request_key", key)?;
        }
        text("title", &self.title, MAX_TEXT_BYTES)?;
        option_text("description", &self.description, MAX_TEXT_BYTES)?;
        label("session", &self.session)?;
        option_text("next_action", &self.next_action, MAX_TEXT_BYTES)?;
        option_label("worktree", &self.worktree)?;
        if let Some(branch) = &self.branch {
            label("branch", branch)?;
            if !valid_ref_name(branch) {
                return Err(invalid("branch is not a valid git ref name"));
            }
        }
        let scope = paths(&self.scope)?;
        relations(&self.depends_on, &self.related_work)?;
        if self
            .takeover_generation
            .is_some_and(|generation| generation < 1)
        {
            return Err(invalid("takeover_generation must be positive"));
        }
        if let Some(choice) = &self.issue_choice {
            choice.validate(who)?;
            if self.source.is_some() {
                return Err(invalid("issue_choice applies only to native Work"));
            }
        }
        if let Some(preflight) = &self.preflight {
            preflight.validate()?;
        }
        Ok(scope)
    }
}

impl ForgeWorkRef {
    fn validate(&self, who: &WorkWho) -> Result<()> {
        validate_repository(&self.repository)?;
        if self.issue.is_empty()
            || self.issue.starts_with('0')
            || !self.issue.bytes().all(|byte| byte.is_ascii_digit())
        {
            return Err(invalid("issue must be a canonical positive decimal id"));
        }
        label("issue", &self.issue)?;
        if self.provider != who.owner.identity.provider
            || self.host != who.owner.identity.host
            || self.repository != who.repository
        {
            return Err(invalid("forge source is outside the token repository"));
        }
        Ok(())
    }
}

impl WorkCheckpoint {
    /// The pause that ends a session whose Work reported no outcome. It keeps the next action and
    /// relations. Its operation id follows the Work's generation and sequence, so a repeat of the
    /// same end is a replay, and any change to the Work fences it.
    pub(super) fn auto_pause(item: &WorkItem, via: &str) -> Self {
        Self {
            work_id: item.id.clone(),
            operation_id: format!("{via}-done-{}-{}", item.generation, item.updated_seq),
            generation: item.generation,
            expected_updated_seq: Some(item.updated_seq),
            session: item.lead_session.clone(),
            kind: CheckpointKind::Pause,
            summary: AUTO_PAUSE_SUMMARY.into(),
            next_action: item
                .next_action
                .clone()
                .or_else(|| Some("Resume or deliberately hand over this unfinished work.".into())),
            scope: None,
            branch: None,
            worktree: None,
            resolve_blocker: false,
            source_refs: Vec::new(),
            depends_on: item.depends_on.clone(),
            related_work: item.related_work.clone(),
        }
    }

    pub(super) fn validate(&self) -> Result<()> {
        validate_id("work", &self.work_id, "wrk")?;
        label("operation_id", &self.operation_id)?;
        if self.generation < 1 {
            return Err(invalid("generation must be positive"));
        }
        if self.expected_updated_seq.is_some_and(|seq| seq < 1) {
            return Err(invalid("expected_updated_seq must be positive"));
        }
        label("session", &self.session)?;
        text("summary", &self.summary, MAX_TEXT_BYTES)?;
        option_text("next_action", &self.next_action, MAX_TEXT_BYTES)?;
        if let Some(scope) = &self.scope {
            paths(scope)?;
        }
        if let Some(branch) = &self.branch {
            label("branch", branch)?;
            if !valid_ref_name(branch) {
                return Err(invalid("branch is not a valid git ref name"));
            }
        }
        option_label("worktree", &self.worktree)?;
        if self.source_refs.len() > MAX_REFS {
            return Err(invalid("source_refs has more than 100 entries"));
        }
        for source in &self.source_refs {
            source.validate()?;
        }
        relations(&self.depends_on, &self.related_work)
    }

    pub(super) fn state(&self) -> WorkState {
        match self.kind {
            CheckpointKind::Progress | CheckpointKind::Decision => WorkState::Active,
            CheckpointKind::Blocker => WorkState::Blocked,
            CheckpointKind::Pause => WorkState::Paused,
            CheckpointKind::Finish => WorkState::Finished,
            CheckpointKind::Handover => WorkState::Handover,
        }
    }

    pub(super) fn normalized_scope(&self) -> Result<Option<Vec<String>>> {
        self.scope.as_deref().map(paths).transpose()
    }
}

impl WorkRepositoryScope {
    pub(super) fn validate(&self) -> Result<()> {
        label("provider", &self.provider)?;
        label("host", &self.host)?;
        validate_repository(&self.repository)?;
        if self.repository_id.is_some_and(|id| id < 1) {
            return Err(invalid("repository_id must be positive"));
        }
        Ok(())
    }
}

impl SourceRef {
    fn validate(&self) -> Result<()> {
        match self {
            Self::RepositoryPath { path, line } => {
                spec_key(path)?;
                if line.is_some_and(|line| line == 0) {
                    return Err(invalid("source line must be positive"));
                }
            }
            Self::Commit { oid, path } => {
                if !matches!(oid.len(), 40 | 64)
                    || !oid.bytes().all(|byte| byte.is_ascii_hexdigit())
                {
                    return Err(invalid(
                        "commit oid must be 40 or 64 hexadecimal characters",
                    ));
                }
                if let Some(path) = path {
                    spec_key(path)?;
                }
            }
            Self::Work { work_id } => validate_id("work", work_id, "wrk")?,
            Self::CapturedEvent {
                session_id,
                event_id,
            } => {
                captured_identifier("captured event session_id", session_id)?;
                captured_identifier("captured event event_id", event_id)?;
            }
            Self::PriorWorkVersion { work_id, event_seq } => {
                validate_id("prior work version", work_id, "wrk")?;
                if *event_seq < 1 {
                    return Err(invalid("prior work version event_seq must be positive"));
                }
            }
        }
        Ok(())
    }
}

impl BriefingQuery {
    pub(super) fn validate(&self) -> Result<()> {
        if !(1..=MAX_WORK_PAGE).contains(&self.limit) {
            return Err(invalid("briefing limit must be 1 to 100"));
        }
        if self.after.is_some_and(|after| after < 0)
            || self.through.is_some_and(|through| through < 0)
        {
            return Err(invalid("briefing cursors must be non-negative"));
        }
        if let Some(id) = &self.delivery {
            validate_id("delivery", id, "dlv")?;
        }
        if let Some(subscriber) = &self.subscriber {
            label("briefing subscriber", subscriber)?;
        }
        if let Some(id) = &self.work_after {
            validate_id("work_after", id, "wrk")?;
        }
        Ok(())
    }
}

impl WorkOverviewQuery {
    pub(super) fn validate(&self, now: i64) -> Result<i64> {
        if !(1..=MAX_WORK_PAGE).contains(&self.limit) {
            return Err(invalid("overview limit must be 1 to 100"));
        }
        let since = self
            .recent_since
            .unwrap_or_else(|| now.saturating_sub(DEFAULT_RECENT_WORK_SECONDS));
        if since < 0 || since > now {
            return Err(invalid(
                "overview recent_since must be between zero and now",
            ));
        }
        Ok(since)
    }
}

pub(super) fn scope_overlap_reasons(
    your_scope: &[String],
    their_scope: &[String],
) -> Vec<WorkPathOverlap> {
    let mut reasons = Vec::new();
    for yours in your_scope {
        for theirs in their_scope {
            if overlaps(yours, theirs) {
                reasons.push(WorkPathOverlap {
                    your_path: display(yours),
                    their_path: display(theirs),
                });
                // Four displayed witnesses plus one overflow witness let
                // callers report incomplete reasons without materializing the
                // Cartesian product of two 256-path scopes.
                if reasons.len() == 5 {
                    return reasons;
                }
            }
        }
    }
    reasons
}

/// The first `max` overlapping pairs in item order; callers pass one past their page limit.
pub(super) fn work_overlap_pairs(items: &[WorkItem], max: usize) -> Vec<WorkOverlapPair> {
    let mut pairs = Vec::new();
    for (index, left) in items.iter().enumerate() {
        for right in &items[index + 1..] {
            if pairs.len() == max {
                return pairs;
            }
            let all_reasons = scope_overlap_reasons(&left.scope, &right.scope);
            let reasons_complete = all_reasons.len() <= 4;
            let reasons = all_reasons
                .into_iter()
                .take(4)
                .map(|reason| WorkPathPair {
                    left_path: reason.your_path,
                    right_path: reason.their_path,
                })
                .collect::<Vec<_>>();
            if !reasons.is_empty() {
                pairs.push(WorkOverlapPair {
                    left: left.clone(),
                    right: right.clone(),
                    reasons,
                    reasons_complete,
                });
            }
        }
    }
    pairs
}

/// Drops trailing items until the compact JSON array fits `budget`; true when any were dropped.
/// Each item is serialized once instead of re-serializing the whole array per drop.
pub(super) fn fit_json_budget<T: Serialize>(items: &mut Vec<T>, budget: usize) -> Result<bool> {
    let mut lengths = items
        .iter()
        .map(|item| serde_json::to_string(item).map(|json| json.len()))
        .collect::<std::result::Result<Vec<_>, _>>()
        .map_err(|error| HubError::Unavailable(error.to_string()))?;
    // `[`, the items joined by `,`, then `]`: exactly what serde_json writes for the array.
    let mut total = 2 + lengths.iter().sum::<usize>() + lengths.len().saturating_sub(1);
    let mut dropped = false;
    while total > budget
        && let Some(length) = lengths.pop()
    {
        items.pop();
        total -= length + usize::from(!lengths.is_empty());
        dropped = true;
    }
    Ok(dropped)
}

impl WorkItem {
    /// The lead will not answer a request soon: it paused, or its claim is no longer active.
    pub(super) fn lead_idle(&self) -> bool {
        self.state == WorkState::Paused || self.claim_status != ClaimStatus::Active
    }
}

/// A lead's claim is active while its token is valid and it checked in recently.
pub(super) fn claim_status(last_checkin_at: i64, lead_valid: bool, now: i64) -> ClaimStatus {
    if now < last_checkin_at {
        ClaimStatus::Unknown
    } else if lead_valid && last_checkin_at >= now - CLAIM_STALE_SECONDS {
        ClaimStatus::Active
    } else {
        ClaimStatus::Stale
    }
}

impl BriefingAck {
    pub(super) fn validate(&self) -> Result<()> {
        validate_id("delivery", &self.delivery, "dlv")
    }
}

pub(super) fn relations(depends_on: &[String], related: &[String]) -> Result<()> {
    if depends_on.len() + related.len() > MAX_RELATIONS {
        return Err(invalid("work has more than 100 relations"));
    }
    let mut seen = BTreeSet::new();
    for id in depends_on.iter().chain(related) {
        validate_id("related work", id, "wrk")?;
        if !seen.insert(id) {
            return Err(invalid("work relations must be unique"));
        }
    }
    Ok(())
}

fn paths(input: &[String]) -> Result<Vec<String>> {
    if input.len() > MAX_SCOPE {
        return Err(invalid("scope has more than 256 paths"));
    }
    let mut paths = input
        .iter()
        .map(|path| spec_key(path))
        .collect::<Result<Vec<_>>>()?;
    paths.sort();
    paths.dedup();
    Ok(paths)
}

pub(super) fn normalized_scope(input: &[String]) -> Result<Vec<String>> {
    paths(input)
}

pub(super) fn uncovered_scope(declared: &[String], requested: &[String]) -> Vec<String> {
    requested
        .iter()
        .filter(|requested| {
            !declared.iter().any(|declared| {
                declared == *requested
                    || (declared.ends_with('/') && requested.starts_with(declared))
            })
        })
        .cloned()
        .collect()
}

fn label(name: &str, value: &str) -> Result<()> {
    text(name, value, MAX_LABEL_BYTES)
}

fn option_label(name: &str, value: &Option<String>) -> Result<()> {
    value
        .as_deref()
        .map(|value| label(name, value))
        .transpose()
        .map(|_| ())
}

fn option_text(name: &str, value: &Option<String>, max: usize) -> Result<()> {
    value
        .as_deref()
        .map(|value| text(name, value, max))
        .transpose()
        .map(|_| ())
}

fn captured_identifier(name: &str, value: &str) -> Result<()> {
    let valid = !value.is_empty()
        && value.len() <= MAX_LABEL_BYTES
        && value.bytes().all(|byte| {
            byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-' | b':' | b'@')
        });
    if valid {
        Ok(())
    } else {
        Err(invalid(&format!(
            "{name} must be 1..{MAX_LABEL_BYTES} bytes of identifier characters"
        )))
    }
}

pub(super) fn validate_id(name: &str, value: &str, prefix: &str) -> Result<()> {
    let valid = value
        .strip_prefix(&format!("{prefix}_"))
        .is_some_and(|hex| {
            hex.len() == 32
                && hex
                    .bytes()
                    .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
        });
    if valid {
        Ok(())
    } else {
        Err(HubError::Invalid(format!(
            "{name} must be {prefix}_ and 32 lowercase hex"
        )))
    }
}

pub(super) fn invalid(message: &str) -> HubError {
    HubError::Invalid(message.to_owned())
}

/// The caller attaches its repository; see `WorkWho::capacity`.
pub(super) fn check_work_payload_capacity(used: i64, delta: i64, limit: i64) -> Result<()> {
    if used.checked_add(delta).is_none_or(|total| total > limit) {
        return Err(HubError::capacity(
            super::capacity::CapacityLimit::WorkStorage,
            None,
        ));
    }
    Ok(())
}

pub(super) fn work_payload_row_bytes(payload_len: usize) -> Result<i64> {
    i64::try_from(payload_len)
        .ok()
        .and_then(|len| len.checked_add(WORK_PAYLOAD_ROW_BYTES))
        .ok_or_else(|| invalid("work payload size is too large"))
}

pub(super) fn check_work_delivery_page(page: &str) -> Result<()> {
    if work_payload_row_bytes(page.len())? > WORK_DELIVERY_RESERVATION_BYTES {
        return Err(invalid(
            "serialized briefing exceeds the 4194304-byte delivery reservation",
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{
        WORK_DELIVERY_RESERVATION_BYTES, WORK_PAYLOAD_ROW_BYTES, check_work_delivery_page,
        check_work_payload_capacity, delivery_cuts,
    };

    #[test]
    fn compaction_ends_only_deliveries_below_a_raised_floor() {
        // Nothing raised: no delivery ends.
        assert_eq!(delivery_cuts([5, 9, 2], [5, 9, 2], 4, 4), (0, 0));
        // Any raised event floor cuts at the highest new floor; merges are separate.
        assert_eq!(delivery_cuts([5, 9, 2], [12, 9, 2], 4, 4), (12, 0));
        assert_eq!(delivery_cuts([5, 9, 2], [5, 9, 2], 4, 7), (0, 7));
        // A raised floor below another, unraised one still cuts: events 600-950 were removed.
        assert_eq!(
            delivery_cuts([500, 900, 1000], [950, 900, 1000], 0, 0),
            (950, 0)
        );
    }

    #[test]
    fn overlap_witnesses_bound_dense_scope_expansions() {
        let requested = (0..256).map(|i| format!("src/{i}/")).collect::<Vec<_>>();
        let assigned = (0..256)
            .map(|i| format!("src/0/{i}.rs"))
            .collect::<Vec<_>>();
        let reasons = super::scope_overlap_reasons(&requested, &assigned);
        assert_eq!(reasons.len(), 5, "four witnesses and one overflow marker");
        assert!(reasons.iter().all(|r| r.your_path == "src/0/**"));
        assert!(super::scope_overlap_reasons(&["docs/".into()], &assigned).is_empty());
    }

    #[test]
    fn work_payload_capacity_is_atomic_math_and_allows_non_growing_effects() {
        assert!(check_work_payload_capacity(90, 10, 100).is_ok());
        assert!(check_work_payload_capacity(100, 0, 100).is_ok());
        assert!(check_work_payload_capacity(100, -10, 100).is_ok());
        assert!(check_work_payload_capacity(91, 10, 100).is_err());
        assert!(check_work_payload_capacity(i64::MAX, 1, i64::MAX).is_err());
    }

    #[test]
    fn json_budget_matches_whole_array_serialization() {
        let items = ["a", "bb", "", "dddd"].map(String::from).to_vec();
        for budget in 0..30 {
            let mut fitted = items.clone();
            let dropped = super::fit_json_budget(&mut fitted, budget).unwrap();
            let mut expected = items.clone();
            while !expected.is_empty() && serde_json::to_string(&expected).unwrap().len() > budget {
                expected.pop();
            }
            assert_eq!(
                (fitted, dropped),
                (expected.clone(), expected.len() < items.len())
            );
        }
    }

    #[test]
    fn delivery_page_must_fit_its_reserved_bytes() {
        let payload = (WORK_DELIVERY_RESERVATION_BYTES - WORK_PAYLOAD_ROW_BYTES) as usize;
        assert!(check_work_delivery_page(&"x".repeat(payload)).is_ok());
        assert!(check_work_delivery_page(&"x".repeat(payload + 1)).is_err());
    }

    #[test]
    fn a_snapshot_stored_before_descriptions_still_decodes() {
        let mut snapshot = serde_json::json!({
            "id": "wrk_aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa", "repository": "acme/widgets",
            "owner": "usr_alice", "owner_login": "alice",
            "source": {"kind": "native", "request_key": "synthetic"},
            "title": "Synthetic", "state": "active", "claim_status": "active",
            "last_checkin_at": 1, "generation": 1, "lead_token": "tok_alice",
            "lead_agent": "agent", "lead_session": "session",
            "participants": [{"owner": "usr_alice", "login": "alice", "token": "tok_alice",
                "agent": "agent", "session": "session", "role": "lead"}],
            "scope": [], "branch": null, "worktree": null, "next_action": null,
            "current": null, "blocker": null, "depends_on": [], "related_work": [],
            "created_at": 1, "updated_at": 1, "created_seq": 1, "updated_seq": 1
        });
        let old: super::WorkItem = serde_json::from_value(snapshot.clone()).unwrap();
        assert_eq!(old.description, None);
        // Work without a description keeps the snapshot shape older readers decode.
        assert!(
            serde_json::to_value(&old)
                .unwrap()
                .get("description")
                .is_none()
        );
        snapshot["description"] = "Says what the Work changes and why.".into();
        let described: super::WorkItem = serde_json::from_value(snapshot).unwrap();
        assert_eq!(
            described.description.as_deref(),
            Some("Says what the Work changes and why.")
        );
    }
}
