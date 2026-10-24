//! Supported skills: strict manifest v2, embedded catalog, repo lock file, and the pure
//! evidence evaluation and admission rules shared by the hub, MCP and CLI.
//!
//! Experimental. Evidence is self-reported and labelled so; nothing here proves it.

use std::collections::BTreeSet;

use serde_json::{Map, Value};
use std::sync::OnceLock;

use serde::{Deserialize, Serialize};

use super::SkillReference;
use super::library::TemplateStepDefinition;
use crate::store::{digest, validate_path};
use crate::{Error, Result};

const MAX_BYTES: usize = 256 * 1024;
const MAX_GATES: usize = 16;
const MAX_EVIDENCE_PER_GATE: usize = 8;
const MAX_STEPS: usize = 64;
const MAX_PINS: usize = 32;
const MAX_SUMMARY: usize = 500;
const MAX_URL: usize = 2048;
const MAX_LIST: usize = 64;
const MAX_MANIFEST_SUMMARY: usize = 200;

/// Parameter values by name: manifest defaults overlaid by a pin's overrides.
pub type Params = Map<String, Value>;

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Subject {
    GitPullRequest,
    NativeChange,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    Author,
    Reviewer,
    Maintainer,
}

/// Ordered weakest to strongest.
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub enum Assurance {
    SelfReported,
    Observed,
    Receipt,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Enforce {
    Stop,
    ForgeStatus,
    Native,
    Advise,
    Signal,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum EvidenceKind {
    TaskBranch,
    RevisionTests,
    IndependentReview,
    FindingsResolved,
    HostedCi,
    HumanApproval,
    Merged,
    PostMergeCheck,
    WorkflowReceipt,
}

impl EvidenceKind {
    /// Evidence that does not belong to one head commit: it never goes stale and does not
    /// advance the head (a merge commit and the post-merge main are not the branch head).
    fn head_bound(self) -> bool {
        !matches!(self, Self::TaskBranch | Self::Merged | Self::PostMergeCheck)
    }
}

/// One evidence requirement of a gate: the kind plus its manifest parameters.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum EvidenceSpec {
    TaskBranch,
    RevisionTests,
    IndependentReview,
    FindingsResolved,
    HostedCi {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        label: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        label_param: Option<String>,
    },
    HumanApproval,
    Merged {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        method: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        method_param: Option<String>,
    },
    PostMergeCheck,
    WorkflowReceipt {
        step: String,
    },
}

impl EvidenceSpec {
    pub fn kind(&self) -> EvidenceKind {
        match self {
            Self::TaskBranch => EvidenceKind::TaskBranch,
            Self::RevisionTests => EvidenceKind::RevisionTests,
            Self::IndependentReview => EvidenceKind::IndependentReview,
            Self::FindingsResolved => EvidenceKind::FindingsResolved,
            Self::HostedCi { .. } => EvidenceKind::HostedCi,
            Self::HumanApproval => EvidenceKind::HumanApproval,
            Self::Merged { .. } => EvidenceKind::Merged,
            Self::PostMergeCheck => EvidenceKind::PostMergeCheck,
            Self::WorkflowReceipt { .. } => EvidenceKind::WorkflowReceipt,
        }
    }

    /// The parameter this spec reads, if any.
    fn param_ref(&self) -> Option<&str> {
        match self {
            Self::HostedCi { label_param, .. } => label_param.as_deref(),
            Self::Merged { method_param, .. } => method_param.as_deref(),
            _ => None,
        }
    }

    /// The label or method the evidence must carry: a literal, else the parameter's string
    /// value; `null` and `any` impose nothing.
    fn accepts(&self, detail: Option<&Detail>, params: &Params) -> bool {
        let param = |name: &Option<String>| {
            name.as_ref()
                .and_then(|n| params.get(n))
                .and_then(Value::as_str)
                .filter(|v| *v != "any")
                .map(str::to_owned)
        };
        let (want, got) = match self {
            Self::HostedCi { label, label_param } => (
                label.clone().or_else(|| param(label_param)),
                detail.and_then(|d| d.label.as_ref()),
            ),
            Self::Merged {
                method,
                method_param,
            } => (
                method.clone().or_else(|| param(method_param)),
                detail.and_then(|d| d.method.as_ref()),
            ),
            _ => return true,
        };
        want.is_none_or(|w| got == Some(&w))
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct GateDefinition {
    pub id: String,
    pub evidence: Vec<EvidenceSpec>,
    pub by: Role,
    pub min_assurance: Assurance,
    pub enforce: Vec<Enforce>,
}

/// When an agent should follow the skill.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Applicability {
    pub triggers: Vec<String>,
    #[serde(default)]
    pub prerequisites: Vec<String>,
    #[serde(default)]
    pub not_for: Vec<String>,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ParameterType {
    String,
    Bool,
    Integer,
}

impl ParameterType {
    fn accepts(self, value: &Value) -> bool {
        value.is_null()
            || match self {
                Self::String => value.is_string(),
                Self::Bool => value.is_boolean(),
                Self::Integer => value.is_i64() || value.is_u64(),
            }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ParameterDefinition {
    pub name: String,
    pub description: String,
    #[serde(rename = "type")]
    pub kind: ParameterType,
    pub default: Value,
    #[serde(default, rename = "enum", skip_serializing_if = "Option::is_none")]
    pub allowed: Option<Vec<Value>>,
}

impl ParameterDefinition {
    fn check(&self, value: &Value) -> bool {
        self.kind.accepts(value)
            && self
                .allowed
                .as_ref()
                .is_none_or(|a| value.is_null() || a.contains(value))
    }
}

/// Template manifest version 2.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct SupportedManifest {
    pub version: u32,
    pub name: String,
    pub workflow_version: u32,
    pub summary: String,
    pub applicability: Applicability,
    #[serde(default)]
    pub parameters: Vec<ParameterDefinition>,
    pub skill: SkillReference,
    pub steps: Vec<TemplateStepDefinition>,
    pub subjects: Vec<Subject>,
    pub roles: Vec<Role>,
    pub gates: Vec<GateDefinition>,
}

fn invalid(message: &str) -> Error {
    Error::Invalid(message.into())
}

impl SupportedManifest {
    /// Parse and validate exact manifest bytes.
    pub fn parse(bytes: &[u8]) -> Result<Self> {
        if bytes.len() > MAX_BYTES {
            return Err(Error::Limit(
                "supported skill manifest exceeds 256 KiB".into(),
            ));
        }
        let manifest: Self = serde_json::from_slice(bytes)?;
        manifest.validate()?;
        Ok(manifest)
    }

    fn validate(&self) -> Result<()> {
        if self.version != 2 || self.workflow_version == 0 {
            return Err(invalid(
                "supported skill manifest requires version 2 and a positive workflow_version",
            ));
        }
        super::compiler::validate_identifier("skill name", &self.name)?;
        if self.summary.is_empty() || self.summary.len() > MAX_MANIFEST_SUMMARY {
            return Err(Error::Limit(
                "manifest summary requires 1 to 200 bytes".into(),
            ));
        }
        let a = &self.applicability;
        let bounded = |l: &[String], min: usize| {
            (min..=8).contains(&l.len()) && l.iter().all(|s| !s.is_empty() && s.len() <= 200)
        };
        if !bounded(&a.triggers, 1) || !bounded(&a.prerequisites, 0) || !bounded(&a.not_for, 0) {
            return Err(Error::Limit(
                "applicability needs 1 to 8 triggers and at most 8 prerequisites and not_for entries, each up to 200 bytes".into(),
            ));
        }
        if self.parameters.len() > 16 {
            return Err(Error::Limit("manifest has more than 16 parameters".into()));
        }
        let mut names = BTreeSet::new();
        for p in &self.parameters {
            super::compiler::validate_identifier("parameter name", &p.name)?;
            if !names.insert(p.name.as_str()) || p.description.len() > 200 || !p.check(&p.default) {
                return Err(Error::Invalid(format!("parameter {} is invalid", p.name)));
            }
        }
        if self.steps.len() > MAX_STEPS {
            return Err(Error::Limit("manifest has too many steps".into()));
        }
        for step in &self.steps {
            super::compiler::validate_identifier("step id", &step.id)?;
        }
        if self.subjects.is_empty() || self.roles.is_empty() {
            return Err(invalid("manifest must declare subjects and roles"));
        }
        if self.gates.is_empty() || self.gates.len() > MAX_GATES {
            return Err(Error::Limit("manifest requires 1 to 16 gates".into()));
        }
        let mut seen = BTreeSet::new();
        for gate in &self.gates {
            super::compiler::validate_identifier("gate id", &gate.id)?;
            if !seen.insert(gate.id.as_str()) {
                return Err(Error::Invalid(format!("duplicate gate id {}", gate.id)));
            }
            if gate.evidence.is_empty() || gate.evidence.len() > MAX_EVIDENCE_PER_GATE {
                return Err(Error::Limit(
                    "a gate requires 1 to 8 evidence entries".into(),
                ));
            }
            for r in gate.evidence.iter().filter_map(EvidenceSpec::param_ref) {
                let string_param = self
                    .parameters
                    .iter()
                    .any(|p| p.name == r && p.kind == ParameterType::String);
                if !string_param {
                    return Err(Error::Invalid(format!(
                        "gate {} references unknown string parameter {r}",
                        gate.id
                    )));
                }
            }
            if !self.roles.contains(&gate.by) {
                return Err(Error::Invalid(format!(
                    "gate {} names an undeclared role",
                    gate.id
                )));
            }
        }
        Ok(())
    }

    /// Manifest defaults overlaid by `overrides`; undeclared names and bad values are refused.
    pub fn effective_params(&self, overrides: Option<&Params>) -> Result<Params> {
        let mut out: Params = self
            .parameters
            .iter()
            .map(|p| (p.name.clone(), p.default.clone()))
            .collect();
        for (name, value) in overrides.into_iter().flatten() {
            let ok = self
                .parameters
                .iter()
                .any(|p| &p.name == name && p.check(value));
            if !ok {
                return Err(Error::Invalid(format!(
                    "parameter {name} is not declared or has a bad value"
                )));
            }
            out.insert(name.clone(), value.clone());
        }
        Ok(out)
    }

    fn gate(&self, id: &str) -> Option<&GateDefinition> {
        self.gates.iter().find(|g| g.id == id)
    }
}

// ---- Embedded catalog -------------------------------------------------------------------

const PR_GATE_V1: &[u8] = include_bytes!("../../skills/supported/pr-gate/v1/skill.json");

#[derive(Clone, Debug, Serialize)]
pub struct CatalogEntry {
    pub name: String,
    pub workflow_version: u32,
    pub sha256: String,
    pub summary: String,
    pub applicability: Applicability,
    pub parameters: Vec<ParameterDefinition>,
    pub gates: Vec<GateDefinition>,
    pub first_party: bool,
    #[serde(skip)]
    pub manifest: SupportedManifest,
}

/// Every skill embedded in this binary.
pub fn catalog() -> &'static [CatalogEntry] {
    static CATALOG: OnceLock<Vec<CatalogEntry>> = OnceLock::new();
    CATALOG.get_or_init(|| {
        [PR_GATE_V1]
            .into_iter()
            .map(|bytes| {
                let manifest = SupportedManifest::parse(bytes).expect("embedded manifest is valid");
                CatalogEntry {
                    name: manifest.name.clone(),
                    workflow_version: manifest.workflow_version,
                    sha256: digest(bytes),
                    summary: manifest.summary.clone(),
                    applicability: manifest.applicability.clone(),
                    parameters: manifest.parameters.clone(),
                    gates: manifest.gates.clone(),
                    first_party: true,
                    manifest,
                }
            })
            .collect()
    })
}

pub fn catalog_entry(name: &str, workflow_version: u32) -> Option<&'static CatalogEntry> {
    catalog()
        .iter()
        .find(|e| e.name == name && e.workflow_version == workflow_version)
}

pub fn catalog_entry_by_sha256(sha256: &str) -> Option<&'static CatalogEntry> {
    catalog().iter().find(|e| e.sha256 == sha256)
}

// ---- Lock file --------------------------------------------------------------------------

pub const LOCK_PATH: &str = "agentscm-skills.lock.json";

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct SkillPin {
    pub name: String,
    pub workflow_version: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    pub sha256: String,
    #[serde(default)]
    pub source: Source,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub params: Option<Params>,
}

/// Where a skill comes from. Only `first_party` resolves in phase 2; the other kinds are
/// reserved and may carry extra fields.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Source {
    pub kind: String,
    #[serde(flatten)]
    pub rest: Map<String, Value>,
}

