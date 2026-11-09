//! Git-bound local validation evidence.
//!
//! A profile over portable evidence v1: a maintainer's full local gate is
//! recorded as a bounded `record` artifact and committed by a signed
//! `worker_asserted` envelope. The verdict is pure: it reads no files, runs no
//! Git and never defaults to success. A signature proves only that a registered
//! key holder signed these bytes, not that the commands ran as recorded.

use std::collections::{BTreeSet, HashSet};

pub mod bundle;
pub mod checks;
pub mod comment;
#[cfg(unix)]
pub mod fetch;
#[cfg(unix)]
pub mod reconcile;
#[cfg(unix)]
pub mod run;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::checks::CheckSubject;
use crate::context::{ChangeId, DependencyClaim, DependencyState};
use crate::evidence::{
    ArtifactAvailability, ArtifactInput, ArtifactRecord, ArtifactStatus, CoverageClaim,
    CurrentIssuerTrust, EvaluationClock, EvidenceOutcome, EvidencePayload, ExecutionBasis,
    ExecutionClaimStatus, Freshness, IntegrityStatus, OutputCommitment, RecoveryState,
    RegistryBindingStatus, SuppliedArtifact, TerminalState, validate_identifier,
    validate_lower_hex, verify_evidence,
};
use crate::policy::issuers::IssuerRegistrySnapshot;
use crate::{Error, Result};

pub const MAX_RECORD_BYTES: usize = 64 * 1024;
const MAX_COMMANDS: usize = 32;
const MAX_ARGV: usize = 64;
const MAX_TEXT: usize = 2048;
const MAX_TOOLCHAIN_LINES: usize = 16;
const MAX_DEPENDENCY_FILES: usize = 16;
const MAX_PATHSPECS: usize = 64;
const DEFAULT_DEPENDENCY_FILES: [&str; 2] = ["Cargo.lock", "rust-toolchain.toml"];
const DEFAULT_TOOLCHAIN_PROBES: [&[&str]; 3] = [
    &["rustc", "-V"],
    &["cargo", "-V"],
    &["cargo", "nextest", "--version"],
];
const MAX_MAX_AGE_SECONDS: u64 = 366 * 24 * 3600;
pub const CHECK_KIND: &str = "command/v1";
pub const REQUIREMENT_ID: &str = "local-validation";
/// Commit-status context of a policy without a `check_id`.
pub const STATUS_CONTEXT: &str = "agentscm/local-validation";
const MAX_CHECK_ID: usize = 40;
const MAX_CONSTITUENT_NAME: usize = 64;
pub const DEFAULT_VERIFIER_WORKFLOW: &str = ".github/workflows/local-validation.yml";
/// Surfaces the local gate never covers, recorded as coverage omissions.
const FIXED_OMISSIONS: [&str; 5] = [
    "ignored tests",
    "windows",
    "container smoke",
    "hub end-to-end",
    "live execution",
];

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct GitValidationSubject {
    pub repository: String,
    pub commit: String,
    pub tree: String,
}

/// The base tip is deliberately not bound: it changes on every push to the base.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct GitValidationContext {
    pub base_ref: String,
    pub merge_base: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct RecipeCommand {
    pub id: String,
    pub argv: Vec<String>,
    pub timeout_secs: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub test_selection: Option<TestSelection>,
    /// Git pathspecs; the command is required only when the change touches one. Absent means
    /// always required. Declaring them changes the recipe digest.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub paths: Option<Vec<String>>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ValidationRecipe {
    pub recipe_id: String,
    pub commands: Vec<RecipeCommand>,
    /// Repository files whose digests are bound into the evidence as dependencies. Absent means
    /// the Rust defaults. Declaring them changes the recipe digest.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dependency_files: Option<Vec<String>>,
    /// Toolchain version probes (argv each) recorded before the always-appended `git --version`.
    /// Absent means the Rust defaults.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub toolchain_probes: Option<Vec<Vec<String>>>,
    /// Groups of recipe commands that report as separate results (statuses). Absent means one
    /// aggregate result. Declaring them changes the recipe digest.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub constituents: Option<Vec<Constituent>>,
}

/// Exact paths and reviewed nextest expressions, independent of forge scheduling.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct TestSelection {
    pub rules: Vec<TestSelectionRule>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct TestSelectionRule {
    pub path: String,
    pub filter: String,
}

/// Why a selection used a subset or retained the full command.
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SelectionReason {
    Selected,
    UnmappedPath,
    NonModifiedPath,
    TrustInput,
    SharedInput,
    EmptyChange,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct CommandSelection {
    pub command_id: String,
    pub reason: SelectionReason,
    pub paths: Vec<String>,
    pub filters: Vec<String>,
    pub argv: Vec<String>,
}

fn effective_argv<'a>(
    command: &'a RecipeCommand,
    selections: &'a [CommandSelection],
) -> &'a [String] {
    selections
        .iter()
        .find(|s| s.command_id == command.id)
        .map_or(&command.argv, |s| &s.argv)
}

