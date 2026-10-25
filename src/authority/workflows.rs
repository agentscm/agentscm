//! Target-scoped workflow requirements and exact-subject completion receipts.
//!
//! The workflow journal remains execution state. Admission trusts only immutable
//! receipts copied into the authority after the runtime has revalidated a run.

use rusqlite::{Connection, OptionalExtension, Transaction, TransactionBehavior, params};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::{
    Authority, read_policy, sql_i64, validate_current_context_subject,
    validate_current_context_without_workflow, validate_identifier,
};
use crate::context::ReviewContext;
use crate::skill_workflow::{
    CollectorTransfer, CompiledPlan, ImportedStepDependency, ImportedTrustClass, RunState, RunView,
    StepAction, StepState, WorkflowDefinition,
};
use crate::{Error, Result};

const REQUIREMENT_VERSION: u32 = 1;
const RECEIPT_VERSION: u32 = 1;
const IMPORTED_RECEIPT_VERSION: u32 = 2;
const PROFILE_REPOSITORY: &str = "00000000000000000000000000000000";
const PROFILE_REVISION: &str = "0000000000000000000000000000000000000000000000000000000000000000";
const PROFILE_CHANGE_ID: &str = "profile";
const REQUIREMENT_DOMAIN: &[u8] = b"agentscm.workflow-requirement.v1\0";
const RECEIPT_DOMAIN: &[u8] = b"agentscm.workflow-receipt.v1\0";
const IMPORTED_RECEIPT_DOMAIN: &[u8] = b"agentscm.workflow-receipt.v2\0";
const MAX_REQUIREMENT_BYTES: usize = 256 * 1024;
const MAX_RECEIPT_BYTES: usize = 512 * 1024;

/// A durable target policy derived from a validated plan, with the current
/// review subject replaced by fixed profile placeholders.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct WorkflowRequirement {
    pub version: u32,
    pub target: String,
    pub definition: WorkflowDefinition,
    pub skill_digest: String,
}

impl WorkflowRequirement {
    /// Preflight action purpose before a configuring caller mutates a Change.
    /// Full compilation and exact-subject validation remain separate requirements.
    pub fn validate_admission_actions(definition: &WorkflowDefinition) -> Result<()> {
        if definition
            .steps
            .iter()
            .any(|step| matches!(step.action, StepAction::AgentAction { .. }))
        {
            return Err(Error::Invalid(
                "productive actions cannot be native admission requirements".into(),
            ));
        }
        Ok(())
    }

    pub fn from_plan(target: &str, plan: &CompiledPlan) -> Result<Self> {
        validate_identifier("target name", target)?;
        let requirement = Self {
            version: REQUIREMENT_VERSION,
            target: target.to_owned(),
            definition: normalized_definition(plan.definition()),
            skill_digest: plan.skill_digest().to_owned(),
        };
        requirement.validate()?;
        Ok(requirement)
    }

    pub fn digest(&self) -> Result<String> {
        Ok(domain_digest(REQUIREMENT_DOMAIN, &self.canonical_bytes()?))
    }

    pub fn instantiate(&self, context: &ReviewContext) -> Result<WorkflowDefinition> {
        self.validate()?;
        context.validate()?;
        if context.target != self.target {
            return Err(Error::Conflict(format!(
                "workflow requirement for target {} cannot bind context target {}",
                self.target, context.target
            )));
        }
        let mut definition = self.definition.clone();
        definition.source.repository = context.change.repository.clone();
        definition.source.revision = context.revision.clone();
        definition.source.policy_epoch = context.policy_epoch;
        for step in &mut definition.steps {
            if let StepAction::Command {
                change,
                change_version,
                context_digest,
                ..
            } = &mut step.action
            {
                *change = context.change.clone();
                *change_version = context.change_version;
                *context_digest = context.digest()?;
            }
        }
        Ok(definition)
    }

    fn validate(&self) -> Result<()> {
        Self::validate_admission_actions(&self.definition)?;
        if self.version != REQUIREMENT_VERSION {
            return Err(Error::Invalid(format!(
                "unsupported workflow requirement version {}",
                self.version
            )));
        }
        validate_identifier("target name", &self.target)?;
        validate_skill_digest(&self.skill_digest)?;
        if self.definition.source.repository != PROFILE_REPOSITORY
            || self.definition.source.revision != PROFILE_REVISION
            || self.definition.source.policy_epoch != 0
            || normalized_definition(&self.definition) != self.definition
        {
            return Err(Error::Invalid(
                "workflow requirement definition is not subject-neutral".into(),
            ));
        }
        let bytes = serde_json::to_vec(self)?;
        if bytes.len() > MAX_REQUIREMENT_BYTES {
            return Err(Error::Limit("workflow requirement exceeds 256 KiB".into()));
        }
        Ok(())
    }

