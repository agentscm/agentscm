//! Productive actions share the workflow graph, but never confer admission authority.
use std::collections::BTreeMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use super::compiler::validate_identifier;
use super::{StepAction, WorkflowDefinition};
use crate::skill_bundle::SkillBundle;
use crate::store::{Engine, validate_path};
use crate::{Error, Result};

pub const MAX_ARTIFACT_BYTES: usize = 256 * 1024;
#[cfg(unix)]
const MAX_INPUT_BYTES: usize = 512 * 1024;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ArtifactDeclaration {
    pub name: String,
    pub media_type: String,
    pub max_bytes: usize,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ActionInput {
    Source {
        path: String,
    },
    Artifact {
        step: String,
        name: String,
    },
    Parameter {
        value: String,
    },
    /// Immutable per-run metadata, supplied by the verified event ingress as data.
    Event {
        field: String,
    },
}

/// Resource bytes are immutable; executables and provider selection are operator settings.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ActionDefinition {
    pub version: u32,
    pub inputs: BTreeMap<String, ActionInput>,
    pub output: ArtifactDeclaration,
    pub action: ProductiveAction,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ProductiveAction {
    Prompt {
        prompt: String,
        required_tools: Vec<String>,
    },
    Command {
        profile: String,
    },
}

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct CompiledAction {
    pub definition: ActionDefinition,
    pub prompt: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct CommandProfile {
    pub executable: PathBuf,
    pub args: Vec<String>,
}

/// Trusted-local capability, loaded explicitly outside candidate source. Commands can
/// exercise the operator account's privileges; this profile is not a sandbox.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ActionProfile {
    pub version: u32,
    pub id: String,
    pub provider: String,
    pub claude_executable: Option<PathBuf>,
    pub model: Option<String>,
    pub commands: BTreeMap<String, CommandProfile>,
    pub timeout_ms: u64,
    pub max_output_bytes: usize,
}

impl ActionProfile {
    pub fn validate(&self) -> Result<()> {
        validate_identifier("action profile", &self.id)?;
        if self.version != 1
            || !(10..=300_000).contains(&self.timeout_ms)
            || !(1..=MAX_ARTIFACT_BYTES).contains(&self.max_output_bytes)
            || self.commands.len() > 32
        {
            return Err(Error::Invalid(
                "invalid action profile limits or version".into(),
            ));
        }
        for (id, profile) in &self.commands {
            validate_identifier("command profile", id)?;
            if !profile.executable.is_absolute()
                || profile.args.len() > 64
                || profile.args.iter().map(String::len).sum::<usize>() > 16 * 1024
                || profile.args.iter().any(|arg| arg.contains('\0'))
            {
                return Err(Error::Invalid(
                    "invalid direct-argument command profile".into(),
                ));
            }
        }
        if let Some(model) = &self.model {
            crate::reviewer::validate_requested_model_selector(model)?;
        }
        Ok(())
    }

    #[cfg(unix)]
    pub fn executable_identities(
        &self,
        excluded_roots: &[&std::path::Path],
    ) -> Result<BTreeMap<String, crate::execution::executable::ExecutableIdentity>> {
        let mut identities = BTreeMap::new();
        if let Some(executable) = &self.claude_executable {
            identities.insert(
                "claude".into(),
                crate::execution::executable::capture(executable, excluded_roots)?,
            );
        }
        for (name, profile) in &self.commands {
            identities.insert(
                format!("command:{name}"),
                crate::execution::executable::capture(&profile.executable, excluded_roots)?,
            );
        }
        Ok(identities)
    }

    pub fn digest(&self) -> Result<String> {
        self.validate()?;
        #[cfg(unix)]
        {
            Ok(crate::store::digest(&serde_json::to_vec(&(
                self,
                self.executable_identities(&[])?,
            ))?))
        }
        #[cfg(not(unix))]
        {
            Err(Error::Invalid(
                "productive executable profiles require Unix".into(),
            ))
        }
    }
}

pub(crate) fn compile(
    engine: &Engine,
    workflow: &WorkflowDefinition,
    bundle: &SkillBundle,
) -> Result<BTreeMap<String, CompiledAction>> {
    let root = workflow
        .skill
        .entrypoint
        .rsplit_once('/')
        .map_or("", |(root, _)| root);
    let mut actions = BTreeMap::new();
    let read_resource = |path: &str| -> Result<&[u8]> {
        validate_path(path)?;
        let full = if root.is_empty() {
            path.to_owned()
        } else {
            format!("{root}/{path}")
        };
        if !workflow.skill.resources.contains(&full) {
            return Err(Error::Invalid(
                "action resource is not explicitly selected".into(),
            ));
        }
        bundle
            .resource(path)
            .ok_or_else(|| Error::Invalid("action resource is missing".into()))
    };
    for step in &workflow.steps {
        let StepAction::AgentAction { resource } = &step.action else {
            continue;
        };
        let bytes = read_resource(resource)?;
        if bytes.len() > 64 * 1024 {
            return Err(Error::Limit("action declaration exceeds 64 KiB".into()));
        }
        let definition: ActionDefinition = serde_json::from_slice(bytes)?;
        definition.validate()?;
        let prompt = match &definition.action {
            ProductiveAction::Prompt { prompt, .. } => {
                let text = std::str::from_utf8(read_resource(prompt)?)
                    .map_err(|_| Error::Invalid("action prompt must be UTF-8".into()))?;
                if text.trim().is_empty() || text.len() > 32 * 1024 {
                    return Err(Error::Limit(
                        "action prompt must contain 1..32768 bytes".into(),
                    ));
                }
                Some(text.to_owned())
            }
            ProductiveAction::Command { .. } => None,
        };
        for input in definition.inputs.values() {
            if let ActionInput::Source { path } = input {
                engine
                    .read_regular_file_bounded(&workflow.source.revision, path, MAX_ARTIFACT_BYTES)?
                    .ok_or_else(|| Error::Limit("action source input exceeds 256 KiB".into()))?;
            }
        }
        actions.insert(step.id.clone(), CompiledAction { definition, prompt });
    }
    for step in &workflow.steps {
        let Some(action) = actions.get(&step.id) else {
            continue;
        };
        for input in action.definition.inputs.values() {
            if let ActionInput::Artifact {
                step: producer,
                name,
            } = input
                && (!step.needs.contains(producer)
                    || actions
                        .get(producer)
                        .is_none_or(|action| action.definition.output.name != *name))
            {
                return Err(Error::Invalid(
                    "artifact input must name an output of an explicit dependency".into(),
                ));
            }
        }
    }
    Ok(actions)
}

impl ActionDefinition {
    pub fn validate(&self) -> Result<()> {
        validate_identifier("artifact name", &self.output.name)?;
        if self.version != 1
            || self.inputs.len() > 16
            || !(1..=MAX_ARTIFACT_BYTES).contains(&self.output.max_bytes)
            || !matches!(
                self.output.media_type.as_str(),
                "text/plain" | "text/markdown" | "application/json"
            )
        {
            return Err(Error::Invalid(
                "unsupported productive action declaration".into(),
            ));
        }
        let mut parameters = 0;
        for (name, input) in &self.inputs {
            validate_identifier("action input", name)?;
            match input {
                ActionInput::Source { path } => validate_path(path)?,
                ActionInput::Artifact { step, name } => {
                    validate_identifier("producer step", step)?;
                    validate_identifier("producer artifact", name)?;
                }
                ActionInput::Parameter { value } => parameters += value.len(),
                ActionInput::Event { field } => validate_identifier("event input field", field)?,
            }
        }
        if parameters > 32 * 1024 {
            return Err(Error::Limit("action parameters exceed 32 KiB".into()));
        }
        match &self.action {
            ProductiveAction::Prompt {
                prompt,
                required_tools,
            } => {
                validate_path(prompt)?;
                if required_tools.len() > 16 || required_tools.iter().any(|tool| tool.len() > 64) {
                    return Err(Error::Limit("too many required action tools".into()));
                }
            }
            ProductiveAction::Command { profile } => {
                validate_identifier("command profile", profile)?
            }
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ActionOutcome {
    Succeeded,
    Failed,
    Blocked,
    Cancelled,
    Uncertain,
    TimedOut,
    OutputLimit,
    InvalidOutput,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ArtifactMetadata {
    pub name: String,
    pub media_type: String,
    pub sha256: String,
    pub bytes: usize,
    pub run_id: String,
    pub step_id: String,
    pub revision: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ActionView {
    pub profile_digest: String,
    pub outcome: Option<ActionOutcome>,
    pub artifact: Option<ArtifactMetadata>,
    pub observed_model: Option<String>,
    pub verification: String,
}

#[cfg(unix)]
pub(crate) fn validate_output(declaration: &ArtifactDeclaration, bytes: &[u8]) -> Result<()> {
    if bytes.is_empty() || bytes.len() > declaration.max_bytes {
        return Err(Error::Limit(
            "action output is empty or exceeds its declaration".into(),
        ));
    }
    let text = std::str::from_utf8(bytes)
        .map_err(|_| Error::Invalid("action output must be UTF-8".into()))?;
    if text.trim().is_empty() {
        return Err(Error::Invalid("action output is blank".into()));
    }
    if declaration.media_type == "application/json" {
        let _: serde_json::Value = serde_json::from_slice(bytes)?;
    }
    Ok(())
}

#[cfg(unix)]
pub(crate) fn inputs(
    engine: &Engine,
    workflow: &WorkflowDefinition,
    action: &CompiledAction,
    event: &BTreeMap<String, String>,
    artifact: impl Fn(&str, &str) -> Result<Vec<u8>>,
) -> Result<BTreeMap<String, String>> {
    let mut result = BTreeMap::new();
    let mut total = 0;
    for (name, binding) in &action.definition.inputs {
        let bytes = match binding {
            ActionInput::Source { path } => engine
                .read_regular_file_bounded(&workflow.source.revision, path, MAX_ARTIFACT_BYTES)?
                .ok_or_else(|| Error::Limit("action source exceeds 256 KiB".into()))?,
            ActionInput::Artifact { step, name } => artifact(step, name)?,
            ActionInput::Parameter { value } => value.as_bytes().to_vec(),
            ActionInput::Event { field } => event
                .get(field)
                .ok_or_else(|| Error::Invalid("required pinned event field is missing".into()))?
                .as_bytes()
                .to_vec(),
        };
        total += bytes.len();
        if total > MAX_INPUT_BYTES {
            return Err(Error::Limit("action inputs exceed 512 KiB".into()));
        }
        result.insert(
            name.clone(),
            String::from_utf8(bytes)
                .map_err(|_| Error::Invalid("action inputs must be UTF-8".into()))?,
        );
    }
    Ok(result)
}

#[cfg(unix)]
#[path = "action_process.rs"]
mod process;
#[cfg(unix)]
pub use process::ActionExecutor;
