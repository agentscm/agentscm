use std::collections::HashSet;
use std::io::{self, Write};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::{Error, Result};

pub const REVIEW_CONTEXT_VERSION: u32 = 1;
pub const MAX_REVIEW_CONTEXT_BYTES: usize = 64 * 1024;
pub const SUPPORTED_CAPABILITIES: &[&str] = &[
    "agentscm.review-context/v1",
    "agentscm.checks.source-paths/v1",
    #[cfg(unix)]
    "agentscm.checks.command/v1",
];

const MAX_IDENTIFIER_BYTES: usize = 128;
const MAX_INTENT_BYTES: usize = 8 * 1024;
const MAX_ENTRY_BYTES: usize = 1024;
const MAX_CRITERIA: usize = 128;
const MAX_SCOPE_ENTRIES: usize = 256;
const MAX_DEPENDENCIES: usize = 256;
const MAX_EVIDENCE: usize = 128;
const MAX_CAPABILITIES: usize = 64;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(deny_unknown_fields)]
pub struct ChangeId {
    pub repository: String,
    pub id: String,
}

impl ChangeId {
    pub fn validate(&self) -> Result<()> {
        validate_identifier("repository ID", &self.repository)?;
        validate_identifier("change ID", &self.id)
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ChangeState {
    pub id: ChangeId,
    pub version: u64,
    pub context_digest: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct BaseState {
    pub revision: String,
    pub target_version: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "state", rename_all = "snake_case", deny_unknown_fields)]
pub enum InputIdentity {
    Digest { sha256: String },
    Unknown {},
    Unavailable { reason: String },
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct DependencyClaim {
    pub identity: String,
    pub state: DependencyState,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "state", rename_all = "snake_case", deny_unknown_fields)]
pub enum DependencyState {
    Declared {},
    Resolved {
        revision: String,
    },
    Observed {
        revision: Option<String>,
        evidence_digest: String,
    },
    Unknown {},
    Unavailable {
        reason: String,
    },
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct RequiredEvidence {
    pub identity: String,
    pub digest: InputIdentity,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ReviewContext {
    pub version: u32,
    pub change: ChangeId,
    pub change_version: u64,
    pub revision: String,
    pub base: Option<BaseState>,
    pub target: String,
    pub policy_epoch: u64,
    pub intent: String,
    pub acceptance_criteria: Vec<String>,
    pub scope: Vec<String>,
    pub exclusions: Vec<String>,
    pub omissions: Vec<String>,
    pub dependencies: Vec<DependencyClaim>,
    pub recipe: InputIdentity,
    pub toolchain: InputIdentity,
    pub environment: InputIdentity,
    pub required_evidence: Vec<RequiredEvidence>,
    pub required_capabilities: Vec<String>,
}

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum ContextReadiness {
    Complete,
    Incomplete { reasons: Vec<String> },
}

#[derive(Serialize)]
struct ReviewContextV1<'a> {
    version: u32,
    change: &'a ChangeId,
    change_version: u64,
    revision: &'a str,
    base: &'a Option<BaseState>,
    target: &'a str,
    policy_epoch: u64,
    intent: &'a str,
    acceptance_criteria: &'a [String],
    scope: &'a [String],
    exclusions: &'a [String],
    omissions: &'a [String],
    dependencies: &'a [DependencyClaim],
    recipe: &'a InputIdentity,
    toolchain: &'a InputIdentity,
    environment: &'a InputIdentity,
    required_evidence: &'a [RequiredEvidence],
    required_capabilities: &'a [String],
}

impl<'a> From<&'a ReviewContext> for ReviewContextV1<'a> {
    fn from(context: &'a ReviewContext) -> Self {
        Self {
            version: context.version,
            change: &context.change,
            change_version: context.change_version,
            revision: &context.revision,
            base: &context.base,
            target: &context.target,
            policy_epoch: context.policy_epoch,
            intent: &context.intent,
            acceptance_criteria: &context.acceptance_criteria,
            scope: &context.scope,
            exclusions: &context.exclusions,
            omissions: &context.omissions,
            dependencies: &context.dependencies,
            recipe: &context.recipe,
            toolchain: &context.toolchain,
            environment: &context.environment,
            required_evidence: &context.required_evidence,
            required_capabilities: &context.required_capabilities,
        }
    }
}

impl ReviewContext {
    pub fn from_slice(input: &[u8]) -> Result<Self> {
        if input.len() > MAX_REVIEW_CONTEXT_BYTES {
            return Err(Error::Limit(format!(
                "review context input exceeds {MAX_REVIEW_CONTEXT_BYTES} bytes"
            )));
        }
        let context: Self = serde_json::from_slice(input)?;
        context.validate()?;
        context.canonical_bytes()?;
        Ok(context.canonicalized())
    }

    pub fn validate(&self) -> Result<()> {
        if self.version != REVIEW_CONTEXT_VERSION {
            return Err(Error::Invalid(format!(
                "unsupported review context version {}",
                self.version
            )));
        }
        self.change.validate()?;
        validate_positive_sql_integer("change version", self.change_version)?;
        validate_digest("revision", &self.revision)?;
        if let Some(base) = &self.base {
            validate_digest("base revision", &base.revision)?;
            validate_positive_sql_integer("base target version", base.target_version)?;
        }
        validate_identifier("target name", &self.target)?;
        validate_sql_integer("policy epoch", self.policy_epoch)?;
        validate_text("intent", &self.intent, MAX_INTENT_BYTES, true)?;
        validate_text_list(
            "acceptance criteria",
            &self.acceptance_criteria,
            MAX_CRITERIA,
            true,
        )?;
        validate_text_list("scope", &self.scope, MAX_SCOPE_ENTRIES, true)?;
        let declared_scope = self
            .scope
            .len()
            .checked_add(self.exclusions.len())
            .and_then(|count| count.checked_add(self.omissions.len()))
            .ok_or_else(|| Error::Limit("scope entry count overflowed".to_owned()))?;
        if declared_scope > MAX_SCOPE_ENTRIES {
            return Err(Error::Limit(format!(
                "scope, exclusion, and omission entries exceed {MAX_SCOPE_ENTRIES}"
            )));
        }
        validate_text_list("exclusions", &self.exclusions, MAX_SCOPE_ENTRIES, false)?;
        validate_text_list("omissions", &self.omissions, MAX_SCOPE_ENTRIES, false)?;
        validate_count("dependencies", self.dependencies.len(), MAX_DEPENDENCIES)?;
        validate_unique_identities(
            "dependency identities",
            self.dependencies
                .iter()
                .map(|dependency| &dependency.identity),
        )?;
        for dependency in &self.dependencies {
            validate_identifier("dependency identity", &dependency.identity)?;
            dependency.state.validate()?;
        }
        self.recipe.validate("recipe")?;
        self.toolchain.validate("toolchain")?;
        self.environment.validate("environment")?;
        validate_count(
            "required evidence",
            self.required_evidence.len(),
            MAX_EVIDENCE,
        )?;
        validate_unique_identities(
            "required evidence identities",
            self.required_evidence
                .iter()
                .map(|evidence| &evidence.identity),
        )?;
        for evidence in &self.required_evidence {
            validate_identifier("required evidence identity", &evidence.identity)?;
            evidence.digest.validate("required evidence digest")?;
        }
        validate_count(
            "required capabilities",
            self.required_capabilities.len(),
            MAX_CAPABILITIES,
        )?;
        validate_unique_strings("required capabilities", &self.required_capabilities)?;
        for capability in &self.required_capabilities {
            validate_identifier("required capability", capability)?;
        }
        validate_raw_text_budget(self)
    }

    pub fn canonical_bytes(&self) -> Result<Vec<u8>> {
        self.validate()?;
        let canonical = self.canonicalized();
        let mut writer = LimitedWriter::new(MAX_REVIEW_CONTEXT_BYTES);
        let result = serde_json::to_writer(&mut writer, &ReviewContextV1::from(&canonical));
        if writer.exceeded {
            return Err(Error::Limit(format!(
                "canonical review context exceeds {MAX_REVIEW_CONTEXT_BYTES} bytes"
            )));
        }
        result?;
        Ok(writer.bytes)
    }

    pub fn digest(&self) -> Result<String> {
        Ok(format!("{:x}", Sha256::digest(self.canonical_bytes()?)))
    }

    pub fn readiness(&self, supported_capabilities: &[&str]) -> ContextReadiness {
        let mut reasons = Vec::new();
        add_input_reason("recipe", &self.recipe, &mut reasons);
        add_input_reason("toolchain", &self.toolchain, &mut reasons);
        add_input_reason("environment", &self.environment, &mut reasons);
        for omission in &self.omissions {
            reasons.push(format!("declared omission: {omission}"));
        }
        for dependency in &self.dependencies {
            match &dependency.state {
                DependencyState::Unknown {} => {
                    reasons.push(format!("dependency {} is unknown", dependency.identity))
                }
                DependencyState::Unavailable { reason } => reasons.push(format!(
                    "dependency {} is unavailable: {reason}",
                    dependency.identity
                )),
                _ => {}
            }
        }
        for evidence in &self.required_evidence {
            add_input_reason(
                &format!("required evidence {}", evidence.identity),
                &evidence.digest,
                &mut reasons,
            );
        }
        for capability in &self.required_capabilities {
            if !supported_capabilities.contains(&capability.as_str()) {
                reasons.push(format!("required capability {capability} is unsupported"));
            }
        }
        reasons.sort();
        reasons.dedup();
        if reasons.is_empty() {
            ContextReadiness::Complete
        } else {
            ContextReadiness::Incomplete { reasons }
        }
    }

    fn canonicalized(&self) -> Self {
        let mut canonical = self.clone();
        canonical.acceptance_criteria.sort();
        canonical.scope.sort();
        canonical.exclusions.sort();
        canonical.omissions.sort();
        canonical
            .dependencies
            .sort_by(|left, right| left.identity.cmp(&right.identity));
        canonical
            .required_evidence
            .sort_by(|left, right| left.identity.cmp(&right.identity));
        canonical.required_capabilities.sort();
        canonical
    }
}

impl InputIdentity {
    fn validate(&self, kind: &str) -> Result<()> {
        match self {
            Self::Digest { sha256 } => validate_digest(kind, sha256),
            Self::Unknown {} => Ok(()),
            Self::Unavailable { reason } => validate_text(
                &format!("{kind} unavailable reason"),
                reason,
                MAX_ENTRY_BYTES,
                true,
            ),
        }
    }
}

impl DependencyState {
    fn validate(&self) -> Result<()> {
        match self {
            Self::Declared {} | Self::Unknown {} => Ok(()),
            Self::Resolved { revision } => validate_digest("dependency revision", revision),
            Self::Observed {
                revision,
                evidence_digest,
            } => {
                if let Some(revision) = revision {
                    validate_digest("observed dependency revision", revision)?;
                }
                validate_digest("dependency evidence digest", evidence_digest)
            }
            Self::Unavailable { reason } => validate_text(
                "dependency unavailable reason",
                reason,
                MAX_ENTRY_BYTES,
                true,
            ),
        }
    }
}

fn add_input_reason(kind: &str, input: &InputIdentity, reasons: &mut Vec<String>) {
    match input {
        InputIdentity::Digest { .. } => {}
        InputIdentity::Unknown {} => reasons.push(format!("{kind} is unknown")),
        InputIdentity::Unavailable { reason } => {
            reasons.push(format!("{kind} is unavailable: {reason}"));
        }
    }
}

fn validate_identifier(kind: &str, value: &str) -> Result<()> {
    if value.is_empty() || value.len() > MAX_IDENTIFIER_BYTES {
        return Err(Error::Limit(format!(
            "{kind} must contain 1 to {MAX_IDENTIFIER_BYTES} bytes"
        )));
    }
    if !value.bytes().all(|byte| {
        byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b':' | b'/')
    }) {
        return Err(Error::Invalid(format!(
            "{kind} contains an unsupported character"
        )));
    }
    Ok(())
}

fn validate_digest(kind: &str, value: &str) -> Result<()> {
    if value.len() != 64
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err(Error::Invalid(format!(
            "{kind} must be a lowercase SHA-256 digest"
        )));
    }
    Ok(())
}