    fn canonical_bytes(&self) -> Result<Vec<u8>> {
        self.validate()?;
        Ok(serde_json::to_vec(self)?)
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct WorkflowReceipt {
    version: u32,
    requirement_digest: String,
    run_id: String,
    plan_digest: String,
    skill_digest: String,
    repository: String,
    revision: String,
    target: String,
    policy_epoch: u64,
    change_repository: String,
    change_id: String,
    change_version: u64,
    context_digest: String,
    run_state: RunState,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    imported_dependencies: Vec<ImportedStepDependency>,
}

impl WorkflowReceipt {
    fn canonical_bytes(&self) -> Result<Vec<u8>> {
        match self.version {
            RECEIPT_VERSION if !self.imported_dependencies.is_empty() => {
                return Err(Error::Invalid(
                    "legacy workflow receipt cannot contain imported dependencies".into(),
                ));
            }
            RECEIPT_VERSION => {}
            IMPORTED_RECEIPT_VERSION if self.imported_dependencies.is_empty() => {
                return Err(Error::Invalid(
                    "imported workflow receipt is missing dependencies".into(),
                ));
            }
            IMPORTED_RECEIPT_VERSION => {}
            _ => {
                return Err(Error::Invalid(
                    "unsupported workflow receipt version".into(),
                ));
            }
        }
        let bytes = serde_json::to_vec(self)?;
        if bytes.len() > MAX_RECEIPT_BYTES {
            return Err(Error::Limit("workflow receipt exceeds 512 KiB".into()));
        }
        Ok(bytes)
    }

    fn digest(&self) -> Result<String> {
        let domain = if self.version == RECEIPT_VERSION {
            RECEIPT_DOMAIN
        } else if self.version == IMPORTED_RECEIPT_VERSION {
            IMPORTED_RECEIPT_DOMAIN
        } else {
            return Err(Error::Invalid(
                "unsupported workflow receipt version".into(),
            ));
        };
        Ok(domain_digest(domain, &self.canonical_bytes()?))
    }
}

/// Internal, non-deserializable result of validating the existing compiled graph.
pub(crate) struct ValidatedImportedWorkflow {
    pub(crate) dependencies: Vec<ImportedStepDependency>,
    pub(crate) native_steps: Vec<String>,
    pub(crate) run_state: RunState,
}

impl Authority {
    /// Installs the first immutable workflow requirement for a target. An exact
    /// retry is idempotent. This profile deliberately has no update/remove path.
    pub fn install_workflow_requirement(
        &mut self,
        requirement: &WorkflowRequirement,
    ) -> Result<String> {
        self.require_writable()?;
        let canonical = requirement.canonical_bytes()?;
        let digest = requirement.digest()?;
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        if let Some(existing) = read_requirement(&tx, &requirement.target)? {
            if existing == *requirement {
                tx.commit()?;
                return Ok(digest);
            }
            return Err(Error::Conflict(format!(
                "target {} already has a different workflow requirement",
                requirement.target
            )));
        }
        let current_policy = read_policy(&tx)?;
        let next_policy = current_policy
            .checked_add(1)
            .ok_or_else(|| Error::Limit("policy epoch exhausted".into()))?;
        if tx.execute(
            "UPDATE authority_state SET policy_epoch=?1 WHERE singleton=1 AND policy_epoch=?2",
            params![next_policy, current_policy],
        )? != 1
        {
            return Err(Error::Conflict(
                "policy epoch changed while installing workflow requirement".into(),
            ));
        }
        tx.execute(
            "INSERT INTO workflow_requirements(target,digest,installed_policy_epoch,canonical_json) \
             VALUES(?1,?2,?3,?4)",
            params![requirement.target, digest, next_policy, canonical],
        )?;
        tx.commit()?;
        Ok(digest)
    }

    pub fn workflow_requirement(&self, target: &str) -> Result<Option<WorkflowRequirement>> {
        validate_identifier("target name", target)?;
        read_requirement(&self.connection, target)
    }

    /// Records a completion that the trusted runtime has revalidated while
    /// holding its journal lease. The authority independently checks the plan,
    /// installed profile and exact current review subject before accepting it.
    pub(crate) fn record_workflow_success(
        &mut self,
        requirement: &WorkflowRequirement,
        plan: &CompiledPlan,
        view: &RunView,
        context: &ReviewContext,
    ) -> Result<String> {
        self.require_writable()?;
        requirement.validate()?;
        let context_digest = context.digest()?;
        let expected_definition = requirement.instantiate(context)?;
        if plan.definition() != &expected_definition
            || plan.skill_digest() != requirement.skill_digest
            || view.plan_digest != plan.digest()
            || view.skill_digest != requirement.skill_digest
            || view.source != expected_definition.source
        {
            return Err(Error::Conflict(
                "workflow completion does not match the installed exact-subject plan".into(),
            ));
        }
        validate_successful_graph(plan, view)?;

        let requirement_digest = requirement.digest()?;
        let receipt = WorkflowReceipt {
            version: RECEIPT_VERSION,
            requirement_digest: requirement_digest.clone(),
            run_id: view.run_id.clone(),
            plan_digest: view.plan_digest.clone(),
            skill_digest: view.skill_digest.clone(),
            repository: context.change.repository.clone(),
            revision: context.revision.clone(),
            target: context.target.clone(),
            policy_epoch: context.policy_epoch,
            change_repository: context.change.repository.clone(),
            change_id: context.change.id.clone(),
            change_version: context.change_version,
            context_digest: context_digest.clone(),
            run_state: view.state,
            imported_dependencies: Vec::new(),
        };
        let canonical = receipt.canonical_bytes()?;
        let digest = receipt.digest()?;
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let installed = read_requirement(&tx, &context.target)?.ok_or_else(|| {
            Error::Conflict(format!(
                "target {} has no installed workflow requirement",
                context.target
            ))
        })?;
        if installed != *requirement || installed.digest()? != requirement_digest {
            return Err(Error::Conflict(
                "installed workflow requirement changed before completion was recorded".into(),
            ));
        }
        validate_current_context_without_workflow(
            &tx,
            &context.change,
            context.change_version,
            &context_digest,
            &context.target,
            &context.revision,
            sql_i64("policy epoch", context.policy_epoch)?,
        )?;
        tx.execute(
            "INSERT INTO workflow_receipts(\
                digest,requirement_digest,run_id,plan_digest,skill_digest,repository,revision,target,\
                policy_epoch,change_repository,change_id,change_version,context_digest,canonical_json\
             ) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14) \
             ON CONFLICT(digest) DO NOTHING",
            params![
                digest,
                requirement_digest,
                receipt.run_id,
                receipt.plan_digest,
                receipt.skill_digest,
                receipt.repository,
                receipt.revision,
                receipt.target,
                sql_i64("policy epoch", receipt.policy_epoch)?,
                receipt.change_repository,
                receipt.change_id,
                sql_i64("change version", receipt.change_version)?,
                receipt.context_digest,
                canonical,
            ],
        )?;
        let stored: Vec<u8> = tx.query_row(
            "SELECT canonical_json FROM workflow_receipts WHERE digest=?1",
            [&digest],
            |row| row.get(0),
        )?;
        if stored != receipt.canonical_bytes()? {
            return Err(Error::Conflict(format!(
                "workflow receipt digest {digest} is bound to different bytes"
            )));
        }
        tx.commit()?;
        Ok(digest)
    }

    pub(crate) fn record_imported_workflow_success(
        &mut self,
        requirement: &WorkflowRequirement,
        plan: &CompiledPlan,
        context: &ReviewContext,
        validated: ValidatedImportedWorkflow,
        origins: &crate::skill_workflow::LiveImportedOrigins,
        load: &mut dyn FnMut(&ImportedStepDependency) -> Result<Vec<u8>>,
    ) -> Result<String> {
        self.require_writable()?;
        requirement.validate()?;
        let expected_definition = requirement.instantiate(context)?;
        if plan.definition() != &expected_definition
            || plan.skill_digest() != requirement.skill_digest
            || !matches!(
                validated.run_state,
                RunState::Succeeded | RunState::CompletedWithOmissions
            )
            || validated.dependencies.len() > 32
        {
            return Err(Error::Conflict(
                "imported completion does not match the installed workflow graph".into(),
            ));
        }
        validate_imported_graph(plan, &validated)?;
        let requirement_digest = requirement.digest()?;
        let context_digest = context.digest()?;
        let receipt = WorkflowReceipt {
            version: IMPORTED_RECEIPT_VERSION,
            requirement_digest: requirement_digest.clone(),
            run_id: imported_run_id(plan, &validated.dependencies)?,
            plan_digest: plan.digest().into(),
            skill_digest: plan.skill_digest().into(),
            repository: context.change.repository.clone(),
            revision: context.revision.clone(),
            target: context.target.clone(),
            policy_epoch: context.policy_epoch,
            change_repository: context.change.repository.clone(),
            change_id: context.change.id.clone(),
            change_version: context.change_version,
            context_digest: context_digest.clone(),
            run_state: validated.run_state,
            imported_dependencies: validated.dependencies,
        };
        let canonical = receipt.canonical_bytes()?;
        let digest = receipt.digest()?;
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let installed = read_requirement(&tx, &context.target)?.ok_or_else(|| {
            Error::Conflict("target has no installed workflow requirement".into())
        })?;
        if installed != *requirement || installed.digest()? != requirement_digest {
            return Err(Error::Conflict(
                "installed workflow requirement changed before imported completion".into(),
            ));
        }
        validate_current_context_subject(
            &tx,
            &context.change,
            context.change_version,
            &context_digest,
            &context.target,
            &context.revision,
            sql_i64("policy epoch", context.policy_epoch)?,
        )?;
        for dependency in &receipt.imported_dependencies {
            if !origins.matches_repository(&context.change.repository)
                || !origins.permits(&dependency.origin)
            {
                return Err(Error::Conflict(
                    "finalization origin manifest changed".into(),
                ));
            }
            let bytes = load(dependency)?;
            if bytes.len() > crate::skill_workflow::MAX_COLLECTOR_TRANSFER_BYTES {
                return Err(Error::Limit(
                    "imported collector evidence exceeds 16 MiB".into(),
                ));
            }
            let transfer = CollectorTransfer::from_slice(&bytes)?;
            if transfer.digest()? != dependency.transfer_digest
                || !dependency_matches_transfer(dependency, &transfer)?
            {
                return Err(Error::Invalid(
                    "imported dependency differs from its original signed transfer".into(),
                ));
            }
            validate_live_dependency(&tx, dependency, &transfer)?;
            tx.execute(
                "INSERT INTO workflow_imported_evidence(digest,repository,step_id,profile_digest,registration_id,registration_epoch,origin_owner_id,origin_agent_id,origin_grant_id,origin_grant_generation,canonical_json) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11) ON CONFLICT(digest) DO NOTHING",
                params![
                    dependency.transfer_digest,
                    context.change.repository,
                    dependency.step_id,
                    dependency.collector_profile_digest,
                    dependency.registration_id,
                    sql_i64("registration epoch", dependency.registration_epoch)?,
                    dependency.origin.actor.owner_id,
                    dependency.origin.actor.agent_id,
                    dependency.origin.grant_id,
                    sql_i64("origin grant generation", dependency.origin.grant_generation)?,
                    bytes,
                ],
            )?;
            let matches: bool = tx.query_row(
                "SELECT canonical_json=?2 FROM workflow_imported_evidence WHERE digest=?1",
                params![dependency.transfer_digest, bytes],
                |row| row.get(0),
            )?;
            if !matches {
                return Err(Error::Conflict(
                    "imported evidence digest is bound to different bytes".into(),
                ));
            }
        }
        insert_receipt(&tx, &digest, &receipt, &canonical)?;
        tx.commit()?;
        Ok(digest)
    }

    pub(crate) fn imported_workflow_origins(
        &self,
        context: &ReviewContext,
    ) -> Result<Vec<crate::skill_workflow::OriginGrantBinding>> {
        let Some(requirement) = read_requirement(&self.connection, &context.target)? else {
            return Ok(Vec::new());
        };
        let requirement_digest = requirement.digest()?;
        let context_digest = context.digest()?;
        let mut statement = self.connection.prepare(
            "SELECT canonical_json FROM workflow_receipts WHERE requirement_digest=?1 AND repository=?2 AND revision=?3 AND target=?4 AND policy_epoch=?5 AND change_repository=?6 AND change_id=?7 AND change_version=?8 AND context_digest=?9",
        )?;
        let rows = statement.query_map(
            params![
                requirement_digest,
                context.change.repository,
                context.revision,
                context.target,
                sql_i64("policy epoch", context.policy_epoch)?,
                context.change.repository,
                context.change.id,
                sql_i64("change version", context.change_version)?,
                context_digest,
            ],
            |row| row.get::<_, Vec<u8>>(0),
        )?;
        let mut origins = Vec::new();
        for row in rows {
            let bytes = row?;
            let receipt: WorkflowReceipt = serde_json::from_slice(&bytes)?;
            if receipt.version != IMPORTED_RECEIPT_VERSION || receipt.canonical_bytes()? != bytes {
                continue;
            }
            for dependency in receipt.imported_dependencies {
                if !origins.contains(&dependency.origin) {
                    origins.push(dependency.origin);
                }
            }
        }
        Ok(origins)
    }
}

pub(super) fn require_current_success(
    tx: &Transaction<'_>,
    context: &ReviewContext,
    imported_origins: Option<&crate::skill_workflow::LiveImportedOrigins>,
) -> Result<bool> {
    if read_requirement(tx, &context.target)?.is_none() {
        return Ok(false);
    }
    match find_current_success(tx, context, imported_origins)? {
        Some((_, imported)) => Ok(imported),
        None => Err(Error::Conflict(format!(
            "target {} requires a successful workflow for the exact current revision and review context",
            context.target
        ))),
    }
}

/// Result of the single-snapshot qualification read.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum CurrentEvidence {
    /// The Change, context, policy or target no longer match the queried subject.
    SubjectChanged,
    /// The subject is current but the admission evidence checks do not pass.
    NotQualified,
    /// Digest of the accepted workflow receipt.
    Receipt(String),
}

impl Authority {
    /// One deferred read snapshot that runs admission's current-subject check and evidence
    /// checks (source paths, workflow receipt, command unless the receipt is imported).
    /// It never writes or commits.
    pub(crate) fn current_qualification(
        &self,
        context: &ReviewContext,
        imported_origins: &crate::skill_workflow::LiveImportedOrigins,
    ) -> Result<CurrentEvidence> {
        // Qualification requires a receipt: a target without a requirement is never qualified,
        // unlike admission's `Ok(false)` pass-through.
        let tx = Transaction::new_unchecked(&self.connection, TransactionBehavior::Deferred)?;
        let current = match validate_current_context_subject(
            &tx,
            &context.change,
            context.change_version,
            &context.digest()?,
            &context.target,
            &context.revision,
            sql_i64("policy epoch", context.policy_epoch)?,
        ) {
            Ok(current) => current,
            Err(Error::Invalid(_) | Error::Conflict(_)) => {
                return Ok(CurrentEvidence::SubjectChanged);
            }
            Err(error) => return Err(error),
        };
        match current_evidence(&tx, &current, imported_origins) {
            Ok(Some(digest)) => Ok(CurrentEvidence::Receipt(digest)),
            Ok(None) | Err(Error::Invalid(_) | Error::Conflict(_)) => {
                Ok(CurrentEvidence::NotQualified)
            }
            Err(error) => Err(error),
        }
    }
}

/// Admission's evidence order: source paths, workflow receipt, then command evidence unless
/// the accepted receipt is the imported kind.
fn current_evidence(
    tx: &Transaction<'_>,
    context: &ReviewContext,
    imported_origins: &crate::skill_workflow::LiveImportedOrigins,
) -> Result<Option<String>> {
    crate::checks::require_current_success(tx, context)?;
    let Some((digest, imported)) = find_current_success(tx, context, Some(imported_origins))?
    else {
        return Ok(None);
    };
    #[cfg(not(unix))]
    let _ = imported;
    #[cfg(unix)]
    if !imported {
        crate::execution::require_current_command_success(tx, context)?;
    }
    Ok(Some(digest))
}

fn find_current_success(
    tx: &Transaction<'_>,
    context: &ReviewContext,
    imported_origins: Option<&crate::skill_workflow::LiveImportedOrigins>,
) -> Result<Option<(String, bool)>> {
    let Some(requirement) = read_requirement(tx, &context.target)? else {
        return Ok(None);
    };
    let requirement_digest = requirement.digest()?;
    let context_digest = context.digest()?;
    let mut statement = tx.prepare(
        "SELECT digest,canonical_json FROM workflow_receipts \
         WHERE requirement_digest=?1 AND repository=?2 AND revision=?3 AND target=?4 \
           AND policy_epoch=?5 AND change_repository=?6 AND change_id=?7 \
           AND change_version=?8 AND context_digest=?9",
    )?;
    let rows = statement.query_map(
        params![
            requirement_digest,
            context.change.repository,
            context.revision,
            context.target,
            sql_i64("policy epoch", context.policy_epoch)?,
            context.change.repository,
            context.change.id,
            sql_i64("change version", context.change_version)?,
            context_digest,
        ],
        |row| Ok((row.get::<_, String>(0)?, row.get::<_, Vec<u8>>(1)?)),
    )?;
    for row in rows {
        let (digest, canonical) = row?;
        let receipt: WorkflowReceipt = serde_json::from_slice(&canonical)?;
        let imported_valid = if receipt.version == IMPORTED_RECEIPT_VERSION {
            imported_origins.is_some_and(|origins| {
                validate_receipt_dependencies(tx, context, &receipt, origins).is_ok()
            })
        } else {
            receipt.version == RECEIPT_VERSION && receipt.imported_dependencies.is_empty()
        };
        if imported_valid
            && receipt.requirement_digest == requirement.digest()?
            && receipt.repository == context.change.repository
            && receipt.revision == context.revision
            && receipt.target == context.target
            && receipt.policy_epoch == context.policy_epoch
            && receipt.change_repository == context.change.repository
            && receipt.change_id == context.change.id
            && receipt.change_version == context.change_version
            && receipt.context_digest == context.digest()?
            && receipt.skill_digest == requirement.skill_digest
            && matches!(
                receipt.run_state,
                RunState::Succeeded | RunState::CompletedWithOmissions
            )
            && receipt.canonical_bytes()? == canonical
            && receipt.digest()? == digest
        {
            return Ok(Some((digest, receipt.version == IMPORTED_RECEIPT_VERSION)));
        }
    }
    Ok(None)
}

pub(super) fn create_schema_v7(tx: &Transaction<'_>) -> Result<()> {
    tx.execute_batch(
        "CREATE TABLE workflow_requirements(
            target TEXT PRIMARY KEY CHECK(length(target) BETWEEN 1 AND 128),
            digest TEXT NOT NULL UNIQUE CHECK(length(digest)=64),
            installed_policy_epoch INTEGER NOT NULL CHECK(installed_policy_epoch>=0),
            canonical_json BLOB NOT NULL CHECK(length(canonical_json)<=262144)
         ) STRICT;
         CREATE TABLE workflow_receipts(
            digest TEXT PRIMARY KEY CHECK(length(digest)=64),
            requirement_digest TEXT NOT NULL REFERENCES workflow_requirements(digest),
            run_id TEXT NOT NULL CHECK(length(run_id) BETWEEN 1 AND 64),
            plan_digest TEXT NOT NULL CHECK(length(plan_digest)=71),
            skill_digest TEXT NOT NULL CHECK(length(skill_digest)=71),
            repository TEXT NOT NULL,
            revision TEXT NOT NULL CHECK(length(revision)=64),
            target TEXT NOT NULL,
            policy_epoch INTEGER NOT NULL CHECK(policy_epoch>=0),
            change_repository TEXT NOT NULL,
            change_id TEXT NOT NULL,
            change_version INTEGER NOT NULL CHECK(change_version>0),
            context_digest TEXT NOT NULL CHECK(length(context_digest)=64),
            canonical_json BLOB NOT NULL CHECK(length(canonical_json)<=524288)
         ) STRICT;
         CREATE INDEX workflow_receipt_subject ON workflow_receipts(
            requirement_digest,repository,revision,target,policy_epoch,
            change_repository,change_id,change_version,context_digest
         );",
    )?;
    Ok(())
}

