//! Cooperative Git Work tasks in the existing journal. Reports never grant native admission.
//!
//! The trusted host adapter resolves visible human references, pins library bytes, captures
//! current source and authorizes human operations. Skill prose and reports cannot do so.

use std::collections::BTreeSet;
use std::path::Path;

use rusqlite::{Connection, OptionalExtension, Transaction, TransactionBehavior, params};
use serde::{Deserialize, Serialize};

use super::{Journal, OpenMode};
use crate::change_record::{
    AssuranceBasisV1, CaptureOmissionV1, ContentAvailabilityV1, CoverageV1, SourceBindingV1,
};
use crate::{Error, Result};

const MAX_RECORD: usize = 512 * 1024;
const MAX_ATTEMPTS: usize = 64;
const MAX_LEASE_MS: u64 = 15 * 60 * 1_000;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum IntentReferenceKind {
    HumanMessage,
    AuthorizedHumanSnapshot,
    HumanAmendment,
    AgentPlan,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct IntentReference {
    pub id: String,
    pub kind: IntentReferenceKind,
    pub source: String,
    pub author: String,
    pub recorded_at_ms: u64,
    pub sha256: String,
    pub coverage: CoverageV1,
    /// Only host-authorized visible content; absent bodies remain referenced, never inferred.
    #[serde(default)]
    pub text: Option<String>,
    #[serde(default)]
    pub reconstructed: bool,
}

impl IntentReference {
    fn validate(&self) -> Result<()> {
        id(&self.id)?;
        text(&self.source, 2_048)?;
        if self.source.starts_with('/') || self.source.starts_with("file:") {
            return invalid("intent references must be opaque identities, not local paths");
        }
        text(&self.author, 256)?;
        digest(&self.sha256)?;
        if let Some(body) = &self.text {
            text(body, 16 * 1024)?;
            if crate::store::digest(body.as_bytes()) != self.sha256 {
                return invalid("intent excerpt differs from its content identity");
            }
        }
        Ok(())
    }

    fn human(&self) -> bool {
        self.kind != IntentReferenceKind::AgentPlan
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct IntentCriterion {
    pub id: String,
    pub description: String,
    pub sources: Vec<String>,
    pub required: bool,
    /// Constraints remain in every checkpoint and child scope.
    pub constraint: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct IntentContract {
    pub version: u64,
    pub previous_digest: Option<String>,
    pub request: Vec<IntentReference>,
    pub amendments: Vec<IntentReference>,
    pub criteria: Vec<IntentCriterion>,
    pub focus: Vec<IntentReference>,
    pub coverage: CoverageV1,
    pub gaps: Vec<String>,
}

impl IntentContract {
    pub fn digest(&self) -> Result<String> {
        self.validate()?;
        identity("contract", self)
    }

    fn validate(&self) -> Result<()> {
        if self.version == 0 || self.criteria.is_empty() || self.criteria.len() > 64 {
            return invalid("intent contract requires a version and 1 to 64 criteria");
        }
        if let Some(prior) = &self.previous_digest {
            digest(prior)?;
        }
        let mut refs = BTreeSet::new();
        for reference in self
            .request
            .iter()
            .chain(&self.amendments)
            .chain(&self.focus)
        {
            reference.validate()?;
            if !refs.insert(&reference.id) {
                return invalid("duplicate intent reference");
            }
        }
        if self.request.len() + self.amendments.len() + self.focus.len() > 128
            || self.request.iter().any(|reference| !reference.human())
            || self
                .amendments
                .iter()
                .any(|reference| reference.kind != IntentReferenceKind::HumanAmendment)
            || self
                .focus
                .iter()
                .any(|reference| reference.kind != IntentReferenceKind::AgentPlan)
        {
            return invalid("intent reference role or count is invalid");
        }
        if self.coverage == CoverageV1::Complete
            && (self.request.is_empty()
                || !self.gaps.is_empty()
                || self
                    .request
                    .iter()
                    .chain(&self.amendments)
                    .any(|reference| {
                        reference.coverage != CoverageV1::Complete || reference.reconstructed
                    }))
        {
            return invalid("complete intent requires available human request coverage");
        }
        let human: BTreeSet<_> = self
            .request
            .iter()
            .chain(&self.amendments)
            .map(|reference| &reference.id)
            .collect();
        let mut criteria = BTreeSet::new();
        for criterion in &self.criteria {
            id(&criterion.id)?;
            text(&criterion.description, 2_048)?;
            if !criteria.insert(&criterion.id)
                || criterion.sources.is_empty()
                || criterion.sources.len() > 16
            {
                return invalid("invalid criterion identity or provenance");
            }
            let mut sources = BTreeSet::new();
            for source in &criterion.sources {
                if !human.contains(source) || !sources.insert(source) {
                    return invalid("criterion must cite unique recorded human references");
                }
            }
        }
        texts(&self.gaps)
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct IntentScope {
    pub parent_work_id: Option<String>,
    pub checkpoint: Option<String>,
    pub criteria: Vec<String>,
    pub coverage: CoverageV1,
    pub gaps: Vec<String>,
    /// The host must resolve these against human-authorized scope, not skill prose.
    #[serde(default)]
    pub source_exclusions: Vec<IntentSourceExclusion>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct IntentSourceExclusion {
    pub omission: CaptureOmissionV1,
    pub reason: String,
    pub authorization_reference: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct IntentSkillBinding {
    pub workflow_version_id: String,
    pub step_id: String,
    pub skill_version_id: String,
    pub skill_sha256: String,
    pub package_digest: String,
    pub entrypoint: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct IntentEvidence {
    pub id: String,
    pub sha256: String,
    pub origin: String,
    /// Exact captured source identity; absent provenance cannot support a criterion.
    pub candidate_digest: Option<String>,
    pub coverage: CoverageV1,
    pub assurance: AssuranceBasisV1,
    pub available: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct IntentSubject {
    pub repository: String,
    pub work_id: String,
    pub root_work_id: String,
    pub contract_digest: String,
    pub scope: IntentScope,
    pub source: SourceBindingV1,
    pub evidence: Vec<IntentEvidence>,
    pub skill: IntentSkillBinding,
    pub evaluator_profile_digest: String,
}

impl IntentSubject {
    pub fn candidate_digest(&self) -> Result<String> {
        identity("candidate", &self.source)
    }

    pub fn digest(&self) -> Result<String> {
        self.validate()?;
        identity("subject", self)
    }

    fn validate(&self) -> Result<()> {
        text(&self.repository, 256)?;
        id(&self.work_id)?;
        id(&self.root_work_id)?;
        digest(&self.contract_digest)?;
        digest(&self.evaluator_profile_digest)?;
        if self.source.repository() != self.repository {
            return invalid("intent candidate repository differs from Work");
        }
        match &self.source {
            SourceBindingV1::Git {
                head,
                tree,
                base,
                worktree_id,
                captured_snapshot,
                omissions,
                ..
            } => {
                id(worktree_id)?;
                for oid in [head, tree, base] {
                    if ![40, 64].contains(&oid.len())
                        || !oid
                            .bytes()
                            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
                    {
                        return invalid("invalid Git source object identity");
                    }
                }
                if let Some(snapshot) = captured_snapshot {
                    digest(&snapshot.digest)?;
                    if snapshot.size_bytes > 32 * 1024 * 1024 {
                        return invalid("captured candidate manifest exceeds limit");
                    }
                }
                if omissions.len() > 8 {
                    return invalid("too many candidate omissions");
                }
            }
            SourceBindingV1::Native { .. } => {
                return invalid("intent host adapter requires a Git source binding");
            }
        }
        let skill = &self.skill;
        for value in [
            &skill.workflow_version_id,
            &skill.step_id,
            &skill.skill_version_id,
        ] {
            id(value)?;
        }
        digest(&skill.skill_sha256)?;
        digest(&skill.package_digest)?;
        crate::store::validate_path(&skill.entrypoint)?;
        if let Some(parent) = &self.scope.parent_work_id {
            id(parent)?;
        }
        if let Some(checkpoint) = &self.scope.checkpoint {
            text(checkpoint, 256)?;
        }
        unique_ids(&self.scope.criteria, 64)?;
        texts(&self.scope.gaps)?;
        if self.scope.source_exclusions.len() > 8 {
            return invalid("too many scoped source exclusions");
        }
        for exclusion in &self.scope.source_exclusions {
            text(&exclusion.reason, 2_048)?;
            id(&exclusion.authorization_reference)?;
            if !matches!(
                exclusion.omission,
                CaptureOmissionV1::IgnoredContent | CaptureOmissionV1::IndexState
            ) {
                return invalid(
                    "missing source content cannot be dismissed as a routine scope exclusion",
                );
            }
        }
        if self.scope.coverage == CoverageV1::Complete && !self.scope.gaps.is_empty() {
            return invalid("complete scope cannot retain unresolved gaps");
        }
        if self.evidence.len() > 128 {
            return invalid("too much intent evidence");
        }
        let mut ids = BTreeSet::new();
        for evidence in &self.evidence {
            id(&evidence.id)?;
            digest(&evidence.sha256)?;
            text(&evidence.origin, 2_048)?;
            if let Some(candidate) = &evidence.candidate_digest {
                digest(candidate)?;
            }
            if !ids.insert(&evidence.id) {
                return invalid("duplicate evidence identity");
            }
        }
        Ok(())
    }

    fn same_binding(&self, other: &Self) -> bool {
        self.repository == other.repository
            && self.work_id == other.work_id
            && self.root_work_id == other.root_work_id
            && self.contract_digest == other.contract_digest
            && self.scope == other.scope
            && self.skill == other.skill
            && self.evaluator_profile_digest == other.evaluator_profile_digest
    }
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum IntentFindingStatus {
    Supported,
    Mismatch,
    Unknown,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct IntentFinding {
    pub criterion: String,
    pub status: IntentFindingStatus,
    pub explanation: String,
    pub evidence_ids: Vec<String>,
    pub corrective_outcome: Option<String>,
    pub recheck: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct IntentAssessment {
    pub criteria: Vec<IntentFinding>,
    pub coverage: CoverageV1,
    pub gaps: Vec<String>,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum IntentOutcome {
    MeetsRecordedCriteria,
    NeedsCorrection,
    NeedsEvidence,
    OperationalError,
    Stale,
    Interrupted,
    Disputed,
    Exception,
}

impl IntentAssessment {
    pub fn outcome(
        &self,
        contract: &IntentContract,
        subject: &IntentSubject,
    ) -> Result<IntentOutcome> {
        contract.validate()?;
        subject.validate()?;
        if contract.digest()? != subject.contract_digest {
            return invalid("assessment contract differs from subject");
        }
        validate_scope(contract, subject)?;
        texts(&self.gaps)?;
        if self.criteria.len() != subject.scope.criteria.len() {
            return invalid("assessment must cover every scoped criterion exactly once");
        }
        let mut seen = BTreeSet::new();
        let candidate = subject.candidate_digest()?;
        let mut mismatch = false;
        let mut unknown = self.coverage != CoverageV1::Complete
            || !self.gaps.is_empty()
            || contract.coverage != CoverageV1::Complete
            || subject.scope.coverage != CoverageV1::Complete;
        let source_complete = match &subject.source {
            SourceBindingV1::Git {
                dirty,
                untracked_count,
                captured_snapshot,
                snapshot_coverage,
                omissions,
                ..
            } => {
                let retained = captured_snapshot
                    .as_ref()
                    .is_some_and(|snapshot| snapshot.availability == ContentAvailabilityV1::Local);
                let omissions_resolved = omissions.iter().all(|omission| {
                    subject
                        .scope
                        .source_exclusions
                        .iter()
                        .any(|exclusion| exclusion.omission == *omission)
                });
                ((!dirty && *untracked_count == 0 && captured_snapshot.is_none()) || retained)
                    && omissions_resolved
                    && (*snapshot_coverage == CoverageV1::Complete
                        || (*snapshot_coverage == CoverageV1::Partial
                            && retained
                            && !omissions.is_empty()))
            }
            _ => false,
        };
        unknown |= !source_complete;
        for row in &self.criteria {
            if !subject.scope.criteria.contains(&row.criterion) || !seen.insert(&row.criterion) {
                return invalid("unknown or duplicate assessment criterion");
            }
            text(&row.explanation, 2_048)?;
            if row.evidence_ids.len() > 16 {
                return invalid("too many finding citations");
            }
            let mut cited = BTreeSet::new();
            let mut applicable = false;
            for citation in &row.evidence_ids {
                let evidence = subject
                    .evidence
                    .iter()
                    .find(|evidence| &evidence.id == citation)
                    .ok_or_else(|| Error::Invalid("finding cites unrecorded evidence".into()))?;
                if !cited.insert(citation) {
                    return invalid("duplicate finding citation");
                }
                if evidence.available
                    && evidence.coverage == CoverageV1::Complete
                    && evidence.candidate_digest.as_deref() == Some(candidate.as_str())
                {
                    applicable = true;
                } else if row.status != IntentFindingStatus::Unknown {
                    return invalid(
                        "conclusive finding cites unavailable, incomplete or stale evidence",
                    );
                }
            }
            match row.status {
                IntentFindingStatus::Supported | IntentFindingStatus::Mismatch if !applicable => {
                    return invalid("conclusive intent finding needs current available evidence");
                }
                IntentFindingStatus::Mismatch => {
                    text(row.corrective_outcome.as_deref().unwrap_or(""), 2_048)?;
                    text(row.recheck.as_deref().unwrap_or(""), 2_048)?;
                    mismatch = true;
                }
                IntentFindingStatus::Unknown => unknown = true,
                _ => {}
            }
        }
        Ok(if mismatch {
            IntentOutcome::NeedsCorrection
        } else if unknown {
            IntentOutcome::NeedsEvidence
        } else {
            IntentOutcome::MeetsRecordedCriteria
        })
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct IntentLimits {
    pub max_correction_rounds: u32,
    pub max_invocations: u32,
    pub wall_time_ms: u64,
}

impl IntentLimits {
    fn validate(&self) -> Result<()> {
        if self.max_correction_rounds > 2
            || !(1..=64).contains(&self.max_invocations)
            || !(1..=24 * 60 * 60 * 1_000).contains(&self.wall_time_ms)
        {
            return invalid(
                "invalid intent budget; at most two automatic correction rounds are supported",
            );
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum IntentPhase {
    Assessment,
    Correction,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum IntentExecution {
    Succeeded,
    Failed,
    Cancelled,
    Uncertain,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum IntentAttemptState {
    Leased,
    Finished,
    Cancelled,
    Recovered,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct IntentAttempt {
    pub attempt_id: String,
    pub fence: u64,
    pub session_id: String,
    pub phase: IntentPhase,
    pub subject: IntentSubject,
    pub started_at_ms: u64,
    pub lease_expires_at_ms: u64,
    pub state: IntentAttemptState,
    pub requested_model: Option<String>,
    pub observed_model: Option<String>,
    pub execution: Option<IntentExecution>,
    pub assessment: Option<IntentAssessment>,
    pub resulting_candidate_digest: Option<String>,
    pub detail: Option<String>,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum IntentDispositionKind {
    Dispute,
    Exception,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct IntentDisposition {
    pub kind: IntentDispositionKind,
    pub criteria: Vec<String>,
    pub reason: String,
    pub authorization: IntentReference,
    pub subject_digest: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct IntentRecord {
    pub subject: IntentSubject,
    pub contracts: Vec<IntentContract>,
    pub limits: IntentLimits,
    pub started_at_ms: u64,
    pub last_observed_at_ms: u64,
    pub invocations: u32,
    pub correction_rounds: u32,
    pub cancelled: bool,
    /// Root-wide dispatch ownership includes descendants and survives host restart.
    pub active_work_id: Option<String>,
    pub attempts: Vec<IntentAttempt>,
    pub dispositions: Vec<IntentDisposition>,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum IntentNext {
    Assess,
    Correct,
    Investigate,
    InProgress,
    Complete,
    HumanDisposition,
    RecoveryRequired,
    BudgetExhausted,
    Cancelled,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct IntentView {
    pub record: IntentRecord,
    pub current: bool,
    pub current_subject_digest: String,
    pub outcome: Option<IntentOutcome>,
    pub next: IntentNext,
    pub root_invocations: u32,
    pub root_correction_rounds: u32,
    pub admission: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct CreateIntent {
    pub request_id: String,
    pub contract: IntentContract,
    pub subject: IntentSubject,
    pub limits: IntentLimits,
    pub now_ms: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct IntentClaim {
    pub request_id: String,
    pub subject: IntentSubject,
    pub attempt_id: String,
    pub session_id: String,
    pub phase: IntentPhase,
    pub now_ms: u64,
    pub lease_ms: u64,
    pub requested_model: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct IntentReport {
    pub request_id: String,
    pub subject: IntentSubject,
    pub attempt_id: String,
    pub session_id: String,
    pub fence: u64,
    pub now_ms: u64,
    pub execution: IntentExecution,
    pub assessment: Option<IntentAssessment>,
    pub observed_model: Option<String>,
    pub detail: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct IntentHeartbeat {
    pub request_id: String,
    pub subject: IntentSubject,
    pub attempt_id: String,
    pub session_id: String,
    pub fence: u64,
    pub now_ms: u64,
    pub lease_ms: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct IntentControl {
    pub request_id: String,
    pub subject: IntentSubject,
    pub session_id: String,
    pub now_ms: u64,
    pub reason: String,
    /// Recovery is an explicit human-authorized operation, never a timeout retry.
    pub authorization: Option<IntentReference>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct IntentAmendment {
    pub request_id: String,
    pub subject: IntentSubject,
    pub contract: IntentContract,
    pub now_ms: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct IntentDispositionRequest {
    pub request_id: String,
    pub subject: IntentSubject,
    pub disposition: IntentDisposition,
    pub now_ms: u64,
}

pub struct IntentService<'a> {
    data_dir: &'a Path,
}

impl<'a> IntentService<'a> {
    pub fn new(data_dir: &'a Path) -> Self {
        Self { data_dir }
    }

    pub fn create(&self, request: &CreateIntent) -> Result<IntentView> {
        request.subject.validate()?;
        request.contract.validate()?;
        request.limits.validate()?;
        validate_scope(&request.contract, &request.subject)?;
        if request.contract.digest()? != request.subject.contract_digest {
            return invalid("created contract differs from subject");
        }
        let mut journal = Journal::open(self.data_dir, OpenMode::Create)?;
        let tx = journal
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        if retry(
            &tx,
            &request.subject,
            &request.request_id,
            "create",
            request,
        )?
        .is_some()
        {
            return current_projection(&tx, &request.subject, request.now_ms);
        }
        if load_optional(&tx, &request.subject.repository, &request.subject.work_id)?.is_some() {
            return conflict("intent Work already exists");
        }
        let inventory: u64 =
            tx.query_row("SELECT count(*) FROM workflow_intent_work", [], |row| {
                row.get(0)
            })?;
        if inventory >= 10_000 {
            return Err(Error::Limit("intent Work inventory is full".into()));
        }
        let root = if request.subject.work_id != request.subject.root_work_id {
            let root = load(
                &tx,
                &request.subject.repository,
                &request.subject.root_work_id,
            )?;
            if root.subject.contract_digest != request.subject.contract_digest
                || root.limits != request.limits
            {
                return conflict("child must inherit root intent and budget");
            }
            let parent_id = request
                .subject
                .scope
                .parent_work_id
                .as_deref()
                .ok_or_else(|| Error::Invalid("child requires its parent Work".into()))?;
            let parent = load(&tx, &request.subject.repository, parent_id)?;
            if parent.subject.root_work_id != request.subject.root_work_id
                || request
                    .subject
                    .scope
                    .criteria
                    .iter()
                    .any(|criterion| !parent.subject.scope.criteria.contains(criterion))
            {
                return conflict("child scope differs from its parent assignment");
            }
            Some(root)
        } else {
            if request.subject.scope.parent_work_id.is_some() {
                return invalid("root Work cannot have a parent");
            }
            None
        };
        let record = IntentRecord {
            subject: request.subject.clone(),
            contracts: vec![request.contract.clone()],
            limits: request.limits.clone(),
            started_at_ms: root
                .as_ref()
                .map_or(request.now_ms, |root| root.started_at_ms),
            last_observed_at_ms: request.now_ms,
            invocations: 0,
            correction_rounds: 0,
            cancelled: false,
            active_work_id: None,
            attempts: Vec::new(),
            dispositions: Vec::new(),
        };
        save(&tx, &record)?;
        let root = root.as_ref().unwrap_or(&record);
        let view = project(record.clone(), root, &request.subject, request.now_ms)?;
        remember(
            &tx,
            &request.subject,
            &request.request_id,
            "create",
            request,
            &view,
        )?;
        tx.commit()?;
        Ok(view)
    }

    /// Read-only. The adapter must capture current bytes again; no HEAD-only fallback is used.
    pub fn status(&self, subject: &IntentSubject, now_ms: u64) -> Result<IntentView> {
        subject.validate()?;
        let journal = Journal::open(self.data_dir, OpenMode::Read)?;
        let record = load(&journal.connection, &subject.repository, &subject.work_id)?;
        let root = load(
            &journal.connection,
            &subject.repository,
            &record.subject.root_work_id,
        )?;
        project(record, &root, subject, now_ms)
    }

    /// Retained local state only. This does not claim source or evidence is still available.
    pub fn inspect(&self, repository: &str, work_id: &str) -> Result<IntentRecord> {
        text(repository, 256)?;
        id(work_id)?;
        let journal = Journal::open(self.data_dir, OpenMode::Read)?;
        load(&journal.connection, repository, work_id)
    }

    pub fn next(&self, subject: &IntentSubject, now_ms: u64) -> Result<IntentView> {
        self.status(subject, now_ms)
    }

    pub fn claim(&self, request: &IntentClaim) -> Result<IntentView> {
        id(&request.attempt_id)?;
        id(&request.session_id)?;
        if !(1..=MAX_LEASE_MS).contains(&request.lease_ms) {
            return invalid("invalid intent attempt lease");
        }
        optional_text(&request.requested_model, 256)?;
        self.mutate(
            &request.subject,
            &request.request_id,
            "claim",
            request,
            request.now_ms,
            |record, root| {
                require_current_binding(record, root, &request.subject)?;
                if root.active_work_id.is_some() {
                    return conflict("root Work already has an unresolved dispatch");
                }
                let view = project(record.clone(), root, &request.subject, request.now_ms)?;
                let permitted = match request.phase {
                    IntentPhase::Assessment => {
                        matches!(view.next, IntentNext::Assess | IntentNext::Investigate)
                    }
                    IntentPhase::Correction => view.next == IntentNext::Correct,
                };
                if !permitted {
                    return conflict("intent progression does not permit this dispatch");
                }
                if request.phase == IntentPhase::Correction
                    && root.invocations.saturating_add(2) > root.limits.max_invocations
                {
                    return conflict(
                        "correction budget must retain an invocation for reassessment",
                    );
                }
                if record.attempts.len() >= MAX_ATTEMPTS
                    || record
                        .attempts
                        .iter()
                        .any(|attempt| attempt.attempt_id == request.attempt_id)
                {
                    return conflict(
                        "intent attempt inventory is full or identity was already used",
                    );
                }
                if request.phase == IntentPhase::Assessment
                    && view.next == IntentNext::Investigate
                    && record.subject == request.subject
                {
                    return conflict(
                        "unknown assessment requires new evidence or an explicit human disposition",
                    );
                }
                root.invocations += 1;
                root.active_work_id = Some(record.subject.work_id.clone());
                if request.phase == IntentPhase::Correction {
                    root.correction_rounds += 1;
                }
                record.subject = request.subject.clone();
                let fence = record
                    .attempts
                    .last()
                    .map_or(1, |attempt| attempt.fence + 1);
                let expiry = request
                    .now_ms
                    .checked_add(request.lease_ms)
                    .ok_or_else(|| Error::Limit("intent lease overflow".into()))?
                    .min(deadline(root)?);
                record.attempts.push(IntentAttempt {
                    attempt_id: request.attempt_id.clone(),
                    fence,
                    session_id: request.session_id.clone(),
                    phase: request.phase,
                    subject: request.subject.clone(),
                    started_at_ms: request.now_ms,
                    lease_expires_at_ms: expiry,
                    state: IntentAttemptState::Leased,
                    requested_model: request.requested_model.clone(),
                    observed_model: None,
                    execution: None,
                    assessment: None,
                    resulting_candidate_digest: None,
                    detail: None,
                });
                Ok(())
            },
        )
    }

    pub fn report(&self, request: &IntentReport) -> Result<IntentView> {
        optional_text(&request.observed_model, 256)?;
        optional_text(&request.detail, 2_048)?;
        self.mutate(
            &request.subject,
            &request.request_id,
            "report",
            request,
            request.now_ms,
            |record, root| {
                require_current_binding(record, root, &request.subject)?;
                if root.cancelled || record.cancelled {
                    return conflict("intent Work was cancelled");
                }
                if root.active_work_id.as_deref() != Some(record.subject.work_id.as_str()) {
                    return conflict("root dispatch ownership changed");
                }
                let contract = record.contracts.last().unwrap();
                let attempt = record
                    .attempts
                    .last_mut()
                    .ok_or_else(|| Error::Conflict("no intent attempt was reserved".into()))?;
                if attempt.state != IntentAttemptState::Leased
                    || attempt.attempt_id != request.attempt_id
                    || attempt.session_id != request.session_id
                    || attempt.fence != request.fence
                    || request.now_ms >= attempt.lease_expires_at_ms
                {
                    return conflict("intent result does not bind the current live attempt");
                }
                if request.execution == IntentExecution::Succeeded {
                    match attempt.phase {
                        IntentPhase::Assessment => {
                            if attempt.subject.source != request.subject.source {
                                return conflict(
                                    "source changed during assessment; recover and reassess",
                                );
                            }
                            for evidence in &attempt.subject.evidence {
                                let preserved = request.subject.evidence.iter().any(|current| {
                                    let mut prior = evidence.clone();
                                    prior.available = current.available;
                                    prior == *current
                                });
                                if !preserved {
                                    return conflict(
                                        "reserved evidence was replaced during assessment",
                                    );
                                }
                            }
                            request
                                .assessment
                                .as_ref()
                                .ok_or_else(|| {
                                    Error::Invalid(
                                        "successful assessment needs criterion findings".into(),
                                    )
                                })?
                                .outcome(contract, &request.subject)?;
                        }
                        IntentPhase::Correction => {
                            if request.assessment.is_some() {
                                return invalid(
                                    "a correction must return source before a fresh assessment",
                                );
                            }
                            if attempt.subject.candidate_digest()?
                                == request.subject.candidate_digest()?
                            {
                                return conflict(
                                    "correction reported success without a changed candidate",
                                );
                            }
                        }
                    }
                } else if request.assessment.is_some() {
                    return invalid(
                        "failed or interrupted execution cannot submit semantic findings",
                    );
                }
                attempt.state = IntentAttemptState::Finished;
                attempt.observed_model = request.observed_model.clone();
                attempt.execution = Some(request.execution);
                attempt.assessment = request.assessment.clone();
                attempt.resulting_candidate_digest = Some(request.subject.candidate_digest()?);
                attempt.detail = request.detail.clone();
                // Assessments cite the evidence actually available when the report was made.
                if attempt.phase == IntentPhase::Assessment {
                    attempt.subject = request.subject.clone();
                }
                record.subject = request.subject.clone();
                if request.execution == IntentExecution::Succeeded {
                    root.active_work_id = None;
                }
                Ok(())
            },
        )
    }

    pub fn heartbeat(&self, request: &IntentHeartbeat) -> Result<IntentView> {
        if !(1..=MAX_LEASE_MS).contains(&request.lease_ms) {
            return invalid("invalid intent attempt lease");
        }
        self.mutate(
            &request.subject,
            &request.request_id,
            "heartbeat",
            request,
            request.now_ms,
            |record, root| {
                require_current_binding(record, root, &request.subject)?;
                if root.cancelled
                    || record.cancelled
                    || root.active_work_id.as_deref() != Some(record.subject.work_id.as_str())
                {
                    return conflict("intent dispatch is cancelled or no longer current");
                }
                let attempt = record
                    .attempts
                    .last_mut()
                    .ok_or_else(|| Error::Conflict("no current intent attempt".into()))?;
                if attempt.state != IntentAttemptState::Leased
                    || attempt.attempt_id != request.attempt_id
                    || attempt.fence != request.fence
                    || attempt.session_id != request.session_id
                    || request.now_ms >= attempt.lease_expires_at_ms
                {
                    return conflict("heartbeat does not bind a live intent attempt");
                }
                if attempt.phase == IntentPhase::Assessment
                    && attempt.subject.source != request.subject.source
                {
                    return conflict("candidate changed during assessment");
                }
                attempt.lease_expires_at_ms = request
                    .now_ms
                    .checked_add(request.lease_ms)
                    .ok_or_else(|| Error::Limit("intent lease overflow".into()))?
                    .min(deadline(root)?);
                Ok(())
            },
        )
    }

    pub fn cancel(&self, request: &IntentControl) -> Result<IntentView> {
        self.control(request, false)
    }

    pub fn recover(&self, request: &IntentControl) -> Result<IntentView> {
        self.control(request, true)
    }

    fn control(&self, request: &IntentControl, recovery: bool) -> Result<IntentView> {
        text(&request.reason, 2_048)?;
        id(&request.session_id)?;
        if recovery {
            require_human(request.authorization.as_ref())?;
        }
        self.mutate(
            &request.subject,
            &request.request_id,
            if recovery { "recover" } else { "cancel" },
            request,
            request.now_ms,
            |record, root| {
                require_current_binding(record, root, &request.subject)?;
                if recovery {
                    if root
                        .active_work_id
                        .as_ref()
                        .is_some_and(|work| work != &record.subject.work_id)
                    {
                        return conflict("recover the unresolved descendant dispatch first");
                    }
                    if let Some(attempt) = record.attempts.last_mut() {
                        if attempt.state == IntentAttemptState::Leased
                            && request.now_ms < attempt.lease_expires_at_ms
                            && !record.cancelled
                        {
                            return conflict(
                                "live intent attempt must be cancelled before recovery",
                            );
                        }
                        attempt.state = IntentAttemptState::Recovered;
                    }
                    record.cancelled = false;
                    root.active_work_id = None;
                    if record.subject.work_id == record.subject.root_work_id {
                        root.cancelled = false;
                    }
                    record.subject = request.subject.clone();
                } else {
                    record.cancelled = true;
                    if record.subject.work_id == record.subject.root_work_id {
                        root.cancelled = true;
                    }
                    if let Some(attempt) = record
                        .attempts
                        .last_mut()
                        .filter(|attempt| attempt.state == IntentAttemptState::Leased)
                    {
                        attempt.state = IntentAttemptState::Cancelled;
                        attempt.execution = Some(IntentExecution::Cancelled);
                    }
                    // Cancelled delivery remains unresolved until explicit recovery.
                }
                // Recovery authorization and reason remain inspectable in the retained operation.
                Ok(())
            },
        )
    }

    pub fn amend(&self, request: &IntentAmendment) -> Result<IntentView> {
        request.contract.validate()?;
        self.mutate(&request.subject, &request.request_id, "amend", request, request.now_ms, |record, root| {
            require_current_binding(record, root, &request.subject)?;
            if record.subject.work_id != record.subject.root_work_id { return invalid("only root Work can amend inherited intent"); }
            if root.active_work_id.is_some() { return conflict("resolve the root dispatch before amending intent"); }
            if record.attempts.last().is_some_and(|attempt| attempt.state == IntentAttemptState::Leased) {
                return conflict("cancel or recover the current attempt before amending intent");
            }
            let prior = record.contracts.last().unwrap();
            if request.contract.version != prior.version + 1 || request.contract.previous_digest.as_deref() != Some(prior.digest()?.as_str())
                || request.contract.request != prior.request
                || request.contract.amendments.len() <= prior.amendments.len()
                || !request.contract.amendments.starts_with(&prior.amendments) {
                return invalid("amendment must retain original request and cite a new human-authorized version");
            }
            record.contracts.push(request.contract.clone());
            record.subject = request.subject.clone();
            record.subject.contract_digest = request.contract.digest()?;
            root.subject.contract_digest = record.subject.contract_digest.clone();
            if record.subject.scope.parent_work_id.is_none() && record.subject.scope.checkpoint.is_none() {
                record.subject.scope.criteria = request.contract.criteria.iter().map(|criterion| criterion.id.clone()).collect();
            }
            validate_scope(&request.contract, &record.subject)?;
            Ok(())
        })
    }

    pub fn disposition(&self, request: &IntentDispositionRequest) -> Result<IntentView> {
        require_human(Some(&request.disposition.authorization))?;
        text(&request.disposition.reason, 2_048)?;
        unique_ids(&request.disposition.criteria, 64)?;
        if request.disposition.subject_digest != request.subject.digest()? {
            return invalid("human disposition is for another subject");
        }
        self.mutate(
            &request.subject,
            &request.request_id,
            "disposition",
            request,
            request.now_ms,
            |record, root| {
                require_current_binding(record, root, &request.subject)?;
                if record.subject != request.subject {
                    return conflict("human disposition requires the current assessed subject");
                }
                if request
                    .disposition
                    .criteria
                    .iter()
                    .any(|criterion| !record.subject.scope.criteria.contains(criterion))
                {
                    return invalid("human disposition cites an unknown scoped criterion");
                }
                if record.dispositions.len() >= 64 {
                    return invalid("too many human dispositions");
                }
                record.dispositions.push(request.disposition.clone());
                Ok(())
            },
        )
    }

    fn mutate<T: Serialize>(
        &self,
        subject: &IntentSubject,
        request_id: &str,
        kind: &str,
        request: &T,
        now: u64,
        change: impl FnOnce(&mut IntentRecord, &mut IntentRecord) -> Result<()>,
    ) -> Result<IntentView> {
        subject.validate()?;
        let mut journal = Journal::open(self.data_dir, OpenMode::Write)?;
        let tx = journal
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        if retry(&tx, subject, request_id, kind, request)?.is_some() {
            return current_projection(&tx, subject, now);
        }
        let mut record = load(&tx, &subject.repository, &subject.work_id)?;
        let mut root = load(&tx, &subject.repository, &record.subject.root_work_id)?;
        if now < root.last_observed_at_ms {
            return conflict("intent observation clock moved backwards");
        }
        change(&mut record, &mut root)?;
        root.last_observed_at_ms = now;
        if record.subject.work_id == record.subject.root_work_id {
            record.invocations = root.invocations;
            record.correction_rounds = root.correction_rounds;
            record.cancelled = root.cancelled;
            record.active_work_id = root.active_work_id.clone();
            record.last_observed_at_ms = root.last_observed_at_ms;
        } else {
            save(&tx, &root)?;
        }
        save(&tx, &record)?;
        let effective_root = if record.subject.work_id == record.subject.root_work_id {
            &record
        } else {
            &root
        };
        let current_subject = if kind == "amend" {
            &record.subject
        } else {
            subject
        };
        let view = project(record.clone(), effective_root, current_subject, now)?;
        remember(&tx, subject, request_id, kind, request, &view)?;
        tx.commit()?;
        Ok(view)
    }
}

fn validate_scope(contract: &IntentContract, subject: &IntentSubject) -> Result<()> {
    for exclusion in &subject.scope.source_exclusions {
        if !contract
            .request
            .iter()
            .chain(&contract.amendments)
            .any(|reference| {
                reference.id == exclusion.authorization_reference
                    && reference.human()
                    && !reference.reconstructed
                    && reference.coverage == CoverageV1::Complete
            })
        {
            return invalid("source exclusion lacks an available human scope reference");
        }
    }
    for criterion in &subject.scope.criteria {
        if !contract.criteria.iter().any(|row| &row.id == criterion) {
            return invalid("scope cites an unknown criterion");
        }
    }
    for criterion in &contract.criteria {
        if (criterion.constraint
            || (criterion.required
                && subject.scope.parent_work_id.is_none()
                && subject.scope.checkpoint.is_none()))
            && !subject.scope.criteria.contains(&criterion.id)
        {
            return invalid("scope omitted a required final outcome or inherited constraint");
        }
    }
    Ok(())
}

fn require_current_binding(
    record: &IntentRecord,
    root: &IntentRecord,
    current: &IntentSubject,
) -> Result<()> {
    if !record.subject.same_binding(current)
        || root.subject.contract_digest != current.contract_digest
    {
        return conflict("intent contract, scope, skill or evaluator binding is stale");
    }
    Ok(())
}

fn deadline(root: &IntentRecord) -> Result<u64> {
    root.started_at_ms
        .checked_add(root.limits.wall_time_ms)
        .ok_or_else(|| Error::Limit("intent wall-time deadline overflow".into()))
}

fn project(
    record: IntentRecord,
    root: &IntentRecord,
    current: &IntentSubject,
    now: u64,
) -> Result<IntentView> {
    let applicable =
        record.subject == *current && root.subject.contract_digest == current.contract_digest;
    let mut outcome = None;
    let mut next = IntentNext::Assess;
    if root.cancelled || record.cancelled {
        outcome = Some(IntentOutcome::Interrupted);
        next = IntentNext::Cancelled;
    } else if let Some(attempt) = record.attempts.last() {
        match attempt.state {
            IntentAttemptState::Leased => {
                let current_dispatch = applicable
                    || (attempt.phase == IntentPhase::Correction
                        && record.subject.same_binding(current)
                        && root.subject.contract_digest == current.contract_digest);
                next = if now < attempt.lease_expires_at_ms && current_dispatch {
                    IntentNext::InProgress
                } else {
                    IntentNext::RecoveryRequired
                };
                if next == IntentNext::RecoveryRequired {
                    outcome = Some(IntentOutcome::Interrupted);
                }
            }
            IntentAttemptState::Cancelled => {
                outcome = Some(IntentOutcome::Interrupted);
                next = IntentNext::RecoveryRequired;
            }
            IntentAttemptState::Recovered => {}
            IntentAttemptState::Finished => match attempt.execution {
                Some(IntentExecution::Succeeded)
                    if attempt.phase == IntentPhase::Assessment
                        && applicable
                        && attempt.subject == *current =>
                {
                    let contract = record
                        .contracts
                        .last()
                        .ok_or_else(|| Error::Invalid("intent contract history is empty".into()))?;
                    let result = attempt
                        .assessment
                        .as_ref()
                        .ok_or_else(|| Error::Invalid("assessment record lacks findings".into()))?
                        .outcome(contract, &attempt.subject)?;
                    outcome = Some(result);
                    next = match result {
                        IntentOutcome::MeetsRecordedCriteria => IntentNext::Complete,
                        IntentOutcome::NeedsCorrection
                            if root.correction_rounds < root.limits.max_correction_rounds =>
                        {
                            IntentNext::Correct
                        }
                        IntentOutcome::NeedsCorrection => IntentNext::BudgetExhausted,
                        _ => IntentNext::Investigate,
                    };
                }
                Some(IntentExecution::Succeeded) if attempt.phase == IntentPhase::Assessment => {
                    outcome = Some(IntentOutcome::Stale);
                }
                Some(IntentExecution::Succeeded) => {}
                _ => {
                    outcome = Some(IntentOutcome::OperationalError);
                    next = IntentNext::RecoveryRequired;
                }
            },
        }
    }
    if !applicable && outcome.is_none() {
        outcome = Some(IntentOutcome::Stale);
    }
    if applicable
        && let Some(disposition) = record
            .dispositions
            .iter()
            .rev()
            .find(|row| row.subject_digest == identity("subject", current).unwrap_or_default())
    {
        outcome = Some(match disposition.kind {
            IntentDispositionKind::Dispute => IntentOutcome::Disputed,
            IntentDispositionKind::Exception => IntentOutcome::Exception,
        });
        next = IntentNext::HumanDisposition;
    }
    if matches!(
        next,
        IntentNext::Assess | IntentNext::Correct | IntentNext::Investigate
    ) && (root.invocations >= root.limits.max_invocations
        || now.saturating_sub(root.started_at_ms) >= root.limits.wall_time_ms)
    {
        next = IntentNext::BudgetExhausted;
    }
    if next == IntentNext::Correct
        && root.invocations.saturating_add(2) > root.limits.max_invocations
    {
        next = IntentNext::BudgetExhausted;
    }
    if !root.cancelled
        && !record.cancelled
        && root
            .active_work_id
            .as_ref()
            .is_some_and(|work| work != &record.subject.work_id)
    {
        next = IntentNext::InProgress;
    }
    if next == IntentNext::Correct {
        let assessments: Vec<_> = record
            .attempts
            .iter()
            .filter(|attempt| {
                attempt.subject.contract_digest == current.contract_digest
                    && attempt.subject.scope == current.scope
            })
            .filter_map(|attempt| attempt.assessment.as_ref())
            .collect();
        if assessments.len() >= 2 {
            let signature = |assessment: &IntentAssessment| {
                assessment
                    .criteria
                    .iter()
                    .filter(|row| row.status == IntentFindingStatus::Mismatch)
                    .map(|row| {
                        (
                            row.criterion.clone(),
                            row.explanation.clone(),
                            row.corrective_outcome.clone(),
                            row.recheck.clone(),
                        )
                    })
                    .collect::<Vec<_>>()
            };
            if signature(assessments[assessments.len() - 1])
                == signature(assessments[assessments.len() - 2])
            {
                next = IntentNext::RecoveryRequired;
            }
        }
    }
    if next == IntentNext::Assess
        && let Some(last) = record.attempts.last().filter(|attempt| {
            attempt.phase == IntentPhase::Correction
                && attempt.execution == Some(IntentExecution::Succeeded)
        })
        && record
            .attempts
            .iter()
            .take(record.attempts.len() - 1)
            .any(|attempt| {
                attempt.subject.candidate_digest().ok() == last.resulting_candidate_digest
            })
    {
        next = IntentNext::RecoveryRequired;
    }
    Ok(IntentView {
        record,
        current: applicable,
        current_subject_digest: current.digest()?,
        outcome,
        next,
        root_invocations: root.invocations,
        root_correction_rounds: root.correction_rounds,
        admission: "nonqualifying_agent_report".into(),
    })
}

pub(super) fn initialize_schema(tx: &Transaction<'_>) -> Result<()> {
    tx.execute_batch(
        "CREATE TABLE IF NOT EXISTS workflow_intent_work(
        repository TEXT NOT NULL,work_id TEXT NOT NULL,root_work_id TEXT NOT NULL,
        record_json BLOB NOT NULL CHECK(length(record_json)<=524288),PRIMARY KEY(repository,work_id)
    ) STRICT;
    CREATE TABLE IF NOT EXISTS workflow_intent_operations(
        repository TEXT NOT NULL,work_id TEXT NOT NULL,request_id TEXT NOT NULL,
        request_digest TEXT NOT NULL CHECK(length(request_digest)=64),
        request_json BLOB NOT NULL CHECK(length(request_json)<=524288),
        outcome_json BLOB NOT NULL CHECK(length(outcome_json)<=524288),
        PRIMARY KEY(repository,work_id,request_id)
    ) STRICT;",
    )?;
    Ok(())
}

pub(super) fn validate_schema(connection: &Connection) -> Result<()> {
    connection.prepare(
        "SELECT repository,work_id,root_work_id,record_json FROM workflow_intent_work LIMIT 0",
    )?;
    connection.prepare("SELECT repository,work_id,request_id,request_digest,request_json,outcome_json FROM workflow_intent_operations LIMIT 0")?;
    Ok(())
}

fn load_optional(
    connection: &Connection,
    repository: &str,
    work: &str,
) -> Result<Option<IntentRecord>> {
    let version: i64 = connection.pragma_query_value(None, "user_version", |row| row.get(0))?;
    if version < 6 {
        return Ok(None);
    }
    let bytes: Option<Vec<u8>> = connection
        .query_row(
            "SELECT record_json FROM workflow_intent_work WHERE repository=?1 AND work_id=?2",
            params![repository, work],
            |row| row.get(0),
        )
        .optional()?;
    bytes
        .map(|bytes| {
            if bytes.len() > MAX_RECORD {
                return invalid("intent record exceeds limit");
            }
            let record: IntentRecord = serde_json::from_slice(&bytes)?;
            if record.subject.repository != repository
                || record.subject.work_id != work
                || record.contracts.is_empty()
            {
                return invalid("retained intent identity is inconsistent");
            }
            record.subject.validate()?;
            Ok(record)
        })
        .transpose()
}

fn load(connection: &Connection, repository: &str, work: &str) -> Result<IntentRecord> {
    load_optional(connection, repository, work)?
        .ok_or_else(|| Error::NotFound("intent Work".into()))
}

fn save(tx: &Transaction<'_>, record: &IntentRecord) -> Result<()> {
    tx.execute("INSERT INTO workflow_intent_work(repository,work_id,root_work_id,record_json) VALUES(?1,?2,?3,?4)
        ON CONFLICT(repository,work_id) DO UPDATE SET record_json=excluded.record_json", params![record.subject.repository,record.subject.work_id,record.subject.root_work_id,encode(record)?])?;
    Ok(())
}

fn retry<T: Serialize>(
    tx: &Transaction<'_>,
    subject: &IntentSubject,
    request_id: &str,
    kind: &str,
    request: &T,
) -> Result<Option<IntentView>> {
    id(request_id)?;
    let expected = request_identity(kind, request)?;
    let prior: Option<(String, Vec<u8>)> = tx.query_row("SELECT request_digest,outcome_json FROM workflow_intent_operations WHERE repository=?1 AND work_id=?2 AND request_id=?3", params![subject.repository,subject.work_id,request_id], |row| Ok((row.get(0)?,row.get(1)?))).optional()?;
    prior
        .map(|(digest, bytes)| {
            if digest != expected {
                return conflict("intent request identity was reused with different bytes");
            }
            if bytes.len() > MAX_RECORD {
                return invalid("intent operation exceeds limit");
            }
            Ok(serde_json::from_slice(&bytes)?)
        })
        .transpose()
}

fn remember<T: Serialize>(
    tx: &Transaction<'_>,
    subject: &IntentSubject,
    request_id: &str,
    kind: &str,
    request: &T,
    view: &IntentView,
) -> Result<()> {
    let operations: u64 = tx.query_row(
        "SELECT count(*) FROM workflow_intent_operations WHERE repository=?1 AND work_id=?2",
        params![subject.repository, subject.work_id],
        |row| row.get(0),
    )?;
    if operations >= 256 {
        return Err(Error::Limit("intent operation inventory is full".into()));
    }
    tx.execute(
        "INSERT INTO workflow_intent_operations VALUES(?1,?2,?3,?4,?5,?6)",
        params![
            subject.repository,
            subject.work_id,
            request_id,
            request_identity(kind, request)?,
            encode(request)?,
            encode(view)?
        ],
    )?;
    Ok(())
}

fn current_projection(
    connection: &Connection,
    subject: &IntentSubject,
    now: u64,
) -> Result<IntentView> {
    let record = load(connection, &subject.repository, &subject.work_id)?;
    let root = load(
        connection,
        &subject.repository,
        &record.subject.root_work_id,
    )?;
    project(record, &root, subject, now)
}

fn request_identity<T: Serialize>(kind: &str, request: &T) -> Result<String> {
    let mut value = serde_json::to_value(request)?;
    if let Some(object) = value.as_object_mut() {
        object.remove("now_ms");
    }
    identity(kind, &value)
}

fn identity<T: Serialize>(kind: &str, value: &T) -> Result<String> {
    use sha2::{Digest, Sha256};
    let bytes = encode(value)?;
    let mut hash = Sha256::new();
    hash.update(b"agentscm.intent.v1\0");
    hash.update(kind.as_bytes());
    hash.update([0]);
    hash.update((bytes.len() as u64).to_be_bytes());
    hash.update(bytes);
    Ok(format!("{:x}", hash.finalize()))
}

fn encode<T: Serialize>(value: &T) -> Result<Vec<u8>> {
    let bytes = serde_json::to_vec(value)?;
    if bytes.len() > MAX_RECORD {
        return Err(Error::Limit("intent record exceeds 512 KiB".into()));
    }
    Ok(bytes)
}

fn unique_ids(values: &[String], max: usize) -> Result<()> {
    if values.is_empty() || values.len() > max {
        return invalid("invalid intent identifier list length");
    }
    let mut seen = BTreeSet::new();
    for value in values {
        id(value)?;
        if !seen.insert(value) {
            return invalid("duplicate intent identifier");
        }
    }
    Ok(())
}

fn id(value: &str) -> Result<()> {
    if value.is_empty()
        || value.len() > 128
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.'))
    {
        return invalid("invalid opaque intent identifier");
    }
    Ok(())
}

fn text(value: &str, limit: usize) -> Result<()> {
    if value.trim().is_empty()
        || value.len() > limit
        || value
            .chars()
            .any(|ch| ch == '\0' || (ch.is_control() && ch != '\n' && ch != '\t'))
    {
        return invalid("invalid or oversized intent text");
    }
    Ok(())
}

fn optional_text(value: &Option<String>, max: usize) -> Result<()> {
    value.as_ref().map_or(Ok(()), |value| text(value, max))
}
fn texts(values: &[String]) -> Result<()> {
    if values.len() > 32 {
        return invalid("too many intent gaps");
    }
    for value in values {
        text(value, 2_048)?;
    }
    Ok(())
}
fn digest(value: &str) -> Result<()> {
    if value.len() != 64
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
    {
        return invalid("invalid intent SHA-256");
    }
    Ok(())
}
fn require_human(reference: Option<&IntentReference>) -> Result<()> {
    let reference = reference.ok_or_else(|| {
        Error::Invalid("explicit human authorization reference is required".into())
    })?;
    reference.validate()?;
    if !reference.human() || reference.reconstructed || reference.coverage != CoverageV1::Complete {
        return invalid("human authorization must be available and complete");
    }
    Ok(())
}
fn invalid<T>(message: &str) -> Result<T> {
    Err(Error::Invalid(message.into()))
}
fn conflict<T>(message: &str) -> Result<T> {
    Err(Error::Conflict(message.into()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::change_record::{ArtifactMediaV1, ArtifactRefV1, RetentionClassV1};

    fn reference() -> IntentReference {
        let body = "Apply the rate limit per account, including requests from different IPs.";
        IntentReference {
            id: "human-1".into(),
            kind: IntentReferenceKind::HumanMessage,
            source: "chat:message-1".into(),
            author: "human".into(),
            recorded_at_ms: 1,
            sha256: crate::store::digest(body.as_bytes()),
            coverage: CoverageV1::Complete,
            text: Some(body.into()),
            reconstructed: false,
        }
    }

    fn fixture() -> (tempfile::TempDir, CreateIntent) {
        let directory = tempfile::tempdir().unwrap();
        crate::store::Engine::open(directory.path()).unwrap();
        let contract = IntentContract {
            version: 1,
            previous_digest: None,
            request: vec![reference()],
            amendments: vec![],
            criteria: vec![IntentCriterion {
                id: "account-limit".into(),
                description: "Requests from one account share its limit across IP addresses".into(),
                sources: vec!["human-1".into()],
                required: true,
                constraint: false,
            }],
            focus: vec![],
            coverage: CoverageV1::Complete,
            gaps: vec![],
        };
        let subject = IntentSubject {
            repository: "example/project".into(),
            work_id: "work-1".into(),
            root_work_id: "work-1".into(),
            contract_digest: contract.digest().unwrap(),
            scope: IntentScope {
                parent_work_id: None,
                checkpoint: None,
                criteria: vec!["account-limit".into()],
                coverage: CoverageV1::Complete,
                gaps: vec![],
                source_exclusions: vec![],
            },
            source: SourceBindingV1::Git {
                repository: "example/project".into(),
                worktree_id: "checkout-1".into(),
                head: "a".repeat(40),
                tree: "b".repeat(40),
                base: "a".repeat(40),
                dirty: false,
                untracked_count: 0,
                omissions: vec![],
                captured_snapshot: None,
                snapshot_coverage: CoverageV1::Complete,
            },
            evidence: vec![],
            skill: IntentSkillBinding {
                workflow_version_id: "workflow-1".into(),
                step_id: "intent-review".into(),
                skill_version_id: "skill-1".into(),
                skill_sha256: "c".repeat(64),
                package_digest: "d".repeat(64),
                entrypoint: "SKILL.md".into(),
            },
            evaluator_profile_digest: "e".repeat(64),
        };
        let mut create = CreateIntent {
            request_id: "create-1".into(),
            contract,
            subject,
            limits: IntentLimits {
                max_correction_rounds: 2,
                max_invocations: 8,
                wall_time_ms: 100_000,
            },
            now_ms: 1,
        };
        evidence(&mut create.subject, "test-1");
        (directory, create)
    }

    fn evidence(subject: &mut IntentSubject, id: &str) {
        subject.evidence = vec![IntentEvidence {
            id: id.into(),
            sha256: "f".repeat(64),
            origin: "host:check-result".into(),
            candidate_digest: Some(subject.candidate_digest().unwrap()),
            coverage: CoverageV1::Complete,
            assurance: AssuranceBasisV1::CallerAsserted,
            available: true,
        }];
    }

    fn assessment(
        subject: &IntentSubject,
        status: IntentFindingStatus,
        explanation: &str,
    ) -> IntentAssessment {
        IntentAssessment {
            criteria: subject
                .scope
                .criteria
                .iter()
                .map(|criterion| IntentFinding {
                    criterion: criterion.clone(),
                    status,
                    explanation: explanation.into(),
                    evidence_ids: subject
                        .evidence
                        .iter()
                        .map(|evidence| evidence.id.clone())
                        .collect(),
                    corrective_outcome: (status == IntentFindingStatus::Mismatch)
                        .then(|| "Share the counter across account requests".into()),
                    recheck: (status == IntentFindingStatus::Mismatch)
                        .then(|| "Exercise one account through two IPs".into()),
                })
                .collect(),
            coverage: CoverageV1::Complete,
            gaps: vec![],
        }
    }

    fn claim(subject: &IntentSubject, phase: IntentPhase, number: u64) -> IntentClaim {
        IntentClaim {
            request_id: format!("claim-{number}"),
            subject: subject.clone(),
            attempt_id: format!("attempt-{number}"),
            session_id: "session-1".into(),
            phase,
            now_ms: number * 10,
            lease_ms: 1_000,
            requested_model: Some("host-model".into()),
        }
    }

    fn report(
        view: &IntentView,
        subject: &IntentSubject,
        result: Option<IntentAssessment>,
        number: u64,
    ) -> IntentReport {
        let attempt = view.record.attempts.last().unwrap();
        IntentReport {
            request_id: format!("report-{number}"),
            subject: subject.clone(),
            attempt_id: attempt.attempt_id.clone(),
            session_id: attempt.session_id.clone(),
            fence: attempt.fence,
            now_ms: attempt.started_at_ms + 1,
            execution: IntentExecution::Succeeded,
            assessment: result,
            observed_model: Some("host-model".into()),
            detail: None,
        }
    }

    fn changed(subject: &IntentSubject, byte: char, evidence_id: &str) -> IntentSubject {
        let mut next = subject.clone();
        if let SourceBindingV1::Git { head, tree, .. } = &mut next.source {
            *head = byte.to_string().repeat(40);
            *tree = byte.to_string().repeat(40);
        }
        evidence(&mut next, evidence_id);
        next
    }

    #[test]
    fn semantic_results_require_exact_criteria_current_citations_and_human_provenance() {
        let (_directory, create) = fixture();
        let good = assessment(
            &create.subject,
            IntentFindingStatus::Supported,
            "Per-account counter is exercised",
        );
        assert_eq!(
            good.outcome(&create.contract, &create.subject).unwrap(),
            IntentOutcome::MeetsRecordedCriteria
        );
        let mut invalid = good.clone();
        invalid.criteria.clear();
        assert!(invalid.outcome(&create.contract, &create.subject).is_err());
        invalid.criteria = vec![good.criteria[0].clone(), good.criteria[0].clone()];
        assert!(invalid.outcome(&create.contract, &create.subject).is_err());
        invalid = good.clone();
        invalid.criteria[0].evidence_ids = vec!["fabricated".into()];
        assert!(invalid.outcome(&create.contract, &create.subject).is_err());
        let stale = changed(&create.subject, 'c', "fresh");
        assert!(good.outcome(&create.contract, &stale).is_err());
        let mut contract = create.contract.clone();
        contract.request[0].text = Some("Weaken the requirement".into());
        assert!(contract.digest().is_err());
        contract = create.contract.clone();
        contract.request[0].reconstructed = true;
        assert!(contract.digest().is_err());
        let mismatch = assessment(
            &create.subject,
            IntentFindingStatus::Mismatch,
            "Counter is keyed only by IP despite passing tests",
        );
        assert_eq!(
            mismatch.outcome(&create.contract, &create.subject).unwrap(),
            IntentOutcome::NeedsCorrection
        );
        let mut wire = serde_json::to_value(good).unwrap();
        wire["overall"] = serde_json::json!("pass");
        assert!(serde_json::from_value::<IntentAssessment>(wire).is_err());
    }

    #[test]
    fn two_corrections_keep_findings_and_allow_the_final_reassessment() {
        let (directory, create) = fixture();
        let service = IntentService::new(directory.path());
        service.create(&create).unwrap();
        let first = service
            .claim(&claim(&create.subject, IntentPhase::Assessment, 1))
            .unwrap();
        let mismatch = assessment(
            &create.subject,
            IntentFindingStatus::Mismatch,
            "Counter uses the IP address",
        );
        let failed = service
            .report(&report(&first, &create.subject, Some(mismatch), 1))
            .unwrap();
        assert_eq!(failed.next, IntentNext::Correct);
        let repair = service
            .claim(&claim(&create.subject, IntentPhase::Correction, 2))
            .unwrap();
        assert!(
            service
                .report(&report(&repair, &create.subject, None, 2))
                .is_err()
        );
        let second_source = changed(&create.subject, 'c', "test-2");
        let editing = service.status(&second_source, 21).unwrap();
        assert!(!editing.current);
        assert_eq!(editing.next, IntentNext::InProgress);
        let heartbeat = service
            .heartbeat(&IntentHeartbeat {
                request_id: "correction-heartbeat".into(),
                subject: second_source.clone(),
                attempt_id: repair.record.attempts.last().unwrap().attempt_id.clone(),
                session_id: "session-1".into(),
                fence: repair.record.attempts.last().unwrap().fence,
                now_ms: 21,
                lease_ms: 1_000,
            })
            .unwrap();
        assert!(!heartbeat.current);
        assert_eq!(heartbeat.next, IntentNext::InProgress);
        assert_eq!(
            service
                .report(&report(&repair, &second_source, None, 2))
                .unwrap()
                .next,
            IntentNext::Assess
        );
        let second = service
            .claim(&claim(&second_source, IntentPhase::Assessment, 3))
            .unwrap();
        let mismatch = assessment(
            &second_source,
            IntentFindingStatus::Mismatch,
            "One alternate endpoint still uses the IP address",
        );
        assert_eq!(
            service
                .report(&report(&second, &second_source, Some(mismatch), 3))
                .unwrap()
                .next,
            IntentNext::Correct
        );
        let repair = service
            .claim(&claim(&second_source, IntentPhase::Correction, 4))
            .unwrap();
        let final_source = changed(&second_source, 'd', "test-3");
        service
            .report(&report(&repair, &final_source, None, 4))
            .unwrap();
        let last = service
            .claim(&claim(&final_source, IntentPhase::Assessment, 5))
            .unwrap();
        let good = assessment(
            &final_source,
            IntentFindingStatus::Supported,
            "Both endpoints use the account identity",
        );
        let finished = service
            .report(&report(&last, &final_source, Some(good), 5))
            .unwrap();
        assert_eq!(finished.next, IntentNext::Complete);
        assert_eq!(finished.root_correction_rounds, 2);
        assert_eq!(finished.root_invocations, 5);
        assert_eq!(finished.record.attempts.len(), 5);
        assert_eq!(
            finished.record.attempts[0]
                .assessment
                .as_ref()
                .unwrap()
                .criteria[0]
                .status,
            IntentFindingStatus::Mismatch
        );
        assert_eq!(finished.admission, "nonqualifying_agent_report");
        let stale = changed(&final_source, 'e', "unassessed");
        assert_eq!(
            service.status(&stale, 60).unwrap().outcome,
            Some(IntentOutcome::Stale)
        );
    }

    #[test]
    fn root_budget_and_dispatch_survive_descendant_sessions_expiry_and_cancellation() {
        let (directory, create) = fixture();
        let service = IntentService::new(directory.path());
        service.create(&create).unwrap();
        let mut child = create.clone();
        child.request_id = "create-child".into();
        child.subject.work_id = "child-1".into();
        child.subject.scope.parent_work_id = Some("work-1".into());
        service.create(&child).unwrap();
        let child_claim = claim(&child.subject, IntentPhase::Assessment, 1);
        let active = service.claim(&child_claim).unwrap();
        assert_eq!(active.root_invocations, 1);
        assert!(
            service
                .claim(&claim(&create.subject, IntentPhase::Assessment, 2))
                .is_err()
        );
        let mut repeat = child_claim.clone();
        repeat.now_ms += 1;
        assert_eq!(service.claim(&repeat).unwrap().root_invocations, 1);
        let restarted = IntentService::new(directory.path());
        assert_eq!(
            restarted.status(&child.subject, 2_000).unwrap().next,
            IntentNext::RecoveryRequired
        );
        let mut late = report(
            &active,
            &child.subject,
            Some(assessment(
                &child.subject,
                IntentFindingStatus::Supported,
                "Observed",
            )),
            1,
        );
        late.now_ms = 2_000;
        assert!(restarted.report(&late).is_err());
        let mut recovery = IntentControl {
            request_id: "recover-1".into(),
            subject: child.subject.clone(),
            session_id: "session-2".into(),
            now_ms: 2_000,
            reason: "Human inspected interrupted execution".into(),
            authorization: None,
        };
        assert!(restarted.recover(&recovery).is_err());
        recovery.authorization = Some(reference());
        assert_eq!(restarted.recover(&recovery).unwrap().root_invocations, 1);
        let mut next_claim = claim(&child.subject, IntentPhase::Assessment, 2);
        next_claim.now_ms = 2_010;
        let next = restarted.claim(&next_claim).unwrap();
        assert_eq!(next.root_invocations, 2);
        assert_eq!(next.record.attempts.last().unwrap().fence, 2);
        let cancel = IntentControl {
            request_id: "cancel-1".into(),
            subject: child.subject.clone(),
            session_id: "session-2".into(),
            now_ms: 2_011,
            reason: "Stop requested".into(),
            authorization: None,
        };
        assert_eq!(
            restarted.cancel(&cancel).unwrap().next,
            IntentNext::Cancelled
        );
        assert!(
            restarted
                .report(&report(
                    &next,
                    &child.subject,
                    Some(assessment(
                        &child.subject,
                        IntentFindingStatus::Supported,
                        "Observed"
                    )),
                    2
                ))
                .is_err()
        );
        assert_eq!(
            restarted
                .inspect("example/project", "work-1")
                .unwrap()
                .invocations,
            2
        );
    }

    #[test]
    fn dirty_and_untracked_snapshot_changes_invalidate_unchanged_head_results() {
        let (_directory, create) = fixture();
        let mut dirty = create.subject.clone();
        if let SourceBindingV1::Git {
            dirty,
            untracked_count,
            omissions,
            captured_snapshot,
            snapshot_coverage,
            ..
        } = &mut dirty.source
        {
            *dirty = true;
            *untracked_count = 1;
            *omissions = vec![
                CaptureOmissionV1::IgnoredContent,
                CaptureOmissionV1::IndexState,
            ];
            *snapshot_coverage = CoverageV1::Partial;
            *captured_snapshot = Some(ArtifactRefV1 {
                digest: "1".repeat(64),
                size_bytes: 128,
                media: ArtifactMediaV1::Json,
                retention: RetentionClassV1::WorkLocal,
                availability: ContentAvailabilityV1::Local,
            });
        }
        evidence(&mut dirty, "working-check");
        let findings = assessment(
            &dirty,
            IntentFindingStatus::Supported,
            "Observed working bytes",
        );
        assert_eq!(
            findings.outcome(&create.contract, &dirty).unwrap(),
            IntentOutcome::NeedsEvidence
        );
        dirty.scope.source_exclusions = vec![
            IntentSourceExclusion {
                omission: CaptureOmissionV1::IgnoredContent,
                reason: "Ignored build products are outside requested source scope".into(),
                authorization_reference: "human-1".into(),
            },
            IntentSourceExclusion {
                omission: CaptureOmissionV1::IndexState,
                reason: "Assess working source rather than staged index".into(),
                authorization_reference: "human-1".into(),
            },
        ];
        assert_eq!(
            findings.outcome(&create.contract, &dirty).unwrap(),
            IntentOutcome::MeetsRecordedCriteria
        );
        let before = dirty.candidate_digest().unwrap();
        if let SourceBindingV1::Git {
            captured_snapshot: Some(snapshot),
            ..
        } = &mut dirty.source
        {
            snapshot.digest = "2".repeat(64);
        }
        assert_ne!(before, dirty.candidate_digest().unwrap());
        assert!(findings.outcome(&create.contract, &dirty).is_err());
        dirty.scope.source_exclusions[0].omission = CaptureOmissionV1::UntrackedContent;
        assert!(dirty.validate().is_err());
    }

    #[test]
    fn complete_source_claim_cannot_hide_unresolved_omissions() {
        let (_directory, create) = fixture();
        for omission in [
            CaptureOmissionV1::SubmoduleContent,
            CaptureOmissionV1::WorkingTreeContent,
        ] {
            let mut subject = create.subject.clone();
            if let SourceBindingV1::Git {
                omissions,
                snapshot_coverage,
                ..
            } = &mut subject.source
            {
                omissions.push(omission);
                *snapshot_coverage = CoverageV1::Complete;
            }
            evidence(&mut subject, "check");
            let claimed_pass = assessment(
                &subject,
                IntentFindingStatus::Supported,
                "Reported criterion support",
            );
            assert_eq!(
                claimed_pass.outcome(&create.contract, &subject).unwrap(),
                IntentOutcome::NeedsEvidence
            );
        }
    }

    #[test]
    fn amendment_after_completed_assessment_requires_a_fresh_assessment() {
        for status in [
            IntentFindingStatus::Supported,
            IntentFindingStatus::Mismatch,
            IntentFindingStatus::Unknown,
        ] {
            let (directory, create) = fixture();
            let service = IntentService::new(directory.path());
            service.create(&create).unwrap();
            let claimed = service
                .claim(&claim(&create.subject, IntentPhase::Assessment, 1))
                .unwrap();
            service
                .report(&report(
                    &claimed,
                    &create.subject,
                    Some(assessment(
                        &create.subject,
                        status,
                        "Initial observed behavior",
                    )),
                    1,
                ))
                .unwrap();
            let mut contract = create.contract.clone();
            contract.version = 2;
            contract.previous_digest = Some(create.contract.digest().unwrap());
            let mut amendment = reference();
            amendment.id = "human-2".into();
            amendment.kind = IntentReferenceKind::HumanAmendment;
            amendment.text = Some("Apply the same account limit to the alternate endpoint.".into());
            amendment.sha256 = crate::store::digest(amendment.text.as_ref().unwrap().as_bytes());
            contract.amendments.push(amendment);
            let observed = changed(&create.subject, 'b', "amendment-source-check");
            let amended = service
                .amend(&IntentAmendment {
                    request_id: "amend-after-assessment".into(),
                    subject: observed.clone(),
                    contract,
                    now_ms: 12,
                })
                .unwrap();
            assert_eq!(amended.outcome, Some(IntentOutcome::Stale));
            assert_eq!(amended.next, IntentNext::Assess);
            assert!(amended.current);
            assert_eq!(amended.record.subject.source, observed.source);
            assert_eq!(amended.record.subject.evidence, observed.evidence);
            assert_eq!(
                amended.record.attempts[0].subject.source,
                create.subject.source
            );
            assert_eq!(amended.record.contracts.len(), 2);
            assert_eq!(
                amended.record.attempts[0].subject.contract_digest,
                create.subject.contract_digest
            );
            assert_eq!(amended.root_invocations, 1);
            assert_eq!(
                service
                    .status(&amended.record.subject, 13)
                    .unwrap()
                    .current_subject_digest,
                amended.current_subject_digest
            );
            assert_eq!(
                service.status(&amended.record.subject, 13).unwrap().outcome,
                Some(IntentOutcome::Stale)
            );
            let next = service
                .claim(&claim(&amended.record.subject, IntentPhase::Assessment, 2))
                .unwrap();
            let final_view = service
                .report(&report(
                    &next,
                    &amended.record.subject,
                    Some(assessment(
                        &amended.record.subject,
                        IntentFindingStatus::Supported,
                        "Alternate endpoint checked",
                    )),
                    2,
                ))
                .unwrap();
            assert_eq!(
                final_view.outcome,
                Some(IntentOutcome::MeetsRecordedCriteria)
            );
            assert_eq!(final_view.root_invocations, 2);
        }
    }

    #[test]
    fn human_amendments_and_dispositions_remain_distinct_from_evaluator_pass() {
        let (directory, create) = fixture();
        let service = IntentService::new(directory.path());
        service.create(&create).unwrap();
        let mut next_contract = create.contract.clone();
        next_contract.version = 2;
        next_contract.previous_digest = Some(create.contract.digest().unwrap());
        let mut amendment = reference();
        amendment.id = "human-2".into();
        amendment.kind = IntentReferenceKind::HumanAmendment;
        amendment.text =
            Some("The account constraint also applies to the alternate endpoint.".into());
        amendment.sha256 = crate::store::digest(amendment.text.as_ref().unwrap().as_bytes());
        next_contract.amendments.push(amendment);
        let amended = service
            .amend(&IntentAmendment {
                request_id: "amend-1".into(),
                subject: create.subject.clone(),
                contract: next_contract,
                now_ms: 2,
            })
            .unwrap();
        assert_eq!(amended.record.contracts.len(), 2);
        assert_eq!(
            service.status(&create.subject, 3).unwrap().outcome,
            Some(IntentOutcome::Stale)
        );
        let subject = amended.record.subject.clone();
        let disposition = IntentDisposition {
            kind: IntentDispositionKind::Dispute,
            criteria: vec!["account-limit".into()],
            reason: "Human disputes the recorded interpretation".into(),
            authorization: reference(),
            subject_digest: subject.digest().unwrap(),
        };
        let view = service
            .disposition(&IntentDispositionRequest {
                request_id: "dispute-1".into(),
                subject: subject.clone(),
                disposition,
                now_ms: 3,
            })
            .unwrap();
        assert_eq!(view.outcome, Some(IntentOutcome::Disputed));
        assert_eq!(view.next, IntentNext::HumanDisposition);
        assert!(
            service
                .claim(&claim(&subject, IntentPhase::Assessment, 1))
                .is_err()
        );
        assert_eq!(
            view.record.contracts[0].request[0].text,
            create.contract.request[0].text
        );
    }

    #[test]
    fn version_five_migration_retains_native_tables_and_adds_intent_atomically() {
        let (directory, create) = fixture();
        let service = IntentService::new(directory.path());
        service.create(&create).unwrap();
        let database = directory.path().join("skill-workflows/state.sqlite");
        let db = Connection::open(&database).unwrap();
        db.execute_batch("DROP TABLE workflow_intent_operations; DROP TABLE workflow_intent_work; PRAGMA user_version=5;").unwrap();
        drop(db);
        let before = std::fs::read(&database).unwrap();
        assert!(matches!(
            service.inspect("example/project", "work-1"),
            Err(Error::NotFound(_))
        ));
        assert!(matches!(
            service.status(&create.subject, 2),
            Err(Error::NotFound(_))
        ));
        let db = Connection::open_with_flags(&database, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
            .unwrap();
        assert_eq!(
            db.pragma_query_value(None, "user_version", |row| row.get::<_, u64>(0))
                .unwrap(),
            5
        );
        assert_eq!(db.query_row("SELECT count(*) FROM sqlite_schema WHERE name IN ('workflow_intent_work','workflow_intent_operations')", [], |row| row.get::<_, u64>(0)).unwrap(), 0);
        drop(db);
        assert_eq!(std::fs::read(&database).unwrap(), before);
        service.create(&create).unwrap();
        let journal = Journal::open(directory.path(), OpenMode::Read).unwrap();
        assert_eq!(
            journal
                .connection
                .pragma_query_value(None, "user_version", |row| row.get::<_, u64>(0))
                .unwrap(),
            6
        );
        super::super::agent_tasks::validate_schema(&journal.connection).unwrap();
        validate_schema(&journal.connection).unwrap();
        drop(journal);
        // Recovery includes the v6 adapter tables and preserves their exact retained bytes.
        drop(crate::authority::Authority::open(directory.path()).unwrap());
        let retained = service.inspect("example/project", "work-1").unwrap();
        let recovery = tempfile::tempdir().unwrap();
        let backup = recovery.path().join("backup");
        crate::local_recovery::backup(directory.path(), &backup, &[], &[], true).unwrap();
        crate::local_recovery::verify(&backup).unwrap();
        let destination = recovery.path().join("restored");
        crate::local_recovery::restore(&backup, &destination).unwrap();
        crate::local_recovery::activate(&destination, true, true).unwrap();
        assert_eq!(
            IntentService::new(&destination.join("data"))
                .inspect("example/project", "work-1")
                .unwrap(),
            retained
        );
        let db = Connection::open(&database).unwrap();
        db.execute_batch("DROP TABLE workflow_intent_work;")
            .unwrap();
        drop(db);
        let missing_table = service.inspect("example/project", "work-1").unwrap_err();
        assert!(!matches!(missing_table, Error::NotFound(_)));
        assert!(service.status(&create.subject, 3).is_err());
    }

    #[test]
    fn uncertain_execution_and_unchanged_findings_pause_without_replenishing_budget() {
        let (directory, create) = fixture();
        let service = IntentService::new(directory.path());
        service.create(&create).unwrap();
        let first = service
            .claim(&claim(&create.subject, IntentPhase::Assessment, 1))
            .unwrap();
        let mismatch = assessment(
            &create.subject,
            IntentFindingStatus::Mismatch,
            "Still keyed by IP",
        );
        service
            .report(&report(&first, &create.subject, Some(mismatch), 1))
            .unwrap();
        let correction = service
            .claim(&claim(&create.subject, IntentPhase::Correction, 2))
            .unwrap();
        let next = changed(&create.subject, 'c', "second-check");
        service
            .report(&report(&correction, &next, None, 2))
            .unwrap();
        let second = service
            .claim(&claim(&next, IntentPhase::Assessment, 3))
            .unwrap();
        let same = assessment(&next, IntentFindingStatus::Mismatch, "Still keyed by IP");
        let paused = service
            .report(&report(&second, &next, Some(same), 3))
            .unwrap();
        assert_eq!(paused.next, IntentNext::RecoveryRequired);
        assert_eq!(paused.root_correction_rounds, 1);
        assert!(
            service
                .claim(&claim(&next, IntentPhase::Correction, 4))
                .is_err()
        );

        let (directory, create) = fixture();
        let service = IntentService::new(directory.path());
        service.create(&create).unwrap();
        let active = service
            .claim(&claim(&create.subject, IntentPhase::Assessment, 1))
            .unwrap();
        let mut uncertain = report(&active, &create.subject, None, 1);
        uncertain.execution = IntentExecution::Uncertain;
        let view = service.report(&uncertain).unwrap();
        assert_eq!(view.outcome, Some(IntentOutcome::OperationalError));
        assert_eq!(view.next, IntentNext::RecoveryRequired);
        assert!(
            service
                .claim(&claim(&create.subject, IntentPhase::Assessment, 2))
                .is_err()
        );
        uncertain.now_ms += 100;
        assert_eq!(service.report(&uncertain).unwrap().root_invocations, 1);
    }

    #[test]
    fn invocation_limits_reserve_a_recheck_and_heartbeats_cannot_revive_expired_attempts() {
        let (directory, mut create) = fixture();
        create.limits.max_invocations = 2;
        let service = IntentService::new(directory.path());
        service.create(&create).unwrap();
        let active = service
            .claim(&claim(&create.subject, IntentPhase::Assessment, 1))
            .unwrap();
        let attempt = active.record.attempts.last().unwrap();
        let heartbeat = IntentHeartbeat {
            request_id: "heartbeat-1".into(),
            subject: create.subject.clone(),
            attempt_id: attempt.attempt_id.clone(),
            fence: attempt.fence,
            session_id: attempt.session_id.clone(),
            now_ms: 100,
            lease_ms: 1_000,
        };
        assert_eq!(
            service
                .heartbeat(&heartbeat)
                .unwrap()
                .record
                .attempts
                .last()
                .unwrap()
                .lease_expires_at_ms,
            1_100
        );
        let mut expired = heartbeat.clone();
        expired.request_id = "late-heartbeat".into();
        expired.now_ms = 1_100;
        assert!(service.heartbeat(&expired).is_err());
        let mismatch = assessment(
            &create.subject,
            IntentFindingStatus::Mismatch,
            "Per-IP behavior observed",
        );
        let mut result = report(&active, &create.subject, Some(mismatch), 1);
        result.now_ms = 101;
        assert_eq!(
            service.report(&result).unwrap().next,
            IntentNext::BudgetExhausted
        );
        let mut correction = claim(&create.subject, IntentPhase::Correction, 2);
        correction.now_ms = 110;
        assert!(service.claim(&correction).is_err());
        assert_eq!(
            service
                .inspect("example/project", "work-1")
                .unwrap()
                .invocations,
            1
        );
    }
}
