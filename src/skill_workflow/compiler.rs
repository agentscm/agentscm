//! Validation and immutable-source compilation for skill workflows.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::path::Path;

use serde::Serialize;
use sha2::{Digest, Sha256};

use super::contracts::{StepAction, StepDefinition, WorkflowDefinition};
use crate::authority::Authority;
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
    let bundle = budget.finish((|| {
        source.load_revision(&definition.source.revision)?;
        resolve_skill(&source, definition)
    })())?;
    validate_authority(&authority, definition)?;
    let skill_digest = bundle.identity().as_str().to_owned();
    let digest = plan_digest(definition, &skill_digest)?;

    Ok(CompiledPlan {
        definition: definition.clone(),
        digest,
        skill_digest,
        ordered_steps,
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

fn validate_definition(definition: &WorkflowDefinition) -> Result<Vec<StepDefinition>> {
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
    }

    stable_topological_order(&definition.steps, &indexes)
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

fn validate_identifier(kind: &str, value: &str) -> Result<()> {
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
    use tempfile::tempdir;

    use super::*;
    use crate::checks::SourcePathExpectation;
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
}