pub(super) fn create_schema_v9(tx: &Transaction<'_>) -> Result<()> {
    tx.execute_batch(
        "CREATE TABLE workflow_imported_evidence(
            digest TEXT PRIMARY KEY CHECK(length(digest)=64),
            repository TEXT NOT NULL CHECK(length(repository)=32),
            step_id TEXT NOT NULL CHECK(length(step_id) BETWEEN 1 AND 128),
            profile_digest TEXT NOT NULL CHECK(length(profile_digest)=64),
            registration_id TEXT NOT NULL,
            registration_epoch INTEGER NOT NULL CHECK(registration_epoch>0),
            origin_owner_id TEXT NOT NULL,
            origin_agent_id TEXT NOT NULL,
            origin_grant_id TEXT NOT NULL,
            origin_grant_generation INTEGER NOT NULL CHECK(origin_grant_generation>0),
            canonical_json BLOB NOT NULL CHECK(length(canonical_json)<=16777216)
         ) STRICT;",
    )?;
    Ok(())
}

fn read_requirement(connection: &Connection, target: &str) -> Result<Option<WorkflowRequirement>> {
    let row = connection
        .query_row(
            "SELECT digest,canonical_json FROM workflow_requirements WHERE target=?1",
            [target],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, Vec<u8>>(1)?)),
        )
        .optional()?;
    row.map(|(digest, canonical)| {
        if canonical.len() > MAX_REQUIREMENT_BYTES {
            return Err(Error::Limit(
                "stored workflow requirement exceeds 256 KiB".into(),
            ));
        }
        let requirement: WorkflowRequirement = serde_json::from_slice(&canonical)?;
        if requirement.target != target
            || requirement.canonical_bytes()? != canonical
            || requirement.digest()? != digest
        {
            return Err(Error::Invalid(format!(
                "stored workflow requirement for target {target} failed integrity validation"
            )));
        }
        Ok(requirement)
    })
    .transpose()
}

