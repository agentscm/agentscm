//! Durable requests used to resolve overlapping shared work.

use serde::{Deserialize, Serialize};

use super::intent::text;
use super::work::{ForgeWorkRef, WorkPathOverlap, validate_id};
use super::{HubError, Result};

pub const MAX_COORDINATION_PAGE: usize = 100;
pub const MAX_ACTIVE_COORDINATION_REQUESTS: i64 = 100;
pub const MAX_ACTIVE_COORDINATION_REQUESTS_PER_OWNER: i64 = 25;
pub const MAX_ACTIVE_COORDINATION_REQUESTS_PER_TOKEN: i64 = 8;
pub const MAX_COORDINATION_UPDATES: i64 = 500;
pub const MAX_PROJECTED_COORDINATION_UPDATES: i64 = 128;
pub const MAX_COORDINATION_REASONS_PER_TARGET: usize = 4;
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
    /// pages; absent when the agent answered itself. `responder` is still the agent, so a reader
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
    /// Set only by the Hub's browser-session route, never read from a request body: the person
    /// who owns the lead agent answered for it. Recorded as `via: "hub"` on the response.
    #[serde(skip)]
    pub via_hub: bool,
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

impl CoordinationReply {
    pub(super) fn validate(&self) -> Result<()> {
        validate_mutation(&self.request_id, &self.session, self.expected_updated_seq)?;
        validate_id("target work", &self.target_work_id, "wrk")?;
        text("operation_id", &self.operation_id, 255)?;
        if let Some(note) = &self.note {
            text("note", note, MAX_NOTE_BYTES)?;
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
        text("operation_id", &self.operation_id, 255)
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

/// The event data of a response, marked `via: "hub"` when a person answered for the agent.
pub(super) fn with_hub_via(mut data: serde_json::Value, hub: bool) -> serde_json::Value {
    if hub {
        data["via"] = serde_json::json!("hub");
    }
    data
}
