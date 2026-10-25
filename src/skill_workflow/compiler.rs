//! Validation and immutable-source compilation for skill workflows.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::path::Path;

use serde::Serialize;
use sha2::{Digest, Sha256};

use super::contracts::{StepAction, StepDefinition, WorkflowDefinition};
use crate::authority::Authority;
use crate::checks::required_binding;
use crate::execution::{COMMAND_CAPABILITY, COMMAND_EVIDENCE, CommandRecipe};
use crate::skill_bundle::{
    MAX_SKILL_RESOURCE_BYTES, MAX_SKILL_RESOURCE_PATH_BYTES, MAX_SKILL_RESOURCES, SkillBundle,
    SkillResource,
};
use crate::store::{Engine, require_regular_entry, validate_id, validate_path};
use crate::{Error, Result};

const WORKFLOW_VERSION: u32 = 1;
const PLAN_DIGEST_DOMAIN: &[u8] = b"agentscm.skill-workflow.plan.v1\0";
const MAX_DEFINITION_BYTES: usize = 256 * 1024;
const MAX_STEPS: usize = 32;
const MAX_DEPENDENCIES: usize = 32;
const MAX_EXPECTATIONS: usize = 32;
const MAX_IDENTIFIER_BYTES: usize = 64;
const MAX_CLAUDE_REVIEW_SKILL_BYTES: usize = 4_096;
const MAX_CLAUDE_REVIEW_SOURCE_BYTES: usize = crate::reviewer::MAX_ROW_BYTES;
const MAX_CLAUDE_REVIEW_AGGREGATE_BYTES: usize = 64 * 1024;
const MAX_SKILL_COMPILATION_READ_BYTES: u64 = (MAX_SKILL_RESOURCE_BYTES as u64) * 16;

/// A validated workflow plan bound to an immutable skill bundle.
///
/// Fields are private and this type intentionally has no `Deserialize`
/// implementation. Callers can only obtain a validated instance through
/// [`compile`].
#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct CompiledPlan {
    pub(crate) definition: WorkflowDefinition,
    pub(crate) digest: String,
    pub(crate) skill_digest: String,
    pub(crate) ordered_steps: Vec<StepDefinition>,
    pub actions: BTreeMap<String, super::action::CompiledAction>,
    /// Exact resource hashes from the already bounded, verified compilation read.
    #[serde(skip)]
    pub(crate) instruction_sha256: BTreeMap<String, String>,
    #[serde(skip)]
    command_recipes: BTreeMap<String, CommandRecipe>,
}

impl CompiledPlan {
    pub fn definition(&self) -> &WorkflowDefinition {
        &self.definition
    }

    pub fn digest(&self) -> &str {
        &self.digest
    }

    pub fn skill_digest(&self) -> &str {
        &self.skill_digest
    }

    pub fn ordered_steps(&self) -> &[StepDefinition] {
        &self.ordered_steps
    }

    pub(crate) fn command_recipe(&self, step_id: &str) -> Option<&CommandRecipe> {
        self.command_recipes.get(step_id)
    }
}

#[derive(Serialize)]
struct CanonicalPlan<'a> {
    definition: &'a WorkflowDefinition,
    skill_digest: &'a str,
}

/// Validates and binds a declaration to the current authority and immutable source.
pub fn compile(data_dir: &Path, definition: &WorkflowDefinition) -> Result<CompiledPlan> {
    validate_definition_size(definition)?;
    let ordered_steps = validate_definition(definition)?;
    // Open authority first so a nonexistent or incomplete root is not created by
    // the engine. Both handles are then rooted in the same operator-selected path.
    let authority = Authority::open_read_only(data_dir)?;
    validate_authority(&authority, definition)?;
    let engine = Engine::open(data_dir)?;
    let (source, budget) = engine.with_read_budget(MAX_SKILL_COMPILATION_READ_BYTES);
    let (bundle, actions) = budget.finish((|| {
        source.load_revision(&definition.source.revision)?;
        let bundle = resolve_skill(&source, definition)?;
        validate_claude_review_sources(&source, definition)?;
        let actions = super::action::compile(&source, definition, &bundle)?;
        Ok((bundle, actions))
    })())?;
    validate_claude_review_skill(definition, &bundle)?;
    let command_recipes = compile_command_recipes(&authority, definition, &bundle)?;
    validate_authority(&authority, definition)?;
    let skill_digest = bundle.identity().as_str().to_owned();
    let digest = plan_digest(definition, &skill_digest)?;

    let root = skill_root(&definition.skill.entrypoint);
    let instruction_sha256 = std::iter::once(&definition.skill.entrypoint)
        .chain(definition.skill.resources.iter())
        .map(|path| {
            let relative = relative_skill_path(path, root);
            let bytes = bundle
                .resource(relative)
                .ok_or_else(|| Error::Invalid("compiled skill resource missing".into()))?;
            Ok((path.clone(), format!("{:x}", Sha256::digest(bytes))))
        })
        .collect::<Result<BTreeMap<_, _>>>()?;
    Ok(CompiledPlan {
        instruction_sha256,
        definition: definition.clone(),
        digest,
        skill_digest,
        ordered_steps,
        actions,
        command_recipes,
    })
}

/// Recompiles a plan against current source and policy and rejects any drift.
pub fn verify_current(data_dir: &Path, plan: &CompiledPlan) -> Result<()> {
    let current = compile(data_dir, plan.definition())?;
    if current != *plan {
        return Err(Error::Conflict(
            "compiled workflow plan no longer matches current source and policy".into(),
        ));
    }
    Ok(())
}

