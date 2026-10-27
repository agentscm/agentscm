//! Git-bound local validation evidence.
//!
//! A profile over portable evidence v1: a maintainer's full local gate is
//! recorded as a bounded `record` artifact and committed by a signed
//! `worker_asserted` envelope. The verdict is pure: it reads no files, runs no
//! Git and never defaults to success. A signature proves only that a registered
//! key holder signed these bytes, not that the commands ran as recorded.

use std::collections::HashSet;

pub mod comment;
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
const DEFAULT_DEPENDENCY_FILES: [&str; 2] = ["Cargo.lock", "rust-toolchain.toml"];
const DEFAULT_TOOLCHAIN_PROBES: [&[&str]; 3] = [
    &["rustc", "-V"],
    &["cargo", "-V"],
    &["cargo", "nextest", "--version"],
];
const MAX_MAX_AGE_SECONDS: u64 = 366 * 24 * 3600;
pub const CHECK_KIND: &str = "command/v1";
pub const REQUIREMENT_ID: &str = "local-validation";
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

    /// The first position where `results` departs from this recipe's commands (ids and argv, in
    /// order), as `(reason, detail)`. A missing command is reported by id.
    fn first_departure(&self, results: &[CommandResult]) -> Option<String> {
        for (index, command) in self.commands.iter().enumerate() {
            match results.get(index) {
                None => return Some(format!("{} did not run", command.id)),
                Some(r) if r.id != command.id || r.argv != command.argv => {
                    return Some(format!("{} differs from the recipe", r.id));
                }
                Some(_) => {}
            }
        }
        results
            .get(self.commands.len())
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
        Ok(serde_json::to_vec(self)?)
    }

    pub fn validate(&self) -> Result<()> {
        self.subject.validate()?;
        self.context.validate()?;
        oid("base tip", &self.base_tip)?;
        validate_lower_hex("recipe digest", &self.recipe_sha256, 32)?;
        if self.results.is_empty() || self.results.len() > MAX_COMMANDS {
            return Err(Error::Limit(format!(
                "record must have 1 to {MAX_COMMANDS} results"
            )));
        }
        let mut ids = HashSet::new();
        for result in &self.results {
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
        Ok(())
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
    let pass = record.shortfall(policy.required_services).is_none()
        && recipe.first_departure(&record.results).is_none();
    let mut omissions: Vec<String> = FIXED_OMISSIONS.iter().map(|s| (*s).to_owned()).collect();
    omissions.extend(
        recipe
            .commands
            .iter()
            .filter(|c| !record.results.iter().any(|r| r.id == c.id))
            .map(|c| format!("skipped: {}", c.id)),
    );
    if !record.services.postgres {
        omissions.push("postgres tests".into());
    }
    if !record.services.s3 {
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
        requirement_id: REQUIREMENT_ID.into(),
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
            expected_count: recipe.commands.len() as u32,
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

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Verdict {
    pub state: VerdictState,
    pub reason: &'static str,
    pub detail: String,
    /// True only once the signature verified against the registry and the signed record names
    /// the observed repository and commit. Anything else is about some other head or is junk.
    pub applies_to_head: bool,
    /// Digest of the assessed envelope; empty when none was assessed.
    pub envelope_sha256: String,
}

impl Verdict {
    fn new(state: VerdictState, reason: &'static str, detail: impl Into<String>) -> Self {
        Self {
            state,
            reason,
            detail: detail.into(),
            applies_to_head: false,
            envelope_sha256: String::new(),
        }
    }

    fn applied(mut self) -> Self {
        self.applies_to_head = true;
        self
    }

    fn failure(reason: &'static str, detail: impl Into<String>) -> Self {
        Self::new(VerdictState::Failure, reason, detail)
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
    let mut verdict = assess_envelope(
        envelope_bytes,
        record_bytes,
        recipe,
        registry,
        policy,
        observed,
        clock,
    );
    verdict.envelope_sha256 = comment::envelope_sha256(envelope_bytes);
    verdict
}

/// Assesses every comment body and returns the verdict that applies to the observed head with
/// the greatest signed `issued_at_ms` (ties: greatest envelope digest), so neither comment order
/// nor reposting an older attestation changes the result. Bodies that do not parse, or that are
/// stale, tampered or signed by an unregistered key, never decide. Without an applicable one, an
/// environmental `Error` (uncertain clock, invalid policy) surfaces; otherwise `Pending`.
pub fn select(
    bodies: &[&str],
    recipe: &ValidationRecipe,
    registry: &IssuerRegistrySnapshot,
    policy: &ValidationPolicy,
    observed: Observed<'_>,
    clock: EvaluationClock,
) -> Verdict {
    let (mut decided, mut error, mut ignored) = (None, None, None);
    let mut decided_key = (0_u64, String::new());
    for body in bodies {
        let Ok((envelope, record)) = comment::parse(body) else {
            continue;
        };
        let verdict = assess(
            &envelope, &record, recipe, registry, policy, observed, clock,
        );
        if verdict.applies_to_head {
            // Only a verified envelope applies to the head, so its issue time is the signed one.
            let issued = serde_json::from_slice::<serde_json::Value>(&envelope)
                .ok()
                .and_then(|v| v["payload"]["issued_at_ms"].as_u64())
                .unwrap_or(0);
            let key = (issued, verdict.envelope_sha256.clone());
            if decided.is_none() || key > decided_key {
                decided_key = key;
                decided = Some(verdict);
            }
        } else {
            ignored = Some(format!("{}: {}", verdict.reason, verdict.detail));
            if verdict.state == VerdictState::Error {
                error = Some(verdict);
            }
        }
    }
    decided.or(error).unwrap_or_else(|| {
        Verdict::new(
            VerdictState::Pending,
            "no attestation for this head",
            ignored.map_or_else(
                || "no attestation found".into(),
                |i| format!("last ignored: {i}"),
            ),
        )
    })
}

fn assess_envelope(
    envelope_bytes: &[u8],
    record_bytes: &[u8],
    recipe: &ValidationRecipe,
    registry: &IssuerRegistrySnapshot,
    policy: &ValidationPolicy,
    observed: Observed<'_>,
    clock: EvaluationClock,
) -> Verdict {
    use VerdictState::Error as Err_;
    if let Err(error) = policy.validate() {
        return Verdict::new(Err_, "policy invalid", error.to_string());
    }
    if recipe.sha256().ok().as_ref() != Some(&policy.recipe_sha256) {
        return Verdict::new(
            Err_,
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
        Err(error) => return Verdict::failure("evidence rejected", error.to_string()),
    };
    if report.integrity() != IntegrityStatus::Verified {
        return Verdict::failure(
            "signature not verified",
            format!("{:?}", report.integrity()),
        );
    }
    let Some(receipt) = report.verified_receipt() else {
        return Verdict::failure("signature not verified", "no verified receipt");
    };
    let payload = receipt.payload();
    if report.registry_binding() != RegistryBindingStatus::Current {
        return Verdict::failure("issuer registry stale", "registry changed since signing");
    }
    match report.current_issuer_trust() {
        CurrentIssuerTrust::Trusted => {}
        CurrentIssuerTrust::ClockUncertain => {
            return Verdict::new(Err_, "clock uncertain", "cannot judge issuer trust");
        }
        other => return Verdict::failure("issuer not trusted", format!("{other:?}")),
    }
    match report.freshness() {
        Freshness::Fresh => {}
        Freshness::ClockUncertain => {
            return Verdict::new(Err_, "clock uncertain", "cannot judge freshness");
        }
        other => return Verdict::failure("attestation not fresh", format!("{other:?}")),
    }
    let EvaluationClock::Known { unix_ms } = clock else {
        return Verdict::new(Err_, "clock uncertain", "cannot judge age");
    };
    let Some(age_ms) = unix_ms.checked_sub(payload.issued_at_ms) else {
        return Verdict::failure("attestation from the future", "issued after the clock");
    };
    if age_ms > policy.max_age_seconds.saturating_mul(1000) {
        return Verdict::failure("attestation too old", "older than policy max age");
    }
    if report.execution_claim() != ExecutionClaimStatus::WorkerAssertion
        || payload.check_kind != CHECK_KIND
        || payload.requirement_id != REQUIREMENT_ID
    {
        return Verdict::failure(
            "unexpected evidence kind",
            "not a local validation attestation",
        );
    }
    if payload.installed_policy_epoch != policy.epoch
        || policy.digest().ok().as_ref() != Some(&payload.installed_policy_sha256)
    {
        return Verdict::failure("policy mismatch", "attestation bound to a different policy");
    }
    let committed = report.artifacts().iter().any(|a| {
        a.artifact_id == "record" && a.status == ArtifactStatus::Full && a.commitment_matches
    });
    if !committed {
        return Verdict::failure(
            "record not committed",
            "record bytes do not match commitment",
        );
    }
    let record = match ValidationRecord::from_slice(record_bytes) {
        Ok(record) => record,
        Err(error) => return Verdict::failure("record invalid", error.to_string()),
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
        return Verdict::failure("record does not match payload", "tampered or mixed record");
    }
    if record.subject.repository != policy.repository
        || record.subject.repository != observed.repository
    {
        return Verdict::failure("wrong repository", record.subject.repository.clone());
    }
    if record.subject.commit != observed.commit
        || record.subject.tree != observed.tree
        || record.context.base_ref != observed.base_ref
        || record.context.merge_base != observed.merge_base
    {
        return Verdict::failure(
            "stale attestation",
            "head, tree or base differs from the record",
        );
    }
    if record.recipe_sha256 != policy.recipe_sha256 {
        return Verdict::failure("recipe mismatch", "recipe differs from policy").applied();
    }
    // Departures inside the recipe order come first; a stopped run is a prefix, reported below.
    let departure = recipe.first_departure(&record.results);
    if let Some(detail) = departure
        .as_ref()
        .filter(|_| record.results.len() >= recipe.commands.len())
    {
        return Verdict::failure("incomplete run", detail.clone()).applied();
    }
    if let Some((reason, detail)) = record.shortfall(policy.required_services) {
        return Verdict::failure(reason, detail).applied();
    }
    if let Some(detail) = departure {
        return Verdict::failure("incomplete run", detail).applied();
    }
    if payload.outcome != EvidenceOutcome::Pass || payload.terminal_state != TerminalState::Success
    {
        return Verdict::failure("outcome not pass", format!("{:?}", payload.outcome)).applied();
    }
    Verdict::new(
        VerdictState::Success,
        "validated",
        "attestation satisfies the exact head",
    )
    .applied()
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