fn validate_positive_sql_integer(kind: &str, value: u64) -> Result<()> {
    if value == 0 {
        return Err(Error::Invalid(format!("{kind} must be positive")));
    }
    validate_sql_integer(kind, value)
}

fn validate_sql_integer(kind: &str, value: u64) -> Result<()> {
    if value > i64::MAX as u64 {
        return Err(Error::Limit(format!("{kind} exceeds SQLite range")));
    }
    Ok(())
}

fn validate_text(kind: &str, value: &str, maximum: usize, required: bool) -> Result<()> {
    if required && value.is_empty() {
        return Err(Error::Invalid(format!("{kind} must not be empty")));
    }
    if value.len() > maximum {
        return Err(Error::Limit(format!("{kind} exceeds {maximum} bytes")));
    }
    Ok(())
}

fn validate_text_list(kind: &str, values: &[String], maximum: usize, required: bool) -> Result<()> {
    validate_count(kind, values.len(), maximum)?;
    if required && values.is_empty() {
        return Err(Error::Invalid(format!("{kind} must not be empty")));
    }
    for value in values {
        validate_text(&format!("{kind} entry"), value, MAX_ENTRY_BYTES, true)?;
    }
    validate_unique_strings(kind, values)
}

fn validate_count(kind: &str, count: usize, maximum: usize) -> Result<()> {
    if count > maximum {
        return Err(Error::Limit(format!("{kind} count exceeds {maximum}")));
    }
    Ok(())
}