/// A named group of recipe commands reported on its own. Commands in no constituent are setup
/// and affect only the aggregate result.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Constituent {
    pub id: String,
    pub name: String,
    /// Recipe command ids; each belongs to at most one constituent.
    pub commands: Vec<String>,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum StopReason {
    Exited,
    TimedOut,
    OutputLimit,
    Cancelled,
    SpawnFailed,
    IoError,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct CommandResult {
    pub id: String,
    pub argv: Vec<String>,
    pub exit_code: Option<i32>,
    pub stop: StopReason,
    pub duration_ms: u64,
    pub stdout_sha256: String,
    pub stderr_sha256: String,
    pub truncated: bool,
    /// Collection lost stream or child completion evidence.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub incomplete: bool,
}

/// Whether the PostgreSQL / S3 test environment was present for the run.
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Services {
    pub postgres: bool,
    pub s3: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ValidationRecord {
    pub subject: GitValidationSubject,
    pub context: GitValidationContext,
    /// Display only; never compared.
    pub base_tip: String,
    pub recipe_sha256: String,
    pub results: Vec<CommandResult>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub selections: Vec<CommandSelection>,
    /// Recipe commands not run because the changed paths did not require them, in recipe order.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub not_required: Vec<String>,
    pub services: Services,
    pub platform: String,
    pub toolchain: Vec<String>,
    pub tree_unchanged_after: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ValidationPolicy {
    pub repository: String,
    pub recipe_sha256: String,
    pub required_services: Services,
    pub max_age_seconds: u64,
    pub epoch: u64,
    /// Git pathspecs. A change whose every path matches one, and that touches no trust input,
    /// needs no attestation. Absent means nothing is exempt.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exempt_paths: Option<Vec<String>>,
    /// Names this check among several in one repository (`^[a-z0-9][a-z0-9-]{0,39}$`). Absent is
    /// the legacy single check. Declaring it changes the policy digest.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub check_id: Option<String>,
}

fn text(kind: &str, value: &str) -> Result<()> {
    if value.is_empty() || value.len() > MAX_TEXT || value.contains('\0') {
        return Err(Error::Limit(format!(
            "{kind} must be NUL-free and contain 1 to {MAX_TEXT} bytes"
        )));
    }
    Ok(())
}

fn oid(kind: &str, value: &str) -> Result<()> {
    validate_lower_hex(kind, value, if value.len() == 40 { 20 } else { 32 })
}

impl GitValidationSubject {
    pub fn validate(&self) -> Result<()> {
        validate_identifier("repository", &self.repository)?;
        oid("commit", &self.commit)?;
        oid("tree", &self.tree)
    }
}

impl GitValidationContext {
    pub fn validate(&self) -> Result<()> {
        validate_identifier("base ref", &self.base_ref)?;
        oid("merge base", &self.merge_base)
    }
}

fn validate_argv(argv: &[String]) -> Result<()> {
    if argv.is_empty() || argv.len() > MAX_ARGV {
        return Err(Error::Limit(format!(
            "argv must have 1 to {MAX_ARGV} items"
        )));
    }
    argv.iter().try_for_each(|arg| text("argv item", arg))
}

/// A relative repository path that is also a valid evidence dependency identity.
fn validate_dependency_path(path: &str) -> Result<()> {
    validate_identifier("dependency file", path)?;
    if path.starts_with('-') || path.split('/').any(|c| matches!(c, "" | "." | "..")) {
        return Err(Error::Invalid(format!(
            "dependency file must be a relative path without empty, '.' or '..' components: {path}"
        )));
    }
    Ok(())
}

/// A Git pathspec that cannot be an option and uses only the long magic form with a fixed word
/// set; short magic, `attr` and `icase` are rejected.
fn pathspec(spec: &str) -> Result<()> {
    text("pathspec", spec)?;
    if spec.starts_with('-') {
        return Err(Error::Invalid("pathspec must not start with '-'".into()));
    }
    if let Some(magic) = spec.strip_prefix(':') {
        let parsed = magic
            .strip_prefix('(')
            .and_then(|rest| rest.split_once(')'))
            .filter(|(words, _)| {
                words
                    .split(',')
                    .all(|w| matches!(w, "glob" | "literal" | "exclude" | "top"))
            });
        let Some((words, path)) = parsed else {
            return Err(Error::Invalid(format!(
                "pathspec magic must be :(glob|literal|exclude|top,...): {spec}"
            )));
        };
        let has = |word| words.split(',').any(|w| w == word);
        if (has("glob") && has("literal")) || path.is_empty() {
            return Err(Error::Invalid(format!(
                "pathspec must not combine glob and literal or have an empty path: {spec}"
            )));
        }
        if path.starts_with('/') {
            return Err(Error::Invalid(format!(
                "pathspec must not start with '/': {spec}"
            )));
        }
    } else if spec.starts_with('/') {
        return Err(Error::Invalid(format!(
            "pathspec must not start with '/': {spec}"
        )));
    }
    let path = spec
        .strip_prefix(':')
        .map_or(spec, |m| m.split_once(')').map_or("", |(_, p)| p));
    if path.split('/').any(|part| part == "..") {
        return Err(Error::Invalid(format!(
            "pathspec must not contain a '..' component: {spec}"
        )));
    }
    Ok(())
}

fn validate_check_id(kind: &str, id: &str) -> Result<()> {
    let mut bytes = id.bytes();
    let ok = id.len() <= MAX_CHECK_ID
        && bytes
            .next()
            .is_some_and(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
        && bytes.all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-');
    if ok {
        Ok(())
    } else {
        Err(Error::Invalid(format!(
            "{kind} must match ^[a-z0-9][a-z0-9-]{{0,{}}}$",
            MAX_CHECK_ID - 1
        )))
    }
}

fn pathspecs(kind: &str, specs: &Option<Vec<String>>) -> Result<()> {
    let Some(specs) = specs else { return Ok(()) };
    if specs.is_empty() || specs.len() > MAX_PATHSPECS {
        return Err(Error::Limit(format!(
            "{kind} must have 1 to {MAX_PATHSPECS} pathspecs"
        )));
    }
    specs.iter().try_for_each(|spec| pathspec(spec))
}

impl ValidationRecipe {
    pub fn validate(&self) -> Result<()> {
        validate_identifier("recipe ID", &self.recipe_id)?;
        if self.commands.is_empty() || self.commands.len() > MAX_COMMANDS {
            return Err(Error::Limit(format!(
                "recipe must have 1 to {MAX_COMMANDS} commands"
            )));
        }
        let mut ids = HashSet::new();
        for command in &self.commands {
            validate_identifier("command ID", &command.id)?;
            validate_argv(&command.argv)?;
            if command.timeout_secs == 0 || !ids.insert(command.id.as_str()) {
                return Err(Error::Invalid("invalid or duplicate recipe command".into()));
            }
            pathspecs("command paths", &command.paths)?;
            if let Some(selection) = &command.test_selection {
                if command.argv != ["cargo", "nextest", "run", "--locked"]
                    || selection.rules.is_empty()
                    || selection.rules.len() > MAX_PATHSPECS
                {
                    return Err(Error::Invalid(
                        "test selection needs unfiltered cargo nextest run and bounded rules"
                            .into(),
                    ));
                }
                let mut paths = HashSet::new();
                for rule in &selection.rules {
                    validate_dependency_path(&rule.path)?;
                    text("test filter", &rule.filter)?;
                    if !paths.insert(&rule.path) {
                        return Err(Error::Invalid("duplicate test selection path".into()));
                    }
                }
            }
        }
        if let Some(files) = &self.dependency_files {
            if files.is_empty() || files.len() > MAX_DEPENDENCY_FILES {
                return Err(Error::Limit(format!(
                    "dependency files must have 1 to {MAX_DEPENDENCY_FILES} paths"
                )));
            }
            let mut seen = HashSet::new();
            for path in files {
                validate_dependency_path(path)?;
                if !seen.insert(path.as_str()) {
                    return Err(Error::Invalid(format!("duplicate dependency file: {path}")));
                }
            }
        }
        if let Some(probes) = &self.toolchain_probes {
            // One line is reserved for the appended `git --version` probe.
            if probes.is_empty() || probes.len() >= MAX_TOOLCHAIN_LINES {
                return Err(Error::Limit(format!(
                    "toolchain probes must have 1 to {} entries",
                    MAX_TOOLCHAIN_LINES - 1
                )));
            }
            probes.iter().try_for_each(|argv| validate_argv(argv))?;
        }
        if let Some(groups) = &self.constituents {
            self.validate_constituents(groups)?;
        }
        Ok(())
    }

    fn validate_constituents(&self, groups: &[Constituent]) -> Result<()> {
        if groups.is_empty() || groups.len() > MAX_COMMANDS {
            return Err(Error::Limit(format!(
                "constituents must have 1 to {MAX_COMMANDS} entries"
            )));
        }
        let command_ids: HashSet<&str> = self.commands.iter().map(|c| c.id.as_str()).collect();
        let (mut ids, mut mapped) = (HashSet::new(), HashSet::new());
        for group in groups {
            validate_check_id("constituent ID", &group.id)?;
            let name_len = group.name.chars().count();
            if !(1..=MAX_CONSTITUENT_NAME).contains(&name_len)
                || group.name.chars().any(char::is_control)
            {
                return Err(Error::Invalid(format!(
                    "constituent name must be 1 to {MAX_CONSTITUENT_NAME} characters without control characters"
                )));
            }
            if group.commands.is_empty() || !ids.insert(group.id.as_str()) {
                return Err(Error::Invalid(
                    "constituent needs commands and a unique ID".into(),
                ));
            }
            for command in &group.commands {
                if !command_ids.contains(command.as_str()) || !mapped.insert(command.as_str()) {
                    return Err(Error::Invalid(format!(
                        "constituent command is unknown or in more than one constituent: {command}"
                    )));
                }
            }
        }
        Ok(())
    }

    /// Declared dependency files, or the Rust defaults.
    pub fn dependency_file_list(&self) -> Vec<String> {
        self.dependency_files.clone().unwrap_or_else(|| {
            DEFAULT_DEPENDENCY_FILES
                .iter()
                .map(|p| (*p).to_owned())
                .collect()
        })
    }

    /// Declared toolchain probes, or the Rust defaults, followed by `git --version`.
    pub fn toolchain_probe_list(&self) -> Vec<Vec<String>> {
        let mut probes = self.toolchain_probes.clone().unwrap_or_else(|| {
            DEFAULT_TOOLCHAIN_PROBES
                .iter()
                .map(|argv| argv.iter().map(|a| (*a).to_owned()).collect())
                .collect()
        });
        probes.push(vec!["git".into(), "--version".into()]);
        probes
    }

    pub fn from_slice(input: &[u8]) -> Result<Self> {
        let recipe: Self = from_bounded_json(input)?;
        recipe.validate()?;
        Ok(recipe)
    }

    /// The first position where `results` departs from this recipe's `required` commands (ids and
    /// argv, in order), as a detail. A missing command is reported by id.
    fn first_departure(
        &self,
        results: &[CommandResult],
        required: &[String],
        selections: &[CommandSelection],
    ) -> Option<String> {
        let commands: Vec<&RecipeCommand> = self
            .commands
            .iter()
            .filter(|c| required.contains(&c.id))
            .collect();
        for (index, command) in commands.iter().enumerate() {
            match results.get(index) {
                None => return Some(format!("{} did not run", command.id)),
                Some(r) if r.id != command.id || r.argv != effective_argv(command, selections) => {
                    return Some(format!("{} differs from the recipe", r.id));
                }
                Some(_) => {}
            }
        }
        results
            .get(commands.len())
            .map(|r| format!("{} is not in the recipe", r.id))
    }

    /// SHA-256 of the recipe's canonical (struct-ordered JSON) bytes.
    pub fn sha256(&self) -> Result<String> {
        self.validate()?;
        Ok(format!("{:x}", Sha256::digest(serde_json::to_vec(self)?)))
    }
}

/// Strict (unknown fields are denied by the types) and size-bounded decoding.
fn from_bounded_json<T: serde::de::DeserializeOwned>(input: &[u8]) -> Result<T> {
    if input.len() > MAX_RECORD_BYTES {
        return Err(Error::Limit(format!(
            "validation input exceeds {MAX_RECORD_BYTES} bytes"
        )));
    }
    Ok(serde_json::from_slice(input)?)
}

impl ValidationRecord {
    pub fn from_slice(input: &[u8]) -> Result<Self> {
        let record: Self = from_bounded_json(input)?;
        record.validate()?;
        Ok(record)
    }

    pub fn to_bytes(&self) -> Result<Vec<u8>> {
        self.validate()?;
        let bytes = serde_json::to_vec(self)?;
        if bytes.len() > MAX_RECORD_BYTES {
            return Err(Error::Limit("validation record exceeds byte limit".into()));
        }
        Ok(bytes)
    }

    pub fn validate(&self) -> Result<()> {
        self.subject.validate()?;
        self.context.validate()?;
        oid("base tip", &self.base_tip)?;
        validate_lower_hex("recipe digest", &self.recipe_sha256, 32)?;
        if self.results.len() > MAX_COMMANDS
            || (self.results.is_empty() && self.not_required.is_empty())
        {
            return Err(Error::Limit(format!(
                "record must have at most {MAX_COMMANDS} results and at least one result or not-required command"
            )));
        }
        if self.selections.len() > MAX_COMMANDS {
            return Err(Error::Limit("too many command selections".into()));
        }
        let mut selected_ids = HashSet::new();
        for selection in &self.selections {
            validate_identifier("selected command", &selection.command_id)?;
            validate_argv(&selection.argv)?;
            if !selected_ids.insert(&selection.command_id)
                || selection.paths.len() > 256
                || selection.filters.len() > MAX_PATHSPECS
            {
                return Err(Error::Limit("duplicate or oversized selection".into()));
            }
            selection
                .paths
                .iter()
                .try_for_each(|p| text("selected path", p))?;
            selection
                .filters
                .iter()
                .try_for_each(|f| text("selected filter", f))?;
        }
        let mut skipped = HashSet::new();
        for id in &self.not_required {
            validate_identifier("command ID", id)?;
            if self.not_required.len() > MAX_COMMANDS || !skipped.insert(id.as_str()) {
                return Err(Error::Invalid(
                    "duplicate or excess not-required command".into(),
                ));
            }
        }
        let mut ids = HashSet::new();
        for result in &self.results {
            if skipped.contains(result.id.as_str()) {
                return Err(Error::Invalid(
                    "command is both a result and not required".into(),
                ));
            }
            validate_identifier("command ID", &result.id)?;
            validate_argv(&result.argv)?;
            validate_lower_hex("stdout digest", &result.stdout_sha256, 32)?;
            validate_lower_hex("stderr digest", &result.stderr_sha256, 32)?;
            if (result.stop == StopReason::Exited) != result.exit_code.is_some()
                || !ids.insert(result.id.as_str())
            {
                return Err(Error::Invalid(
                    "inconsistent or duplicate command result".into(),
                ));
            }
        }
        text("platform", &self.platform)?;
        if self.toolchain.is_empty() || self.toolchain.len() > MAX_TOOLCHAIN_LINES {
            return Err(Error::Limit(format!(
                "toolchain must have 1 to {MAX_TOOLCHAIN_LINES} lines"
            )));
        }
        self.toolchain
            .iter()
            .try_for_each(|line| text("toolchain line", line))
    }

    /// The first reason this run cannot count as a complete pass, if any.
    /// `required` names the services whose absence makes the run incomplete.
    fn shortfall(&self, required: Services) -> Option<(&'static str, String)> {
        for r in &self.results {
            if r.stop != StopReason::Exited {
                return Some((
                    "command did not exit",
                    format!("{} stopped: {:?}", r.id, r.stop),
                ));
            }
            if r.exit_code != Some(0) {
                return Some((
                    "command failed",
                    match r.exit_code {
                        Some(code) => format!("{} exited with {code}", r.id),
                        None => format!("{} exited without a code", r.id),
                    },
                ));
            }
            if r.incomplete {
                return Some((
                    "capture incomplete",
                    format!("{} capture was incomplete", r.id),
                ));
            }
            if r.truncated {
                return Some(("output truncated", format!("{} output was truncated", r.id)));
            }
        }
        if (required.postgres && !self.services.postgres) || (required.s3 && !self.services.s3) {
            return Some((
                "required service missing",
                "service environment absent".into(),
            ));
        }
        if !self.tree_unchanged_after {
            return Some(("tree changed", "working tree changed during the run".into()));
        }
        None
    }
}

impl ValidationPolicy {
    pub fn from_slice(input: &[u8]) -> Result<Self> {
        let policy: Self = from_bounded_json(input)?;
        policy.validate()?;
        Ok(policy)
    }

    pub fn validate(&self) -> Result<()> {
        validate_identifier("repository", &self.repository)?;
        validate_lower_hex("recipe digest", &self.recipe_sha256, 32)?;
        if self.max_age_seconds == 0 || self.max_age_seconds > MAX_MAX_AGE_SECONDS {
            return Err(Error::Limit("max age out of range".into()));
        }
        pathspecs("exempt paths", &self.exempt_paths)?;
        if let Some(id) = &self.check_id {
            validate_check_id("check ID", id)?;
        }
        Ok(())
    }

    /// The requirement this policy's evidence is signed for.
    pub fn requirement_id(&self) -> String {
        self.check_id.as_ref().map_or_else(
            || REQUIREMENT_ID.to_owned(),
            |id| format!("{REQUIREMENT_ID}/{id}"),
        )
    }

    /// The commit-status context of this check's aggregate result.
    pub fn status_context(&self) -> String {
        self.check_id.as_ref().map_or_else(
            || STATUS_CONTEXT.to_owned(),
            |id| format!("{STATUS_CONTEXT}/{id}"),
        )
    }

    /// The commit-status context of one constituent of this check.
    pub fn constituent_status_context(&self, constituent_id: &str) -> String {
        format!("{}/{constituent_id}", self.status_context())
    }

    pub fn digest(&self) -> Result<String> {
        self.validate()?;
        Ok(format!("{:x}", Sha256::digest(serde_json::to_vec(self)?)))
    }
}

fn domain_digest(domain: &str, fields: &[&str]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(domain.as_bytes());
    hasher.update([0]);
    for field in fields {
        hasher.update((field.len() as u64).to_be_bytes());
        hasher.update(field.as_bytes());
    }
    format!("{:x}", hasher.finalize())
}

pub fn subject_digest(subject: &GitValidationSubject) -> String {
    domain_digest(
        "agentscm.git-validation-subject.v1",
        &[&subject.repository, &subject.commit, &subject.tree],
    )
}

pub fn context_digest(context: &GitValidationContext) -> String {
    domain_digest(
        "agentscm.git-validation-context.v1",
        &[&context.base_ref, &context.merge_base],
    )
}

fn toolchain_digest(record: &ValidationRecord) -> String {
    let lines: Vec<&str> = record.toolchain.iter().map(String::as_str).collect();
    domain_digest("agentscm.git-validation-toolchain.v1", &lines)
}

fn environment_digest(record: &ValidationRecord) -> String {
    let flag = |b: bool| if b { "1" } else { "0" };
    domain_digest(
        "agentscm.git-validation-environment.v1",
        &[
            &record.platform,
            flag(record.services.postgres),
            flag(record.services.s3),
        ],
    )
}

fn commit_change_id(commit: &str) -> String {
    format!("git-commit:{commit}")
}

/// Producer-side inputs that are not part of the record.
pub struct Issuance<'a> {
    pub issuer_id: &'a str,
    pub key_id: &'a str,
    pub attempt_id: &'a str,
    pub run_generation: u64,
    pub issued_at_ms: u64,
    pub expires_at_ms: u64,
    pub issuer_registry_sha256: &'a str,
    /// (identity, sha256) of each declared dependency file, in recipe order.
    pub dependencies: &'a [(String, String)],
}

/// Builds the payload to sign and the exact `record` artifact bytes it commits to.
pub fn evidence_payload(
    record: &ValidationRecord,
    recipe: &ValidationRecipe,
    policy: &ValidationPolicy,
    issuance: &Issuance<'_>,
) -> Result<(EvidencePayload, Vec<u8>)> {
    let record_bytes = record.to_bytes()?;
    let record_sha256 = format!("{:x}", Sha256::digest(&record_bytes));
    let required: Vec<String> = recipe
        .commands
        .iter()
        .filter(|c| !record.not_required.contains(&c.id))
        .map(|c| c.id.clone())
        .collect();
    let pass = record.shortfall(policy.required_services).is_none()
        && recipe
            .first_departure(&record.results, &required, &record.selections)
            .is_none();
    let mut omissions: Vec<String> = FIXED_OMISSIONS.iter().map(|s| (*s).to_owned()).collect();
    omissions.extend(
        record
            .selections
            .iter()
            .filter(|s| s.reason == SelectionReason::Selected)
            .map(|s| format!("reduced test selection: {}", s.command_id)),
    );
    omissions.extend(
        required
            .iter()
            .filter(|id| !record.results.iter().any(|r| &r.id == *id))
            .map(|id| format!("skipped: {id}")),
    );
    omissions.extend(
        record
            .not_required
            .iter()
            .map(|id| format!("not required: {id}")),
    );
    if policy.required_services.postgres && !record.services.postgres {
        omissions.push("postgres tests".into());
    }
    if policy.required_services.s3 && !record.services.s3 {
        omissions.push("s3 tests".into());
    }
    let dependency = |identity: &str, revision: &str| DependencyClaim {
        identity: identity.into(),
        state: DependencyState::Resolved {
            revision: revision.into(),
        },
    };
    let empty = format!("{:x}", Sha256::digest(b""));
    let payload = EvidencePayload {
        version: 1,
        requirement_id: policy.requirement_id(),
        check_kind: CHECK_KIND.into(),
        subject: CheckSubject {
            repository: record.subject.repository.clone(),
            change: ChangeId {
                repository: record.subject.repository.clone(),
                id: commit_change_id(&record.subject.commit),
            },
            change_version: 1,
            context_digest: context_digest(&record.context),
            revision: subject_digest(&record.subject),
            recipe_sha256: record.recipe_sha256.clone(),
            toolchain_sha256: toolchain_digest(record),
            environment_sha256: environment_digest(record),
        },
        installed_policy_sha256: policy.digest()?,
        installed_policy_epoch: policy.epoch,
        issuer_registry_sha256: issuance.issuer_registry_sha256.into(),
        dependency_snapshot_sha256: None,
        attempt_id: issuance.attempt_id.into(),
        run_generation: issuance.run_generation,
        // The recipe carries both the commands and their timeouts.
        request_sha256: record.recipe_sha256.clone(),
        limits_sha256: record.recipe_sha256.clone(),
        terminal_record_sha256: record_sha256.clone(),
        issuer_id: issuance.issuer_id.into(),
        key_id: issuance.key_id.into(),
        issued_at_ms: issuance.issued_at_ms,
        expires_at_ms: issuance.expires_at_ms,
        terminal_state: if pass {
            TerminalState::Success
        } else {
            TerminalState::Failed
        },
        recovery_state: RecoveryState::NotRequired,
        outcome: if pass {
            EvidenceOutcome::Pass
        } else {
            EvidenceOutcome::Fail
        },
        execution_basis: ExecutionBasis::WorkerAsserted,
        qualification_claim: None,
        coverage: CoverageClaim {
            scope: record.results.iter().map(|r| r.id.clone()).collect(),
            expected_count: required.len() as u32,
            observed_count: record.results.len() as u32,
            inventory_complete: false,
            omissions,
        },
        dependencies: issuance
            .dependencies
            .iter()
            .map(|(identity, revision)| dependency(identity, revision))
            .collect(),
        artifacts: vec![ArtifactRecord {
            artifact_id: "record".into(),
            media_type: "application/json".into(),
            sha256: record_sha256,
            bytes: record_bytes.len() as u64,
            claimed_availability: ArtifactAvailability::Full,
            expires_at_ms: None,
        }],
        output: OutputCommitment {
            stdout_sha256: empty.clone(),
            stdout_bytes: 0,
            stderr_sha256: empty,
            stderr_bytes: 0,
            truncated: record.results.iter().any(|r| r.truncated),
        },
    };
    payload.validate()?;
    Ok((payload, record_bytes))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VerdictState {
    Success,
    Failure,
    Error,
    /// No attestation for the head was found (see [`select`]).
    Pending,
}

/// What a verdict means, independent of its display reason.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Outcome {
    Passed,
    Failed,
    /// No authenticated attestation for this check was found.
    Missing,
    StaleHead,
    StalePolicy,
    Expired,
    /// The signing key's own validity ended; re-attesting with that key cannot help.
    KeyExpired,
    Revoked,
    RegistryStale,
    /// Evidence that does not authenticate or does not bind to this check, head or recipe.
    Unverified,
    Error,
    /// Used by reconcile: only exempt paths changed.
    Exempt,
    /// No command of a constituent is required by the changed paths.
    NotRequired,
    /// A constituent whose required commands did not all run.
    DidNotRun,
}

impl Outcome {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Passed => "passed",
            Self::Failed => "failed",
            Self::Missing => "missing",
            Self::StaleHead => "stale_head",
            Self::StalePolicy => "stale_policy",
            Self::Expired => "expired",
            Self::KeyExpired => "key_expired",
            Self::Revoked => "revoked",
            Self::RegistryStale => "registry_stale",
            Self::Unverified => "unverified",
            Self::Error => "error",
            Self::Exempt => "exempt",
            Self::NotRequired => "not_required",
            Self::DidNotRun => "did_not_run",
        }
    }

    /// The pending reason for a lifecycle cause of a once-valid attestation.
    fn lifecycle_reason(self) -> Option<&'static str> {
        Some(match self {
            Self::StaleHead => "attestation is for a different head",
            Self::StalePolicy => "attestation predates the current policy",
            Self::Expired => "attestation expired",
            Self::KeyExpired => "attestation signing key expired",
            Self::Revoked => "attestation signing key revoked",
            Self::RegistryStale => "issuer registry changed since attestation",
            _ => return None,
        })
    }
}