/// Preflight immutable source and structural inputs before updating a Change.
/// This does not create an executable plan or authorize a model call.
pub fn validate_sources(data_dir: &Path, definition: &WorkflowDefinition) -> Result<()> {
    validate_definition_size(definition)?;
    validate_definition(definition)?;
    let engine = Engine::open(data_dir)?;
    let (source, budget) = engine.with_read_budget(MAX_SKILL_COMPILATION_READ_BYTES);
    let bundle = budget.finish((|| {
        source.load_revision(&definition.source.revision)?;
        let bundle = resolve_skill(&source, definition)?;
        validate_claude_review_sources(&source, definition)?;
        super::action::compile(&source, definition, &bundle)?;
        Ok(bundle)
    })())?;
    validate_claude_review_skill(definition, &bundle)?;
    Ok(())
}

pub fn read_command_recipe(data_dir: &Path, revision: &str, path: &str) -> Result<CommandRecipe> {
    validate_path(path)?;
    let engine = Engine::open(data_dir)?;
    let (source, budget) = engine.with_read_budget(2 * 1024 * 1024);
    let bytes = budget
        .finish(source.read_regular_file_bounded(revision, path, 256 * 1024))?
        .ok_or_else(|| Error::Limit("command recipe exceeds 256 KiB".into()))?;
    let recipe: CommandRecipe = serde_json::from_slice(&bytes)?;
    recipe.validate()?;
    Ok(recipe)
}

/// Validates retained structure and identity without requiring current applicability.
/// This detects inconsistent local state; it does not authenticate its producer.
pub(super) fn validate_retained_definition(
    definition: &WorkflowDefinition,
    skill_digest: &str,
    recorded_plan_digest: &str,
) -> Result<Vec<StepDefinition>> {
    validate_definition_size(definition)?;
    let ordered = validate_definition(definition)?;
    let skill_hex = skill_digest
        .strip_prefix("sha256:")
        .ok_or_else(|| Error::Invalid("stored workflow skill digest does not use sha256".into()))?;
    validate_sha256("stored workflow skill digest", skill_hex)?;
    if plan_digest(definition, skill_digest)? != recorded_plan_digest {
        return Err(Error::Invalid(
            "stored workflow definition does not match its plan identity".into(),
        ));
    }
    Ok(ordered)
}

fn validate_definition_size(definition: &WorkflowDefinition) -> Result<()> {
    let bytes = serde_json::to_vec(definition)?;
    if bytes.len() > MAX_DEFINITION_BYTES {
        return Err(Error::Limit(format!(
            "workflow definition exceeds {MAX_DEFINITION_BYTES} bytes"
        )));
    }
    Ok(())
}

pub(super) fn validate_definition(definition: &WorkflowDefinition) -> Result<Vec<StepDefinition>> {
    if definition.version != WORKFLOW_VERSION {
        return Err(Error::Invalid(format!(
            "unsupported workflow definition version {}",
            definition.version
        )));
    }
    validate_identifier("workflow name", &definition.name)?;
    validate_repository(&definition.source.repository)?;
    validate_id(&definition.source.revision)?;
    validate_skill_reference(definition)?;
    if definition
        .steps
        .iter()
        .any(|step| matches!(step.action, StepAction::ClaudeCodeReview { .. }))
        && !definition
            .steps
            .iter()
            .any(|step| matches!(step.action, StepAction::Command { .. }))
    {
        return Err(Error::Invalid(
            "Claude review requires an exact command review context".into(),
        ));
    }

    if definition.steps.is_empty() || definition.steps.len() > MAX_STEPS {
        return Err(Error::Limit(format!(
            "workflow must contain between 1 and {MAX_STEPS} steps"
        )));
    }

    let mut indexes = HashMap::with_capacity(definition.steps.len());
    for (index, step) in definition.steps.iter().enumerate() {
        validate_identifier("step ID", &step.id)?;
        if indexes.insert(step.id.as_str(), index).is_some() {
            return Err(Error::Invalid(format!(
                "duplicate workflow step ID: {}",
                step.id
            )));
        }
        validate_step(step)?;
        validate_step_binding(definition, step)?;
    }

    stable_topological_order(&definition.steps, &indexes)
}

fn validate_step_binding(definition: &WorkflowDefinition, step: &StepDefinition) -> Result<()> {
    let StepAction::Command { runner, change, .. } = &step.action else {
        return Ok(());
    };
    if change.repository != definition.source.repository {
        return Err(Error::Conflict(format!(
            "command step {} change repository does not match workflow source",
            step.id
        )));
    }
    if runner.len() > MAX_SKILL_RESOURCE_PATH_BYTES {
        return Err(Error::Limit(format!(
            "command runner path exceeds {MAX_SKILL_RESOURCE_PATH_BYTES} relative bytes"
        )));
    }
    let root = skill_root(&definition.skill.entrypoint);
    if !definition
        .skill
        .resources
        .iter()
        .any(|path| relative_skill_path(path, root) == runner)
    {
        return Err(Error::Invalid(format!(
            "command step {} runner is not an explicitly declared skill resource: {runner}",
            step.id
        )));
    }
    Ok(())
}

