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
        Ok(())
    }
}

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
    if !matches!(item.state, WorkState::Active | WorkState::Blocked) || !holds_reservation {
        return Err(HubError::Invalid(format!(
            "Nothing was joined: Work {} is not active with a held reservation, so no edit could run under a join. Resume or take over the Work, or ask its lead.",
            item.id
        )));
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