impl VerdictState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Success => "success",
            Self::Failure => "failure",
            Self::Error => "error",
            Self::Pending => "pending",
        }
    }
}

/// The result of one constituent, derived only from the signed record and the trusted recipe.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConstituentVerdict {
    pub id: String,
    pub name: String,
    pub state: VerdictState,
    pub outcome: Outcome,
    pub reason: &'static str,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Verdict {
    pub state: VerdictState,
    pub outcome: Outcome,
    pub reason: &'static str,
    pub detail: String,
    /// True only once the signature verified against the registry and the signed record names
    /// the observed repository and commit. Anything else is about some other head or is junk.
    pub applies_to_head: bool,
    /// Digest of the assessed envelope; empty when none was assessed.
    pub envelope_sha256: String,
    /// Per-constituent results; empty without constituents or when the run has a structural
    /// problem, in which case every context inherits this verdict.
    pub constituents: Vec<ConstituentVerdict>,
}

impl Verdict {
    fn new(
        state: VerdictState,
        outcome: Outcome,
        reason: &'static str,
        detail: impl Into<String>,
    ) -> Self {
        Self {
            state,
            outcome,
            reason,
            detail: detail.into(),
            applies_to_head: false,
            envelope_sha256: String::new(),
            constituents: Vec::new(),
        }
    }

