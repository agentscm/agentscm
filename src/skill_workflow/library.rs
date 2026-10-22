//! Reusable workflow templates loaded from exact native source bytes.

use std::path::Path;

use serde::{Deserialize, Serialize};

use super::{SkillReference, StepAction, StepDefinition, WorkflowDefinition, WorkflowSource};
use crate::authority::Authority;
use crate::checks::SourcePathExpectation;
use crate::context::ReviewContext;
use crate::store::{Engine, validate_id, validate_path};
use crate::{Error, Result};

const MAX_TEMPLATE_BYTES: usize = 256 * 1024;

/// One named version; publish another version at a separate immutable source path.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct WorkflowTemplate {
    pub version: u32,
    pub name: String,
    pub workflow_version: u32,
    pub skill: SkillReference,
    pub steps: Vec<TemplateStepDefinition>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct TemplateStepDefinition {
    pub id: String,
    pub needs: Vec<String>,
    pub required: bool,
    pub action: TemplateStepAction,
}

/// Subject bindings come from the current Change, never from template prose.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum TemplateStepAction {
    SourcePaths {
        expectations: Vec<SourcePathExpectation>,
    },
    RequireCapability {
        capability: String,
    },
    Command {
        runner: String,
    },
    ClaudeCodeReview {
        paths: Vec<String>,
        model: String,
    },
}

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct WorkflowTemplateReference {
    pub repository: String,
    pub revision: String,
    pub path: String,
    pub sha256: String,
    pub name: String,
    pub workflow_version: u32,
}

/// Verified immutable input, not an executable plan or an admission decision.
#[derive(Clone, Debug)]
pub struct WorkflowTemplateSnapshot {
    reference: WorkflowTemplateReference,
    template: WorkflowTemplate,
}

impl WorkflowTemplateSnapshot {
    pub fn reference(&self) -> &WorkflowTemplateReference {
        &self.reference
    }

    pub fn template(&self) -> &WorkflowTemplate {
        &self.template
    }

    pub fn instantiate(&self, context: &ReviewContext) -> Result<WorkflowDefinition> {
        context.validate()?;
        if context.change.repository != self.reference.repository
            || context.revision != self.reference.revision
        {
            return Err(Error::Conflict(
                "workflow template does not bind the Change repository and revision".into(),
            ));
        }
        let mut skill = self.template.skill.clone();
        if !skill.resources.contains(&self.reference.path) {
            skill.resources.push(self.reference.path.clone());
        }
        let context_digest = context.digest()?;
        let definition = WorkflowDefinition {
            version: 1,
            name: format!("{}:v{}", self.template.name, self.template.workflow_version),
            source: WorkflowSource {
                repository: context.change.repository.clone(),
                revision: context.revision.clone(),
                policy_epoch: context.policy_epoch,
            },
            skill,
            steps: self
                .template
                .steps
                .iter()
                .map(|step| StepDefinition {
                    id: step.id.clone(),
                    needs: step.needs.clone(),
                    required: step.required,
                    action: match &step.action {
                        TemplateStepAction::SourcePaths { expectations } => {
                            StepAction::SourcePaths {
                                expectations: expectations.clone(),
                            }
                        }
                        TemplateStepAction::RequireCapability { capability } => {
                            StepAction::RequireCapability {
                                capability: capability.clone(),
                            }
                        }
                        TemplateStepAction::Command { runner } => StepAction::Command {
                            runner: runner.clone(),
                            change: context.change.clone(),
                            change_version: context.change_version,
                            context_digest: context_digest.clone(),
                        },
                        TemplateStepAction::ClaudeCodeReview { paths, model } => {
                            StepAction::ClaudeCodeReview {
                                paths: paths.clone(),
                                model: model.clone(),
                            }
                        }
                    },
                })
                .collect(),
        };
        super::compiler::validate_definition(&definition)?;
        Ok(definition)
    }
}

/// Read one bounded regular manifest from an existing native repository.
pub fn load(data_dir: &Path, revision: &str, path: &str) -> Result<WorkflowTemplateSnapshot> {
    validate_id(revision)?;
    validate_path(path)?;
    let authority = Authority::open_read_only(data_dir)?;
    let repository = authority.repository_id()?;
    let engine = Engine::open(data_dir)?;
    let (source, budget) = engine.with_read_budget(2 * 1024 * 1024);
    let bytes = budget
        .finish(source.read_regular_file_bounded(revision, path, MAX_TEMPLATE_BYTES))?
        .ok_or_else(|| Error::Limit("workflow template exceeds 256 KiB".into()))?;
    let template: WorkflowTemplate = serde_json::from_slice(&bytes)?;
    if template.version != 1 || template.workflow_version == 0 {
        return Err(Error::Invalid(
            "workflow template requires schema version 1 and a positive workflow_version".into(),
        ));
    }
    super::compiler::validate_identifier("workflow template name", &template.name)?;
    // The compiler validates canonical names, all graph/action limits and skill
    // containment after binding. Check the manifest's own containment here too.
    let root = template
        .skill
        .entrypoint
        .rsplit_once('/')
        .map_or("", |(root, _)| root);
    if path == template.skill.entrypoint
        || (!root.is_empty() && !path.starts_with(&format!("{root}/")))
    {
        return Err(Error::Invalid(
            "workflow template must be a resource within its skill directory".into(),
        ));
    }
    Ok(WorkflowTemplateSnapshot {
        reference: WorkflowTemplateReference {
            repository,
            revision: revision.into(),
            path: path.into(),
            sha256: crate::store::digest(&bytes),
            name: template.name.clone(),
            workflow_version: template.workflow_version,
        },
        template,
    })
}
