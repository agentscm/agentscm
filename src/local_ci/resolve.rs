use std::collections::{BTreeMap, BTreeSet};

use crate::context::DependencyState;
use crate::{Error, Result};

use super::{
    ConfigBinding, ImmutablePlan, MAX_DOCUMENT_BYTES, MAX_WORKFLOW_BYTES, PermittedOmission,
    PlanSubject, RequiredCell, Requirements, SourceIdentity, VERSION, WorkflowFormat, raw_sha256,
};

/// Controller-owned immutable source access. Implementations must enforce the byte limit,
/// repository authorization and exact revision membership; a path is never a checkout path.
pub trait SourceReader {
    fn verify_source(&self, source: &SourceIdentity) -> Result<()>;
    fn read_file(&self, source: &SourceIdentity, path: &str, limit: usize) -> Result<Vec<u8>>;
}

/// Validated requirements read from an independently selected trusted source. The caller owns
/// authorization of that selection; matching hashes do not make a candidate configuration trusted.
#[derive(Clone, Debug)]
pub struct TrustedRequirements {
    binding: ConfigBinding,
    requirements: Requirements,
}

#[derive(Clone, Debug)]
pub struct PlanInputs {
    pub subject: PlanSubject,
    pub event_inputs: BTreeMap<String, String>,
    pub omissions: Vec<PermittedOmission>,
    /// Capabilities qualified by the controller for its selected execution profile. These are
    /// not discovered from worker claims or inferred from a successful process exit.
    pub qualified_capabilities: BTreeSet<String>,
}

/// A plan resolved against trusted requirements and exact immutable workflow bytes.
#[derive(Clone, Debug)]
pub struct ResolvedPlan {
    plan: ImmutablePlan,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AdapterKind {
    Command,
    Actions,
}

/// A read-only adapter descriptor. It is not runtime qualification or dispatch authority.
/// Both adapters see the entire same plan, including selected omissions; no filtering occurs.
pub struct AdapterInput<'a> {
    kind: AdapterKind,
    plan: &'a ImmutablePlan,
}

impl TrustedRequirements {
    pub fn load(binding: ConfigBinding, reader: &impl SourceReader) -> Result<Self> {
        binding.validate()?;
        reader.verify_source(&binding.source)?;
        let bytes = reader.read_file(&binding.source, &binding.path, MAX_DOCUMENT_BYTES)?;
        if bytes.len() > MAX_DOCUMENT_BYTES || raw_sha256(&bytes) != binding.sha256 {
            return Err(Error::Invalid(
                "trusted configuration bytes do not match binding".into(),
            ));
        }
        let requirements = Requirements::from_slice(&bytes)?;
        if requirements.repository != binding.source.repository() {
            return Err(Error::Invalid(
                "trusted configuration belongs to another repository".into(),
            ));
        }
        verify_workflows(&requirements, &binding.source, reader)?;
        Ok(Self {
            binding,
            requirements,
        })
    }

    pub fn binding(&self) -> &ConfigBinding {
        &self.binding
    }
    pub fn requirements(&self) -> &Requirements {
        &self.requirements
    }

    /// Recompute this for verification with independently observed subject/event inputs and
    /// currently trusted configuration/profile capabilities, never values copied from a receipt.
    pub fn resolve(&self, inputs: PlanInputs, reader: &impl SourceReader) -> Result<ResolvedPlan> {
        inputs.subject.validate()?;
        if inputs.subject.head().repository() != self.requirements.repository
            || !self.binding.source.same_kind(inputs.subject.head())
        {
            return Err(Error::Invalid(
                "plan source differs from trusted repository/source kind".into(),
            ));
        }
        let (head, base, result) = match &inputs.subject {
            PlanSubject::PrHead { head, base } => (head, base, None),
            PlanSubject::MergeResult { head, base, result } => (head, base, Some(result)),
        };
        for source in [Some(head), Some(base), result].into_iter().flatten() {
            reader.verify_source(source)?;
        }
        for omission in &inputs.omissions {
            if !self.requirements.permitted_omissions.contains(omission) {
                return Err(Error::Invalid(
                    "omission is not authorized by trusted requirements".into(),
                ));
            }
        }
        let plan = ImmutablePlan {
            version: VERSION,
            subject: inputs.subject,
            configuration: self.binding.clone(),
            policy_epoch: self.requirements.policy_epoch,
            requirements_sha256: self.requirements.digest()?,
            workflows: self.requirements.workflows.clone(),
            event_inputs: inputs.event_inputs,
            cells: self.requirements.cells.clone(),
            omissions: inputs.omissions,
        };
        plan.validate()?;
        for cell in &plan.cells {
            if plan
                .omissions
                .iter()
                .any(|o| o.job == cell.job && o.cell == cell.id)
            {
                continue;
            }
            for capability in &cell.required_capabilities {
                if !inputs.qualified_capabilities.contains(capability) {
                    return Err(Error::Invalid(format!(
                        "required capability is not qualified: {capability}"
                    )));
                }
            }
            for dependency in &cell.dependencies {
                if !matches!(dependency.state, DependencyState::Resolved { .. }) {
                    return Err(Error::Invalid(format!(
                        "required dependency is unresolved: {}",
                        dependency.identity
                    )));
                }
            }
        }
        verify_workflows(&self.requirements, plan.subject.execution_source(), reader)?;
        // Store the canonical inventory so adapter iteration is deterministic too.
        Ok(ResolvedPlan {
            plan: ImmutablePlan::from_slice(&plan.canonical_bytes()?)?,
        })
    }
}

fn verify_workflows(
    requirements: &Requirements,
    source: &SourceIdentity,
    reader: &impl SourceReader,
) -> Result<()> {
    for workflow in &requirements.workflows {
        let bytes = reader.read_file(source, &workflow.path, MAX_WORKFLOW_BYTES)?;
        if bytes.len() > MAX_WORKFLOW_BYTES || raw_sha256(&bytes) != workflow.sha256 {
            return Err(Error::Invalid(format!(
                "workflow differs from reviewed bytes: {}",
                workflow.path
            )));
        }
    }
    Ok(())
}

impl ResolvedPlan {
    pub fn plan(&self) -> &ImmutablePlan {
        &self.plan
    }

    pub fn adapter_input(&self, kind: AdapterKind) -> Result<AdapterInput<'_>> {
        if kind == AdapterKind::Actions
            && !self
                .plan
                .workflows
                .iter()
                .any(|w| w.format == WorkflowFormat::Actions)
        {
            return Err(Error::Invalid(
                "Actions adapter requires an explicitly bound Actions workflow".into(),
            ));
        }
        Ok(AdapterInput {
            kind,
            plan: &self.plan,
        })
    }
}

impl AdapterInput<'_> {
    pub fn kind(&self) -> AdapterKind {
        self.kind
    }
    pub fn plan(&self) -> &ImmutablePlan {
        self.plan
    }
    pub fn cells(&self) -> &[RequiredCell] {
        &self.plan.cells
    }
    pub fn omissions(&self) -> &[PermittedOmission] {
        &self.plan.omissions
    }
}