fn insert_receipt(
    tx: &Transaction<'_>,
    digest: &str,
    receipt: &WorkflowReceipt,
    canonical: &[u8],
) -> Result<()> {
    tx.execute(
        "INSERT INTO workflow_receipts(\
            digest,requirement_digest,run_id,plan_digest,skill_digest,repository,revision,target,\
            policy_epoch,change_repository,change_id,change_version,context_digest,canonical_json\
         ) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14) \
         ON CONFLICT(digest) DO NOTHING",
        params![
            digest,
            receipt.requirement_digest,
            receipt.run_id,
            receipt.plan_digest,
            receipt.skill_digest,
            receipt.repository,
            receipt.revision,
            receipt.target,
            sql_i64("policy epoch", receipt.policy_epoch)?,
            receipt.change_repository,
            receipt.change_id,
            sql_i64("change version", receipt.change_version)?,
            receipt.context_digest,
            canonical,
        ],
    )?;
    let stored: Vec<u8> = tx.query_row(
        "SELECT canonical_json FROM workflow_receipts WHERE digest=?1",
        [digest],
        |row| row.get(0),
    )?;
    if stored != canonical {
        return Err(Error::Conflict(format!(
            "workflow receipt digest {digest} is bound to different bytes"
        )));
    }
    Ok(())
}