    fn applied(mut self) -> Self {
        self.applies_to_head = true;
        self
    }

    fn with_constituents(mut self, constituents: &[ConstituentVerdict]) -> Self {
        self.constituents = constituents.to_vec();
        self
    }

    fn failure(outcome: Outcome, reason: &'static str, detail: impl Into<String>) -> Self {
        Self::new(VerdictState::Failure, outcome, reason, detail)
    }
}

/// The Git state the verifier observed for the exact head being judged.
#[derive(Clone, Copy, Debug)]
pub struct Observed<'a> {
    pub repository: &'a str,
    pub commit: &'a str,
    pub tree: &'a str,
    pub base_ref: &'a str,
    pub merge_base: &'a str,
    /// Recipe command ids the changed paths require, in recipe order, computed from Git.
    pub required: &'a [String],
    pub selections: &'a [CommandSelection],
}

/// How far an envelope got before its verdict, which decides what it may influence.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Standing {
    /// Did not authenticate, or has not been examined yet.
    Unauthenticated,
    /// Authentic but signed for another check or kind: invisible to this check.
    Foreign,
    /// Authentic and signed for this check.
    ThisCheck,
}

/// Decides whether an attestation satisfies the observed head. Anything other
/// than every check holding is a `Failure` or `Error`, never `Success`.
pub fn assess(
    envelope_bytes: &[u8],
    record_bytes: &[u8],
    recipe: &ValidationRecipe,
    registry: &IssuerRegistrySnapshot,
    policy: &ValidationPolicy,
    observed: Observed<'_>,
    clock: EvaluationClock,
) -> Verdict {
    assess_standing(
        envelope_bytes,
        record_bytes,
        recipe,
        registry,
        policy,
        observed,
        clock,
    )
    .0
}

