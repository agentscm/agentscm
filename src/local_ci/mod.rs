//! Immutable local CI declarations. These contracts do not execute jobs or grant admission.
mod json;

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::context::{DependencyClaim, DependencyState};
use crate::evidence::{validate_identifier, validate_lower_hex};
use crate::local_validation::GitValidationSubject;
use crate::{Error, Result};

pub const VERSION: u32 = 1;
pub const MAX_DOCUMENT_BYTES: usize = 256 * 1024;
pub const MAX_CELLS: usize = 128;
pub const MAX_WORKFLOW_BYTES: usize = 64 * 1024;
const MAX_ITEMS: usize = 32;
const MAX_TEXT: usize = 2048;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum SourceIdentity {
    Git {
        subject: GitValidationSubject,
    },
    Native {
        repository: String,
        revision: String,
    },
}

impl SourceIdentity {
    pub fn repository(&self) -> &str {
        match self {
            Self::Git { subject } => &subject.repository,
            Self::Native { repository, .. } => repository,
        }
    }

    pub fn validate(&self) -> Result<()> {
        match self {
            Self::Git { subject } => subject.validate(),
            Self::Native {
                repository,
                revision,
            } => {
                validate_identifier("repository", repository)?;
                digest("native revision", revision)
            }
        }
    }

    fn same_kind(&self, other: &Self) -> bool {
        matches!(
            (self, other),
            (Self::Git { .. }, Self::Git { .. }) | (Self::Native { .. }, Self::Native { .. })
        )
    }
}

/// The head remains bound even when execution targets a separately observed merge result.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum PlanSubject {
    PrHead {
        head: SourceIdentity,
        base: SourceIdentity,
    },
    MergeResult {
        head: SourceIdentity,
        base: SourceIdentity,
        result: SourceIdentity,
    },
}

impl PlanSubject {
    pub fn head(&self) -> &SourceIdentity {
        match self {
            Self::PrHead { head, .. } | Self::MergeResult { head, .. } => head,
        }
    }

    pub fn execution_source(&self) -> &SourceIdentity {
        match self {
            Self::PrHead { head, .. } => head,
            Self::MergeResult { result, .. } => result,
        }
    }