impl Default for Source {
    fn default() -> Self {
        Self {
            kind: "first_party".into(),
            rest: Map::new(),
        }
    }
}

impl Source {
    fn first_party(&self) -> bool {
        self.kind == "first_party"
    }
}

impl SkillPin {
    /// A first-party pin of `entry` with optional parameter overrides.
    pub fn first_party(entry: &CatalogEntry, params: Option<Params>) -> Self {
        Self {
            name: entry.name.clone(),
            workflow_version: entry.workflow_version,
            path: None,
            sha256: entry.sha256.clone(),
            source: Source::default(),
            params: params.filter(|p| !p.is_empty()),
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct SkillLock {
    pub version: u32,
    pub skills: Vec<SkillPin>,
}

impl SkillLock {
    pub fn parse(bytes: &[u8]) -> Result<Self> {
        if bytes.len() > MAX_BYTES {
            return Err(Error::Limit("skills lock file exceeds 256 KiB".into()));
        }
        let lock: Self = serde_json::from_slice(bytes)?;
        if lock.version != 1 {
            return Err(invalid("skills lock file requires version 1"));
        }
        if lock.skills.len() > MAX_PINS {
            return Err(Error::Limit(
                "skills lock file has more than 32 pins".into(),
            ));
        }
        let mut seen = BTreeSet::new();
        for pin in &lock.skills {
            super::compiler::validate_identifier("skill name", &pin.name)?;
            if let Some(path) = &pin.path {
                validate_path(path)?;
            }
            let reserved = ["first_party", "repo_local", "third_party", "private"];
            if !reserved.contains(&pin.source.kind.as_str())
                || (pin.source.first_party() && !pin.source.rest.is_empty())
            {
                return Err(invalid(
                    "skills lock pin has an unknown or malformed source",
                ));
            }
            if pin.params.as_ref().is_some_and(|p| p.len() > 16) {
                return Err(Error::Limit("a pin has more than 16 params".into()));
            }
            crate::checks::validate_digest("skill sha256", &pin.sha256)?;
            if pin.workflow_version == 0 || !seen.insert(&pin.name) {
                return Err(invalid(
                    "skills lock pins need one positive version per skill",
                ));
            }
        }
        Ok(lock)
    }
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AdoptionStatus {
    Adopted,
    UnknownVersion,
    UnsupportedSource,
    InvalidParams,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct PinStatus {
    #[serde(flatten)]
    pub pin: SkillPin,
    pub status: AdoptionStatus,
}

/// A pin is adopted only when its source is first party, its sha256 matches an embedded
/// manifest of the same name and version, and its params fit that manifest.
pub fn resolve(lock: &SkillLock) -> Vec<PinStatus> {
    lock.skills
        .iter()
        .map(|pin| {
            let entry = catalog_entry_by_sha256(&pin.sha256)
                .filter(|e| e.name == pin.name && e.workflow_version == pin.workflow_version);
            let status = match entry {
                _ if !pin.source.first_party() => AdoptionStatus::UnsupportedSource,
                None => AdoptionStatus::UnknownVersion,
                Some(e) if e.manifest.effective_params(pin.params.as_ref()).is_err() => {
                    AdoptionStatus::InvalidParams
                }
                Some(_) => AdoptionStatus::Adopted,
            };
            PinStatus {
                pin: pin.clone(),
                status,
            }
        })
        .collect()
}

/// The "Supported skills" instruction section for init-repo; empty when nothing is pinned.
pub fn instruction_section(lock: &SkillLock) -> String {
    if lock.skills.is_empty() {
        return String::new();
    }
    let mut out = String::from(
        "### Supported skills\n\nThis repository pins these AgentSCM skills in `agentscm-skills.lock.json`:\n",
    );
    for pin in resolve(lock) {
        let p = &pin.pin;
        let entry = catalog_entry_by_sha256(&p.sha256)
            .filter(|e| e.name == p.name && e.workflow_version == p.workflow_version);
        match (pin.status, entry) {
            (AdoptionStatus::Adopted, Some(e)) => {
                let a = &e.manifest.applicability;
                out += &format!(
                    "\n- {} v{} (adopted): {}\n  Follow when: {}.\n",
                    p.name,
                    p.workflow_version,
                    e.summary,
                    a.triggers.join("; ")
                );
                if !a.not_for.is_empty() {
                    out += &format!("  Not for: {}.\n", a.not_for.join("; "));
                }
                out += &format!("  Procedure: `{}`\n", e.manifest.skill.entrypoint);
            }
            (status, _) => {
                let why = match status {
                    AdoptionStatus::UnsupportedSource => "unsupported source",
                    AdoptionStatus::InvalidParams => "invalid params",
                    _ => "unknown version",
                };
                out += &format!(
                    "\n- {} v{} ({why}): not enforced; update the pin.\n",
                    p.name, p.workflow_version
                );
            }
        }
    }
    out += "\nCheck where you are with `agentscm skill status` (MCP `agentscm_skill_status`) and record each gate with `agentscm skill evidence` (MCP `agentscm_skill_evidence`). Evidence recorded this way is self-reported and labelled so.";
    out
}

/// The lock file as written to disk: pretty JSON with a trailing newline.
pub fn write_lock(lock: &SkillLock) -> Result<Vec<u8>> {
    let mut bytes = serde_json::to_vec_pretty(lock)?;
    bytes.push(b'\n');
    // Refuse to write what parse would refuse to read.
    SkillLock::parse(&bytes)?;
    Ok(bytes)
}

/// Add `pin`, replacing any existing pin of the same skill name in place.
pub fn upsert_pin(lock: &mut SkillLock, pin: SkillPin) {
    match lock.skills.iter_mut().find(|p| p.name == pin.name) {
        Some(existing) => *existing = pin,
        None => lock.skills.push(pin),
    }
}

/// Remove the pin named `name`; whether there was one.
pub fn remove_pin(lock: &mut SkillLock, name: &str) -> bool {
    let before = lock.skills.len();
    lock.skills.retain(|p| p.name != name);
    lock.skills.len() != before
}

// ---- Evidence and run model -------------------------------------------------------------

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct Finding {
    pub id: String,
    pub blocking: bool,
    pub summary: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Dismissal {
    pub id: String,
    pub reason: String,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct Detail {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i64>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub findings: Vec<Finding>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub resolved: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub dismissed: Vec<Dismissal>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub method: Option<String>,
}

/// What an agent records.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Evidence {
    /// The skill name; the hub resolves it, the pure rules ignore it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub skill: Option<String>,
    pub gate: String,
    pub kind: EvidenceKind,
    pub head_sha: String,
    pub assurance: Assurance,
    pub summary: String,
    #[serde(default)]
    pub url: Option<String>,
    #[serde(default)]
    pub branch: Option<String>,
    #[serde(default)]
    pub pull_request: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<Detail>,
}

impl Evidence {
    fn validate(&self) -> Result<()> {
        let text = |what: &str, s: &str, max: usize| {
            if s.len() > max {
                Err(Error::Limit(format!("{what} exceeds {max} bytes")))
            } else {
                Ok(())
            }
        };
        if self.head_sha.len() != 40
            || !self
                .head_sha
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        {
            return Err(invalid("head_sha must be 40 lowercase hex characters"));
        }
        if self.assurance != Assurance::SelfReported {
            return Err(invalid("agents may only record self_reported evidence"));
        }
        text("summary", &self.summary, MAX_SUMMARY)?;
        text("url", self.url.as_deref().unwrap_or(""), MAX_URL)?;
        text("branch", self.branch.as_deref().unwrap_or(""), 255)?;
        if let Some(d) = &self.detail {
            for s in [&d.command, &d.label, &d.method].into_iter().flatten() {
                text("detail text", s, MAX_SUMMARY)?;
            }
            if d.findings.len() > MAX_LIST
                || d.resolved.len() > MAX_LIST
                || d.dismissed.len() > MAX_LIST
            {
                return Err(Error::Limit(
                    "detail lists are limited to 64 entries".into(),
                ));
            }
            for f in &d.findings {
                text("finding id", &f.id, 128)?;
                text("finding summary", &f.summary, MAX_SUMMARY)?;
            }
            for id in &d.resolved {
                text("finding id", id, 128)?;
            }
            for x in &d.dismissed {
                text("finding id", &x.id, 128)?;
                text("dismissal reason", &x.reason, MAX_SUMMARY)?;
            }
        }
        Ok(())
    }
}

/// Evidence as the hub stores it.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct StoredEvidence {
    pub id: String,
    pub session: String,
    pub agent: String,
    pub recorded_at: u64,
    #[serde(flatten)]
    pub evidence: Evidence,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RunState {
    Open,
    Completed,
    Abandoned,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum GateState {
    Done,
    Current,
    NotReached,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct GateView {
    pub id: String,
    pub state: GateState,
    pub by: Role,
    pub enforce: Vec<Enforce>,
    pub min_assurance: Assurance,
    pub assurance: Option<Assurance>,
    pub evidence: Vec<StoredEvidence>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct RunSkill {
    pub name: String,
    pub workflow_version: u32,
    pub sha256: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct RunView {
    pub run_id: String,
    pub repo: String,
    pub branch: String,
    pub pull_request: Option<u64>,
    pub head_sha: String,
    pub state: RunState,
    pub skill: RunSkill,
    pub params: Params,
    pub current_gate: Option<String>,
    pub next_action: String,
    pub blocks_finish: bool,
    pub gates: Vec<GateView>,
    pub stale_evidence: Vec<StoredEvidence>,
    pub updated_at: u64,
}

/// What the hub knows about a run apart from its evidence.
#[derive(Clone, Debug)]
pub struct RunIdentity {
    pub run_id: String,
    pub repo: String,
    pub branch: String,
    pub pull_request: Option<u64>,
    pub skill_sha256: String,
    /// Effective parameters (see `SupportedManifest::effective_params`).
    pub params: Params,
    pub updated_at: u64,
}

// ---- Evaluation -------------------------------------------------------------------------

/// The head is set by the latest head-bound author evidence, else the latest evidence.
fn head_of(manifest: &SupportedManifest, evidence: &[StoredEvidence]) -> String {
    let by_author = |e: &&StoredEvidence| {
        e.evidence.kind.head_bound()
            && manifest
                .gate(&e.evidence.gate)
                .is_some_and(|g| g.by == Role::Author)
    };
    evidence
        .iter()
        .rev()
        .find(by_author)
        .or_else(|| evidence.iter().rev().find(|e| e.evidence.kind.head_bound()))
        .or(evidence.last())
        .map_or_else(String::new, |e| e.evidence.head_sha.clone())
}

fn current(e: &StoredEvidence, head: &str) -> bool {
    !e.evidence.kind.head_bound() || e.evidence.head_sha == head
}

/// Blocking finding ids of `evidence[at]` not resolved or dismissed by later current evidence.
fn unaddressed(evidence: &[StoredEvidence], at: usize, head: &str) -> Vec<String> {
    let mut closed = BTreeSet::new();
    for e in evidence[at + 1..]
        .iter()
        .filter(|e| e.evidence.kind == EvidenceKind::FindingsResolved && current(e, head))
    {
        if let Some(d) = &e.evidence.detail {
            closed.extend(d.resolved.iter().map(String::as_str));
            closed.extend(
                d.dismissed
                    .iter()
                    .filter(|x| !x.reason.trim().is_empty())
                    .map(|x| x.id.as_str()),
            );
        }
    }
    evidence[at]
        .evidence
        .detail
        .iter()
        .flat_map(|d| &d.findings)
        .filter(|f| f.blocking && !closed.contains(f.id.as_str()))
        .map(|f| f.id.clone())
        .collect()
}

/// The review that counts: the latest on the current head. A push after a review needs a new
/// review, so code added while fixing findings is reviewed too.
fn counted_review(evidence: &[StoredEvidence], head: &str) -> Option<usize> {
    evidence
        .iter()
        .enumerate()
        .filter(|(_, e)| e.evidence.kind == EvidenceKind::IndependentReview)
        .rfind(|(_, e)| e.evidence.head_sha == head)
        .map(|(i, _)| i)
}

/// Whether `gate` is satisfied, and the weakest assurance of the evidence that satisfied it.
fn satisfied(
    gate: &GateDefinition,
    evidence: &[StoredEvidence],
    head: &str,
    params: &Params,
) -> (bool, Option<Assurance>) {
    let mut weakest: Option<Assurance> = None;
    let mut note = |a: Assurance| weakest = Some(weakest.map_or(a, |w| w.min(a)));
    for spec in &gate.evidence {
        match spec.kind() {
            EvidenceKind::IndependentReview => {
                match counted_review(evidence, head).map(|i| &evidence[i]) {
                    Some(e) if e.evidence.assurance >= gate.min_assurance => {
                        note(e.evidence.assurance)
                    }
                    _ => return (false, None),
                }
            }
            EvidenceKind::FindingsResolved => {
                let Some(at) = counted_review(evidence, head) else {
                    return (false, None);
                };
                let mut used = vec![&evidence[at]];
                if !unaddressed(evidence, at, head).is_empty() {
                    return (false, None);
                }
                used.extend(evidence[at + 1..].iter().filter(|e| {
                    e.evidence.kind == EvidenceKind::FindingsResolved && current(e, head)
                }));
                for e in used {
                    note(e.evidence.assurance);
                }
            }
            kind => {
                let hit = evidence.iter().rev().find(|e| {
                    e.evidence.gate == gate.id
                        && e.evidence.kind == kind
                        && current(e, head)
                        && e.evidence.assurance >= gate.min_assurance
                        && spec.accepts(e.evidence.detail.as_ref(), params)
                });
                match hit {
                    Some(e) => note(e.evidence.assurance),
                    None => return (false, None),
                }
            }
        }
    }
    (true, weakest)
}

fn next_action(gate: &str, head: &str, params: &Params) -> String {
    let text = |name: &str| {
        params
            .get(name)
            .and_then(Value::as_str)
            .filter(|v| *v != "any")
    };
    let short: String = head.chars().take(7).collect();
    let short = if short.is_empty() {
        "the branch".into()
    } else {
        short
    };
    match gate {
        "branch" => "Create a task branch and record the branch gate.".into(),
        "tests" => format!("Run the tests on {short} and record the result."),
        "review" => format!("Needs an independent review of {short} by another session."),
        "fix" => "Fix the blocking review findings, then record them resolved or dismissed with a reason.".into(),
        "ci" => match text("ci_label") {
            Some(label) => format!("Add the {label} label and wait for green CI on {short}."),
            None => format!("Wait for green CI on {short}."),
        },
        "approval" => "Waiting for maintainer approval.".into(),
        "merge" => {
            let mut s = match text("merge_method") {
                Some(m) => format!("Waiting for a maintainer to merge ({m})."),
                None => "Waiting for a maintainer to merge.".into(),
            };
            if params.get("retarget_stacked").and_then(Value::as_bool) == Some(true) {
                s += " Retarget dependent pull requests afterwards.";
            }
            s
        }
        "post_merge" => "Run the post-merge regression pass on the final main and record it.".into(),
        other => format!("Record evidence for gate {other}."),
    }
}

/// Render a run from its evidence in recorded order. Pure.
pub fn evaluate(
    manifest: &SupportedManifest,
    identity: &RunIdentity,
    evidence: &[StoredEvidence],
) -> RunView {
    let head = head_of(manifest, evidence);
    let mut gates = Vec::new();
    let mut open_gate: Option<&GateDefinition> = None;
    for gate in &manifest.gates {
        let (ok, assurance) = satisfied(gate, evidence, &head, &identity.params);
        let state = if open_gate.is_some() {
            GateState::NotReached
        } else if ok {
            GateState::Done
        } else {
            open_gate = Some(gate);
            GateState::Current
        };
        gates.push(GateView {
            id: gate.id.clone(),
            state,
            by: gate.by,
            enforce: gate.enforce.clone(),
            min_assurance: gate.min_assurance,
            assurance: (state == GateState::Done).then_some(assurance).flatten(),
            evidence: evidence
                .iter()
                .filter(|e| e.evidence.gate == gate.id && current(e, &head))
                .cloned()
                .collect(),
        });
    }
    let (state, next, blocks) = match open_gate {
        Some(g) => (
            RunState::Open,
            next_action(&g.id, &head, &identity.params),
            g.enforce.contains(&Enforce::Stop),
        ),
        None => (RunState::Completed, "All gates are complete.".into(), false),
    };
    RunView {
        run_id: identity.run_id.clone(),
        repo: identity.repo.clone(),
        branch: identity.branch.clone(),
        pull_request: identity
            .pull_request
            .or_else(|| evidence.iter().rev().find_map(|e| e.evidence.pull_request)),
        head_sha: head.clone(),
        state,
        skill: RunSkill {
            name: manifest.name.clone(),
            workflow_version: manifest.workflow_version,
            sha256: identity.skill_sha256.clone(),
        },
        params: identity.params.clone(),
        current_gate: open_gate.map(|g| g.id.clone()),
        next_action: next,
        blocks_finish: blocks,
        gates,
        stale_evidence: evidence
            .iter()
            .filter(|e| !current(e, &head))
            .cloned()
            .collect(),
        updated_at: evidence
            .iter()
            .map(|e| e.recorded_at)
            .max()
            .unwrap_or(identity.updated_at),
    }
}

// ---- Admission --------------------------------------------------------------------------

/// Why evidence is refused. `code` is one of `unknown_gate`, `stale_head`, `not_independent`
/// or `invalid` (bounds and assurance).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Refusal {
    pub code: &'static str,
    pub message: String,
}

fn refuse(code: &'static str, message: impl Into<String>) -> Refusal {
    Refusal {
        code,
        message: message.into(),
    }
}

/// Whether `session` may record `new` given the run's `existing` evidence. Pure.
pub fn admit(
    manifest: &SupportedManifest,
    existing: &[StoredEvidence],
    new: &Evidence,
    session: &str,
) -> std::result::Result<(), Refusal> {
    new.validate()
        .map_err(|e| refuse("invalid", e.to_string()))?;
    let gate = manifest
        .gate(&new.gate)
        .filter(|g| g.evidence.iter().any(|s| s.kind() == new.kind))
        .ok_or_else(|| {
            refuse(
                "unknown_gate",
                format!("gate {} does not accept {:?}", new.gate, new.kind),
            )
        })?;
    if gate.by != Role::Author && new.kind.head_bound() && !existing.is_empty() {
        let head = head_of(manifest, existing);
        if head != new.head_sha {
            return Err(refuse(
                "stale_head",
                format!("the run head is {}, not {}", head, new.head_sha),
            ));
        }
    }
    if new.kind == EvidenceKind::IndependentReview {
        let author = existing.iter().any(|e| {
            e.session == session
                && manifest
                    .gate(&e.evidence.gate)
                    .is_some_and(|g| g.by == Role::Author)
        });
        if author {
            return Err(refuse(
                "not_independent",
                "an author session cannot review its own work",
            ));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const A: &str = "a1b2c3d4e5f60718293a4b5c6d7e8f9012345678";
    const B: &str = "b1b2c3d4e5f60718293a4b5c6d7e8f9012345678";

    fn manifest() -> SupportedManifest {
        catalog_entry("pr-gate", 1).unwrap().manifest.clone()
    }

    fn ident() -> RunIdentity {
        RunIdentity {
            run_id: "r1".into(),
            repo: "acme/widgets".into(),
            branch: "feat/x".into(),
            pull_request: None,
            skill_sha256: "0".repeat(64),
            params: manifest().effective_params(None).unwrap(),
            updated_at: 1,
        }
    }

    fn ev(
        gate: &str,
        kind: EvidenceKind,
        head: &str,
        session: &str,
        detail: Option<Detail>,
    ) -> StoredEvidence {
        StoredEvidence {
            id: "e".into(),
            session: session.into(),
            agent: "claude".into(),
            recorded_at: 10,
            evidence: Evidence {
                skill: None,
                gate: gate.into(),
                kind,
                head_sha: head.into(),
                assurance: Assurance::SelfReported,
                summary: "s".into(),
                url: None,
                branch: Some("feat/x".into()),
                pull_request: None,
                detail,
            },
        }
    }

    fn review(head: &str, findings: &[(&str, bool)]) -> StoredEvidence {
        ev(
            "review",
            EvidenceKind::IndependentReview,
            head,
            "rev",
            Some(Detail {
                findings: findings
                    .iter()
                    .map(|(id, b)| Finding {
                        id: (*id).into(),
                        blocking: *b,
                        summary: "x".into(),
                    })
                    .collect(),
                ..Detail::default()
            }),
        )
    }

    fn tests(head: &str) -> StoredEvidence {
        ev("tests", EvidenceKind::RevisionTests, head, "auth", None)
    }

    fn branch() -> StoredEvidence {
        ev("branch", EvidenceKind::TaskBranch, A, "auth", None)
    }

    fn run(list: &[StoredEvidence]) -> RunView {
        evaluate(&manifest(), &ident(), list)
    }

    #[test]
    fn embedded_manifest_parses_and_catalog_is_listed() {
        let c = catalog();
        assert_eq!(c.len(), 1);
        assert_eq!(c[0].name, "pr-gate");
        assert_eq!(c[0].gates.len(), 8);
        assert_eq!(c[0].sha256.len(), 64);
        assert!(!c[0].summary.is_empty());
    }

    #[test]
    fn parse_refusals() {
        let good = String::from_utf8(PR_GATE_V1.to_vec()).unwrap();
        let bad = |s: String| SupportedManifest::parse(s.as_bytes()).unwrap_err();
        assert!(matches!(
            bad(good.replacen("\"version\": 2", "\"version\": 1", 1)),
            Error::Invalid(_)
        ));
        assert!(matches!(
            bad(good.replacen("\"steps\"", "\"extra\": 1, \"steps\"", 1)),
            Error::Json(_)
        ));
        assert!(matches!(
            bad(good.replacen("\"id\": \"tests\"", "\"id\": \"branch\"", 1)),
            Error::Invalid(_)
        ));
        let mut undeclared: Value = serde_json::from_str(&good).unwrap();
        undeclared["roles"] = serde_json::json!(["author"]);
        assert!(matches!(bad(undeclared.to_string()), Error::Invalid(_)));
        let big = format!("{good}{}", " ".repeat(MAX_BYTES));
        assert!(matches!(bad(big), Error::Limit(_)));
    }

    #[test]
    fn lock_resolves_adopted_and_unknown_version() {
        let sha = catalog()[0].sha256.clone();
        let lock = |sha: &str, v: u32| {
            format!(
                r#"{{"version":1,"skills":[{{"name":"pr-gate","workflow_version":{v},"path":"skills/supported/pr-gate/v1/skill.json","sha256":"{sha}"}}]}}"#
            )
        };
        let with = |extra: &str| lock(&sha, 1).replace("}]}", &format!(",{extra}}}]}}"));
        let status = |text: String| resolve(&SkillLock::parse(text.as_bytes()).unwrap())[0].status;
        assert_eq!(
            status(with(
                r#""source":{"kind":"third_party","url":"u","commit":"c"}"#
            )),
            AdoptionStatus::UnsupportedSource
        );
        assert_eq!(
            status(with(r#""params":{"merge_method":"squash"}"#)),
            AdoptionStatus::Adopted
        );
        assert_eq!(
            status(with(r#""params":{"merge_method":"fast"}"#)),
            AdoptionStatus::InvalidParams
        );
        assert_eq!(
            status(with(r#""params":{"nope":1}"#)),
            AdoptionStatus::InvalidParams
        );
        assert!(
            SkillLock::parse(with(r#""source":{"kind":"first_party","x":1}"#).as_bytes()).is_err()
        );
        assert!(SkillLock::parse(with(r#""source":{"kind":"weird"}"#).as_bytes()).is_err());
        let ok = SkillLock::parse(lock(&sha, 1).as_bytes()).unwrap();
        assert_eq!(resolve(&ok)[0].status, AdoptionStatus::Adopted);
        let other = SkillLock::parse(lock(&"0".repeat(64), 1).as_bytes()).unwrap();
        assert_eq!(resolve(&other)[0].status, AdoptionStatus::UnknownVersion);
        let wrong = SkillLock::parse(lock(&sha, 2).as_bytes()).unwrap();
        assert_eq!(resolve(&wrong)[0].status, AdoptionStatus::UnknownVersion);
        assert!(
            SkillLock::parse(lock(&sha, 1).replace("skills/supported", "../x").as_bytes()).is_err()
        );
        assert!(SkillLock::parse(br#"{"version":2,"skills":[]}"#).is_err());
        assert!(SkillLock::parse(br#"{"version":1,"skills":[],"x":1}"#).is_err());
    }

    #[test]
    fn lock_helpers_upsert_remove_and_write() {
        let entry = catalog_entry("pr-gate", 1).unwrap();
        let mut lock = SkillLock {
            version: 1,
            skills: vec![],
        };
        upsert_pin(&mut lock, SkillPin::first_party(entry, None));
        let mut params = Params::new();
        params.insert("ci_label".into(), "full".into());
        upsert_pin(&mut lock, SkillPin::first_party(entry, Some(params)));
        assert_eq!(lock.skills.len(), 1);
        let bytes = write_lock(&lock).unwrap();
        assert!(bytes.ends_with(b"\n"));
        assert_eq!(SkillLock::parse(&bytes).unwrap(), lock);
        assert!(remove_pin(&mut lock, "pr-gate"));
        assert!(!remove_pin(&mut lock, "pr-gate"));
    }

    #[test]
    fn instruction_section_lists_pins() {
        let sha = catalog()[0].sha256.clone();
        let pin = |sha: &str| SkillPin {
            sha256: sha.into(),
            ..SkillPin::first_party(&catalog()[0], None)
        };
        let text = instruction_section(&SkillLock {
            version: 1,
            skills: vec![pin(&sha), pin(&"0".repeat(64))],
        });
        assert!(text.contains("pr-gate v1 (adopted)"));
        assert!(text.contains("Follow when: opening a pull request"));
        assert!(text.contains("pr-gate v1 (unknown version)"));
        assert!(text.contains("agentscm_skill_evidence"));
        assert_eq!(
            instruction_section(&SkillLock {
                version: 1,
                skills: vec![]
            }),
            ""
        );
    }

    #[test]
    fn manifest_parameters_are_checked() {
        let m = manifest();
        let mut over = Params::new();
        over.insert("ci_label".into(), "x".into());
        let p = m.effective_params(Some(&over)).unwrap();
        assert_eq!(p["ci_label"], "x");
        assert_eq!(p["merge_method"], "any");
        assert_eq!(p["retarget_stacked"], true);
        over.insert("retarget_stacked".into(), "yes".into());
        assert!(m.effective_params(Some(&over)).is_err());
        let good = String::from_utf8(PR_GATE_V1.to_vec()).unwrap();
        let bad = good.replace(
            "\"label_param\": \"ci_label\"",
            "\"label_param\": \"missing\"",
        );
        assert!(SupportedManifest::parse(bad.as_bytes()).is_err());
    }

    #[test]
    fn params_drive_next_action_and_ci_label_matching() {
        let mut ident = ident();
        ident.params.insert("ci_label".into(), "ci-extended".into());
        ident.params.insert("merge_method".into(), "squash".into());
        let mut list = vec![branch(), tests(A), review(A, &[])];
        let m = manifest();
        assert_eq!(
            evaluate(&m, &ident, &list).next_action,
            "Add the ci-extended label and wait for green CI on a1b2c3d."
        );
        assert_eq!(run(&list).next_action, "Wait for green CI on a1b2c3d.");
        // Wrong label does not satisfy; the right one does.
        let ci = |l: &str| {
            ev(
                "ci",
                EvidenceKind::HostedCi,
                A,
                "auth",
                Some(Detail {
                    label: Some(l.into()),
                    ..Detail::default()
                }),
            )
        };
        list.push(ci("other"));
        assert_eq!(
            evaluate(&m, &ident, &list).current_gate.as_deref(),
            Some("ci")
        );
        assert_eq!(run(&list).current_gate.as_deref(), Some("approval"));
        list.push(ci("ci-extended"));
        list.push(ev(
            "approval",
            EvidenceKind::HumanApproval,
            A,
            "maint",
            None,
        ));
        let v = evaluate(&m, &ident, &list);
        assert_eq!(
            v.next_action,
            "Waiting for a maintainer to merge (squash). Retarget dependent pull requests afterwards."
        );
        assert_eq!(v.params["merge_method"], "squash");
    }

    #[test]
    fn repo_lock_pins_the_embedded_manifest() {
        let lock = SkillLock::parse(include_bytes!("../../agentscm-skills.lock.json")).unwrap();
        assert_eq!(resolve(&lock)[0].status, AdoptionStatus::Adopted);
        assert_eq!(
            lock.skills[0].params.as_ref().unwrap()["ci_label"],
            "ci-extended"
        );
    }

    #[test]
    fn transitions_from_empty_to_completed() {
        let v = run(&[]);
        assert_eq!(v.current_gate.as_deref(), Some("branch"));
        assert!(!v.blocks_finish);

        let mut list = vec![branch()];
        let v = run(&list);
        assert_eq!(v.current_gate.as_deref(), Some("tests"));
        assert!(v.blocks_finish);

        list.push(tests(A));
        let v = run(&list);
        assert_eq!(v.current_gate.as_deref(), Some("review"));
        assert!(v.blocks_finish);
        assert_eq!(
            v.next_action,
            "Needs an independent review of a1b2c3d by another session."
        );
        assert_eq!(v.gates[1].assurance, Some(Assurance::SelfReported));

        // A new author push makes the tests stale; running them again is current once more.
        list.push(tests(B));
        let v = run(&list);
        assert_eq!(v.head_sha, B);
        assert_eq!(v.stale_evidence.len(), 1);
        list.push(review(B, &[("f1", true)]));
        let v = run(&list);
        assert_eq!(v.current_gate.as_deref(), Some("fix"));
        assert!(v.blocks_finish);

        let resolved = Detail {
            resolved: vec!["f1".into()],
            ..Detail::default()
        };
        list.push(ev(
            "fix",
            EvidenceKind::FindingsResolved,
            B,
            "auth",
            Some(resolved),
        ));
        let v = run(&list);
        assert_eq!(v.current_gate.as_deref(), Some("ci"));
        assert!(!v.blocks_finish);

        let ci = Detail {
            label: Some("ci-extended".into()),
            ..Detail::default()
        };
        list.push(ev("ci", EvidenceKind::HostedCi, B, "auth", Some(ci)));
        list.push(ev(
            "approval",
            EvidenceKind::HumanApproval,
            B,
            "maint",
            None,
        ));
        let merged = Detail {
            method: Some("squash".into()),
            ..Detail::default()
        };
        list.push(ev("merge", EvidenceKind::Merged, A, "maint", Some(merged)));
        assert_eq!(run(&list).current_gate.as_deref(), Some("post_merge"));
        list.push(ev(
            "post_merge",
            EvidenceKind::PostMergeCheck,
            A,
            "auth",
            None,
        ));
        let v = run(&list);
        assert_eq!(v.state, RunState::Completed);
        assert_eq!(v.current_gate, None);
        assert!(
            v.gates
                .iter()
                .all(|g| g.assurance == Some(Assurance::SelfReported))
        );
    }

    #[test]
    fn push_after_review_requires_rereview() {
        let mut list = vec![
            branch(),
            tests(A),
            review(A, &[("f1", true), ("f2", false)]),
        ];
        assert_eq!(run(&list).current_gate.as_deref(), Some("fix"));
        list.push(tests(B));
        // Head moved: the review of A no longer counts, even once its findings are resolved.
        assert_eq!(run(&list).current_gate.as_deref(), Some("review"));
        let resolved = Detail {
            resolved: vec!["f1".into()],
            ..Detail::default()
        };
        list.push(ev(
            "fix",
            EvidenceKind::FindingsResolved,
            B,
            "auth",
            Some(resolved),
        ));
        assert_eq!(run(&list).current_gate.as_deref(), Some("review"));
        list.push(review(B, &[]));
        assert_eq!(run(&list).current_gate.as_deref(), Some("ci"));

        // A clean review does not survive a later push either.
        let clean = vec![branch(), tests(A), review(A, &[]), tests(B)];
        assert_eq!(run(&clean).current_gate.as_deref(), Some("review"));
    }

    #[test]
    fn dismissal_needs_a_reason() {
        let dismiss = |reason: &str| Detail {
            dismissed: vec![Dismissal {
                id: "f1".into(),
                reason: reason.into(),
            }],
            ..Detail::default()
        };
        let mut list = vec![branch(), tests(A), review(A, &[("f1", true)])];
        list.push(ev(
            "fix",
            EvidenceKind::FindingsResolved,
            A,
            "auth",
            Some(dismiss(" ")),
        ));
        assert_eq!(run(&list).current_gate.as_deref(), Some("fix"));
        list.push(ev(
            "fix",
            EvidenceKind::FindingsResolved,
            A,
            "auth",
            Some(dismiss("not a bug")),
        ));
        assert_eq!(run(&list).current_gate.as_deref(), Some("ci"));
    }

    fn new_evidence(gate: &str, kind: EvidenceKind, head: &str) -> Evidence {
        ev(gate, kind, head, "x", None).evidence
    }

    #[test]
    fn admission_rules() {
        let m = manifest();
        let list = vec![branch(), tests(A)];
        let review_new = new_evidence("review", EvidenceKind::IndependentReview, A);
        assert_eq!(
            admit(&m, &list, &review_new, "auth").unwrap_err().code,
            "not_independent"
        );
        assert!(admit(&m, &list, &review_new, "other").is_ok());
        let stale = new_evidence("review", EvidenceKind::IndependentReview, B);
        assert_eq!(
            admit(&m, &list, &stale, "other").unwrap_err().code,
            "stale_head"
        );
        let unknown = new_evidence("nope", EvidenceKind::RevisionTests, A);
        assert_eq!(
            admit(&m, &list, &unknown, "s").unwrap_err().code,
            "unknown_gate"
        );
        let wrong_kind = new_evidence("tests", EvidenceKind::HumanApproval, A);
        assert_eq!(
            admit(&m, &list, &wrong_kind, "s").unwrap_err().code,
            "unknown_gate"
        );
        // Authors may push a new head.
        assert!(
            admit(
                &m,
                &list,
                &new_evidence("tests", EvidenceKind::RevisionTests, B),
                "auth"
            )
            .is_ok()
        );
        // First evidence of a run, and a merge commit, are not stale.
        assert!(admit(&m, &[], &review_new, "other").is_ok());
        assert!(
            admit(
                &m,
                &list,
                &new_evidence("merge", EvidenceKind::Merged, B),
                "m"
            )
            .is_ok()
        );
    }

    #[test]
    fn admission_bounds() {
        let m = manifest();
        let mut e = new_evidence("tests", EvidenceKind::RevisionTests, A);
        e.summary = "x".repeat(501);
        assert_eq!(admit(&m, &[], &e, "s").unwrap_err().code, "invalid");
        e.summary = "ok".into();
        e.url = Some("u".repeat(2049));
        assert_eq!(admit(&m, &[], &e, "s").unwrap_err().code, "invalid");
        e.url = None;
        e.head_sha = "abc".into();
        assert_eq!(admit(&m, &[], &e, "s").unwrap_err().code, "invalid");
        e.head_sha = A.into();
        e.assurance = Assurance::Receipt;
        assert_eq!(admit(&m, &[], &e, "s").unwrap_err().code, "invalid");
    }

    #[test]
    fn min_assurance_is_enforced_and_views_round_trip() {
        let mut m = manifest();
        m.gates[1].min_assurance = Assurance::Observed;
        let v = evaluate(&m, &ident(), &[branch(), tests(A)]);
        assert_eq!(v.current_gate.as_deref(), Some("tests"));
        let v = run(&[branch(), tests(A)]);
        let json = serde_json::to_string(&v).unwrap();
        assert!(json.contains("\"self_reported\""));
        assert_eq!(serde_json::from_str::<RunView>(&json).unwrap(), v);
    }
}