/// Like [`assess`], also reporting whether the envelope is authenticated evidence for this check
/// that failed only for a lifecycle cause. Such evidence may explain a pending reason but never
/// opts a head in.
pub(crate) fn assess_lifecycle(
    envelope_bytes: &[u8],
    record_bytes: &[u8],
    recipe: &ValidationRecipe,
    registry: &IssuerRegistrySnapshot,
    policy: &ValidationPolicy,
    observed: Observed<'_>,
    clock: EvaluationClock,
) -> (Verdict, bool) {
    let (verdict, standing) = assess_standing(
        envelope_bytes,
        record_bytes,
        recipe,
        registry,
        policy,
        observed,
        clock,
    );
    let lifecycle = standing == Standing::ThisCheck && verdict.outcome.lifecycle_reason().is_some();
    (verdict, lifecycle)
}

fn assess_standing(
    envelope_bytes: &[u8],
    record_bytes: &[u8],
    recipe: &ValidationRecipe,
    registry: &IssuerRegistrySnapshot,
    policy: &ValidationPolicy,
    observed: Observed<'_>,
    clock: EvaluationClock,
) -> (Verdict, Standing) {
    let mut standing = Standing::Unauthenticated;
    let mut verdict = assess_envelope(
        envelope_bytes,
        record_bytes,
        recipe,
        registry,
        policy,
        observed,
        clock,
        &mut standing,
    );
    verdict.envelope_sha256 = comment::envelope_sha256(envelope_bytes);
    (verdict, standing)
}