fn imported_run_id(plan: &CompiledPlan, dependencies: &[ImportedStepDependency]) -> Result<String> {
    let bytes = serde_json::to_vec(&(plan.digest(), dependencies))?;
    let mut hasher = Sha256::new();
    hasher.update(b"agentscm.imported-workflow-run.v1\0");
    hasher.update((bytes.len() as u64).to_be_bytes());
    hasher.update(bytes);
    Ok(format!("{:x}", hasher.finalize()))
}

fn validate_imported_graph(
    plan: &CompiledPlan,
    validated: &ValidatedImportedWorkflow,
) -> Result<()> {
    let native = validated
        .native_steps
        .iter()
        .collect::<std::collections::HashSet<_>>();
    if native.len() != validated.native_steps.len() {
        return Err(Error::Invalid(
            "duplicate native imported workflow step".into(),
        ));
    }
    let mut dependencies = std::collections::HashMap::new();
    for dependency in &validated.dependencies {
        if dependencies
            .insert(dependency.step_id.as_str(), dependency)
            .is_some()
        {
            return Err(Error::Invalid("duplicate imported step dependency".into()));
        }
    }
    let mut omitted = false;
    for step in plan.ordered_steps() {
        let satisfied = match &step.action {
            StepAction::Command { .. } => dependencies.get(step.id.as_str()).is_some_and(|value| {
                value.trust_class == ImportedTrustClass::EnrolledCommandCollector
            }),
            StepAction::ClaudeCodeReview { .. } => {
                dependencies.get(step.id.as_str()).is_some_and(|value| {
                    value.trust_class == ImportedTrustClass::EnrolledFiniteReviewController
                })
            }
            StepAction::SourcePaths { .. } => native.contains(&step.id),
            StepAction::RequireCapability { .. } | StepAction::AgentAction { .. } => false,
        };
        if step.required && !satisfied {
            return Err(Error::Conflict(format!(
                "required workflow step {} lacks independently validated evidence",
                step.id
            )));
        }
        omitted |= !step.required && !satisfied;
    }
    if dependencies
        .keys()
        .any(|step| !plan.ordered_steps().iter().any(|value| value.id == *step))
        || native
            .iter()
            .any(|step| !plan.ordered_steps().iter().any(|value| value.id == ***step))
    {
        return Err(Error::Invalid(
            "imported workflow completion names a foreign step".into(),
        ));
    }
    let expected = if omitted {
        RunState::CompletedWithOmissions
    } else {
        RunState::Succeeded
    };
    if validated.run_state != expected {
        return Err(Error::Invalid(
            "imported workflow state does not match graph completion".into(),
        ));
    }
    Ok(())
}

