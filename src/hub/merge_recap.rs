//! Bounded projection of tracked publication merges for Work overview surfaces.

use serde::Serialize;

use super::github::valid_ref_name;
use super::work::WorkOverview;
use super::{HubError, Result};

const RESULT_LIMIT: usize = 20;
pub(super) const QUERY_LIMIT: i64 = 21;

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct MergeRecapItem {
    pub id: String,
    pub title: Option<String>,
    pub branch: String,
    pub base: String,
    pub head_commit: String,
    pub merge_commit: String,
    pub pull_number: i64,
    pub updated_at: i64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct MergeRecap {
    pub since: i64,
    pub until: i64,
    pub time_basis: &'static str,
    pub scope: &'static str,
    pub merges: Vec<MergeRecapItem>,
    pub complete: bool,
    /// Narrow matching rows inspected, including the possible twenty-first truncation marker.
    pub scanned_count: usize,
}

#[derive(Debug, Serialize)]
pub struct WorkOverviewWithMergeRecap {
    #[serde(flatten)]
    pub overview: WorkOverview,
    pub merge_recap: Option<MergeRecap>,
}

pub(super) fn finish(
    since: i64,
    until: i64,
    mut merges: Vec<MergeRecapItem>,
) -> Result<MergeRecap> {
    if merges.len() > QUERY_LIMIT as usize
        || merges.iter().any(|item| {
            !valid_id(&item.id)
                || !valid_ref_name(&item.branch)
                || !valid_ref_name(&item.base)
                || item.branch == item.base
                || !valid_hex(&item.head_commit, &[40])
                || !valid_hex(&item.merge_commit, &[40, 64])
                || item.pull_number <= 0
                || !(since..=until).contains(&item.updated_at)
                || item.title.as_ref().is_some_and(|title| {
                    title.is_empty() || title.len() > 256 || title.chars().any(char::is_control)
                })
        })
    {
        return Err(HubError::Unavailable(
            "invalid retained merge recap identity".into(),
        ));
    }
    merges.sort_by(|left, right| {
        right
            .updated_at
            .cmp(&left.updated_at)
            .then_with(|| right.id.cmp(&left.id))
    });
    let scanned_count = merges.len();
    let complete = scanned_count <= RESULT_LIMIT;
    merges.truncate(RESULT_LIMIT);
    Ok(MergeRecap {
        since,
        until,
        time_basis: "last_recorded_update",
        scope: "tracked_publications",
        merges,
        complete,
        scanned_count,
    })
}

fn valid_id(value: &str) -> bool {
    value.len() == 36 && value.starts_with("pub_") && valid_hex(&value[4..], &[32])
}

fn valid_hex(value: &str, lengths: &[usize]) -> bool {
    lengths.contains(&value.len())
        && value
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
}