fn validate_step(step: &StepDefinition) -> Result<()> {
    if step.needs.len() > MAX_DEPENDENCIES {
        return Err(Error::Limit(format!(
            "step {} exceeds {MAX_DEPENDENCIES} dependencies",
            step.id
        )));
    }
    let mut dependencies = HashSet::with_capacity(step.needs.len());
    for dependency in &step.needs {
        validate_identifier("step dependency", dependency)?;
        if dependency == &step.id {
            return Err(Error::Invalid(format!(
                "step {} depends on itself",
                step.id
            )));
        }
        if !dependencies.insert(dependency.as_str()) {
            return Err(Error::Invalid(format!(
                "step {} contains duplicate dependency {dependency}",
                step.id
            )));
        }
    }

    match &step.action {
        StepAction::AgentAction { resource } => {
            validate_path(resource)?;
        }
        StepAction::SourcePaths { expectations } => {
            if expectations.is_empty() || expectations.len() > MAX_EXPECTATIONS {
                return Err(Error::Limit(format!(
                    "source-paths step {} must contain between 1 and {MAX_EXPECTATIONS} expectations",
                    step.id
                )));
            }
            let mut paths = HashSet::with_capacity(expectations.len());
            for expectation in expectations {
                validate_path(&expectation.path)?;
                validate_sha256("expected source-path digest", &expectation.sha256)?;
                if !paths.insert(expectation.path.as_str()) {
                    return Err(Error::Invalid(format!(
                        "step {} contains duplicate source path {}",
                        step.id, expectation.path
                    )));
                }
            }
        }
        StepAction::RequireCapability { capability } => {
            validate_identifier("required capability", capability)?;
        }
        StepAction::ClaudeCodeReview { paths, model } => {
            if paths.is_empty() || paths.len() > 32 {
                return Err(Error::Limit(
                    "Claude review requires 1..32 explicit source paths".into(),
                ));
            }
            let mut unique = HashSet::new();
            for path in paths {
                validate_path(path)?;
                if !unique.insert(path) {
                    return Err(Error::Invalid("duplicate Claude review source path".into()));
                }
            }
            crate::reviewer::validate_requested_model_selector(model)?;
        }
        StepAction::Command {
            runner,
            change,
            change_version,
            context_digest,
        } => {
            validate_path(runner)?;
            if runner == "SKILL.md" {
                return Err(Error::Invalid(
                    "command runner must be an explicitly declared skill resource".into(),
                ));
            }
            change.validate()?;
            if change_version == &0 || *change_version > i64::MAX as u64 {
                return Err(Error::Invalid(
                    "command change version must be a positive signed 64-bit integer".into(),
                ));
            }
            validate_sha256("command context digest", context_digest)?;
        }
    }
    Ok(())
}

fn stable_topological_order(
    steps: &[StepDefinition],
    indexes: &HashMap<&str, usize>,
) -> Result<Vec<StepDefinition>> {
    let mut indegree = vec![0usize; steps.len()];
    let mut dependents = vec![Vec::new(); steps.len()];
    for (index, step) in steps.iter().enumerate() {
        indegree[index] = step.needs.len();
        for dependency in &step.needs {
            let Some(&dependency_index) = indexes.get(dependency.as_str()) else {
                return Err(Error::Invalid(format!(
                    "step {} depends on missing step {dependency}",
                    step.id
                )));
            };
            dependents[dependency_index].push(index);
        }
    }

    let mut ready: BTreeSet<usize> = indegree
        .iter()
        .enumerate()
        .filter_map(|(index, &count)| (count == 0).then_some(index))
        .collect();
    let mut ordered = Vec::with_capacity(steps.len());
    while let Some(index) = ready.pop_first() {
        ordered.push(steps[index].clone());
        for &dependent in &dependents[index] {
            indegree[dependent] -= 1;
            if indegree[dependent] == 0 {
                ready.insert(dependent);
            }
        }
    }
    if ordered.len() != steps.len() {
        return Err(Error::Invalid(
            "workflow step dependencies contain a cycle".into(),
        ));
    }
    Ok(ordered)
}

fn validate_skill_reference(definition: &WorkflowDefinition) -> Result<()> {
    let reference = &definition.skill;
    if reference.resources.len() >= MAX_SKILL_RESOURCES {
        return Err(Error::Limit(format!(
            "skill bundle allows at most {} explicit resources plus SKILL.md",
            MAX_SKILL_RESOURCES - 1
        )));
    }
    validate_path(&reference.entrypoint)?;
    if reference.entrypoint.rsplit('/').next() != Some("SKILL.md") {
        return Err(Error::Invalid(
            "skill entrypoint must end in exact SKILL.md".into(),
        ));
    }

    let root = skill_root(&reference.entrypoint);
    let mut resources = HashSet::with_capacity(reference.resources.len());
    for resource in &reference.resources {
        validate_path(resource)?;
        if is_within_skill_root(resource, root)
            && relative_skill_path(resource, root).len() > MAX_SKILL_RESOURCE_PATH_BYTES
        {
            return Err(Error::Limit(format!(
                "skill resource path exceeds {MAX_SKILL_RESOURCE_PATH_BYTES} relative bytes"
            )));
        }
        if !is_within_skill_root(resource, root) {
            return Err(Error::Invalid(format!(
                "skill resource is outside the entrypoint directory: {resource}"
            )));
        }
        if resource == &reference.entrypoint {
            return Err(Error::Invalid(
                "skill resources must not repeat the automatic entrypoint".into(),
            ));
        }
        if !resources.insert(resource.as_str()) {
            return Err(Error::Invalid(format!(
                "duplicate skill resource path: {resource}"
            )));
        }
    }
    Ok(())
}

fn resolve_skill(engine: &Engine, definition: &WorkflowDefinition) -> Result<SkillBundle> {
    let reference = &definition.skill;
    let root = skill_root(&reference.entrypoint);
    let mut resources = Vec::with_capacity(reference.resources.len() + 1);
    resources.push(read_skill_resource(
        engine,
        &definition.source.revision,
        &reference.entrypoint,
        "SKILL.md",
    )?);
    for path in &reference.resources {
        let relative = relative_skill_path(path, root);
        resources.push(read_skill_resource(
            engine,
            &definition.source.revision,
            path,
            relative,
        )?);
    }
    SkillBundle::new(resources)
}