fn dependency_matches_transfer(
    dependency: &ImportedStepDependency,
    transfer: &CollectorTransfer,
) -> Result<bool> {
    Ok(dependency.transfer_digest == transfer.digest()?
        && dependency.task_id == transfer.header.task_id
        && dependency.task_digest == transfer.header.task_digest
        && dependency.attempt_id == transfer.header.attempt_id
        && dependency.attempt_digest == transfer.header.attempt_digest
        && dependency.fence == transfer.header.fence
        && dependency.registration_id == transfer.header.executor.registration_id
        && dependency.registration_epoch == transfer.header.executor.registration_epoch
        && dependency.verification_key_id == transfer.header.executor.verification_key_id
        && dependency.collector_profile_id == transfer.header.collector_profile_id
        && dependency.collector_profile_digest == transfer.header.collector_profile_digest
        && dependency.origin == transfer.header.origin
        && matches!(
            (&dependency.trust_class, &transfer.payload),
            (
                ImportedTrustClass::EnrolledCommandCollector,
                crate::skill_workflow::CollectorPayload::Command { .. }
            ) | (
                ImportedTrustClass::EnrolledFiniteReviewController,
                crate::skill_workflow::CollectorPayload::Review { .. }
            )
        ))
}

fn validate_live_dependency(
    connection: &Connection,
    dependency: &ImportedStepDependency,
    transfer: &CollectorTransfer,
) -> Result<()> {
    let registration = super::workflow_executors::read_current_registration(
        connection,
        &dependency.registration_id,
    )?
    .ok_or_else(|| Error::Conflict("imported evidence executor is no longer enrolled".into()))?;
    if registration.revocation.is_some()
        || registration.enrollment.repository != transfer.header.repository
        || registration.enrollment.registration_epoch != dependency.registration_epoch
        || registration.enrollment.verification_key_id != dependency.verification_key_id
        || !registration
            .enrollment
            .step_ids
            .contains(&dependency.step_id)
    {
        return Err(Error::Conflict(
            "imported evidence executor registration is stale or revoked".into(),
        ));
    }
    let profile = super::workflow_executors::read_collector_profile(
        connection,
        &dependency.collector_profile_id,
    )?
    .ok_or_else(|| Error::Conflict("imported evidence collector profile is missing".into()))?;
    if profile.digest()? != dependency.collector_profile_digest
        || profile.registration_id != dependency.registration_id
        || profile.registration_epoch != dependency.registration_epoch
        || profile.repository != transfer.header.repository
        || profile.step_id != dependency.step_id
    {
        return Err(Error::Conflict(
            "imported evidence collector profile is stale".into(),
        ));
    }
    transfer.validate_profile_identity(&profile)?;
    transfer.verify_signature(&registration.public_key_hex)
}