fn validate_unique_strings(kind: &str, values: &[String]) -> Result<()> {
    let mut seen = HashSet::with_capacity(values.len());
    for value in values {
        if !seen.insert(value) {
            return Err(Error::Invalid(format!("{kind} contains a duplicate")));
        }
    }
    Ok(())
}

fn validate_unique_identities<'a>(
    kind: &str,
    identities: impl Iterator<Item = &'a String>,
) -> Result<()> {
    let mut seen = HashSet::new();
    for identity in identities {
        if !seen.insert(identity) {
            return Err(Error::Invalid(format!("{kind} contains a duplicate")));
        }
    }
    Ok(())
}

fn validate_raw_text_budget(context: &ReviewContext) -> Result<()> {
    let mut total = 0usize;
    let mut add = |value: &str| -> Result<()> {
        total = total
            .checked_add(value.len())
            .ok_or_else(|| Error::Limit("review context text size overflowed".to_owned()))?;
        if total > MAX_REVIEW_CONTEXT_BYTES {
            return Err(Error::Limit(format!(
                "review context text exceeds {MAX_REVIEW_CONTEXT_BYTES} bytes"
            )));
        }
        Ok(())
    };
    add(&context.change.repository)?;
    add(&context.change.id)?;
    add(&context.revision)?;
    if let Some(base) = &context.base {
        add(&base.revision)?;
    }
    add(&context.target)?;
    add(&context.intent)?;
    for value in context
        .acceptance_criteria
        .iter()
        .chain(&context.scope)
        .chain(&context.exclusions)
        .chain(&context.omissions)
    {
        add(value)?;
    }
    for dependency in &context.dependencies {
        add(&dependency.identity)?;
        match &dependency.state {
            DependencyState::Resolved { revision } => add(revision)?,
            DependencyState::Observed {
                revision,
                evidence_digest,
            } => {
                if let Some(revision) = revision {
                    add(revision)?;
                }
                add(evidence_digest)?;
            }
            DependencyState::Unavailable { reason } => add(reason)?,
            DependencyState::Declared {} | DependencyState::Unknown {} => {}
        }
    }
    for input in [&context.recipe, &context.toolchain, &context.environment] {
        add_input_text(input, &mut add)?;
    }
    for evidence in &context.required_evidence {
        add(&evidence.identity)?;
        add_input_text(&evidence.digest, &mut add)?;
    }
    for capability in &context.required_capabilities {
        add(capability)?;
    }
    Ok(())
}

fn add_input_text(input: &InputIdentity, add: &mut impl FnMut(&str) -> Result<()>) -> Result<()> {
    match input {
        InputIdentity::Digest { sha256 } => add(sha256),
        InputIdentity::Unknown {} => Ok(()),
        InputIdentity::Unavailable { reason } => add(reason),
    }
}

struct LimitedWriter {
    bytes: Vec<u8>,
    limit: usize,
    exceeded: bool,
}

impl LimitedWriter {
    fn new(limit: usize) -> Self {
        Self {
            bytes: Vec::new(),
            limit,
            exceeded: false,
        }
    }
}

impl Write for LimitedWriter {
    fn write(&mut self, input: &[u8]) -> io::Result<usize> {
        if input.len() > self.limit.saturating_sub(self.bytes.len()) {
            self.exceeded = true;
            return Err(io::Error::new(
                io::ErrorKind::FileTooLarge,
                "review context exceeded its canonical byte limit",
            ));
        }
        self.bytes.extend_from_slice(input);
        Ok(input.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}