fn validate_claude_review_skill(
    definition: &WorkflowDefinition,
    bundle: &SkillBundle,
) -> Result<()> {
    if !definition
        .steps
        .iter()
        .any(|step| matches!(step.action, StepAction::ClaudeCodeReview { .. }))
    {
        return Ok(());
    }
    let bytes = bundle
        .resource("SKILL.md")
        .ok_or_else(|| Error::Invalid("Claude review SKILL.md is missing".into()))?;
    if bytes.len() > MAX_CLAUDE_REVIEW_SKILL_BYTES {
        return Err(Error::Limit(format!(
            "Claude review SKILL.md exceeds {MAX_CLAUDE_REVIEW_SKILL_BYTES} bytes"
        )));
    }
    let text = std::str::from_utf8(bytes)
        .map_err(|_| Error::Invalid("Claude review SKILL.md must be UTF-8".into()))?;
    if text.trim().is_empty() {
        return Err(Error::Invalid(
            "Claude review SKILL.md must contain review instructions".into(),
        ));
    }
    Ok(())
}

fn validate_claude_review_sources(engine: &Engine, definition: &WorkflowDefinition) -> Result<()> {
    for step in &definition.steps {
        let StepAction::ClaudeCodeReview { paths, .. } = &step.action else {
            continue;
        };
        let mut aggregate_bytes = 0usize;
        for path in paths {
            let bytes = engine
                .read_regular_file_bounded(
                    &definition.source.revision,
                    path,
                    MAX_CLAUDE_REVIEW_SOURCE_BYTES,
                )?
                .ok_or_else(|| {
                    Error::Limit(format!(
                        "Claude review source {path} exceeds {MAX_CLAUDE_REVIEW_SOURCE_BYTES} bytes"
                    ))
                })?;
            let text = std::str::from_utf8(&bytes).map_err(|_| {
                Error::Invalid(format!("Claude review source {path} must be UTF-8"))
            })?;
            let lines = text.lines().count();
            if lines == 0 || lines > 16_384 {
                return Err(Error::Limit(format!(
                    "Claude review source {path} has unsupported line count"
                )));
            }
            aggregate_bytes = aggregate_bytes.checked_add(bytes.len()).ok_or_else(|| {
                Error::Limit("Claude review source aggregate size overflowed".into())
            })?;
            if aggregate_bytes > MAX_CLAUDE_REVIEW_AGGREGATE_BYTES {
                return Err(Error::Limit(format!(
                    "Claude review source exceeds the {MAX_CLAUDE_REVIEW_AGGREGATE_BYTES} byte aggregate profile"
                )));
            }
        }
    }
    Ok(())
}

fn compile_command_recipes(
    authority: &Authority,
    definition: &WorkflowDefinition,
    bundle: &SkillBundle,
) -> Result<BTreeMap<String, CommandRecipe>> {
    let mut recipes = BTreeMap::new();
    for step in &definition.steps {
        let StepAction::Command {
            runner,
            change,
            change_version,
            context_digest,
        } = &step.action
        else {
            continue;
        };
        // Structural validation already checked repository and explicit resource
        // membership. Here resolve the declared bytes and current authority.
        let bytes = bundle.resource(runner).ok_or_else(|| {
            Error::Invalid(format!(
                "command step {} runner is missing from the resolved skill bundle: {runner}",
                step.id
            ))
        })?;
        let recipe: CommandRecipe = serde_json::from_slice(bytes).map_err(|error| {
            Error::Invalid(format!(
                "command step {} runner is not a valid command recipe: {error}",
                step.id
            ))
        })?;
        recipe.validate()?;

        let state = authority.change(change)?;
        if state.version != *change_version || state.context_digest != *context_digest {
            return Err(Error::Conflict(format!(
                "command step {} change no longer matches version {change_version} and context {context_digest}",
                step.id
            )));
        }
        let context = authority.review_context(context_digest)?;
        if context.change != *change
            || context.change_version != *change_version
            || context.revision != definition.source.revision
            || context.policy_epoch != definition.source.policy_epoch
            || context.digest()? != *context_digest
        {
            return Err(Error::Conflict(format!(
                "command step {} review context does not bind the workflow source",
                step.id
            )));
        }
        let Some((recipe_digest, toolchain_digest, environment_digest)) =
            required_binding(&context, COMMAND_CAPABILITY, COMMAND_EVIDENCE)?
        else {
            return Err(Error::Invalid(format!(
                "command step {} review context does not request capability {COMMAND_CAPABILITY}",
                step.id
            )));
        };
        if recipe.digest()? != recipe_digest
            || recipe.toolchain_sha256 != toolchain_digest
            || recipe.environment_sha256 != environment_digest
        {
            return Err(Error::Conflict(format!(
                "command step {} recipe, toolchain or environment does not match its review context",
                step.id
            )));
        }
        recipes.insert(step.id.clone(), recipe);
    }
    Ok(recipes)
}

fn read_skill_resource(
    engine: &Engine,
    revision: &str,
    native_path: &str,
    bundle_path: &str,
) -> Result<SkillResource> {
    let (kind, bytes) = engine.read_entry(revision, native_path)?;
    require_regular_entry("skill workflow compiler", native_path, kind)?;
    Ok(SkillResource::new(bundle_path, bytes))
}

