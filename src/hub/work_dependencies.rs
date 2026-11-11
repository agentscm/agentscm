//! Declared behavior dependencies and versioned notices, never inferred semantic conflicts.
use serde::{Deserialize, Serialize};

use super::work_reconciliation::{ConditionDisposition, PrerequisiteCondition};
use super::{HubError, Result};

pub const MAX_DEPENDENCY_NOTICE_BYTES: usize = 128 * 1024;
pub const MAX_DEPENDENCY_PAGE_BYTES: usize = 1024 * 1024;

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct WorkDependency {
    pub condition: PrerequisiteCondition,
    pub disposition: ConditionDisposition,
    pub prerequisite_updated_seq: Option<i64>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct WorkDependencyProjection {
    /// None identifies proposed legacy depends_on edges that have not been admitted.
    pub declaration_seq: Option<i64>,
    pub conditions: Vec<WorkDependency>,
    pub evidence_digest: String,
    pub observed_at: i64,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct DependencyVersion {
    pub work_id: String,
    pub updated_seq: Option<i64>,
    pub generation: Option<i64>,
}

/// The latest recorded relationship version. A current projection is read separately below;
/// neither an old notice nor a completed Work establishes prerequisite success.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct DependencyNoticeRecord {
    pub work_id: String,
    pub generation: i64,
    pub declaration_seq: i64,
    pub evidence_digest: String,
    pub conditions: Vec<ConditionDisposition>,
    pub notification_fanout_complete: bool,
    pub prerequisites: Vec<DependencyVersion>,
    /// Current dependent and prerequisite Work IDs; removed targets lose mailbox access.
    pub recipients: Vec<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct WorkDependencyNotice {
    pub sequence: i64,
    pub recorded: DependencyNoticeRecord,
    pub current: WorkDependencyProjection,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct WorkDependencyNoticeQuery {
    pub session: String,
    pub after: i64,
    pub through: Option<i64>,
    pub limit: usize,
}
impl WorkDependencyNoticeQuery {
    pub(super) fn validate(&self) -> Result<()> {
        super::intent::text("session", &self.session, 255)?;
        if self.after < 0
            || self.through.is_some_and(|through| through < self.after)
            || !(1..=100).contains(&self.limit)
        {
            return Err(HubError::Invalid(
                "invalid dependency notice cursor or limit".into(),
            ));
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct WorkDependencyNoticePage {
    pub through: i64,
    pub next_after: i64,
    pub complete: bool,
    pub notices: Vec<WorkDependencyNotice>,
    /// This mailbox retains the latest version per dependent Work; audit history is separate.
    pub latest_only: bool,
    /// Pages are current transactional reads, not a retained immutable delivery.
    pub atomic_snapshot: bool,
    pub coverage_complete: bool,
}
