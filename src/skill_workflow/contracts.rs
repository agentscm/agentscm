//! Strict, bounded declarations for source-bound skill workflows.

use serde::{Deserialize, Serialize};

use crate::checks::SourcePathExpectation;
use crate::context::ChangeId;

/// A complete workflow declaration supplied by a trusted local controller.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct WorkflowDefinition {
    pub version: u32,
    pub name: String,
    pub source: WorkflowSource,
    pub skill: SkillReference,
    pub steps: Vec<StepDefinition>,
}

/// The immutable native source and current policy to which a workflow is bound.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct WorkflowSource {
    pub repository: String,
    pub revision: String,
    pub policy_epoch: u64,
}

/// A `SKILL.md` entrypoint and its explicitly selected immutable resources.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct SkillReference {
    pub entrypoint: String,
    pub resources: Vec<String>,
}

/// One node in the workflow dependency graph.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct StepDefinition {
    pub id: String,
    pub needs: Vec<String>,
    pub required: bool,
    pub action: StepAction,
}

/// The deliberately small action profile implemented by the initial runtime.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum StepAction {
    /// Productive output, never native admission evidence.
    AgentAction {
        resource: String,
    },
    SourcePaths {
        expectations: Vec<SourcePathExpectation>,
    },
    RequireCapability {
        capability: String,
    },
    /// Run an explicitly selected packaged command through an existing
    /// `command/v1` review context. `runner` is a relative skill-resource
    /// path; `SKILL.md` prose is never interpreted as an executable recipe.
    Command {
        runner: String,
        change: ChangeId,
        change_version: u64,
        context_digest: String,
    },
    /// Assess explicit native source paths using the pinned skill instructions.
    /// The Claude executable and subscription are selected by the operator,
    /// outside candidate source. The model has no tools or landing authority.
    ClaudeCodeReview {
        paths: Vec<String>,
        model: String,
    },
}