/// Assesses every comment body and returns the verdict that applies to the observed head with
/// the greatest signed `issued_at_ms` (ties: greatest envelope digest), so neither comment order
/// nor reposting an older attestation changes the result. Bodies that do not parse, or that are
/// stale, tampered or signed by an unregistered key, never decide. Without an applicable one, an
/// environmental `Error` (uncertain clock, invalid policy) surfaces; otherwise `Pending`, whose
/// reason names the newest authenticated lifecycle cause for this check (stale head or policy,
/// expiry, revoked key, changed registry) and is otherwise "no attestation for this head".
/// Another check's evidence is invisible, and unauthenticated bodies never choose the reason.
pub fn select(
    bodies: &[&str],
    recipe: &ValidationRecipe,
    registry: &IssuerRegistrySnapshot,
    policy: &ValidationPolicy,
    observed: Observed<'_>,
    clock: EvaluationClock,
) -> Verdict {
    select_pairs(
        bodies.iter().filter_map(|body| comment::parse(body).ok()),
        recipe,
        registry,
        policy,
        observed,
        clock,
    )
}

/// Selects directly from exact signed bytes with the same ordering and trust rules as
/// [`select`]. Presentation limits and comment authors confer no authority here. Callers
/// must bound the inventory and fail closed when an archived object is unavailable.
pub fn select_pairs<E: AsRef<[u8]>, R: AsRef<[u8]>>(
    pairs: impl IntoIterator<Item = (E, R)>,
    recipe: &ValidationRecipe,
    registry: &IssuerRegistrySnapshot,
    policy: &ValidationPolicy,
    observed: Observed<'_>,
    clock: EvaluationClock,
) -> Verdict {
    let (mut decided, mut error, mut ignored, mut lifecycle) = (None, None, None, None);
    let mut decided_key = (0_u64, String::new());
    let mut lifecycle_key = (0_u64, String::new());
    for (envelope, record) in pairs {
        let (envelope, record) = (envelope.as_ref(), record.as_ref());
        let (verdict, standing) =
            assess_standing(envelope, record, recipe, registry, policy, observed, clock);
        if standing == Standing::Foreign {
            continue;
        }
        // Only a verified envelope has a trustworthy signed issue time.
        let issued = || {
            serde_json::from_slice::<serde_json::Value>(envelope)
                .ok()
                .and_then(|v| v["payload"]["issued_at_ms"].as_u64())
                .unwrap_or(0)
        };
        if verdict.applies_to_head {
            let key = (issued(), verdict.envelope_sha256.clone());
            if decided.is_none() || key > decided_key {
                decided_key = key;
                decided = Some(verdict);
            }
        } else {
            ignored = Some(format!("{}: {}", verdict.reason, verdict.detail));
            if standing == Standing::ThisCheck && verdict.outcome.lifecycle_reason().is_some() {
                let key = (issued(), verdict.envelope_sha256.clone());
                if lifecycle.is_none() || key > lifecycle_key {
                    lifecycle_key = key;
                    lifecycle = Some(verdict.outcome);
                }
            }
            if verdict.state == VerdictState::Error {
                error = Some(verdict);
            }
        }
    }
    decided.or(error).unwrap_or_else(|| {
        let detail = ignored.map_or_else(
            || "no attestation found".into(),
            |i| format!("last ignored: {i}"),
        );
        match lifecycle {
            Some(outcome) => Verdict::new(
                VerdictState::Pending,
                outcome,
                outcome.lifecycle_reason().unwrap_or_default(),
                detail,
            ),
            None => Verdict::new(
                VerdictState::Pending,
                Outcome::Missing,
                "no attestation for this head",
                detail,
            ),
        }
    })
}

fn result_is_clean(r: &CommandResult) -> bool {
    r.stop == StopReason::Exited && r.exit_code == Some(0) && !r.incomplete && !r.truncated
}

