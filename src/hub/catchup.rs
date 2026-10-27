//! Durable, explicit return-to-work deliveries.
//!
//! A delivery freezes both the existing Work/coordination event watermark and
//! the merge-observation watermark. Reading or rendering a page never advances
//! either acknowledgement cursor.

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::github::valid_ref_name;
use super::work::{WorkItem, normalized_scope, validate_id};
use super::{HubError, Result};

pub const MAX_CATCHUP_PAGE: usize = 50;
pub const CATCHUP_PREVIEW_LIMIT: usize = 20;
pub const MAX_CATCHUP_PATHS: usize = 256;
pub const HUMAN_SUBSCRIBER: &str = "human";
pub const CATCHUP_DELIVERY_CAPACITY_BYTES: i64 = 128 * 1024 * 1024;
pub const CATCHUP_AGENT_DELIVERY_CAPACITY_BYTES: i64 = 96 * 1024 * 1024;
pub const CATCHUP_AGENT_TOKEN_DELIVERY_CAPACITY_BYTES: i64 = 32 * 1024 * 1024;
pub const MAX_ACTIVE_HUMAN_DELIVERIES: i64 = 32;
pub const MAX_ACTIVE_AGENT_DELIVERIES_PER_TOKEN: i64 = 16;
pub const MAX_ACTIVE_CATCHUP_DELIVERIES: i64 = 128;
pub const MAX_ACTIVE_AGENT_DELIVERIES: i64 =
    MAX_ACTIVE_CATCHUP_DELIVERIES - MAX_ACTIVE_HUMAN_DELIVERIES;
pub const MAX_CATCHUP_MERGE_ROWS: i64 = 100_000;
pub const MAX_CATCHUP_MERGE_BYTES: i64 = 512 * 1024 * 1024;
pub const MAX_CATCHUP_PAGE_PAYLOAD_BYTES: usize = 3 * 1024 / 2 * 1024;

pub(super) struct CatchupPageWindow<'a> {
    pub subscriber: &'a str,
    pub delivery_client: &'a str,
    pub delivery: &'a str,
    pub event_from: i64,
    pub event_through: i64,
    pub event_after: i64,
    pub merge_from: i64,
    pub merge_through: i64,
    pub merge_after: i64,
    pub limit: usize,
}

pub(super) struct TrackedPublicationMergeInput {
    pub source_id: String,
    pub title: String,
    pub branch: String,
    pub base: String,
    pub head_commit: String,
    pub merge_commit: Option<String>,
    pub pull_number: Option<i64>,
    pub paths: Vec<String>,
    pub observed_at: i64,
}

