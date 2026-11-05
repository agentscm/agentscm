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

/// A server-read provider merge. Titles, paths and actor tokens are deliberately omitted.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct MergeEvidence {
    pub id: String,
    pub sequence: i64,
    pub source: String,
    pub source_id: String,
    pub head_sha: String,
    pub merge_sha: String,
    pub occurred_at_seconds: Option<i64>,
    pub observed_at_seconds: i64,
}
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct PullEvidence {
    pub target: WorkPrLinkTarget,
    pub merges: Vec<MergeEvidence>,
    pub observed_at_ms: Option<i64>,
    pub observed_state: String,
    pub observed_head: Option<String>,
    #[serde(default)]
    pub observed_merge_sha: Option<String>,
}
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct SourceEvidence {
    pub source: String,
    pub state: String,
    pub checked_at_seconds: i64,
    pub unread_pages: i64,
    pub deferred: i64,
}
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct PrerequisiteVersion {
    pub work_id: String,
    pub updated_seq: Option<i64>,
}
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ReconciliationEvidence {
    pub provider: String,
    pub host: String,
    pub repository_id: Option<i64>,
    pub merge_through: i64,
    pub merge_floor: i64,
    pub sources: Vec<SourceEvidence>,
    pub pulls: Vec<PullEvidence>,
    pub prerequisites: Vec<PrerequisiteVersion>,
}
impl ReconciliationEvidence {
    pub(super) fn targets(view: &WorkDeclarationsView) -> Vec<WorkPrLinkTarget> {
        let mut out: Vec<_> = view
            .declarations
            .links
            .iter()
            .map(|l| l.target.clone())
            .collect();
        out.extend(view.compatibility_link.clone());
        out.extend(
            view.declarations
                .prerequisites
                .iter()
                .filter_map(|p| match &p.requirement {
                    PrerequisiteRequirement::MergedPullRequest { target, .. } => {
                        Some(target.clone())
                    }
                    _ => None,
                }),
        );
        out.sort_by(|a, b| {
            (a.repository_id, a.number, &a.head_sha).cmp(&(b.repository_id, b.number, &b.head_sha))
        });
        out.dedup();
        out
    }
    pub(super) fn digest(&self) -> String {
        digest(self)
    }
}
#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ConditionState {
    Satisfied,
    NotSatisfied,
    Unknown,
}
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ConditionDisposition {
    pub id: String,
    pub state: ConditionState,
    pub obstructing: bool,
    pub limits: Vec<String>,
}
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ReassessmentRequest {
    pub id: String,
    pub condition_id: String,
    pub blocker_report_digest: String,
    pub reason: String,
}
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct DurableReconciliation {
    pub work_id: String,
    pub generation: i64,
    pub work_seq: i64,
    pub declaration_seq: i64,
    pub evidence_digest: String,
    pub conditions: Vec<ConditionDisposition>,
    pub links: Vec<ConditionDisposition>,
    pub reassessments: Vec<ReassessmentRequest>,
    pub reservation_disposition: String,
    /// Policy-allowed transitions, each still requiring the existing actor authorization.
    pub reservation_remedies: Vec<String>,
    pub next_action: String,
    pub limits: Vec<String>,
    pub evidence: ReconciliationEvidence,
}
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ReconciliationView {
    pub schema_version: u16,
    pub work_id: String,
    pub generation: i64,
    pub updated_seq: i64,
    pub declaration_seq: i64,
    /// Historical retained result. Its states are not current unless `current` is present.
    pub retained: Option<DurableReconciliation>,
    pub current: Option<DurableReconciliation>,
    pub stale_reasons: Vec<String>,
    /// Digest of the freshly read evidence, even when retained evaluation is stale.
    /// This identifies facts, not provider success or a current evaluation.
    #[serde(default)]
    pub current_evidence_digest: Option<String>,
    /// Fresh source coverage, independent of a retained evaluation and its stale reasons.
    #[serde(default)]
    pub current_source_coverage: Vec<SourceEvidence>,
}
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ReconciliationApply {
    pub work_id: String,
    pub operation_id: String,
    pub generation: i64,
    pub session: String,
    pub expected_updated_seq: i64,
    pub declaration_seq: i64,
}
impl ReconciliationApply {
    pub(super) fn validate(&self) -> Result<()> {
        validate_id("work", &self.work_id, "wrk")?;
        label(&self.operation_id)?;
        label(&self.session)?;
        if self.generation < 1 || self.expected_updated_seq < 1 || self.declaration_seq < 1 {
            return Err(invalid("reconciliation fences must be positive"));
        }
        Ok(())
    }
    pub(super) fn authorize(&self, who: &WorkWho, item: &WorkItem) -> Result<()> {
        if who.read_only {
            return Err(HubError::Unauthorized);
        }
        if item.generation != self.generation
            || item.lead_token != who.token
            || item.lead_session != self.session
        {
            return Err(invalid(
                "only the current lead session and generation may reconcile Work",
            ));
        }
        Ok(())
    }
}
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ReconciliationResult {
    pub view: ReconciliationView,
    pub receipt: CheckpointReceipt,
}
fn digest(value: &impl Serialize) -> String {
    super::value_metrics::identity(&[
        &serde_json::to_string(value).expect("reconciliation serializes")
    ])
}
/// A retained individual provider read can qualify a publication's otherwise unverified head.
/// The lookup key identifies the requested target, not necessarily the returned provider object.
/// Require raw provider identities too; legacy tombstones without them cannot qualify a head.
pub(super) fn merged_observation(
    number: i64,
    repository_id: i64,
    value: &serde_json::Value,
) -> Option<(String, String)> {
    if repository_id < 1
        || value["observation_conflict"] == true
        || value["number"].as_i64() != Some(number)
        || value["provider_number"].as_i64() != Some(number)
        || value["provider_base_repository_id"].as_i64() != Some(repository_id)
        || value["state"] != "closed"
        || value["merged"] != true
    {
        return None;
    }
    let sha = |key: &str| {
        value[key]
            .as_str()
            .filter(|s| matches!(s.len(), 40 | 64) && s.bytes().all(|b| b.is_ascii_hexdigit()))
            .map(str::to_owned)
    };
    Some((sha("head_sha")?, sha("merge_commit_sha")?))
}