/// Per-constituent results from the signed record and trusted recipe. Empty without constituents
/// and for any structural problem (tree changed, missing service, order departure, payload
/// outcome inconsistent with the results), so every context then inherits the check verdict.
fn constituent_verdicts(
    recipe: &ValidationRecipe,
    record: &ValidationRecord,
    policy: &ValidationPolicy,
    observed: Observed<'_>,
    payload_pass: bool,
) -> Vec<ConstituentVerdict> {
    let Some(groups) = &recipe.constituents else {
        return Vec::new();
    };
    let required = policy.required_services;
    let service_missing =
        (required.postgres && !record.services.postgres) || (required.s3 && !record.services.s3);
    let commands: Vec<&RecipeCommand> = recipe
        .commands
        .iter()
        .filter(|c| observed.required.contains(&c.id))
        .collect();
    let in_order = record.results.len() <= commands.len()
        && record
            .results
            .iter()
            .zip(&commands)
            .all(|(r, c)| r.id == c.id && r.argv == effective_argv(c, observed.selections));
    let commands_clean =
        record.results.len() == commands.len() && record.results.iter().all(result_is_clean);
    if !record.tree_unchanged_after
        || service_missing
        || !in_order
        || payload_pass != commands_clean
    {
        return Vec::new();
    }
    groups
        .iter()
        .map(|group| {
            let mapped: Vec<&String> = group
                .commands
                .iter()
                .filter(|id| observed.required.contains(id))
                .collect();
            let ran = |id: &String| record.results.iter().find(|r| &r.id == id);
            let (state, outcome, reason) = if mapped.is_empty() {
                (VerdictState::Success, Outcome::NotRequired, "not required")
            } else if mapped
                .iter()
                .any(|id| ran(id).is_some_and(|r| !result_is_clean(r)))
            {
                (VerdictState::Failure, Outcome::Failed, "failed")
            } else if mapped.iter().all(|id| ran(id).is_some()) {
                (VerdictState::Success, Outcome::Passed, "passed")
            } else {
                (
                    VerdictState::Pending,
                    Outcome::DidNotRun,
                    "did not run: an earlier command failed",
                )
            };
            ConstituentVerdict {
                id: group.id.clone(),
                name: group.name.clone(),
                state,
                outcome,
                reason,
            }
        })
        .collect()
}

