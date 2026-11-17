//! Durable requests used to resolve overlapping shared work.

use serde::{Deserialize, Serialize};

use super::error_envelope::{FieldProblem, InputProblem};
use super::handoff::Via;
use super::intent::text;
use super::work::{ForgeWorkRef, WorkItem, WorkPathOverlap, WorkState, validate_id};
use super::{HubError, Result};

pub const MAX_COORDINATION_PAGE: usize = 100;
pub const MAX_ACTIVE_COORDINATION_REQUESTS: i64 = 100;
pub const MAX_ACTIVE_COORDINATION_REQUESTS_PER_OWNER: i64 = 25;
pub const MAX_ACTIVE_COORDINATION_REQUESTS_PER_TOKEN: i64 = 8;
pub const MAX_COORDINATION_UPDATES: i64 = 500;
pub const MAX_PROJECTED_COORDINATION_UPDATES: i64 = 128;
pub const MAX_COORDINATION_REASONS_PER_TARGET: usize = 4;
/// A request that a live lead has not acknowledged this long after it was made is raised to
/// that Work's owner. Shorter than a claim going stale, so the lead is still live when it is.
pub const ACK_BOUND_SECS: i64 = 10 * 60;
const MAX_NOTE_BYTES: usize = 500;

pub(super) fn is_automatic_request_key(value: &str) -> bool {
    value.strip_prefix("req_").is_some_and(|suffix| {
        suffix.len() == 32
            && suffix
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    })
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct CoordinationAgent {
    pub token: String,
    pub agent: String,
    pub session: String,
    pub owner: String,
    pub login: String,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct CoordinationProposal {
    pub work_id: Option<String>,
    pub source: Option<ForgeWorkRef>,
    pub request_key: Option<String>,
    pub title: String,
    pub scope: Vec<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct CoordinationTarget {
    pub work_id: String,
    pub lead: CoordinationAgent,
    pub generation: i64,
    pub reasons: Vec<WorkPathOverlap>,
    pub reasons_complete: bool,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct CoordinationAcknowledgement {
    /// None identifies the requester; Some identifies the current lead of that work.
    pub target_work_id: Option<String>,
    pub agent: CoordinationAgent,
    pub generation: i64,
    pub through_seq: i64,
    pub acknowledged_at: i64,
    pub seq: i64,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CoordinationResponseKind {
    ProposeSplit,
    Join,
    Handover,
    Decline,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct CoordinationResponse {
    pub id: String,
    pub operation_id: String,
    pub target_work_id: String,
    pub responder: CoordinationAgent,
    pub target_generation: i64,
    pub kind: CoordinationResponseKind,
    pub note: Option<String>,
    /// `hub` when the person who owns the responding agent answered for it from the Hub's web
    /// pages, `slack` when they answered through a connected app with that `via`; absent when
    /// the agent answered itself. `responder` is still the agent, so a reader
    /// that ignores this field sees the same shape as before.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub via: Option<String>,
    /// The requested paths this reply released from the target Work's reservation; absent when
    /// it released none.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub released_paths: Vec<String>,
    pub created_at: i64,
    pub seq: i64,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CoordinationStatus {
    Pending,
    Responded,
    Resolved,
    Withdrawn,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct CoordinationRequest {
    /// Read-time explanatory context; never persisted as coordination authority.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub holder_intents: Option<super::work_intent::HolderIntents>,
    pub id: String,
    pub requester: CoordinationAgent,
    pub proposal: CoordinationProposal,
    pub targets: Vec<CoordinationTarget>,
    pub status: CoordinationStatus,
    pub acknowledgements: Vec<CoordinationAcknowledgement>,
    pub acknowledgements_complete: bool,
    pub responses: Vec<CoordinationResponse>,
    pub responses_complete: bool,
    pub created_at: i64,
    pub updated_at: i64,
    pub created_seq: i64,
    pub updated_seq: i64,
    /// The requester's token can no longer act (revoked or expired, its owner disabled, or its
    /// repository renamed since it was minted), so nobody can resolve or withdraw the request
    /// for it. Derived when the request is read, not stored: a current affected lead, or the
    /// person who owns it, may then withdraw the request. Left out of the JSON when false.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub requester_unavailable: bool,
    /// Whether the paths an open request asks for are still held by another Work or live
    /// intent. Derived when the requester reads its own inbox, not stored; absent for a
    /// settled request and for any other reader.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub requested_paths: Option<RequestedPathsHold>,
    /// While `requested_paths` is `held`, the requested paths no longer held, sorted (empty
    /// when the request names more than `MAX_RELEASED_PATHS`). Derived beside it, never stored.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub released_paths: Vec<String>,
    /// False when `released_paths` may be missing paths: the request names more than
    /// `MAX_RELEASED_PATHS`, or more than `MAX_RELEASE_EVENTS` releases were recorded. Left
    /// out of the JSON when true.
    #[serde(default = "complete_default", skip_serializing_if = "is_true")]
    pub released_paths_complete: bool,
}

const fn complete_default() -> bool {
    true
}

const fn is_true(value: &bool) -> bool {
    *value
}

/// What a request's requester has done since the holder replied, as the holder reads it.
#[derive(Clone, Copy, Debug, Serialize, PartialEq, Eq)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum RequesterState {
    NotSeen,
    Seen {
        at: i64,
    },
    /// The acknowledgements are a partial page and none of them is the requester's.
    Unknown,
}

impl CoordinationRequest {
    /// The requester's latest state: it acknowledged the newest reply (when
    /// it did) or has not. Derived from the request's acknowledgements, never stored.
    pub fn requester_state(&self) -> RequesterState {
        let replied = self
            .responses
            .iter()
            .map(|reply| reply.seq)
            .max()
            .unwrap_or(0);
        match self
            .acknowledgements
            .iter()
            .filter(|ack| {
                ack.target_work_id.is_none()
                    && ack.agent.token == self.requester.token
                    && ack.through_seq >= replied
            })
            .map(|ack| ack.acknowledged_at)
            .max()
        {
            Some(at) => RequesterState::Seen { at },
            None if self.acknowledgements_complete => RequesterState::NotSeen,
            None => RequesterState::Unknown,
        }
    }
}

/// The most release events read for one request; past it no path is reported released.
pub const MAX_RELEASE_EVENTS: i64 = 64;

/// The most requested paths probed one by one for a partial release.
pub const MAX_RELEASED_PATHS: usize = 32;

/// The requested paths a target Work actually held, read from its `coordination_target_released`
/// events (`data` is each event's JSON): the requested path itself when an event's `paths`
/// cover it or name none (a full release or end), or the released path when it lies under a
/// requested directory. The target's scope is not recorded on the request, so a path the
/// target never held is never a candidate. Sorted and without repeats; the caller bounds it.
pub(super) fn held_candidates(scope: &[String], events: &[String]) -> Vec<String> {
    let mut out = std::collections::BTreeSet::new();
    for data in events {
        let Ok(data) = serde_json::from_str::<serde_json::Value>(data) else {
            continue;
        };
        let paths = data["paths"]
            .as_array()
            .map(|paths| paths.iter().filter_map(|p| p.as_str()).collect::<Vec<_>>())
            .unwrap_or_default();
        for spec in scope {
            let key = super::intent::spec_key(spec).unwrap_or_else(|_| spec.clone());
            if paths.is_empty() {
                out.insert(spec.clone());
            }
            for path in &paths {
                let released = super::intent::key_of(path);
                if !super::intent::overlaps(&key, released) {
                    continue;
                }
                // Under a requested directory only the released path was held, not the directory.
                let under = key.ends_with('/') && released != key && released.starts_with(&key);
                out.insert(if under {
                    (*path).to_owned()
                } else {
                    spec.clone()
                });
            }
        }
    }
    out.into_iter().collect()
}

/// A requested path in the display form every released-paths list uses.
pub(super) fn display_spec(spec: &str) -> String {
    super::intent::display(&super::intent::spec_key(spec).unwrap_or_else(|_| spec.to_owned()))
}

/// Whether the paths a request asks for are still held.
#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RequestedPathsHold {
    Held,
    Released,
    Unknown,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct CoordinationInboxQuery {
    pub session: Option<String>,
    pub after: Option<i64>,
    pub through: Option<i64>,
    #[serde(default = "default_limit")]
    pub limit: usize,
}

impl Default for CoordinationInboxQuery {
    fn default() -> Self {
        Self {
            session: None,
            after: None,
            through: None,
            limit: default_limit(),
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct CoordinationListQuery {
    pub after: Option<i64>,
    pub through: Option<i64>,
    #[serde(default)]
    pub active_only: bool,
    #[serde(default = "default_limit")]
    pub limit: usize,
}

impl Default for CoordinationListQuery {
    fn default() -> Self {
        Self {
            after: None,
            through: None,
            active_only: false,
            limit: default_limit(),
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct CoordinationInbox {
    /// Read-time explanatory context; absent on legacy responses and durable records.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub holder_intents: Option<super::work_intent::HolderIntents>,
    pub from: i64,
    pub through: i64,
    pub next_after: i64,
    pub requests: Vec<CoordinationRequest>,
    pub complete: bool,
    pub retention_gap: bool,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct CoordinationAck {
    pub request_id: String,
    pub target_work_id: Option<String>,
    pub session: String,
    pub expected_updated_seq: i64,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct CoordinationReply {
    pub request_id: String,
    pub operation_id: String,
    pub target_work_id: String,
    pub session: String,
    pub expected_updated_seq: i64,
    pub kind: CoordinationResponseKind,
    pub note: Option<String>,
    /// Requested paths to release from the target Work in the same step, keeping the rest of
    /// its scope: each must be a path the request asked for and an exact scope entry of the
    /// target. Not valid with `join`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub release_paths: Vec<String>,
    /// Set only by the person routes, never read from a request body: the person who owns the
    /// lead agent answered for it from the Hub's web pages (`via: "hub"` on the response) or
    /// through a connected app (`via: "slack"`). `Agent` when the agent answered itself.
    #[serde(skip)]
    pub via: Via,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct CoordinationResolve {
    pub request_id: String,
    pub operation_id: String,
    pub session: String,
    pub expected_updated_seq: i64,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct CoordinationWithdraw {
    pub request_id: String,
    pub operation_id: String,
    pub session: String,
    pub expected_updated_seq: i64,
    /// The target Work and generation a lead saw when it closed a request whose requester is
    /// gone. Refused unless the caller leads that Work at that generation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target: Option<CoordinationWithdrawTarget>,
    /// Set only by the Hub's browser-session route, never read from a request body: the person
    /// who owns the lead agent closed the request for it. Recorded as `via: "hub"` on the event.
    #[serde(skip)]
    pub via_hub: bool,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct CoordinationWithdrawTarget {
    pub work_id: String,
    pub generation: i64,
}

const fn default_limit() -> usize {
    MAX_COORDINATION_PAGE
}

fn validate_cursor(after: Option<i64>, through: Option<i64>, limit: usize) -> Result<()> {
    if !(1..=MAX_COORDINATION_PAGE).contains(&limit) {
        return Err(invalid("coordination limit must be 1 to 100"));
    }
    if after.is_some_and(|cursor| cursor < 0) || through.is_some_and(|cursor| cursor < 0) {
        return Err(invalid("coordination cursors must be non-negative"));
    }
    if let (Some(after), Some(through)) = (after, through)
        && after > through
    {
        return Err(invalid("coordination after must not exceed through"));
    }
    Ok(())
}

impl CoordinationInboxQuery {
    pub(super) fn validate(&self) -> Result<()> {
        validate_cursor(self.after, self.through, self.limit)?;
        if let Some(session) = &self.session {
            text("session", session, 255)?;
        }
        Ok(())
    }
}

impl CoordinationListQuery {
    pub(super) fn validate(&self) -> Result<()> {
        validate_cursor(self.after, self.through, self.limit)
    }
}

fn validate_mutation(request_id: &str, session: &str, expected_updated_seq: i64) -> Result<()> {
    validate_id("coordination request", request_id, "crq")?;
    text("session", session, 255)?;
    if expected_updated_seq < 1 {
        return Err(invalid("expected_updated_seq must be positive"));
    }
    Ok(())
}

impl CoordinationAck {
    pub(super) fn validate(&self) -> Result<()> {
        validate_mutation(&self.request_id, &self.session, self.expected_updated_seq)?;
        if let Some(work) = &self.target_work_id {
            validate_id("target work", work, "wrk")?;
        }
        Ok(())
    }
}

/// Like `text`, but names the field in the typed error so the caller can correct it.
fn validate_note(note: &str) -> Result<()> {
    let (problem, hint) = if note.len() > MAX_NOTE_BYTES {
        (FieldProblem::TooLong, "Shorten note to at most 500 bytes.")
    } else if text("note", note, MAX_NOTE_BYTES).is_err() {
        (
            FieldProblem::Invalid,
            "Send a non-empty note of 1-500 bytes without control or invisible characters.",
        )
    } else {
        return Ok(());
    };
    Err(HubError::Input(
        InputProblem::new("note", problem, hint)
            .into_iter()
            .collect(),
    ))
}

impl CoordinationReply {
    pub(super) fn validate(&self) -> Result<()> {
        validate_mutation(&self.request_id, &self.session, self.expected_updated_seq)?;
        validate_id("target work", &self.target_work_id, "wrk")?;
        text("operation_id", &self.operation_id, 255)?;
        if let Some(note) = &self.note {
            validate_note(note)?;
        }
        if !self.release_paths.is_empty() {
            if self.kind == CoordinationResponseKind::Join {
                return Err(invalid(
                    "release_paths is not valid with join: a join keeps the whole scope. Reply propose_split, handover or decline to release some requested files",
                ));
            }
            super::work::normalized_scope(&self.release_paths)?;
        }
        Ok(())
    }

    /// The release of the named paths from the target Work, run in the reply's transaction.
    /// Refuses a path the request did not ask for; the transition refuses the rest.
    pub(super) fn release(
        &self,
        requested: &[String],
        work_id: &str,
        generation: i64,
        updated_seq: i64,
        release_id: &str,
        session: &str,
    ) -> Result<super::work_transition::WorkTransitionRequest> {
        // A proposal's scope holds index keys (`dir/`), which are not display specs.
        let requested: Vec<String> = requested
            .iter()
            .map(|spec| super::intent::spec_key(spec).unwrap_or_else(|_| spec.clone()))
            .collect();
        let unrequested: Vec<String> = super::work::normalized_scope(&self.release_paths)?
            .into_iter()
            // A requested directory covers the files under it.
            .filter(|key| {
                !requested
                    .iter()
                    .any(|ask| key == ask || (ask.ends_with('/') && key.starts_with(ask.as_str())))
            })
            .map(|key| super::intent::display(&key))
            .collect();
        if !unrequested.is_empty() {
            return Err(HubError::Invalid(format!(
                "release_paths names paths this request did not ask for: {}",
                unrequested.join(", ")
            )));
        }
        Ok(super::work_transition::WorkTransitionRequest {
            work_id: work_id.to_owned(),
            operation_id: release_id.to_owned(),
            generation,
            expected_updated_seq: updated_seq,
            // A person acts for the lead without a session; the agent acts in its own.
            session: (self.via == Via::Agent).then(|| session.to_owned()),
            transition: super::work_transition::WorkTransitionKind::ReleasePaths,
            reason: format!(
                "released by a reply to coordination request {}",
                self.request_id
            ),
            superseded_by: None,
            participants: Vec::new(),
            paths: self.release_paths.clone(),
        })
    }
}

/// Why a session that neither made a request nor leads one of its targets cannot read its overlap consents.
pub const CONSENTS_NOT_PARTY: &str = "only the session that made this coordination request, or the session that leads one of its target Work items, can read its overlap consents, and the session in this call is neither. Pass the requester's session, or the session that leads a target Work item";
/// Why a session that neither made a request nor leads one of its targets cannot resolve it.
pub const RESOLVE_NOT_PARTY: &str = "only the session that made this coordination request, or the session that leads one of its target Work items, can resolve it, and the session in this call is neither. If you lead the target Work, pass the session that leads it. Otherwise answer with agentscm_coordination_reply (`agentscm work coordination-reply`) and leave resolving to the requester";

impl CoordinationResolve {
    pub(super) fn validate(&self) -> Result<()> {
        validate_mutation(&self.request_id, &self.session, self.expected_updated_seq)?;
        text("operation_id", &self.operation_id, 255)
    }
}

impl CoordinationWithdraw {
    pub(super) fn validate(&self) -> Result<()> {
        validate_mutation(&self.request_id, &self.session, self.expected_updated_seq)?;
        text("operation_id", &self.operation_id, 255)?;
        if let Some(target) = &self.target {
            validate_id("target work", &target.work_id, "wrk")?;
            if target.generation < 1 {
                return Err(invalid("target generation must be positive"));
            }
        }
        Ok(())
    }
}

pub(super) fn status_name(status: CoordinationStatus) -> &'static str {
    match status {
        CoordinationStatus::Pending => "pending",
        CoordinationStatus::Responded => "responded",
        CoordinationStatus::Resolved => "resolved",
        CoordinationStatus::Withdrawn => "withdrawn",
    }
}

pub(super) fn parse_status(value: &str) -> Result<CoordinationStatus> {
    match value {
        "pending" => Ok(CoordinationStatus::Pending),
        "responded" => Ok(CoordinationStatus::Responded),
        "resolved" => Ok(CoordinationStatus::Resolved),
        "withdrawn" => Ok(CoordinationStatus::Withdrawn),
        _ => Err(HubError::Unavailable("invalid coordination status".into())),
    }
}

pub(super) fn response_kind_name(kind: CoordinationResponseKind) -> &'static str {
    match kind {
        CoordinationResponseKind::ProposeSplit => "propose_split",
        CoordinationResponseKind::Join => "join",
        CoordinationResponseKind::Handover => "handover",
        CoordinationResponseKind::Decline => "decline",
    }
}

pub(super) fn parse_response_kind(value: &str) -> Result<CoordinationResponseKind> {
    match value {
        "propose_split" => Ok(CoordinationResponseKind::ProposeSplit),
        "join" => Ok(CoordinationResponseKind::Join),
        "handover" => Ok(CoordinationResponseKind::Handover),
        "decline" => Ok(CoordinationResponseKind::Decline),
        _ => Err(HubError::Unavailable(
            "invalid coordination response kind".into(),
        )),
    }
}

pub(super) fn invalid(message: &str) -> HubError {
    HubError::Invalid(message.to_owned())
}

/// The event data of a response, marked `via` (`hub` or `slack`) when a person answered for the
/// agent.
pub(super) fn with_via(mut data: serde_json::Value, via: Option<&str>) -> serde_json::Value {
    if let Some(via) = via {
        data["via"] = serde_json::json!(via);
    }
    data
}

/// Adds the display paths a reply released to its `coordination_responded` event data; skipped
/// when it released none, so earlier events keep their bytes.
pub(super) fn with_released(mut data: serde_json::Value, released: &[String]) -> serde_json::Value {
    if !released.is_empty() {
        data["released_paths"] = serde_json::json!(released);
    }
    data
}

const JOIN_UNCOVERED_SHOWN: usize = 3;

/// Refuses a `join` reply when a requested path that overlaps the target Work is wider than that
/// Work's declared scope. A helper edits only inside the declared scope, so such a join could not
/// be used for those paths. The overlap is recomputed from the whole proposal, not the request's
/// capped reasons.
pub(super) fn join_uncovered(
    kind: CoordinationResponseKind,
    target_scope: &[String],
    proposal_scope: &[String],
) -> Result<()> {
    if kind != CoordinationResponseKind::Join {
        return Ok(());
    }
    let overlapping = proposal_scope
        .iter()
        .filter(|yours| {
            target_scope
                .iter()
                .any(|theirs| super::intent::overlaps(yours, theirs))
        })
        .cloned()
        .collect::<Vec<_>>();
    let uncovered = super::work::uncovered_scope(target_scope, &overlapping);
    if uncovered.is_empty() {
        return Ok(());
    }
    let shown = super::intent::display_all(uncovered.iter().take(JOIN_UNCOVERED_SHOWN)).join(", ");
    let more = uncovered.len().saturating_sub(JOIN_UNCOVERED_SHOWN);
    let more = if more > 0 {
        format!(" and {more} more")
    } else {
        String::new()
    };
    Err(HubError::invalid_or_changed(format!(
        "The requested paths {shown}{more} are wider than your Work's declared scope, so a join would let the requester edit only the files your Work declares. To agree to more, checkpoint your scope to include them and reply join, or grant agentscm_work_overlap_consent for exact named files."
    )))
}

/// Refuses a helper or reviewer join that the session could not edit under, before anything is
/// written: the later touch refuses every edit unless this is the session's only eligible Work,
/// the Work holds a reservation and the paths lie in its scope. `holds_reservation` and
/// `elsewhere` (the id of another Work the same session is already eligible on) come from the
/// store. An empty `requested` scope joins without edits and is never refused.
pub(super) fn helper_join_editable(
    item: &WorkItem,
    holds_reservation: bool,
    elsewhere: Option<&str>,
    requested: &[String],
) -> Result<()> {
    if requested.is_empty() {
        return Ok(());
    }
    // Finished, paused or unreserved Work joins and acquires nothing, so no edit follows.
    if !matches!(item.state, WorkState::Active | WorkState::Blocked) || !holds_reservation {
        return Ok(());
    }
    let uncovered = super::work::uncovered_scope(&item.scope, requested);
    if !uncovered.is_empty() {
        let shown =
            super::intent::display_all(uncovered.iter().take(JOIN_UNCOVERED_SHOWN)).join(", ");
        let more = uncovered.len().saturating_sub(JOIN_UNCOVERED_SHOWN);
        let more = if more > 0 {
            format!(" and {more} more")
        } else {
            String::new()
        };
        return Err(HubError::Invalid(format!(
            "Nothing was joined: {shown}{more} lie outside Work {}'s declared scope, so edits to them would be refused. Ask the lead to checkpoint its scope to include them, use agentscm_work_overlap_consent for exact named files, or start your own Work over them.",
            item.id
        )));
    }
    if let Some(other) = elsewhere {
        return Err(HubError::Invalid(format!(
            "Nothing was joined: this session already edits under Work {other}, and a session edits under one Work at a time. Pause or finish that Work with agentscm_done, or join from another session."
        )));
    }
    Ok(())
}

#[cfg(test)]
mod join_uncovered_tests {
    use super::*;

    fn s(items: &[&str]) -> Vec<String> {
        items.iter().map(|item| (*item).to_owned()).collect()
    }

    #[test]
    fn covered_or_unrelated_paths_pass() {
        let kind = CoordinationResponseKind::Join;
        assert!(join_uncovered(kind, &s(&["src/a.rs"]), &s(&["src/a.rs", "docs/x.md"])).is_ok());
        assert!(join_uncovered(kind, &s(&["src/"]), &s(&["src/a/b.rs"])).is_ok());
    }

    #[test]
    fn wider_paths_are_named_and_bounded() {
        let kind = CoordinationResponseKind::Join;
        let error = join_uncovered(kind, &s(&["src/a.rs"]), &s(&["src/", "docs/x.md"]))
            .unwrap_err()
            .to_string();
        assert!(error.contains("src/**"), "{error}");
        assert!(!error.contains("docs/x.md"), "{error}");
        assert!(error.contains("agentscm_work_overlap_consent"), "{error}");
        let target = s(&["a/x", "b/x", "c/x", "d/x", "e/x"]);
        let error = join_uncovered(kind, &target, &s(&["a/", "b/", "c/", "d/", "e/"]))
            .unwrap_err()
            .to_string();
        assert!(error.contains("and 2 more"), "{error}");
        assert!(!error.contains("d/**"), "{error}");
    }

    #[test]
    fn other_kinds_pass() {
        for kind in [
            CoordinationResponseKind::ProposeSplit,
            CoordinationResponseKind::Handover,
            CoordinationResponseKind::Decline,
        ] {
            assert!(join_uncovered(kind, &s(&["src/a.rs"]), &s(&["src/"])).is_ok());
        }
    }
}