    pub fn validate(&self) -> Result<()> {
        let (head, base, result) = match self {
            Self::PrHead { head, base } => (head, base, None),
            Self::MergeResult { head, base, result } => (head, base, Some(result)),
        };
        for source in [Some(head), Some(base), result].into_iter().flatten() {
            source.validate()?;
            if !head.same_kind(source) || head.repository() != source.repository() {
                return Err(Error::Invalid(
                    "plan subject mixes repositories or source kinds".into(),
                ));
            }
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum WorkflowFormat {
    Commands,
    Actions,
}

/// SHA-256 of exact file bytes, including whitespace. Not a parsed YAML/JSON digest.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct WorkflowBinding {
    pub path: String,
    pub format: WorkflowFormat,
    pub sha256: String,
}

impl WorkflowBinding {
    pub fn from_bytes(path: String, format: WorkflowFormat, bytes: &[u8]) -> Result<Self> {
        if bytes.len() > MAX_WORKFLOW_BYTES {
            return Err(Error::Limit("workflow bytes exceed limit".into()));
        }
        let binding = Self {
            path,
            format,
            sha256: raw_sha256(bytes),
        };
        binding.validate()?;
        Ok(binding)
    }

    pub fn validate(&self) -> Result<()> {
        path("workflow path", &self.path, false)?;
        digest("workflow digest", &self.sha256)
    }
}

/// The controller selects this source independently of the candidate. No field grants trust.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ConfigBinding {
    pub source: SourceIdentity,
    pub path: String,
    pub sha256: String,
}

impl ConfigBinding {
    pub fn validate(&self) -> Result<()> {
        self.source.validate()?;
        path("configuration path", &self.path, false)?;
        digest("configuration bytes digest", &self.sha256)
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ResourceRequirements {
    pub timeout_secs: u64,
    pub cpu_millis: u32,
    pub memory_bytes: u64,
    pub disk_bytes: u64,
}

impl ResourceRequirements {
    pub fn validate(&self) -> Result<()> {
        if self.timeout_secs == 0
            || self.timeout_secs > 24 * 3600
            || self.cpu_millis == 0
            || self.cpu_millis > 256_000
            || self.memory_bytes == 0
            || self.memory_bytes > (1_u64 << 50)
            || self.disk_bytes == 0
            || self.disk_bytes > (1_u64 << 50)
        {
            return Err(Error::Limit(
                "invalid local CI resource requirements".into(),
            ));
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct CommandSpec {
    pub argv: Vec<String>,
    /// Repository relative; `.` explicitly selects the root.
    pub working_directory: String,
    pub environment: BTreeMap<String, String>,
    /// When present, must name a command-format workflow bound by the requirements.
    pub reviewed_script: Option<String>,
    pub resources: ResourceRequirements,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct RunnerProfile {
    pub id: String,
    pub sha256: String,
    pub platform: String,
    pub architecture: String,
    pub toolchain_sha256: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ServiceRequirement {
    pub id: String,
    /// Pinned service definition/image identity. A mutable image tag is insufficient.
    pub sha256: String,
    pub environment: BTreeMap<String, String>,
    pub resources: ResourceRequirements,
}

/// Explicit, pre-expanded cell inventory. No adapter may expand or shrink it independently.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct RequiredCell {
    pub job: String,
    pub id: String,
    pub matrix: BTreeMap<String, String>,
    pub command: CommandSpec,
    pub runner: RunnerProfile,
    pub services: Vec<ServiceRequirement>,
    pub dependencies: Vec<DependencyClaim>,
    pub required_capabilities: Vec<String>,
}

impl RequiredCell {
    pub fn key(&self) -> (&str, &str) {
        (&self.job, &self.id)
    }

    pub fn validate(&self) -> Result<()> {
        validate_identifier("job", &self.job)?;
        validate_identifier("cell", &self.id)?;
        string_map("matrix", &self.matrix, false)?;
        let command = &self.command;
        if command.argv.is_empty() || command.argv.len() > 64 {
            return Err(Error::Limit("argv must contain 1 to 64 items".into()));
        }
        for (index, arg) in command.argv.iter().enumerate() {
            text("argv", arg, index != 0)?;
        }
        path("working directory", &command.working_directory, true)?;
        string_map("environment", &command.environment, true)?;
        if let Some(script) = &command.reviewed_script {
            path("reviewed script", script, false)?;
        }
        command.resources.validate()?;
        validate_identifier("runner profile", &self.runner.id)?;
        digest("runner profile digest", &self.runner.sha256)?;
        validate_identifier("platform", &self.runner.platform)?;
        validate_identifier("architecture", &self.runner.architecture)?;
        digest("toolchain digest", &self.runner.toolchain_sha256)?;
        count("services", self.services.len(), MAX_ITEMS)?;
        let mut services = BTreeSet::new();
        for service in &self.services {
            validate_identifier("service", &service.id)?;
            unique(&mut services, &service.id, "service")?;
            digest("service digest", &service.sha256)?;
            string_map("service environment", &service.environment, true)?;
            service.resources.validate()?;
        }
        count("dependencies", self.dependencies.len(), MAX_ITEMS)?;
        let mut dependencies = BTreeSet::new();
        for dependency in &self.dependencies {
            validate_identifier("dependency", &dependency.identity)?;
            unique(&mut dependencies, &dependency.identity, "dependency")?;
            match &dependency.state {
                DependencyState::Resolved { revision } => digest("dependency revision", revision)?,
                DependencyState::Observed {
                    revision,
                    evidence_digest,
                } => {
                    if let Some(revision) = revision {
                        digest("dependency revision", revision)?;
                    }
                    digest("dependency evidence digest", evidence_digest)?;
                }
                DependencyState::Unavailable { reason } => {
                    text("dependency reason", reason, false)?
                }
                DependencyState::Declared {} | DependencyState::Unknown {} => {}
            }
        }
        count("capabilities", self.required_capabilities.len(), MAX_ITEMS)?;
        let mut capabilities = BTreeSet::new();
        for capability in &self.required_capabilities {
            validate_identifier("capability", capability)?;
            unique(&mut capabilities, capability, "capability")?;
        }
        Ok(())
    }

    fn canonicalize(&mut self) {
        self.services.sort_by(|a, b| a.id.cmp(&b.id));
        self.dependencies
            .sort_by(|a, b| a.identity.cmp(&b.identity));
        self.required_capabilities.sort();
    }
}

/// An allowed exclusion from the declared inventory, with the exact trusted reason.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct PermittedOmission {
    pub job: String,
    pub cell: String,
    pub reason: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Requirements {
    pub version: u32,
    pub repository: String,
    pub policy_epoch: u64,
    pub workflows: Vec<WorkflowBinding>,
    pub cells: Vec<RequiredCell>,
    pub permitted_omissions: Vec<PermittedOmission>,
}

impl Requirements {
    pub fn from_slice(bytes: &[u8]) -> Result<Self> {
        let value: Self = json::from_slice(bytes)?;
        value.validate()?;
        Ok(value)
    }

    pub fn validate(&self) -> Result<()> {
        version(self.version)?;
        validate_identifier("repository", &self.repository)?;
        if self.policy_epoch == 0 || self.policy_epoch > i64::MAX as u64 {
            return Err(Error::Invalid(
                "policy epoch must be positive signed 64-bit".into(),
            ));
        }
        validate_inventory(&self.workflows, &self.cells, &self.permitted_omissions)?;
        bounded_bytes(self).map(|_| ())
    }

    pub fn canonical_bytes(&self) -> Result<Vec<u8>> {
        self.validate()?;
        let mut value = self.clone();
        canonical_inventory(
            &mut value.workflows,
            &mut value.cells,
            &mut value.permitted_omissions,
        );
        bounded_bytes(&value)
    }

    pub fn digest(&self) -> Result<String> {
        domain_digest(
            b"agentscm.local-ci.requirements.v1\0",
            &self.canonical_bytes()?,
        )
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ImmutablePlan {
    pub version: u32,
    pub subject: PlanSubject,
    pub configuration: ConfigBinding,
    pub policy_epoch: u64,
    /// Canonical requirements digest, distinct from the raw configuration bytes digest.
    pub requirements_sha256: String,
    pub workflows: Vec<WorkflowBinding>,
    pub event_inputs: BTreeMap<String, String>,
    pub cells: Vec<RequiredCell>,
    /// Selected exclusions; resolution must compare them with trusted permitted omissions.
    pub omissions: Vec<PermittedOmission>,
}

impl ImmutablePlan {
    pub fn from_slice(bytes: &[u8]) -> Result<Self> {
        let value: Self = json::from_slice(bytes)?;
        value.validate()?;
        Ok(value)
    }

    pub fn validate(&self) -> Result<()> {
        version(self.version)?;
        self.subject.validate()?;
        self.configuration.validate()?;
        if self.configuration.source.repository() != self.subject.head().repository() {
            return Err(Error::Invalid(
                "configuration belongs to another repository".into(),
            ));
        }
        if self.policy_epoch == 0 || self.policy_epoch > i64::MAX as u64 {
            return Err(Error::Invalid(
                "policy epoch must be positive signed 64-bit".into(),
            ));
        }
        digest("requirements digest", &self.requirements_sha256)?;
        string_map("event inputs", &self.event_inputs, false)?;
        // Plan cells are the complete inventory, including cells explicitly omitted.
        validate_inventory(&self.workflows, &self.cells, &self.omissions)?;
        bounded_bytes(self).map(|_| ())
    }

    pub fn canonical_bytes(&self) -> Result<Vec<u8>> {
        self.validate()?;
        let mut value = self.clone();
        canonical_inventory(&mut value.workflows, &mut value.cells, &mut value.omissions);
        bounded_bytes(&value)
    }

    pub fn digest(&self) -> Result<String> {
        domain_digest(b"agentscm.local-ci.plan.v1\0", &self.canonical_bytes()?)
    }
}

fn validate_inventory(
    workflows: &[WorkflowBinding],
    cells: &[RequiredCell],
    omissions: &[PermittedOmission],
) -> Result<()> {
    if workflows.is_empty() || cells.is_empty() {
        return Err(Error::Invalid(
            "local CI requires workflows and cells".into(),
        ));
    }
    count("workflows", workflows.len(), MAX_ITEMS)?;
    count("cells", cells.len(), MAX_CELLS)?;
    count("omissions", omissions.len(), MAX_CELLS)?;
    let mut workflow_paths = BTreeMap::new();
    for workflow in workflows {
        workflow.validate()?;
        if workflow_paths
            .insert(workflow.path.as_str(), workflow.format)
            .is_some()
        {
            return Err(Error::Invalid("duplicate workflow path".into()));
        }
    }
    let mut keys = BTreeSet::new();
    for cell in cells {
        cell.validate()?;
        if !keys.insert(cell.key()) {
            return Err(Error::Invalid("duplicate job/cell identity".into()));
        }
        if let Some(script) = &cell.command.reviewed_script {
            if workflow_paths.get(script.as_str()) != Some(&WorkflowFormat::Commands) {
                return Err(Error::Invalid(
                    "reviewed script is not a bound command workflow".into(),
                ));
            }
        }
    }
    let mut omitted = BTreeSet::new();
    for omission in omissions {
        validate_identifier("omitted job", &omission.job)?;
        validate_identifier("omitted cell", &omission.cell)?;
        text("omission reason", &omission.reason, false)?;
        let key = (omission.job.as_str(), omission.cell.as_str());
        if !keys.contains(&key) || !omitted.insert(key) {
            return Err(Error::Invalid("unknown or duplicate omitted cell".into()));
        }
    }
    Ok(())
}

fn canonical_inventory(
    workflows: &mut [WorkflowBinding],
    cells: &mut [RequiredCell],
    omissions: &mut [PermittedOmission],
) {
    workflows.sort_by(|a, b| a.path.cmp(&b.path));
    cells.sort_by(|a, b| a.key().cmp(&b.key()));
    for cell in cells {
        cell.canonicalize();
    }
    omissions.sort_by(|a, b| (&a.job, &a.cell).cmp(&(&b.job, &b.cell)));
}

pub(crate) fn raw_sha256(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn domain_digest(domain: &[u8], bytes: &[u8]) -> Result<String> {
    let mut hash = Sha256::new();
    hash.update(domain);
    hash.update(bytes);
    Ok(format!("{:x}", hash.finalize()))
}

fn bounded_bytes(value: &impl Serialize) -> Result<Vec<u8>> {
    let bytes = serde_json::to_vec(value)?;
    if bytes.len() > MAX_DOCUMENT_BYTES {
        return Err(Error::Limit("local CI document exceeds byte limit".into()));
    }
    Ok(bytes)
}

fn version(value: u32) -> Result<()> {
    if value != VERSION {
        return Err(Error::Invalid(format!(
            "unsupported local CI version {value}"
        )));
    }
    Ok(())
}

pub(crate) fn digest(kind: &str, value: &str) -> Result<()> {
    validate_lower_hex(kind, value, 32)
}

pub(crate) fn path(kind: &str, value: &str, root: bool) -> Result<()> {
    text(kind, value, false)?;
    if root && value == "." {
        return Ok(());
    }
    if value.starts_with(['/', '-'])
        || value.contains(['\\', ':'])
        || value.chars().any(char::is_control)
        || crate::git_path_profile::uses_reserved_namespace(value)
        || value.split('/').any(|part| matches!(part, "" | "." | ".."))
    {
        return Err(Error::Invalid(format!(
            "{kind} must be a portable repository-relative path"
        )));
    }
    Ok(())
}

fn text(kind: &str, value: &str, empty: bool) -> Result<()> {
    if (!empty && value.is_empty()) || value.len() > MAX_TEXT || value.contains('\0') {
        return Err(Error::Limit(format!("invalid or oversized {kind}")));
    }
    Ok(())
}

fn string_map(kind: &str, values: &BTreeMap<String, String>, environment: bool) -> Result<()> {
    count(kind, values.len(), MAX_ITEMS)?;
    for (key, value) in values {
        validate_identifier(kind, key)?;
        if environment
            && (!key.bytes().enumerate().all(|(i, b)| {
                b == b'_' || b.is_ascii_alphabetic() || (i > 0 && b.is_ascii_digit())
            }))
        {
            return Err(Error::Invalid("invalid environment variable name".into()));
        }
        text(kind, value, true)?;
    }
    Ok(())
}

fn count(kind: &str, actual: usize, maximum: usize) -> Result<()> {
    if actual > maximum {
        return Err(Error::Limit(format!("{kind} exceeds {maximum} entries")));
    }
    Ok(())
}

fn unique<'a>(set: &mut BTreeSet<&'a str>, value: &'a str, kind: &str) -> Result<()> {
    if !set.insert(value) {
        return Err(Error::Invalid(format!("duplicate {kind}")));
    }
    Ok(())
}
