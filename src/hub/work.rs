//! Durable shared engineering work for stand-ups and agent hand-offs.

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};

use super::github::valid_ref_name;
use super::intent::{spec_key, text};
use super::store::{User, validate_repository};
use super::{HubError, Result};

pub const MAX_WORK_PAGE: usize = 100;
pub const MAX_BRIEFING_BYTES: usize = 2 * 1024 * 1024;
pub const CLAIM_STALE_SECONDS: i64 = 15 * 60;
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
    pub work_id: Option<String>,
    pub source: Option<ForgeWorkRef>,
    pub request_key: Option<String>,
    pub title: String,
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
    RepositoryPath { path: String, line: Option<u32> },
    Commit { oid: String, path: Option<String> },
    Work { work_id: String },
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
    pub id: String,
    pub repository: String,
    pub owner: String,
    pub owner_login: String,
    pub source: WorkSource,
    pub title: String,
    pub state: WorkState,
    /// A request-time projection. Stale and unknown claims remain claimed.
    pub claim_status: ClaimStatus,
    pub last_checkin_at: i64,
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
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct WorkRepositoryScope {
    pub provider: String,
    pub host: String,
    pub repository: String,
    pub repository_id: Option<i64>,
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
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct WorkStartResult {
    pub item: WorkItem,
    pub created: bool,
    pub joined: bool,
    pub took_over: bool,
    pub conflict: Option<WorkStartConflict>,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum WorkConflictKind {
    ClaimHeld,
    StaleTakeoverRequired,
    GenerationChanged,
    SessionGoalChanged,
    MetadataChanged,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct WorkStartConflict {
    pub kind: WorkConflictKind,
    pub current_generation: i64,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct BriefingQuery {
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

impl WorkStart {
    pub(super) fn validate(&self, who: &WorkWho) -> Result<Vec<String>> {
        if usize::from(self.work_id.is_some())
            + usize::from(self.source.is_some())
            + usize::from(self.request_key.is_some())
            != 1
        {
            return Err(invalid(
                "exactly one of work_id, source or request_key is required",
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
        if let Some(id) = &self.work_after {
            validate_id("work_after", id, "wrk")?;
        }
        Ok(())
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

pub(super) fn check_work_payload_capacity(used: i64, delta: i64, limit: i64) -> Result<()> {
    if used.checked_add(delta).is_none_or(|total| total > limit) {
        return Err(HubError::WorkCapacity);
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
        check_work_payload_capacity,
    };

    #[test]
    fn work_payload_capacity_is_atomic_math_and_allows_non_growing_effects() {
        assert!(check_work_payload_capacity(90, 10, 100).is_ok());
        assert!(check_work_payload_capacity(100, 0, 100).is_ok());
        assert!(check_work_payload_capacity(100, -10, 100).is_ok());
        assert!(check_work_payload_capacity(91, 10, 100).is_err());
        assert!(check_work_payload_capacity(i64::MAX, 1, i64::MAX).is_err());
    }

    #[test]
    fn delivery_page_must_fit_its_reserved_bytes() {
        let payload = (WORK_DELIVERY_RESERVATION_BYTES - WORK_PAYLOAD_ROW_BYTES) as usize;
        assert!(check_work_delivery_page(&"x".repeat(payload)).is_ok());
        assert!(check_work_delivery_page(&"x".repeat(payload + 1)).is_err());
    }
}