#[allow(clippy::too_many_arguments)]
fn assess_envelope(
    envelope_bytes: &[u8],
    record_bytes: &[u8],
    recipe: &ValidationRecipe,
    registry: &IssuerRegistrySnapshot,
    policy: &ValidationPolicy,
    observed: Observed<'_>,
    clock: EvaluationClock,
    standing: &mut Standing,
) -> Verdict {
    use VerdictState::Error as Err_;
    if let Err(error) = policy.validate() {
        return Verdict::new(Err_, Outcome::Error, "policy invalid", error.to_string());
    }
    if recipe.constituents.is_some() && policy.check_id.is_none() {
        return Verdict::new(
            Err_,
            Outcome::Error,
            "policy invalid",
            "a recipe with constituents needs a policy with a check ID",
        );
    }
    if recipe.sha256().ok().as_ref() != Some(&policy.recipe_sha256) {
        return Verdict::new(
            Err_,
            Outcome::Error,
            "recipe does not match policy",
            "the trusted recipe differs from the policy digest",
        );
    }
    let inputs = [ArtifactInput {
        artifact_id: "record",
        supplied: SuppliedArtifact::Full(record_bytes),
    }];
    let report = match verify_evidence(envelope_bytes, registry, clock, &inputs) {
        Ok(report) => report,
        Err(error) => {
            return Verdict::failure(Outcome::Unverified, "evidence rejected", error.to_string());
        }
    };
    if report.integrity() != IntegrityStatus::Verified {
        return Verdict::failure(
            Outcome::Unverified,
            "signature not verified",
            format!("{:?}", report.integrity()),
        );
    }
    let Some(receipt) = report.verified_receipt() else {
        return Verdict::failure(
            Outcome::Unverified,
            "signature not verified",
            "no verified receipt",
        );
    };
    let payload = receipt.payload();
    // Another check's evidence never decides or influences this one, so identity comes first.
    if payload.check_kind != CHECK_KIND || payload.requirement_id != policy.requirement_id() {
        *standing = Standing::Foreign;
        return Verdict::failure(
            Outcome::Unverified,
            "unexpected evidence kind",
            "not a local validation attestation for this check",
        );
    }
    *standing = Standing::ThisCheck;
    match report.current_issuer_trust() {
        CurrentIssuerTrust::Trusted => {}
        CurrentIssuerTrust::ClockUncertain => {
            return Verdict::new(
                Err_,
                Outcome::Error,
                "clock uncertain",
                "cannot judge issuer trust",
            );
        }
        other => {
            let outcome = match other {
                CurrentIssuerTrust::Revoked => Outcome::Revoked,
                CurrentIssuerTrust::Expired => Outcome::KeyExpired,
                _ => Outcome::Unverified,
            };
            return Verdict::failure(outcome, "issuer not trusted", format!("{other:?}"));
        }
    }
    if report.registry_binding() != RegistryBindingStatus::Current {
        return Verdict::failure(
            Outcome::RegistryStale,
            "issuer registry stale",
            "registry changed since signing",
        );
    }
    match report.freshness() {
        Freshness::Fresh => {}
        Freshness::ClockUncertain => {
            return Verdict::new(
                Err_,
                Outcome::Error,
                "clock uncertain",
                "cannot judge freshness",
            );
        }
        other => {
            let outcome = if other == Freshness::Expired {
                Outcome::Expired
            } else {
                Outcome::Unverified
            };
            return Verdict::failure(outcome, "attestation not fresh", format!("{other:?}"));
        }
    }
    let EvaluationClock::Known { unix_ms } = clock else {
        return Verdict::new(Err_, Outcome::Error, "clock uncertain", "cannot judge age");
    };
    let Some(age_ms) = unix_ms.checked_sub(payload.issued_at_ms) else {
        return Verdict::failure(
            Outcome::Unverified,
            "attestation from the future",
            "issued after the clock",
        );
    };
    if age_ms > policy.max_age_seconds.saturating_mul(1000) {
        return Verdict::failure(
            Outcome::Expired,
            "attestation too old",
            "older than policy max age",
        );
    }
    if report.execution_claim() != ExecutionClaimStatus::WorkerAssertion {
        return Verdict::failure(
            Outcome::Unverified,
            "unexpected evidence kind",
            "not a local validation attestation",
        );
    }
    if payload.installed_policy_epoch != policy.epoch
        || policy.digest().ok().as_ref() != Some(&payload.installed_policy_sha256)
    {
        return Verdict::failure(
            Outcome::StalePolicy,
            "policy mismatch",
            "attestation bound to a different policy",
        );
    }
    let committed = report.artifacts().iter().any(|a| {
        a.artifact_id == "record" && a.status == ArtifactStatus::Full && a.commitment_matches
    });
    if !committed {
        return Verdict::failure(
            Outcome::Unverified,
            "record not committed",
            "record bytes do not match commitment",
        );
    }
    let record = match ValidationRecord::from_slice(record_bytes) {
        Ok(record) => record,
        Err(error) => {
            return Verdict::failure(Outcome::Unverified, "record invalid", error.to_string());
        }
    };
    let subject = &payload.subject;
    if subject.repository != record.subject.repository
        || subject.change.id != commit_change_id(&record.subject.commit)
        || subject.revision != subject_digest(&record.subject)
        || subject.context_digest != context_digest(&record.context)
        || subject.recipe_sha256 != record.recipe_sha256
        || subject.toolchain_sha256 != toolchain_digest(&record)
        || subject.environment_sha256 != environment_digest(&record)
    {
        return Verdict::failure(
            Outcome::Unverified,
            "record does not match payload",
            "tampered or mixed record",
        );
    }
    if record.subject.repository != policy.repository
        || record.subject.repository != observed.repository
    {
        return Verdict::failure(
            Outcome::Unverified,
            "wrong repository",
            record.subject.repository.clone(),
        );
    }
    if record.subject.commit != observed.commit
        || record.subject.tree != observed.tree
        || record.context.base_ref != observed.base_ref
        || record.context.merge_base != observed.merge_base
    {
        return Verdict::failure(
            Outcome::StaleHead,
            "stale attestation",
            "head, tree or base differs from the record",
        );
    }
    if record.recipe_sha256 != policy.recipe_sha256 {
        return Verdict::failure(
            Outcome::Unverified,
            "recipe mismatch",
            "recipe differs from policy",
        )
        .applied();
    }
    if record.selections != observed.selections {
        return Verdict::failure(
            Outcome::Unverified,
            "selection mismatch",
            "signed selection differs from independently observed paths",
        )
        .applied();
    }
    let not_required: Vec<&String> = recipe
        .commands
        .iter()
        .map(|c| &c.id)
        .filter(|id| !observed.required.contains(id))
        .collect();
    if let Some(id) = record
        .not_required
        .iter()
        .find(|id| observed.required.contains(id))
    {
        return Verdict::failure(
            Outcome::Failed,
            "incomplete run",
            format!("{id} is required by the changed paths"),
        )
        .applied();
    }
    if !record.not_required.iter().eq(not_required.iter().copied()) {
        return Verdict::failure(
            Outcome::Failed,
            "incomplete run",
            "not-required commands differ from the changed paths",
        )
        .applied();
    }
    // The signed coverage must describe exactly this record; inventory_complete is never trusted.
    let coverage = &payload.coverage;
    let scope: BTreeSet<&str> = coverage.scope.iter().map(String::as_str).collect();
    let ran: BTreeSet<&str> = record.results.iter().map(|r| r.id.as_str()).collect();
    if !record
        .selections
        .iter()
        .filter(|s| s.reason == SelectionReason::Selected)
        .all(|s| {
            coverage
                .omissions
                .contains(&format!("reduced test selection: {}", s.command_id))
        })
        || scope != ran
        || coverage.scope.len() != record.results.len()
        || coverage.observed_count as usize != record.results.len()
        || coverage.expected_count as usize != observed.required.len()
        || !record
            .not_required
            .iter()
            .all(|id| coverage.omissions.contains(&format!("not required: {id}")))
    {
        return Verdict::failure(
            Outcome::Unverified,
            "coverage mismatch",
            "signed coverage differs from the record",
        )
        .applied();
    }
    // Departures inside the recipe order come first; a stopped run is a prefix, reported below.
    let departure = recipe.first_departure(&record.results, observed.required, observed.selections);
    if let Some(detail) = departure
        .as_ref()
        .filter(|_| record.results.len() >= observed.required.len())
    {
        return Verdict::failure(Outcome::Failed, "incomplete run", detail.clone()).applied();
    }
    let payload_pass = payload.outcome == EvidenceOutcome::Pass
        && payload.terminal_state == TerminalState::Success;
    let constituents = constituent_verdicts(recipe, &record, policy, observed, payload_pass);
    if let Some((reason, detail)) = record.shortfall(policy.required_services) {
        return Verdict::failure(Outcome::Failed, reason, detail)
            .applied()
            .with_constituents(&constituents);
    }
    if let Some(detail) = departure {
        return Verdict::failure(Outcome::Failed, "incomplete run", detail)
            .applied()
            .with_constituents(&constituents);
    }
    if !payload_pass {
        return Verdict::failure(
            Outcome::Failed,
            "outcome not pass",
            format!("{:?}", payload.outcome),
        )
        .applied();
    }
    Verdict::new(
        VerdictState::Success,
        Outcome::Passed,
        "validated",
        "attestation satisfies the exact head",
    )
    .applied()
    .with_constituents(&constituents)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Tracked files must stay eligible for native capture, so none may use a reserved name.
    #[test]
    fn tracked_paths_avoid_reserved_namespaces() {
        let Ok(out) = std::process::Command::new("git")
            .args(["ls-files", "-z"])
            .current_dir(env!("CARGO_MANIFEST_DIR"))
            .output()
        else {
            eprintln!("skipped: git is unavailable");
            return;
        };
        if !out.status.success() {
            eprintln!("skipped: not a git checkout");
            return;
        }
        let paths = String::from_utf8_lossy(&out.stdout);
        let reserved: Vec<&str> = paths
            .split('\0')
            .filter(|p| crate::git_path_profile::uses_reserved_namespace(p))
            .collect();
        assert!(reserved.is_empty(), "reserved tracked paths: {reserved:?}");
    }

    const SHA1: &str = "0123456789abcdef0123456789abcdef01234567";

    #[test]
    fn golden_digests() {
        let subject = GitValidationSubject {
            repository: "repo-1".into(),
            commit: SHA1.into(),
            tree: SHA1.into(),
        };
        let context = GitValidationContext {
            base_ref: "refs/heads/main".into(),
            merge_base: SHA1.into(),
        };
        assert_eq!(
            subject_digest(&subject),
            "a4ddaefa8289d8c85bb0cb5caaf7ffc5014c119824d348c9c86cd91baab60072"
        );
        assert_eq!(
            context_digest(&context),
            "fa8abd4452ab89cb98ff40fd1597ab409aacf4e75b63f24c051a7913a1f2179e"
        );
        assert_ne!(subject_digest(&subject), context_digest(&context));
    }
}