pub(super) fn pull_evidence(
    target: WorkPrLinkTarget,
    merges: Vec<super::catchup::CatchupMergeObservation>,
    observation: Option<(String, i64, bool)>,
) -> PullEvidence {
    let mut out = PullEvidence {
        target,
        merges: merges
            .into_iter()
            .map(|m| MergeEvidence {
                id: m.id,
                sequence: m.seq,
                source: super::catchup::source_name(m.source).into(),
                source_id: m.source_id,
                head_sha: m.head_commit,
                merge_sha: m.merge_commit,
                occurred_at_seconds: m.occurred_at,
                observed_at_seconds: m.observed_at,
            })
            .collect(),
        observed_at_ms: None,
        observed_state: "unobserved".into(),
        observed_head: None,
        observed_merge_sha: None,
    };
    if let Some((data, at, deleted)) = observation {
        out.observed_at_ms = Some(at);
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(&data) {
            let merged = merged_observation(out.target.number, out.target.repository_id, &v);
            out.observed_merge_sha = merged.as_ref().map(|(_, commit)| commit.clone());
            out.observed_state = if v["observation_conflict"] == true {
                "ambiguous"
            } else if merged.is_some() {
                "merged"
            } else if v["state"] == "closed" && v["merged"] == false {
                "closed_unmerged"
            } else if !deleted && v["state"] == "open" {
                "open"
            } else {
                "unknown"
            }
            .into();
            out.observed_head = v["head_sha"]
                .as_str()
                .filter(|s| matches!(s.len(), 40 | 64) && s.bytes().all(|c| c.is_ascii_hexdigit()))
                .map(str::to_owned);
            if out.observed_state == "open" && out.observed_head.is_none() {
                out.observed_state = "malformed".into();
            }
        } else {
            out.observed_state = "malformed".into();
        }
    }
    out
}
fn disposition(
    id: &str,
    target: Option<&WorkPrLinkTarget>,
    evidence: &ReconciliationEvidence,
) -> ConditionDisposition {
    let mut limits = Vec::new();
    let state = match target.and_then(|t| evidence.pulls.iter().find(|p| p.target == *t)) {
        None => {
            limits.push("unbound_legacy_condition".into());
            ConditionState::Unknown
        }
        Some(p) if !p.merges.is_empty() => {
            let head = if p.merges.len() != 1 {
                limits.push("ambiguous_merge_evidence".into());
                None
            } else {
                let merge = &p.merges[0];
                if merge.source == "forge_pull" {
                    Some(merge.head_sha.as_str())
                } else if p.observed_state == "merged"
                    && p.observed_merge_sha
                        .as_deref()
                        .is_some_and(|sha| sha.eq_ignore_ascii_case(&merge.merge_sha))
                {
                    p.observed_head.as_deref()
                } else {
                    limits.push("merge_head_unconfirmed".into());
                    None
                }
            };
            if p.merges.iter().any(|m| m.occurred_at_seconds.is_none()) {
                limits.push("merge_time_unknown".into());
            }
            match head {
                Some(head) if head.eq_ignore_ascii_case(&p.target.head_sha) => {
                    ConditionState::Satisfied
                }
                Some(_) => {
                    limits.push("merge_head_changed".into());
                    ConditionState::Unknown
                }
                None => ConditionState::Unknown,
            }
        }
        Some(p) => {
            if evidence.merge_floor > 0 {
                limits.push("merge_retention_gap".into());
            }
            let covered = evidence.sources.len() == 2
                && evidence
                    .sources
                    .iter()
                    .all(|s| s.state == "observed_only" && s.unread_pages == 0 && s.deferred == 0);
            if !covered {
                limits.push("coverage_unknown_or_incomplete".into());
                ConditionState::Unknown
            } else if evidence.merge_floor == 0
                && matches!(p.observed_state.as_str(), "open" | "closed_unmerged")
            {
                ConditionState::NotSatisfied
            } else {
                limits.push(format!("observation_{}", p.observed_state));
                ConditionState::Unknown
            }
        }
    };
    ConditionDisposition {
        id: id.into(),
        state,
        obstructing: state != ConditionState::Satisfied,
        limits,
    }
}
pub(super) fn evaluate(
    item: &WorkItem,
    view: &WorkDeclarationsView,
    evidence: ReconciliationEvidence,
    basis: Option<&str>,
) -> DurableReconciliation {
    let mut conditions = Vec::new();
    for c in &view.declarations.prerequisites {
        let target = match &c.requirement {
            PrerequisiteRequirement::MergedPullRequest { target, .. } => Some(target),
            _ => None,
        };
        let mut row = disposition(&c.id, target, &evidence);
        if evidence
            .prerequisites
            .iter()
            .any(|v| v.work_id == c.requirement.work_id() && v.updated_seq.is_none())
        {
            row.state = ConditionState::Unknown;
            row.obstructing = true;
            row.limits.push("prerequisite_unreadable".into());
        }
        conditions.push(row);
    }
    let mut links: Vec<_> = view
        .declarations
        .links
        .iter()
        .map(|l| disposition(&l.id, Some(&l.target), &evidence))
        .collect();
    if let Some(t) = &view.compatibility_link {
        links.push(disposition("compatibility", Some(t), &evidence));
    }
    let blocker_digest = item.blocker.as_ref().map(digest);
    let mut reassessments = Vec::new();
    if let Some(blocker) = &blocker_digest {
        for condition in &conditions {
            if condition.state != ConditionState::Satisfied {
                continue;
            }
            let declaration = view
                .declarations
                .prerequisites
                .iter()
                .find(|p| p.id == condition.id);
            let merges = declaration.and_then(|d| match &d.requirement {
                PrerequisiteRequirement::MergedPullRequest { target, .. } => evidence
                    .pulls
                    .iter()
                    .find(|p| p.target == *target)
                    .map(|p| &p.merges),
                _ => None,
            });
            reassessments.push(ReassessmentRequest {
                id: super::value_metrics::identity(&[&item.id, blocker, &condition.id, &digest(&declaration), &digest(&merges)]),
                condition_id: condition.id.clone(),
                blocker_report_digest: blocker.clone(),
                reason: "A declared prerequisite is satisfied; reassess the existing free-text blocker without rewriting its history.".into(),
            });
        }
    }
    let merged = links.iter().any(|l| l.state == ConditionState::Satisfied);
    let disposition = match basis {
        None => "released",
        Some("retained") => "explicitly_retained",
        Some(_) if merged => "held_owner_decision_required",
        Some(_) => "held",
    };
    let remedies = super::work_lifecycle::remedies(item.state, basis.is_some())
        .into_iter()
        .map(|kind| kind.as_str().to_owned())
        .collect();
    let next = if !reassessments.is_empty() {
        "Reassess the preserved blocker; other prerequisites and Work completion remain separate."
    } else if conditions.iter().any(|c| c.obstructing) {
        "Resolve or obtain evidence for the remaining declared prerequisites."
    } else if disposition == "held_owner_decision_required" {
        "The owner must decide whether to release, retain or complete Work."
    } else {
        "Declared prerequisites impose no current obstruction; this does not establish Work completion."
    };
    DurableReconciliation {
        work_id: item.id.clone(),
        generation: item.generation,
        work_seq: item.updated_seq,
        declaration_seq: view.declaration_seq,
        evidence_digest: evidence.digest(),
        conditions,
        links,
        reassessments,
        reservation_disposition: disposition.into(),
        reservation_remedies: remedies,
        next_action: next.into(),
        limits: vec![
            "retained_provider_observations_only".into(),
            "explicit_reconciliation_required".into(),
        ],
        evidence,
    }
}
pub(super) fn reconciliation_view(
    item: &WorkItem,
    declarations: &WorkDeclarationsView,
    evidence: &ReconciliationEvidence,
    retained: Option<DurableReconciliation>,
) -> ReconciliationView {
    let mut stale = Vec::new();
    match &retained {
        None => stale.push("not_reconciled".into()),
        Some(s) => {
            if s.work_seq != item.updated_seq || s.generation != item.generation {
                stale.push("work_changed".into());
            }
            if s.declaration_seq != declarations.declaration_seq {
                stale.push("declarations_changed".into());
            }
            if s.evidence_digest != evidence.digest() {
                stale.push("provider_or_prerequisite_evidence_changed".into());
            }
        }
    }
    ReconciliationView {
        schema_version: 2,
        work_id: item.id.clone(),
        generation: item.generation,
        updated_seq: item.updated_seq,
        declaration_seq: declarations.declaration_seq,
        current: if stale.is_empty() {
            retained.clone()
        } else {
            None
        },
        retained,
        stale_reasons: stale,
        current_evidence_digest: Some(evidence.digest()),
        current_source_coverage: evidence.sources.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn malformed_open_never_establishes_negative_prerequisite() {
        let target = WorkPrLinkTarget {
            repository_id: 1,
            number: 1,
            head_sha: "a".repeat(40),
        };
        let pull = pull_evidence(
            target.clone(),
            vec![],
            Some((r#"{"state":"open","head_sha":"broken"}"#.into(), 1, false)),
        );
        assert_eq!(pull.observed_state, "malformed");
        let evidence = ReconciliationEvidence {
            provider: "github".into(),
            host: "github.com".into(),
            repository_id: Some(1),
            merge_through: 0,
            merge_floor: 0,
            sources: ["forge_pull", "tracked_publication"]
                .into_iter()
                .map(|source| SourceEvidence {
                    source: source.into(),
                    state: "observed_only".into(),
                    checked_at_seconds: 1,
                    unread_pages: 0,
                    deferred: 0,
                })
                .collect(),
            pulls: vec![pull],
            prerequisites: vec![],
        };
        assert_eq!(
            disposition("condition", Some(&target), &evidence).state,
            ConditionState::Unknown
        );
    }
}
