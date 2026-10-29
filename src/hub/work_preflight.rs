//! Atomic source and declared-scope checks for durable Work admission.

use serde::{Deserialize, Serialize};

use super::catchup::{CatchupMergeObservation, CatchupMergeSource};
use super::work::{WorkConflictKind, WorkItem, WorkScopeOverlap, WorkStartResult};
use super::{HubError, Result};

pub const MAX_PREFLIGHT_MERGES: usize = 50;

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct WorkPreflightRequest {
    /// Immutable source revision inspected by the caller.
    pub source_revision: String,
    /// First merge sequence inspected by the caller (exclusive).
    pub merge_after: i64,
    /// Fixed merge high-water inspected by the caller (inclusive).
    pub merge_through: i64,
    /// Caller-reported confirmation that it inspected the local Git relation between
    /// `source_revision` and every checked merge. This fact is reported in the result; the Hub
    /// does not claim it independently established Git ancestry.
    #[serde(default)]
    pub source_relation_checked: bool,
    /// Typed merge identities deliberately checked by the caller.
    #[serde(default)]
    pub checked_merges: Vec<CheckedMerge>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct CheckedMerge {
    pub source: CatchupMergeSource,
    pub source_id: String,
    pub pull_number: i64,
    pub merge_commit: String,
    /// Caller-reported result of the local ancestry check for this exact merge commit.
    pub in_source: bool,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum WorkPreflightStatus {
    ClearWithinCheckedWindow,
    ReviewRequired,
    Unknown,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum WorkPreflightReason {
    MissingScope,
    MissingSource,
    MissingHighwater,
    MergeHighwaterChanged,
    MergeHistoryGap,
    MergeCoverageUnknown,
    MergeWindowIncomplete,
    UncheckedMerge,
    MergePathsIncomplete,
    /// The inspected revision is the exact head of an observed merged pull request.
    /// This is a duplicate-subject signal, not an ancestry inference.
    SourceHeadAlreadyMerged,
    CheckedMergeUnknown,
    SourceMergeMissing,
    SourceRelationUnknown,
    ActiveScopeOverlap,
    /// The stable repository ID has unfinished or explicitly requested Work under an older name.
    RepositoryIdentityRenamed,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct WorkPreflightResult {
    pub status: WorkPreflightStatus,
    pub source_revision: Option<String>,
    pub merge_after: Option<i64>,
    pub merge_through: Option<i64>,
    pub current_merge_through: i64,
    pub source_relation_checked: bool,
    pub subject_match: Option<WorkItem>,
    pub scope_overlaps: Vec<WorkScopeOverlap>,
    pub merges: Vec<CatchupMergeObservation>,
    pub checked_observations: Vec<CatchupMergeObservation>,
    pub reasons: Vec<WorkPreflightReason>,
    pub complete: bool,
}

impl WorkPreflightRequest {
    pub(super) fn validate(&self) -> Result<()> {
        if !valid_hex(&self.source_revision, &[40, 64]) {
            return Err(invalid(
                "preflight source_revision must be 40 or 64 lowercase hex",
            ));
        }
        if self.merge_after < 0 || self.merge_through < 0 || self.merge_after > self.merge_through {
            return Err(invalid("invalid preflight merge window"));
        }
        if self.checked_merges.len() > MAX_PREFLIGHT_MERGES {
            return Err(invalid("preflight has more than 50 checked merges"));
        }
        let mut identities = std::collections::BTreeSet::new();
        for checked in &self.checked_merges {
            if checked.source_id.is_empty()
                || checked.source_id.len() > 255
                || checked
                    .source_id
                    .chars()
                    .any(|character| character.is_control() || super::advice::invisible(character))
                || checked.pull_number < 1
                || !valid_hex(&checked.merge_commit, &[40, 64])
                || !identities.insert((checked.pull_number, checked.merge_commit.as_str()))
            {
                return Err(invalid("invalid or duplicate checked merge identity"));
            }
        }
        Ok(())
    }
}

pub(super) fn checked(request: &WorkPreflightRequest, merge: &CatchupMergeObservation) -> bool {
    request.checked_merges.iter().any(|checked| {
        checked.pull_number == merge.pull_number
            && checked.merge_commit == merge.merge_commit
            && checked.source == merge.source
            && checked.source_id == merge.source_id
    })
}

/// An included exact merge needs no path inventory to establish source freshness. The ancestry
/// assertion remains caller-reported; matching a PR number alone never establishes inclusion.
pub(super) fn included(request: &WorkPreflightRequest, merge: &CatchupMergeObservation) -> bool {
    request.source_revision == merge.merge_commit
        || (request.source_relation_checked
            && request.checked_merges.iter().any(|candidate| {
                candidate.in_source
                    && candidate.source == merge.source
                    && candidate.source_id == merge.source_id
                    && candidate.pull_number == merge.pull_number
                    && candidate.merge_commit == merge.merge_commit
            }))
}

pub(super) fn status(reasons: &[WorkPreflightReason]) -> WorkPreflightStatus {
    if reasons.iter().any(|reason| {
        matches!(
            reason,
            WorkPreflightReason::MissingScope
                | WorkPreflightReason::MissingSource
                | WorkPreflightReason::MissingHighwater
                | WorkPreflightReason::MergeHistoryGap
                | WorkPreflightReason::MergeCoverageUnknown
                | WorkPreflightReason::MergeWindowIncomplete
                | WorkPreflightReason::MergePathsIncomplete
                | WorkPreflightReason::CheckedMergeUnknown
                | WorkPreflightReason::SourceRelationUnknown
        )
    }) {
        WorkPreflightStatus::Unknown
    } else if reasons.is_empty() {
        WorkPreflightStatus::ClearWithinCheckedWindow
    } else {
        WorkPreflightStatus::ReviewRequired
    }
}

/// An explicit new-lead start with a scope overlap must preserve the existing durable
/// coordination path even when its source preflight is not clear. The caller still receives the
/// non-clear preflight and no Work claim; exact existing-subject and resume paths retain their
/// normal admission rules.
pub(super) fn coordinates_active_scope_overlap(
    start: &super::work::WorkStart,
    result: &WorkPreflightResult,
) -> bool {
    start.preflight.is_some()
        && result.status != WorkPreflightStatus::ClearWithinCheckedWindow
        && result.subject_match.is_none()
        && !result.scope_overlaps.is_empty()
}

/// A non-clear preflight may enter `start_in` only to use its existing coordination protocol.
/// Refuse every other result before commit, including a future rule change that creates, joins or
/// takes over Work while the preflight still requires review.
pub(super) fn require_overlap_coordination(
    result: &WorkStartResult,
    preflight: &WorkPreflightResult,
) -> Result<()> {
    let scope_conflict = result.conflict.as_ref().is_some_and(|conflict| {
        conflict.kind == WorkConflictKind::ScopeOverlap && !conflict.overlaps.is_empty()
    });
    if !result.created
        && !result.joined
        && !result.took_over
        && scope_conflict
        && result.coordination_request.is_some()
    {
        Ok(())
    } else {
        Err(HubError::WorkPreflight(Box::new(preflight.clone())))
    }
}

pub(super) fn push_reason(reasons: &mut Vec<WorkPreflightReason>, reason: WorkPreflightReason) {
    if !reasons.contains(&reason) {
        reasons.push(reason);
    }
}

fn valid_hex(value: &str, lengths: &[usize]) -> bool {
    lengths.contains(&value.len())
        && value
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
}

fn invalid(message: &str) -> HubError {
    HubError::Invalid(message.to_owned())
}