/// A bound forge repository ID is the durable catch-up namespace. The name remains the namespace
/// only for unbound/self-hosted repositories, where no verified stable identity exists.
pub(super) fn repository_namespace(repository: &str, repository_id: Option<i64>) -> String {
    repository_id.map_or_else(|| repository.to_owned(), |id| format!("@repo-id:{id}"))
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CatchupMergeSource {
    TrackedPublication,
    ForgePull,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CatchupCoverageState {
    ObservedOnly,
    Incomplete,
    Unavailable,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CatchupSourceStatus {
    pub source: CatchupMergeSource,
    pub state: CatchupCoverageState,
    pub checked_at: i64,
    pub unread_pages: u64,
    pub deferred: u64,
    /// Lower bound of the closed-proposal window that was read, or `None` when
    /// no window was read. History before it is not observed.
    pub window_from: Option<i64>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CatchupRepositoryScope {
    pub provider: String,
    pub host: String,
    pub repository: String,
    pub repository_id: Option<i64>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NewCatchupMerge {
    pub source: CatchupMergeSource,
    pub source_id: String,
    pub occurred_at: Option<i64>,
    pub title: Option<String>,
    pub branch: String,
    pub base: String,
    pub head_commit: String,
    pub merge_commit: String,
    pub pull_number: i64,
    pub paths: Vec<String>,
    pub paths_complete: bool,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct CatchupMergeObservation {
    pub id: String,
    pub seq: i64,
    pub source: CatchupMergeSource,
    pub source_id: String,
    pub occurred_at: Option<i64>,
    pub observed_at: i64,
    pub title: Option<String>,
    pub branch: String,
    pub base: String,
    pub head_commit: String,
    pub merge_commit: String,
    pub pull_number: i64,
    pub paths: Vec<String>,
    pub paths_complete: bool,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct CatchupWorkEvent {
    pub seq: i64,
    pub at: i64,
    pub kind: String,
    pub work_id: String,
    pub item: WorkItem,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct CatchupCoordinationEvent {
    pub seq: i64,
    pub at: i64,
    pub kind: String,
    pub request_id: String,
    pub title: String,
    pub status: String,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct CatchupRetention {
    pub work_gap: bool,
    pub coordination_gap: bool,
    pub merge_gap: bool,
    pub event_available_from: i64,
    pub merge_available_from: i64,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct CatchupSourceCoverage {
    pub source: CatchupMergeSource,
    pub state: CatchupCoverageState,
    pub checked_at: Option<i64>,
    pub unread_pages: u64,
    pub deferred: u64,
    /// Lower bound of the closed-proposal window the last pass read; history
    /// before it is not observed. Absent for older payloads and other sources.
    #[serde(default)]
    pub window_from: Option<i64>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct CatchupQuery {
    #[serde(default = "default_subscriber")]
    pub subscriber: String,
    #[serde(default = "default_delivery_client")]
    pub delivery_client: String,
    pub delivery: Option<String>,
    pub event_after: Option<i64>,
    pub event_through: Option<i64>,
    pub merge_after: Option<i64>,
    pub merge_through: Option<i64>,
    #[serde(default = "default_limit")]
    pub limit: usize,
}

impl Default for CatchupQuery {
    fn default() -> Self {
        Self {
            subscriber: default_subscriber(),
            delivery_client: default_delivery_client(),
            delivery: None,
            event_after: None,
            event_through: None,
            merge_after: None,
            merge_through: None,
            limit: default_limit(),
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct CatchupPage {
    pub subscriber: String,
    pub delivery_client: String,
    pub delivery: String,
    pub event_from: i64,
    pub event_through: i64,
    pub event_after: i64,
    pub next_event_after: i64,
    pub merge_from: i64,
    pub merge_through: i64,
    pub merge_after: i64,
    pub next_merge_after: i64,
    pub work_events: Vec<CatchupWorkEvent>,
    pub coordination_events: Vec<CatchupCoordinationEvent>,
    pub merges: Vec<CatchupMergeObservation>,
    pub retention: CatchupRetention,
    pub source_coverage: Vec<CatchupSourceCoverage>,
    pub complete: bool,
}

/// A bounded, read-only first look for automatic startup surfaces. It creates
/// no delivery and cannot be acknowledged or continued.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct CatchupPreviewQuery {
    pub subscriber: String,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct CatchupPreview {
    pub subscriber: String,
    pub event_ack: i64,
    pub event_through: i64,
    pub merge_ack: i64,
    pub merge_through: i64,
    pub work_events: Vec<CatchupWorkEvent>,
    pub coordination_events: Vec<CatchupCoordinationEvent>,
    pub merges: Vec<CatchupMergeObservation>,
    pub retention: CatchupRetention,
    pub source_coverage: Vec<CatchupSourceCoverage>,
    pub complete: bool,
}

/// Trusted-local proposal or application of one retained publication-projection repair.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct CatchupPublicationRepair {
    pub repository: String,
    pub publication: String,
    pub merge_commit: String,
    pub apply: bool,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct CatchupPublicationRepairResult {
    pub publication: String,
    pub repository: String,
    pub previous_merge_commit_sha256: Option<String>,
    pub proposed_merge_commit: String,
    pub applied: bool,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct CatchupAck {
    pub delivery: String,
    pub expected_event_from: i64,
    pub expected_merge_from: i64,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CatchupAckStatus {
    Advanced,
    AlreadyAcknowledged,
    Conflict,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct CatchupAckResult {
    pub status: CatchupAckStatus,
    pub event_through: i64,
    pub merge_through: i64,
    pub current_event_ack: i64,
    pub current_merge_ack: i64,
}

impl CatchupQuery {
    pub(super) fn validate(&self) -> Result<()> {
        catchup_label("catch-up subscriber", &self.subscriber)?;
        catchup_label("catch-up delivery client", &self.delivery_client)?;
        if !(1..=MAX_CATCHUP_PAGE).contains(&self.limit) {
            return Err(invalid("catch-up limit must be 1 to 50"));
        }
        for cursor in [
            self.event_after,
            self.event_through,
            self.merge_after,
            self.merge_through,
        ] {
            if cursor.is_some_and(|value| value < 0) {
                return Err(invalid("catch-up cursors must be non-negative"));
            }
        }
        match &self.delivery {
            None if self.event_after.is_none()
                && self.event_through.is_none()
                && self.merge_after.is_none()
                && self.merge_through.is_none() => {}
            Some(delivery)
                if self.event_after.is_some()
                    && self.event_through.is_some()
                    && self.merge_after.is_some()
                    && self.merge_through.is_some() =>
            {
                validate_id("catch-up delivery", delivery, "cup")?;
            }
            _ => {
                return Err(invalid(
                    "catch-up continuation requires delivery and every cursor",
                ));
            }
        }
        Ok(())
    }
}

impl CatchupPreviewQuery {
    pub(super) fn validate(&self) -> Result<()> {
        catchup_label("catch-up subscriber", &self.subscriber)
    }
}

impl CatchupPublicationRepair {
    pub(super) fn validate(&self) -> Result<()> {
        super::store::validate_repository(&self.repository)?;
        validate_id("publication", &self.publication, "pub")?;
        if !valid_hex(&self.merge_commit, &[40, 64]) {
            return Err(invalid(
                "merge commit must be 40 or 64 lowercase hexadecimal characters",
            ));
        }
        Ok(())
    }
}

impl CatchupPreview {
    pub(super) fn from_page(page: CatchupPage) -> Self {
        Self {
            subscriber: page.subscriber,
            event_ack: page.event_from,
            event_through: page.event_through,
            merge_ack: page.merge_from,
            merge_through: page.merge_through,
            work_events: page.work_events,
            coordination_events: page.coordination_events,
            merges: page.merges,
            retention: page.retention,
            source_coverage: page.source_coverage,
            complete: page.complete,
        }
    }
}

impl CatchupAck {
    pub(super) fn validate(&self) -> Result<()> {
        validate_id("catch-up delivery", &self.delivery, "cup")?;
        if self.expected_event_from < 0 || self.expected_merge_from < 0 {
            return Err(invalid(
                "catch-up acknowledgement cursors must be non-negative",
            ));
        }
        Ok(())
    }
}

impl NewCatchupMerge {
    pub(super) fn validate(&self, observed_at: i64) -> Result<()> {
        catchup_label("catch-up merge source id", &self.source_id)?;
        if observed_at < 0 || self.occurred_at.is_some_and(|at| at < 0) {
            return Err(invalid("catch-up merge times must be non-negative"));
        }
        if self.pull_number < 1
            || !valid_catchup_ref(&self.branch)
            || !valid_catchup_ref(&self.base)
            || self.branch == self.base
            || !valid_hex(&self.head_commit, &[40, 64])
            || !valid_hex(&self.merge_commit, &[40, 64])
        {
            return Err(invalid("invalid catch-up merge identity"));
        }
        if self.title.as_ref().is_some_and(|title| {
            title.is_empty()
                || title.len() > 256
                || title
                    .chars()
                    .any(|character| character.is_control() || super::advice::invisible(character))
        }) {
            return Err(invalid("invalid catch-up merge title"));
        }
        if self.paths.len() > MAX_CATCHUP_PATHS {
            return Err(invalid("catch-up merge has more than 256 paths"));
        }
        if normalized_scope(&self.paths)? != self.paths {
            return Err(invalid(
                "catch-up merge paths must be normalized, sorted and unique",
            ));
        }
        Ok(())
    }
}

impl CatchupSourceStatus {
    pub(super) fn validate(&self) -> Result<()> {
        if self.checked_at < 0 || self.unread_pages > 1_000_000 || self.deferred > 1_000_000 {
            return Err(invalid("invalid catch-up source status"));
        }
        if self
            .window_from
            .is_some_and(|from| from < 0 || from > self.checked_at)
        {
            return Err(invalid("invalid catch-up source window"));
        }
        Ok(())
    }
}

impl CatchupRepositoryScope {
    pub(super) fn validate(&self) -> Result<()> {
        if !matches!(self.provider.as_str(), "github" | "gitlab")
            || self.host.is_empty()
            || self.host.len() > 253
            || self.repository.is_empty()
            || self.repository.len() > 255
            || !self.repository.contains('/')
            || self.repository_id.is_some_and(|id| id < 0)
        {
            return Err(invalid("invalid catch-up repository scope"));
        }
        Ok(())
    }
}

pub(super) fn source_name(source: CatchupMergeSource) -> &'static str {
    match source {
        CatchupMergeSource::TrackedPublication => "tracked_publication",
        CatchupMergeSource::ForgePull => "forge_pull",
    }
}

/// Produces the bounded plain-text projection stored in an observation.
/// The authoritative publication or forge record remains unchanged.
pub(super) fn bounded_title(value: &str) -> Option<String> {
    let mut title = String::new();
    for character in value
        .chars()
        .filter(|character| !character.is_control() && !super::advice::invisible(*character))
    {
        if title.len() + character.len_utf8() > 256 {
            break;
        }
        title.push(character);
    }
    (!title.is_empty()).then_some(title)
}

pub(super) fn tracked_publication_merge(
    input: TrackedPublicationMergeInput,
) -> Result<NewCatchupMerge> {
    let TrackedPublicationMergeInput {
        source_id,
        title,
        branch,
        base,
        head_commit,
        merge_commit,
        pull_number,
        mut paths,
        observed_at,
    } = input;
    paths.sort();
    paths.dedup();
    let paths_complete = paths.len() <= MAX_CATCHUP_PATHS;
    paths.truncate(MAX_CATCHUP_PATHS);
    let merge = NewCatchupMerge {
        source: CatchupMergeSource::TrackedPublication,
        source_id,
        occurred_at: None,
        title: bounded_title(&title),
        branch,
        base,
        head_commit,
        merge_commit: merge_commit
            .ok_or_else(|| HubError::Unavailable("merged publication lacks merge commit".into()))?,
        pull_number: pull_number
            .ok_or_else(|| HubError::Unavailable("merged publication lacks pull request".into()))?,
        paths,
        paths_complete,
    };
    merge.validate(observed_at)?;
    Ok(merge)
}

/// Maps an authenticated agent session to a subscriber namespace that can
/// never alias the shared person-level `human` cursor.
pub(super) fn agent_subscriber(session: &str) -> Result<String> {
    if session.is_empty()
        || session
            .chars()
            .any(|character| character.is_control() || super::advice::invisible(character))
    {
        return Err(invalid("agent catch-up session is invalid"));
    }
    let direct = format!("agent:{session}");
    if direct.len() <= 200 {
        return Ok(direct);
    }
    Ok(format!("agent:{:x}", Sha256::digest(session.as_bytes())))
}

pub(super) fn parse_source(value: &str) -> Result<CatchupMergeSource> {
    match value {
        "tracked_publication" => Ok(CatchupMergeSource::TrackedPublication),
        "forge_pull" => Ok(CatchupMergeSource::ForgePull),
        _ => Err(HubError::Unavailable(
            "invalid retained catch-up source".into(),
        )),
    }
}

pub(super) fn coverage_name(state: CatchupCoverageState) -> &'static str {
    match state {
        CatchupCoverageState::ObservedOnly => "observed_only",
        CatchupCoverageState::Incomplete => "incomplete",
        CatchupCoverageState::Unavailable => "unavailable",
    }
}

pub(super) fn parse_coverage(value: &str) -> Result<CatchupCoverageState> {
    match value {
        "observed_only" => Ok(CatchupCoverageState::ObservedOnly),
        "incomplete" => Ok(CatchupCoverageState::Incomplete),
        "unavailable" => Ok(CatchupCoverageState::Unavailable),
        _ => Err(HubError::Unavailable(
            "invalid retained catch-up coverage".into(),
        )),
    }
}

fn default_subscriber() -> String {
    HUMAN_SUBSCRIBER.to_owned()
}

fn default_delivery_client() -> String {
    "default".to_owned()
}

const fn default_limit() -> usize {
    20
}

fn valid_hex(value: &str, lengths: &[usize]) -> bool {
    lengths.contains(&value.len())
        && value
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
}

fn valid_catchup_ref(value: &str) -> bool {
    valid_ref_name(value) && !value.chars().any(super::advice::invisible)
}

fn invalid(message: &str) -> HubError {
    HubError::Invalid(message.to_owned())
}

fn catchup_label(name: &str, value: &str) -> Result<()> {
    if value.is_empty()
        || value.len() > 200
        || value
            .chars()
            .any(|character| character.is_control() || super::advice::invisible(character))
    {
        Err(HubError::Invalid(format!(
            "{name} must be 1 to 200 bytes without control characters"
        )))
    } else {
        Ok(())
    }
}