fn validate_authority(authority: &Authority, definition: &WorkflowDefinition) -> Result<()> {
    let repository = authority.repository_id()?;
    if repository != definition.source.repository {
        return Err(Error::Conflict(format!(
            "workflow repository {} does not match current repository {repository}",
            definition.source.repository
        )));
    }
    let policy_epoch = authority.policy_epoch()?;
    if policy_epoch != definition.source.policy_epoch {
        return Err(Error::Conflict(format!(
            "workflow policy epoch {} is not current epoch {policy_epoch}",
            definition.source.policy_epoch
        )));
    }
    Ok(())
}

fn plan_digest(definition: &WorkflowDefinition, skill_digest: &str) -> Result<String> {
    let canonical = serde_json::to_vec(&CanonicalPlan {
        definition,
        skill_digest,
    })?;
    let mut hasher = Sha256::new();
    hasher.update(PLAN_DIGEST_DOMAIN);
    hasher.update((canonical.len() as u64).to_be_bytes());
    hasher.update(canonical);
    Ok(format!("sha256:{:x}", hasher.finalize()))
}

pub(super) fn validate_identifier(kind: &str, value: &str) -> Result<()> {
    if value.is_empty()
        || value.len() > MAX_IDENTIFIER_BYTES
        || !value.bytes().all(|byte| {
            byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b':' | b'/')
        })
    {
        return Err(Error::Invalid(format!(
            "{kind} must be 1 to {MAX_IDENTIFIER_BYTES} canonical ASCII characters"
        )));
    }
    Ok(())
}

fn validate_repository(repository: &str) -> Result<()> {
    if repository.len() != 32
        || !repository
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err(Error::Invalid(
            "workflow repository must be 32 lowercase hex characters".into(),
        ));
    }
    Ok(())
}

fn validate_sha256(kind: &str, digest: &str) -> Result<()> {
    if digest.len() != 64
        || !digest
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err(Error::Invalid(format!(
            "{kind} must be 64 lowercase hex characters"
        )));
    }
    Ok(())
}

fn skill_root(entrypoint: &str) -> &str {
    entrypoint.rsplit_once('/').map_or("", |(root, _)| root)
}

fn is_within_skill_root(path: &str, root: &str) -> bool {
    if root.is_empty() {
        true
    } else {
        path.strip_prefix(root)
            .is_some_and(|suffix| suffix.starts_with('/'))
    }
}

fn relative_skill_path<'a>(path: &'a str, root: &str) -> &'a str {
    if root.is_empty() {
        path
    } else {
        path.strip_prefix(root)
            .and_then(|suffix| suffix.strip_prefix('/'))
            .expect("validated skill resource is within its root")
    }
}

#[cfg(test)]
mod tests {
    #[cfg(unix)]
    use std::collections::BTreeMap;

    use tempfile::tempdir;

    use super::*;
    use crate::checks::SourcePathExpectation;
    #[cfg(unix)]
    use crate::context::{ChangeId, InputIdentity, RequiredEvidence, ReviewContext};
    #[cfg(unix)]
    use crate::execution::{
        COMMAND_CAPABILITY, COMMAND_EVIDENCE, CommandRecipe, ExecutionLimits, SyscallPolicy,
        environment_digest,
    };
    use crate::model::{EntryChange, EntryKind};

    fn fixture() -> (tempfile::TempDir, WorkflowDefinition) {
        let root = tempdir().unwrap();
        let engine = Engine::open(root.path()).unwrap();
        let authority = Authority::open(root.path()).unwrap();
        let repository = authority.repository_id().unwrap();
        let revision = engine
            .checkpoint_entries(
                None,
                &[
                    EntryChange {
                        path: "skills/a/SKILL.md".into(),
                        content: Some(b"# A\n".to_vec()),
                        kind: EntryKind::Regular,
                    },
                    EntryChange {
                        path: "skills/a/ref.txt".into(),
                        content: Some(b"reference".to_vec()),
                        kind: EntryKind::Regular,
                    },
                    EntryChange {
                        path: "src/lib.rs".into(),
                        content: Some(b"fn example() {}\n".to_vec()),
                        kind: EntryKind::Regular,
                    },
                ],
            )
            .unwrap();
        drop(authority);
        drop(engine);
        let definition = WorkflowDefinition {
            version: 1,
            name: "qualification".into(),
            source: super::super::contracts::WorkflowSource {
                repository,
                revision,
                policy_epoch: 0,
            },
            skill: super::super::contracts::SkillReference {
                entrypoint: "skills/a/SKILL.md".into(),
                resources: vec!["skills/a/ref.txt".into()],
            },
            steps: vec![
                StepDefinition {
                    id: "finish".into(),
                    needs: vec!["check".into()],
                    required: false,
                    action: StepAction::RequireCapability {
                        capability: "model.review".into(),
                    },
                },
                StepDefinition {
                    id: "check".into(),
                    needs: vec![],
                    required: true,
                    action: StepAction::SourcePaths {
                        expectations: vec![SourcePathExpectation {
                            path: "src/lib.rs".into(),
                            sha256: crate::store::digest(b"fn example() {}\n"),
                        }],
                    },
                },
            ],
        };
        (root, definition)
    }

    #[cfg(unix)]
    fn command_recipe() -> CommandRecipe {
        let environment = BTreeMap::new();
        CommandRecipe {
            version: 1,
            image: format!("sha256:{}", "b".repeat(64)),
            argv: vec!["/bin/true".into()],
            environment_sha256: environment_digest(&environment).unwrap(),
            environment,
            toolchain_sha256: "a".repeat(64),
            limits: ExecutionLimits {
                timeout_ms: 1_000,
                max_output_bytes: 4_096,
                memory_bytes: 64 * 1024 * 1024,
                scratch_bytes: 4 * 1024 * 1024,
                pids: 16,
                cpu_millis: 1_000,
            },
            syscall_policy: SyscallPolicy::DockerBuiltin,
            no_new_privileges: true,
        }
    }