fn validate_receipt_dependencies(
    tx: &Transaction<'_>,
    context: &ReviewContext,
    receipt: &WorkflowReceipt,
    origins: &crate::skill_workflow::LiveImportedOrigins,
) -> Result<()> {
    if receipt.imported_dependencies.is_empty()
        || !origins.matches_repository(&context.change.repository)
    {
        return Err(Error::Conflict(
            "imported workflow admission lacks a live origin authorization guard".into(),
        ));
    }
    if receipt.imported_dependencies.len() > 32 {
        return Err(Error::Limit("too many imported dependencies".into()));
    }
    let expected_command = crate::execution::expected_subject(context)?;
    let mut command_satisfied =
        crate::execution::require_current_command_success(tx, context).is_ok();
    for dependency in &receipt.imported_dependencies {
        if !origins.permits(&dependency.origin) {
            return Err(Error::Conflict(
                "originating evidence actor or grant is no longer authorized".into(),
            ));
        }
        let length: i64 = tx.query_row(
            "SELECT length(canonical_json) FROM workflow_imported_evidence WHERE digest=?1",
            [&dependency.transfer_digest],
            |row| row.get(0),
        )?;
        if length < 0 || length > crate::skill_workflow::MAX_COLLECTOR_TRANSFER_BYTES as i64 {
            return Err(Error::Limit("imported transfer exceeds 16 MiB".into()));
        }
        let bytes: Vec<u8> = tx.query_row(
            "SELECT canonical_json FROM workflow_imported_evidence WHERE digest=?1 AND repository=?2 AND step_id=?3",
            params![dependency.transfer_digest, context.change.repository, dependency.step_id],
            |row| row.get(0),
        )?;
        let transfer = CollectorTransfer::from_slice(&bytes)?;
        if !dependency_matches_transfer(dependency, &transfer)? {
            return Err(Error::Invalid(
                "workflow receipt dependency differs from retained imported evidence".into(),
            ));
        }
        validate_live_dependency(tx, dependency, &transfer)?;
        if let crate::skill_workflow::CollectorPayload::Command { request, record } =
            &transfer.payload
        {
            crate::skill_workflow::validate_command_record(request, record)?;
            command_satisfied |= dependency.trust_class
                == ImportedTrustClass::EnrolledCommandCollector
                && expected_command.as_ref() == Some(&request.subject)
                && crate::skill_workflow::command_record_succeeded(record);
        }
    }
    if !command_satisfied {
        return Err(Error::Conflict(
            "required command evidence has no exact successful native or imported command".into(),
        ));
    }
    Ok(())
}

fn normalized_definition(definition: &WorkflowDefinition) -> WorkflowDefinition {
    let mut normalized = definition.clone();
    normalized.source.repository = PROFILE_REPOSITORY.into();
    normalized.source.revision = PROFILE_REVISION.into();
    normalized.source.policy_epoch = 0;
    for step in &mut normalized.steps {
        if let StepAction::Command {
            change,
            change_version,
            context_digest,
            ..
        } = &mut step.action
        {
            change.repository = PROFILE_REPOSITORY.into();
            change.id = PROFILE_CHANGE_ID.into();
            *change_version = 1;
            *context_digest = PROFILE_REVISION.into();
        }
    }
    normalized
}

fn validate_successful_graph(plan: &CompiledPlan, view: &RunView) -> Result<()> {
    if plan
        .ordered_steps()
        .iter()
        .any(|step| matches!(step.action, StepAction::AgentAction { .. }))
    {
        return Err(Error::Invalid(
            "productive actions cannot finalize native evidence".into(),
        ));
    }
    if !matches!(
        view.state,
        RunState::Succeeded | RunState::CompletedWithOmissions
    ) || plan.ordered_steps().len() != view.steps.len()
    {
        return Err(Error::Conflict(
            "workflow run did not complete with an admissible graph result".into(),
        ));
    }
    for (declared, observed) in plan.ordered_steps().iter().zip(&view.steps) {
        if declared.id != observed.id || declared.required != observed.required {
            return Err(Error::Conflict(
                "workflow result graph does not match its compiled plan".into(),
            ));
        }
        if declared.required && observed.state != StepState::Succeeded {
            return Err(Error::Conflict(format!(
                "required workflow step {} did not succeed",
                declared.id
            )));
        }
        if matches!(
            observed.state,
            StepState::Pending | StepState::Running | StepState::Cancelled
        ) {
            return Err(Error::Conflict(format!(
                "workflow step {} is not terminal and admissible",
                declared.id
            )));
        }
    }
    Ok(())
}

fn validate_skill_digest(value: &str) -> Result<()> {
    let Some(raw) = value.strip_prefix("sha256:") else {
        return Err(Error::Invalid(
            "workflow skill digest must use sha256".into(),
        ));
    };
    if raw.len() != 64
        || !raw
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err(Error::Invalid(
            "workflow skill digest must be lowercase SHA-256".into(),
        ));
    }
    Ok(())
}

fn domain_digest(domain: &[u8], canonical: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(domain);
    hasher.update((canonical.len() as u64).to_be_bytes());
    hasher.update(canonical);
    format!("{:x}", hasher.finalize())
}

#[cfg(test)]
mod tests {
    use tempfile::TempDir;

    use super::*;
    use crate::checks::SourcePathExpectation;
    use crate::context::{ChangeId, InputIdentity, REVIEW_CONTEXT_VERSION};
    use crate::skill_workflow::{SkillReference, StepDefinition, WorkflowSource};

    fn requirement(name: &str) -> WorkflowRequirement {
        WorkflowRequirement {
            version: REQUIREMENT_VERSION,
            target: "main".into(),
            definition: WorkflowDefinition {
                version: 1,
                name: name.into(),
                source: WorkflowSource {
                    repository: PROFILE_REPOSITORY.into(),
                    revision: PROFILE_REVISION.into(),
                    policy_epoch: 0,
                },
                skill: SkillReference {
                    entrypoint: "skills/review/SKILL.md".into(),
                    resources: Vec::new(),
                },
                steps: vec![StepDefinition {
                    id: "source".into(),
                    needs: Vec::new(),
                    required: true,
                    action: StepAction::SourcePaths {
                        expectations: vec![SourcePathExpectation {
                            path: "src/lib.rs".into(),
                            sha256: "a".repeat(64),
                        }],
                    },
                }],
            },
            skill_digest: format!("sha256:{}", "b".repeat(64)),
        }
    }

