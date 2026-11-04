//! Durable caller-declared PR sets and prerequisite conditions, independent of legacy reports.
//! Declarations do not establish provider facts, completion, approval or reservation authority.
use super::work::{CheckpointReceipt, WorkItem, WorkState, WorkWho, validate_id};
use super::work_links::{self, WorkPrLinkTarget};
use super::{HubError, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;

pub const MAX_DECLARATIONS: usize = 32;

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct DeclaredPrLink {
    pub id: String,
    pub target: WorkPrLinkTarget,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum PrerequisiteRequirement {
    /// An exact caller-declared PR/head, never a whole-task success assertion.
    MergedPullRequest {
        work_id: String,
        target: WorkPrLinkTarget,
    },
    /// Migrated legacy dependency without an evidence target; remains unknown until replaced.
    LegacyWork { work_id: String },
}
impl PrerequisiteRequirement {
    pub fn work_id(&self) -> &str {
        match self {
            Self::MergedPullRequest { work_id, .. } | Self::LegacyWork { work_id } => work_id,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct PrerequisiteCondition {
    pub id: String,
    pub requirement: PrerequisiteRequirement,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct WorkDeclarations {
    pub links: Vec<DeclaredPrLink>,
    pub prerequisites: Vec<PrerequisiteCondition>,
}
impl WorkDeclarations {
    pub(super) fn legacy(item: &WorkItem) -> Self {
        let mut ids = item.depends_on.clone();
        ids.sort();
        ids.dedup();
        Self {
            links: vec![],
            prerequisites: ids
                .into_iter()
                .map(|work_id| PrerequisiteCondition {
                    id: format!("legacy-{work_id}"),
                    requirement: PrerequisiteRequirement::LegacyWork { work_id },
                })
                .collect(),
        }
    }
    pub(super) fn targets(&self) -> Vec<String> {
        self.prerequisites
            .iter()
            .map(|p| p.requirement.work_id().to_owned())
            .collect()
    }
    pub(super) fn normalized(&self, repository_id: i64, work_id: &str) -> Result<Self> {
        if self.links.len() > MAX_DECLARATIONS || self.prerequisites.len() > MAX_DECLARATIONS {
            return Err(invalid(
                "at most 32 PR links and 32 prerequisites are allowed",
            ));
        }
        let mut ids = BTreeSet::new();
        for link in &self.links {
            label(&link.id)?;
            if !ids.insert(&link.id) {
                return Err(invalid("duplicate PR link id"));
            }
            target(&link.target, repository_id)?;
        }
        ids.clear();
        for condition in &self.prerequisites {
            label(&condition.id)?;
            if !ids.insert(&condition.id) {
                return Err(invalid("duplicate prerequisite id"));
            }
            let related = condition.requirement.work_id();
            validate_id("prerequisite work", related, "wrk")?;
            if related == work_id {
                return Err(invalid("work cannot depend on itself"));
            }
            if let PrerequisiteRequirement::MergedPullRequest { target: value, .. } =
                &condition.requirement
            {
                target(value, repository_id)?;
            }
        }
        let mut out = self.clone();
        out.links.sort_by(|a, b| a.id.cmp(&b.id));
        out.prerequisites.sort_by(|a, b| a.id.cmp(&b.id));
        Ok(out)
    }
}
fn invalid(message: &str) -> HubError {
    HubError::Invalid(message.into())
}
fn label(value: &str) -> Result<()> {
    if value.is_empty()
        || value.len() > 255
        || value
            .chars()
            .any(|c| c.is_control() || super::advice::invisible(c))
    {
        return Err(invalid(
            "declaration labels must contain 1 to 255 visible bytes",
        ));
    }
    Ok(())
}
fn target(value: &WorkPrLinkTarget, repository_id: i64) -> Result<()> {
    if value.repository_id != repository_id
        || value.number < 1
        || !matches!(value.head_sha.len(), 40 | 64)
        || !value
            .head_sha
            .bytes()
            .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
    {
        return Err(invalid(
            "PR target requires the authorized repository ID, positive number and full lowercase head SHA",
        ));
    }
    Ok(())
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct WorkDeclarationsUpdate {
    pub work_id: String,
    pub operation_id: String,
    pub generation: i64,
    pub session: String,
    pub expected_updated_seq: i64,
    /// A complete replacement of the additional link set and durable prerequisites.
    pub declarations: WorkDeclarations,
}
impl WorkDeclarationsUpdate {
    pub(super) fn validate(&self) -> Result<()> {
        validate_id("work", &self.work_id, "wrk")?;
        label(&self.operation_id)?;
        label(&self.session)?;
        if self.generation < 1 || self.expected_updated_seq < 1 {
            return Err(invalid(
                "generation and expected_updated_seq must be positive",
            ));
        }
        Ok(())
    }
    pub(super) fn authorize(&self, who: &WorkWho, item: &WorkItem) -> Result<i64> {
        if who.read_only {
            return Err(HubError::Unauthorized);
        }
        if item.generation != self.generation
            || item.lead_token != who.token
            || item.lead_session != self.session
        {
            return Err(invalid(
                "only the current lead session and generation may update declarations",
            ));
        }
        work_links::trusted_repository_id(who)
    }
    pub(super) fn check_current(&self, item: &WorkItem) -> Result<()> {
        if item.state == WorkState::Finished {
            return Err(invalid(
                "finished Work must be reopened before changing declarations",
            ));
        }
        if item.updated_seq != self.expected_updated_seq {
            return Err(invalid("Work changed or declaration receipt expired"));
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct WorkDeclarationsView {
    pub schema_version: u16,
    pub work_id: String,
    pub generation: i64,
    pub updated_seq: i64,
    pub declaration_seq: i64,
    /// The old single-link endpoint owns this independent compatibility slot.
    pub compatibility_link: Option<WorkPrLinkTarget>,
    pub declarations: WorkDeclarations,
}
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct WorkDeclarationsResult {
    pub view: WorkDeclarationsView,
    pub receipt: CheckpointReceipt,
}
pub(super) fn view(
    item: &WorkItem,
    declaration_seq: i64,
    declarations: WorkDeclarations,
    compatibility_link: Option<WorkPrLinkTarget>,
) -> WorkDeclarationsView {
    WorkDeclarationsView {
        schema_version: 2,
        work_id: item.id.clone(),
        generation: item.generation,
        updated_seq: item.updated_seq,
        declaration_seq,
        compatibility_link,
        declarations,
    }
}