    #[cfg(unix)]
    fn command_fixture() -> (tempfile::TempDir, WorkflowDefinition, CommandRecipe) {
        let root = tempdir().unwrap();
        let engine = Engine::open(root.path()).unwrap();
        let mut authority = Authority::open(root.path()).unwrap();
        let repository = authority.repository_id().unwrap();
        let recipe = command_recipe();
        let revision = engine
            .checkpoint_entries(
                None,
                &[
                    EntryChange {
                        path: "skills/a/SKILL.md".into(),
                        content: Some(b"# Command skill\n".to_vec()),
                        kind: EntryKind::Regular,
                    },
                    EntryChange {
                        path: "skills/a/runner.json".into(),
                        content: Some(serde_json::to_vec(&recipe).unwrap()),
                        kind: EntryKind::Regular,
                    },
                    EntryChange {
                        path: "src/lib.rs".into(),
                        content: Some(b"fn example() {}\n".to_vec()),
                        kind: EntryKind::Regular,
                    },
                ],
            )
            .unwrap();
        let digest = |sha256: &str| InputIdentity::Digest {
            sha256: sha256.to_owned(),
        };
        let context = ReviewContext {
            version: 1,
            change: ChangeId {
                repository: repository.clone(),
                id: "workflow-command".into(),
            },
            change_version: 1,
            revision: revision.clone(),
            base: None,
            target: "main".into(),
            policy_epoch: 0,
            intent: "run the declared packaged command".into(),
            acceptance_criteria: vec!["command exits successfully".into()],
            scope: vec!["src/lib.rs".into()],
            exclusions: vec![],
            omissions: vec![],
            dependencies: vec![],
            recipe: digest(&recipe.digest().unwrap()),
            toolchain: digest(&recipe.toolchain_sha256),
            environment: digest(&recipe.environment_sha256),
            required_evidence: vec![RequiredEvidence {
                identity: COMMAND_EVIDENCE.into(),
                digest: digest(&recipe.digest().unwrap()),
            }],
            required_capabilities: vec![
                "agentscm.review-context/v1".into(),
                COMMAND_CAPABILITY.into(),
            ],
        };
        let context_digest = context.digest().unwrap();
        authority.start_change(&context).unwrap();
        let definition = WorkflowDefinition {
            version: 1,
            name: "command-workflow".into(),
            source: super::super::contracts::WorkflowSource {
                repository,
                revision,
                policy_epoch: 0,
            },
            skill: super::super::contracts::SkillReference {
                entrypoint: "skills/a/SKILL.md".into(),
                resources: vec!["skills/a/runner.json".into()],
            },
            steps: vec![StepDefinition {
                id: "run".into(),
                needs: vec![],
                required: true,
                action: StepAction::Command {
                    runner: "runner.json".into(),
                    change: context.change,
                    change_version: 1,
                    context_digest,
                },
            }],
        };
        drop(authority);
        drop(engine);
        (root, definition, recipe)
    }

    #[test]
    fn resource_count_is_rejected_before_opening_source() {
        let (root, mut definition) = fixture();
        definition.skill.resources = (0..MAX_SKILL_RESOURCES)
            .map(|index| format!("skills/a/resource-{index}.txt"))
            .collect();
        let missing_root = root.path().join("unopened");
        assert!(matches!(
            compile(&missing_root, &definition),
            Err(Error::Limit(_))
        ));
        assert!(!missing_root.exists());
    }

    #[test]
    fn relative_resource_path_limit_is_checked_before_opening_source() {
        let (root, mut definition) = fixture();
        let relative = format!("{}r", "a/".repeat(256));
        definition.skill.resources = vec![format!("skills/a/{relative}")];
        let missing_root = root.path().join("unopened");
        assert!(matches!(
            compile(&missing_root, &definition),
            Err(Error::Limit(_))
        ));
        assert!(!missing_root.exists());
    }

    #[test]
    fn retained_identity_validation_preserves_history_and_rejects_changed_definition() {
        let (root, definition) = fixture();
        let plan = compile(root.path(), &definition).unwrap();
        Authority::open(root.path())
            .unwrap()
            .advance_policy(0)
            .unwrap();
        let ordered =
            validate_retained_definition(&definition, plan.skill_digest(), plan.digest()).unwrap();
        assert_eq!(ordered.as_slice(), plan.ordered_steps());
        let mut changed = definition;
        changed.name = "changed".into();
        assert!(
            validate_retained_definition(&changed, plan.skill_digest(), plan.digest(),).is_err()
        );
        assert!(
            validate_retained_definition(plan.definition(), "sha256:bad", plan.digest(),).is_err()
        );
    }

    #[test]
    fn compiles_source_bound_plan_in_stable_dependency_order() {
        let (root, definition) = fixture();
        let first = compile(root.path(), &definition).unwrap();
        let second = compile(root.path(), &definition).unwrap();

        assert_eq!(first, second);
        assert_eq!(first.definition(), &definition);
        assert_eq!(first.ordered_steps()[0].id, "check");
        assert_eq!(first.ordered_steps()[1].id, "finish");
        assert!(first.digest().starts_with("sha256:"));
        assert!(first.skill_digest().starts_with("sha256:"));
        verify_current(root.path(), &first).unwrap();
    }

    #[test]
    fn rejects_missing_dependencies_and_cycles() {
        let (root, mut definition) = fixture();
        definition.steps[0].needs = vec!["missing".into()];
        assert!(compile(root.path(), &definition).is_err());

        definition.steps[0].needs = vec!["check".into()];
        definition.steps[1].needs = vec!["finish".into()];
        assert!(compile(root.path(), &definition).is_err());
    }