    fn context() -> ReviewContext {
        ReviewContext {
            version: REVIEW_CONTEXT_VERSION,
            change: ChangeId {
                repository: "c".repeat(32),
                id: "change-1".into(),
            },
            change_version: 3,
            revision: "d".repeat(64),
            base: None,
            target: "main".into(),
            policy_epoch: 9,
            intent: "test workflow binding".into(),
            acceptance_criteria: vec!["the workflow passes".into()],
            scope: vec!["source".into()],
            exclusions: Vec::new(),
            omissions: Vec::new(),
            dependencies: Vec::new(),
            recipe: InputIdentity::Digest {
                sha256: "e".repeat(64),
            },
            toolchain: InputIdentity::Digest {
                sha256: "f".repeat(64),
            },
            environment: InputIdentity::Digest {
                sha256: "1".repeat(64),
            },
            required_evidence: Vec::new(),
            required_capabilities: vec!["agentscm.review-context/v1".into()],
        }
    }

    #[test]
    fn install_is_immutable_idempotent_and_advances_policy_once() {
        let directory = TempDir::new().unwrap();
        let mut authority = Authority::open(directory.path()).unwrap();
        let required = requirement("required");
        let digest = authority.install_workflow_requirement(&required).unwrap();
        assert_eq!(authority.policy_epoch().unwrap(), 1);
        assert_eq!(
            authority.install_workflow_requirement(&required).unwrap(),
            digest
        );
        assert_eq!(authority.policy_epoch().unwrap(), 1);
        assert_eq!(
            authority.workflow_requirement("main").unwrap(),
            Some(required)
        );
        assert!(matches!(
            authority.install_workflow_requirement(&requirement("different")),
            Err(Error::Conflict(message)) if message.contains("different workflow requirement")
        ));
        assert_eq!(authority.policy_epoch().unwrap(), 1);
    }

    #[test]
    fn instantiation_binds_exact_subject_without_changing_profile() {
        let required = requirement("required");
        let original = required.clone();
        let context = context();
        let instantiated = required.instantiate(&context).unwrap();
        assert_eq!(instantiated.source.repository, context.change.repository);
        assert_eq!(instantiated.source.revision, context.revision);
        assert_eq!(instantiated.source.policy_epoch, context.policy_epoch);
        assert_eq!(required, original);
        assert_eq!(normalized_definition(&instantiated), required.definition);
    }

    /// A qualified non-imported receipt with real command evidence needs an OCI run, and
    /// finalization refuses without it. So a native receipt is stored directly for a context
    /// that requires command evidence no attempt supplies; a control context without the
    /// requirement accepts the same shape of receipt.
    #[cfg(unix)]
    #[test]
    fn missing_command_evidence_maps_to_not_qualified() {
        let directory = TempDir::new().unwrap();
        let engine = crate::store::Engine::open(directory.path()).unwrap();
        let revision = engine
            .checkpoint(
                None,
                &[crate::model::FileChange {
                    path: "a".into(),
                    content: Some(b"a".to_vec()),
                }],
            )
            .unwrap();
        let mut authority = Authority::open(directory.path()).unwrap();
        let required = requirement("required");
        authority.install_workflow_requirement(&required).unwrap();
        let repository = authority.repository_id().unwrap();
        let live = crate::skill_workflow::LiveImportedOrigins::native_only(repository.clone());
        let mut outcomes = Vec::new();
        for (id, command) in [("change-plain", false), ("change-command", true)] {
            let mut context = context();
            context.change = ChangeId {
                repository: repository.clone(),
                id: id.into(),
            };
            context.change_version = 1;
            context.revision = revision.clone();
            context.policy_epoch = authority.policy_epoch().unwrap();
            if command {
                context
                    .required_evidence
                    .push(crate::context::RequiredEvidence {
                        identity: crate::execution::COMMAND_EVIDENCE.into(),
                        digest: context.recipe.clone(),
                    });
                context
                    .required_capabilities
                    .insert(0, crate::execution::COMMAND_CAPABILITY.into());
            }
            authority.start_change(&context).unwrap();
            let receipt = WorkflowReceipt {
                version: RECEIPT_VERSION,
                requirement_digest: required.digest().unwrap(),
                run_id: "run-1".into(),
                plan_digest: format!("sha256:{}", "c".repeat(64)),
                skill_digest: required.skill_digest.clone(),
                repository: context.change.repository.clone(),
                revision: context.revision.clone(),
                target: context.target.clone(),
                policy_epoch: context.policy_epoch,
                change_repository: context.change.repository.clone(),
                change_id: context.change.id.clone(),
                change_version: context.change_version,
                context_digest: context.digest().unwrap(),
                run_state: RunState::Succeeded,
                imported_dependencies: Vec::new(),
            };
            authority
                .connection
                .execute(
                    "INSERT INTO workflow_receipts(digest,requirement_digest,run_id,plan_digest,\
                     skill_digest,repository,revision,target,policy_epoch,change_repository,\
                     change_id,change_version,context_digest,canonical_json) \
                     VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14)",
                    params![
                        receipt.digest().unwrap(),
                        receipt.requirement_digest,
                        receipt.run_id,
                        receipt.plan_digest,
                        receipt.skill_digest,
                        receipt.repository,
                        receipt.revision,
                        receipt.target,
                        receipt.policy_epoch,
                        receipt.change_repository,
                        receipt.change_id,
                        receipt.change_version,
                        receipt.context_digest,
                        receipt.canonical_bytes().unwrap(),
                    ],
                )
                .unwrap();
            outcomes.push((
                authority.current_qualification(&context, &live).unwrap(),
                receipt.digest().unwrap(),
            ));
        }
        assert_eq!(
            outcomes[0].0,
            CurrentEvidence::Receipt(outcomes[0].1.clone())
        );
        assert_eq!(outcomes[1].0, CurrentEvidence::NotQualified);
    }
}