    #[test]
    fn rejects_repository_policy_and_skill_kind_mismatches() {
        let (root, mut definition) = fixture();
        definition.source.repository = "0".repeat(32);
        assert!(compile(root.path(), &definition).is_err());

        let (root, mut definition) = fixture();
        definition.source.policy_epoch = 1;
        assert!(compile(root.path(), &definition).is_err());

        let root = tempdir().unwrap();
        let engine = Engine::open(root.path()).unwrap();
        let authority = Authority::open(root.path()).unwrap();
        let repository = authority.repository_id().unwrap();
        let revision = engine
            .checkpoint_entries(
                None,
                &[EntryChange {
                    path: "SKILL.md".into(),
                    content: Some(b"target".to_vec()),
                    kind: EntryKind::Symlink,
                }],
            )
            .unwrap();
        drop(authority);
        drop(engine);
        let mut definition = fixture().1;
        definition.source.repository = repository;
        definition.source.revision = revision;
        definition.skill.entrypoint = "SKILL.md".into();
        definition.skill.resources.clear();
        assert!(compile(root.path(), &definition).is_err());
    }

    #[test]
    fn strict_action_serde_rejects_unknown_fields() {
        let action = serde_json::from_str::<StepAction>(
            r#"{"kind":"require_capability","capability":"model","extra":true}"#,
        );
        assert!(action.is_err());
    }

    #[cfg(unix)]
    #[test]
    fn claude_review_skill_profile_is_preflighted_by_compile_and_source_validation() {
        for (skill, expect_limit) in [
            (b" \n\t".to_vec(), false),
            (vec![0xff], false),
            (vec![b'x'; MAX_CLAUDE_REVIEW_SKILL_BYTES + 1], true),
        ] {
            let (root, mut definition, recipe) = command_fixture();
            definition.steps.push(StepDefinition {
                id: "review".into(),
                needs: vec!["run".into()],
                required: true,
                action: StepAction::ClaudeCodeReview {
                    paths: vec!["src/lib.rs".into()],
                    model: "sonnet".into(),
                },
            });
            let engine = Engine::open(root.path()).unwrap();
            definition.source.revision = engine
                .checkpoint_entries(
                    None,
                    &[
                        EntryChange {
                            path: "skills/a/SKILL.md".into(),
                            content: Some(skill),
                            kind: EntryKind::Regular,
                        },
                        EntryChange {
                            path: "skills/a/runner.json".into(),
                            content: Some(serde_json::to_vec(&recipe).unwrap()),
                            kind: EntryKind::Regular,
                        },
                        EntryChange {
                            path: "src/lib.rs".into(),
                            content: Some(b"fn example() {}\n".to_vec()),
                            kind: EntryKind::Regular,
                        },
                    ],
                )
                .unwrap();
            drop(engine);

            for error in [
                compile(root.path(), &definition).unwrap_err(),
                validate_sources(root.path(), &definition).unwrap_err(),
            ] {
                match (expect_limit, error) {
                    (true, Error::Limit(message)) | (false, Error::Invalid(message)) => {
                        assert!(message.contains("Claude review SKILL.md"));
                    }
                    (_, other) => panic!("unexpected Claude review skill error: {other}"),
                }
            }
        }
    }

    #[cfg(unix)]
    #[test]
    fn claude_review_source_profile_is_preflighted_by_compile_and_source_validation() {
        let cases = [
            (
                "missing",
                Vec::<EntryChange>::new(),
                vec!["src/missing.rs".to_owned()],
                "src/missing.rs",
            ),
            (
                "empty",
                vec![EntryChange {
                    path: "src/review.rs".into(),
                    content: Some(Vec::new()),
                    kind: EntryKind::Regular,
                }],
                vec!["src/review.rs".to_owned()],
                "unsupported line count",
            ),
            (
                "non-utf8",
                vec![EntryChange {
                    path: "src/review.rs".into(),
                    content: Some(vec![0xff]),
                    kind: EntryKind::Regular,
                }],
                vec!["src/review.rs".to_owned()],
                "must be UTF-8",
            ),
            (
                "oversized",
                vec![EntryChange {
                    path: "src/review.rs".into(),
                    content: Some(vec![b'x'; MAX_CLAUDE_REVIEW_SOURCE_BYTES + 1]),
                    kind: EntryKind::Regular,
                }],
                vec!["src/review.rs".to_owned()],
                "exceeds",
            ),
            (
                "non-regular",
                vec![EntryChange {
                    path: "src/review.rs".into(),
                    content: Some(b"target".to_vec()),
                    kind: EntryKind::Symlink,
                }],
                vec!["src/review.rs".to_owned()],
                "regular",
            ),
        ];

        for (label, review_entries, paths, expected) in cases {
            let (root, mut definition, recipe) = command_fixture();
            definition.steps.push(StepDefinition {
                id: "review".into(),
                needs: vec!["run".into()],
                required: true,
                action: StepAction::ClaudeCodeReview {
                    paths,
                    model: "sonnet".into(),
                },
            });
            let engine = Engine::open(root.path()).unwrap();
            let mut entries = vec![
                EntryChange {
                    path: "skills/a/SKILL.md".into(),
                    content: Some(b"# Review skill\n".to_vec()),
                    kind: EntryKind::Regular,
                },
                EntryChange {
                    path: "skills/a/runner.json".into(),
                    content: Some(serde_json::to_vec(&recipe).unwrap()),
                    kind: EntryKind::Regular,
                },
            ];
            entries.extend(review_entries);
            definition.source.revision = engine.checkpoint_entries(None, &entries).unwrap();
            drop(engine);

            for error in [
                compile(root.path(), &definition).unwrap_err(),
                validate_sources(root.path(), &definition).unwrap_err(),
            ] {
                assert!(
                    error.to_string().contains(expected),
                    "{label} preflight returned unexpected error: {error}"
                );
            }
        }

        let (root, mut definition, recipe) = command_fixture();
        let mut paths = Vec::new();
        let mut entries = vec![
            EntryChange {
                path: "skills/a/SKILL.md".into(),
                content: Some(b"# Review skill\n".to_vec()),
                kind: EntryKind::Regular,
            },
            EntryChange {
                path: "skills/a/runner.json".into(),
                content: Some(serde_json::to_vec(&recipe).unwrap()),
                kind: EntryKind::Regular,
            },
        ];
        for index in 0..5 {
            let path = format!("src/review-{index}.rs");
            paths.push(path.clone());
            entries.push(EntryChange {
                path,
                content: Some(vec![b'x'; 14_000]),
                kind: EntryKind::Regular,
            });
        }
        definition.steps.push(StepDefinition {
            id: "review".into(),
            needs: vec!["run".into()],
            required: true,
            action: StepAction::ClaudeCodeReview {
                paths,
                model: "sonnet".into(),
            },
        });
        let engine = Engine::open(root.path()).unwrap();
        definition.source.revision = engine.checkpoint_entries(None, &entries).unwrap();
        drop(engine);
        for error in [
            compile(root.path(), &definition).unwrap_err(),
            validate_sources(root.path(), &definition).unwrap_err(),
        ] {
            assert!(
                error.to_string().contains("aggregate profile"),
                "aggregate preflight returned unexpected error: {error}"
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn claude_review_rejects_unsupported_model_before_source_access() {
        let (root, mut definition, _) = command_fixture();
        definition.steps.push(StepDefinition {
            id: "review".into(),
            needs: vec!["run".into()],
            required: true,
            action: StepAction::ClaudeCodeReview {
                paths: vec!["src/lib.rs".into()],
                model: "latest".into(),
            },
        });
        definition.source.revision = "0".repeat(64);

        for error in [
            compile(root.path(), &definition).unwrap_err(),
            validate_sources(root.path(), &definition).unwrap_err(),
        ] {
            assert!(
                error.to_string().contains("Claude review model"),
                "unsupported model returned unexpected error: {error}"
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn compiles_exact_packaged_command_against_current_review_context() {
        let (root, definition, recipe) = command_fixture();
        let plan = compile(root.path(), &definition).unwrap();

        assert_eq!(plan.command_recipe("run"), Some(&recipe));
        assert!(plan.command_recipe("missing").is_none());
        assert_eq!(
            recipe.digest().unwrap(),
            plan.command_recipe("run").unwrap().digest().unwrap()
        );
        assert!(
            !serde_json::to_string(&plan)
                .unwrap()
                .contains("command_recipes")
        );
    }

    #[cfg(unix)]
    #[test]
    fn rejects_undeclared_or_invalid_packaged_command_runner() {
        let (root, mut definition, _) = command_fixture();
        definition.skill.resources.clear();
        assert!(matches!(
            compile(root.path(), &definition),
            Err(Error::Invalid(_))
        ));

        let (root, mut definition, _) = command_fixture();
        let engine = Engine::open(root.path()).unwrap();
        definition.source.revision = engine
            .checkpoint_entries(
                None,
                &[
                    EntryChange {
                        path: "skills/a/SKILL.md".into(),
                        content: Some(b"# Command skill\n".to_vec()),
                        kind: EntryKind::Regular,
                    },
                    EntryChange {
                        path: "skills/a/runner.json".into(),
                        content: Some(b"{\"version\":1,\"unexpected\":true}".to_vec()),
                        kind: EntryKind::Regular,
                    },
                ],
            )
            .unwrap();
        assert!(matches!(
            compile(root.path(), &definition),
            Err(Error::Invalid(_))
        ));
    }

    #[cfg(unix)]
    #[test]
    fn rejects_stale_or_mismatched_command_binding() {
        let (root, mut definition, _) = command_fixture();
        let StepAction::Command { change_version, .. } = &mut definition.steps[0].action else {
            unreachable!();
        };
        *change_version = 2;
        assert!(matches!(
            compile(root.path(), &definition),
            Err(Error::Conflict(_))
        ));

        let (root, mut definition, _) = command_fixture();
        let StepAction::Command { change, .. } = &mut definition.steps[0].action else {
            unreachable!();
        };
        change.repository = "0".repeat(32);
        assert!(matches!(
            compile(root.path(), &definition),
            Err(Error::Conflict(_))
        ));

        let (root, mut definition, _) = command_fixture();
        let StepAction::Command {
            change,
            change_version,
            context_digest,
            ..
        } = &mut definition.steps[0].action
        else {
            unreachable!();
        };
        let mut authority = Authority::open(root.path()).unwrap();
        let state = authority.change(change).unwrap();
        let mut context = authority.review_context(&state.context_digest).unwrap();
        let wrong_recipe = InputIdentity::Digest {
            sha256: "c".repeat(64),
        };
        context.change_version = 2;
        context.recipe = wrong_recipe.clone();
        context.required_evidence[0].digest = wrong_recipe;
        let changed = authority.update_change_context(1, &context).unwrap();
        *change_version = changed.version;
        *context_digest = changed.context_digest;
        drop(authority);
        assert!(matches!(
            compile(root.path(), &definition),
            Err(Error::Conflict(_))
        ));
    }
}
