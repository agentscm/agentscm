use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::checks::{CheckSubject, sha256};
use crate::{Error, Result};

const RECIPE_VERSION: u32 = 1;
const REQUEST_DOMAIN: &[u8] = b"agentscm.execution-request.v1\0";
const MAX_ARGS: usize = 64;
const MAX_COMMAND_BYTES: usize = 16 * 1024;
const MAX_ENVIRONMENT_ENTRIES: usize = 32;
const MAX_ENVIRONMENT_BYTES: usize = 16 * 1024;
const MAX_ATTEMPT_ID_BYTES: usize = 128;
const MAX_DETAIL_BYTES: usize = 8 * 1024;

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SyscallPolicy {
    DockerBuiltin,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ExecutionLimits {
    pub timeout_ms: u64,
    pub max_output_bytes: u64,
    pub memory_bytes: u64,
    pub scratch_bytes: u64,
    pub pids: u32,
    pub cpu_millis: u32,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct CommandRecipe {
    pub version: u32,
    pub image: String,
    pub argv: Vec<String>,
    pub environment: BTreeMap<String, String>,
    pub toolchain_sha256: String,
    pub environment_sha256: String,
    pub limits: ExecutionLimits,
    pub syscall_policy: SyscallPolicy,
    pub no_new_privileges: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ExecutionRequest {
    pub attempt_id: String,
    pub subject: CheckSubject,
    pub run_generation: u64,
    pub recipe: CommandRecipe,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ExecutionOutcome {
    Success,
    Failed,
    TimedOut,
    Cancelled,
    OutputLimit,
    Unavailable,
    Incomplete,
    Indeterminate,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct CleanupHandle {
    pub container_name: String,
    pub owner_label: String,
    pub creation_confirmed: bool,
    pub runtime_identity: String,
    pub container_id: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ExecutionRecord {
    pub version: u32,
    pub request: ExecutionRequest,
    pub request_sha256: String,
    pub outcome: ExecutionOutcome,
    pub exit_code: Option<i32>,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    pub output_truncated: bool,
    pub cleanup_confirmed: bool,
    pub cleanup_handle: Option<CleanupHandle>,
    pub isolation_observed: bool,
    pub runtime_identity: Option<String>,
    pub detail: String,
}

impl ExecutionLimits {
    pub fn validate(&self) -> Result<()> {
        bounded("timeout_ms", self.timeout_ms, 1, 300_000)?;
        bounded("max_output_bytes", self.max_output_bytes, 256, 1_048_576)?;
        bounded(
            "memory_bytes",
            self.memory_bytes,
            16 * 1024 * 1024,
            4 * 1024 * 1024 * 1024,
        )?;
        bounded(
            "scratch_bytes",
            self.scratch_bytes,
            1024 * 1024,
            1024 * 1024 * 1024,
        )?;
        bounded("pids", u64::from(self.pids), 1, 256)?;
        bounded("cpu_millis", u64::from(self.cpu_millis), 100, 4_000)
    }
}

impl CommandRecipe {
    pub fn validate(&self) -> Result<()> {
        if self.version != RECIPE_VERSION {
            return Err(Error::Invalid(format!(
                "unsupported command recipe version {}",
                self.version
            )));
        }
        validate_image(&self.image)?;
        if self.argv.is_empty() || self.argv.len() > MAX_ARGS {
            return Err(Error::Limit(format!(
                "command argv must contain 1 to {MAX_ARGS} entries"
            )));
        }
        if !self.argv[0].starts_with('/') {
            return Err(Error::Invalid(
                "command executable must be an absolute path".into(),
            ));
        }
        let command_bytes = aggregate_bytes(self.argv.iter().map(String::as_str), "command argv")?;
        if command_bytes > MAX_COMMAND_BYTES {
            return Err(Error::Limit(format!(
                "command argv exceeds {MAX_COMMAND_BYTES} bytes"
            )));
        }
        if self.environment.len() > MAX_ENVIRONMENT_ENTRIES {
            return Err(Error::Limit(format!(
                "command environment exceeds {MAX_ENVIRONMENT_ENTRIES} entries"
            )));
        }
        let mut environment_bytes = 0usize;
        for (name, value) in &self.environment {
            validate_environment_name(name)?;
            if matches!(name.as_str(), "HOME" | "TMPDIR") && value != "/scratch" {
                return Err(Error::Invalid(format!(
                    "environment variable {name} must be /scratch"
                )));
            }
            if value.contains('\0') {
                return Err(Error::Invalid(format!(
                    "environment value for {name} contains NUL"
                )));
            }
            environment_bytes = environment_bytes
                .checked_add(name.len())
                .and_then(|total| total.checked_add(value.len()))
                .ok_or_else(|| Error::Limit("command environment byte count overflow".into()))?;
        }
        if environment_bytes > MAX_ENVIRONMENT_BYTES {
            return Err(Error::Limit(format!(
                "command environment exceeds {MAX_ENVIRONMENT_BYTES} bytes"
            )));
        }
        validate_digest("toolchain digest", &self.toolchain_sha256)?;
        validate_digest("environment digest", &self.environment_sha256)?;
        if self.environment_sha256 != environment_digest(&self.environment)? {
            return Err(Error::Invalid(
                "environment digest does not match canonical environment bytes".into(),
            ));
        }
        if !self.no_new_privileges {
            return Err(Error::Invalid("no_new_privileges must be true".into()));
        }
        self.limits.validate()
    }

    pub fn canonical_bytes(&self) -> Result<Vec<u8>> {
        self.validate()?;
        Ok(serde_json::to_vec(self)?)
    }

    pub fn digest(&self) -> Result<String> {
        Ok(sha256(&self.canonical_bytes()?))
    }
}

impl ExecutionRequest {
    pub fn validate(&self) -> Result<()> {
        validate_attempt_id(&self.attempt_id)?;
        if self.run_generation == 0 || self.run_generation > i64::MAX as u64 {
            return Err(Error::Invalid(
                "run_generation must be a positive signed 64-bit integer".into(),
            ));
        }
        validate_subject(&self.subject)?;
        self.recipe.validate()?;
        if self.subject.recipe_sha256 != self.recipe.digest()? {
            return Err(Error::Invalid(
                "subject recipe digest does not match the command recipe".into(),
            ));
        }
        if self.subject.toolchain_sha256 != self.recipe.toolchain_sha256 {
            return Err(Error::Invalid(
                "subject toolchain digest does not match the command recipe".into(),
            ));
        }
        if self.subject.environment_sha256 != self.recipe.environment_sha256 {
            return Err(Error::Invalid(
                "subject environment digest does not match the command recipe".into(),
            ));
        }
        Ok(())
    }

    pub fn canonical_bytes(&self) -> Result<Vec<u8>> {
        self.validate()?;
        Ok(serde_json::to_vec(self)?)
    }

    pub fn digest(&self) -> Result<String> {
        let canonical = self.canonical_bytes()?;
        let mut hasher = Sha256::new();
        hasher.update(REQUEST_DOMAIN);
        hasher.update(canonical);
        Ok(format!("{:x}", hasher.finalize()))
    }
}

impl ExecutionRecord {
    pub fn canonical_bytes(&self) -> Result<Vec<u8>> {
        self.validate()?;
        Ok(serde_json::to_vec(self)?)
    }

    pub fn digest(&self) -> Result<String> {
        let mut hasher = Sha256::new();
        hasher.update(b"agentscm.execution-record.v1\0");
        hasher.update(self.canonical_bytes()?);
        Ok(format!("{:x}", hasher.finalize()))
    }

    pub fn validate(&self) -> Result<()> {
        if self.version != 1 {
            return Err(Error::Invalid(format!(
                "unsupported execution record version {}",
                self.version
            )));
        }
        self.request.validate()?;
        validate_digest("request digest", &self.request_sha256)?;
        if self.request_sha256 != self.request.digest()? {
            return Err(Error::Invalid(
                "execution record request digest does not match its request".into(),
            ));
        }
        match (&self.cleanup_handle, self.cleanup_confirmed) {
            (Some(handle), false) => {
                handle.validate(&self.request)?;
                if self.runtime_identity.as_deref() != Some(handle.runtime_identity.as_str()) {
                    return Err(Error::Invalid(
                        "cleanup handle runtime does not match the execution record".into(),
                    ));
                }
            }
            (None, false) => {
                return Err(Error::Invalid(
                    "unconfirmed cleanup requires a cleanup handle".into(),
                ));
            }
            (Some(_), true) => {
                return Err(Error::Invalid(
                    "confirmed cleanup cannot retain a cleanup handle".into(),
                ));
            }
            (None, true) => {}
        }
        let output_bytes = self
            .stdout
            .len()
            .checked_add(self.stderr.len())
            .ok_or_else(|| Error::Limit("execution output byte count overflow".into()))?;
        if output_bytes as u64 > self.request.recipe.limits.max_output_bytes {
            return Err(Error::Limit(
                "execution record exceeds its aggregate output limit".into(),
            ));
        }
        if self.detail.len() > MAX_DETAIL_BYTES || self.detail.contains('\0') {
            return Err(Error::Limit(format!(
                "execution detail must be NUL-free and at most {MAX_DETAIL_BYTES} bytes"
            )));
        }
        if let Some(identity) = &self.runtime_identity
            && (identity.is_empty() || identity.len() > 256 || identity.contains('\0'))
        {
            return Err(Error::Invalid("runtime identity is invalid".into()));
        }
        if matches!(
            self.outcome,
            ExecutionOutcome::Success | ExecutionOutcome::Failed
        ) && (!self.cleanup_confirmed
            || !self.isolation_observed
            || self.runtime_identity.is_none()
            || self.output_truncated
            || self.exit_code.is_none())
        {
            return Err(Error::Invalid("completed execution requires observed isolation, runtime, full output and confirmed cleanup".into()));
        }
        if !self.cleanup_confirmed && self.outcome != ExecutionOutcome::Indeterminate {
            return Err(Error::Invalid(
                "unconfirmed cleanup must remain indeterminate".into(),
            ));
        }
        if self.outcome == ExecutionOutcome::Failed && self.exit_code == Some(0) {
            return Err(Error::Invalid(
                "failed execution cannot report exit code zero".into(),
            ));
        }
        if matches!(self.outcome, ExecutionOutcome::Success) && self.exit_code != Some(0) {
            return Err(Error::Invalid(
                "successful execution must report exit code zero".into(),
            ));
        }
        if matches!(self.outcome, ExecutionOutcome::OutputLimit) && !self.output_truncated {
            return Err(Error::Invalid(
                "output-limit execution must report truncated output".into(),
            ));
        }
        Ok(())
    }
}

impl CleanupHandle {
    pub fn validate(&self, request: &ExecutionRequest) -> Result<()> {
        let nonce = self
            .container_name
            .strip_prefix("agentscm-")
            .ok_or_else(|| Error::Invalid("cleanup container name has an invalid prefix".into()))?;
        validate_digest("cleanup container nonce", nonce)?;
        let expected_owner = format!("{}:{nonce}", request.digest()?);
        if self.owner_label != expected_owner {
            return Err(Error::Invalid(
                "cleanup owner label does not match its request and container nonce".into(),
            ));
        }
        if self.runtime_identity.is_empty()
            || self.runtime_identity.len() > 256
            || self.runtime_identity.contains('\0')
        {
            return Err(Error::Invalid("cleanup runtime identity is invalid".into()));
        }
        match &self.container_id {
            Some(container_id) => validate_digest("cleanup container ID", container_id)?,
            None if self.creation_confirmed => {
                return Err(Error::Invalid(
                    "confirmed container creation requires an immutable container ID".into(),
                ));
            }
            None => {}
        }
        Ok(())
    }
}

/// SHA-256 of serde_json's canonical serialization of the ordered environment map.
pub fn environment_digest(environment: &BTreeMap<String, String>) -> Result<String> {
    Ok(sha256(&serde_json::to_vec(environment)?))
}

fn validate_subject(subject: &CheckSubject) -> Result<()> {
    validate_identifier("repository ID", &subject.repository, true)?;
    subject.change.validate()?;
    if subject.repository != subject.change.repository {
        return Err(Error::Invalid(
            "execution subject repository identities do not match".into(),
        ));
    }
    if subject.change_version == 0 || subject.change_version > i64::MAX as u64 {
        return Err(Error::Invalid(
            "change version must be a positive signed 64-bit integer".into(),
        ));
    }
    validate_digest("context digest", &subject.context_digest)?;
    validate_digest("revision", &subject.revision)?;
    validate_digest("recipe digest", &subject.recipe_sha256)?;
    validate_digest("toolchain digest", &subject.toolchain_sha256)?;
    validate_digest("environment digest", &subject.environment_sha256)
}

pub(crate) fn validate_attempt_id(value: &str) -> Result<()> {
    validate_identifier("attempt ID", value, false)
}

fn validate_identifier(kind: &str, value: &str, extended: bool) -> Result<()> {
    if value.is_empty() || value.len() > MAX_ATTEMPT_ID_BYTES {
        return Err(Error::Limit(format!(
            "{kind} must contain 1 to {MAX_ATTEMPT_ID_BYTES} bytes"
        )));
    }
    if !value.bytes().all(|byte| {
        byte.is_ascii_alphanumeric()
            || matches!(byte, b'-' | b'_')
            || (extended && matches!(byte, b'.' | b':' | b'/'))
    }) {
        return Err(Error::Invalid(format!(
            "{kind} contains unsupported characters"
        )));
    }
    Ok(())
}

fn validate_image(image: &str) -> Result<()> {
    let digest = image
        .strip_prefix("sha256:")
        .ok_or_else(|| Error::Invalid("image must use a sha256 digest reference".into()))?;
    validate_digest("image digest", digest)
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

fn validate_environment_name(name: &str) -> Result<()> {
    if name.is_empty()
        || !name.is_ascii()
        || !name
            .bytes()
            .all(|byte| byte == b'_' || byte.is_ascii_uppercase() || byte.is_ascii_digit())
        || name.as_bytes()[0].is_ascii_digit()
    {
        return Err(Error::Invalid(format!(
            "environment name {name:?} must use uppercase ASCII identifiers"
        )));
    }
    let denied = matches!(
        name,
        "SHELL"
            | "ENV"
            | "BASH_ENV"
            | "BASHOPTS"
            | "SHELLOPTS"
            | "CDPATH"
            | "GLOBIGNORE"
            | "IFS"
            | "PS4"
    ) || name.starts_with("LD_")
        || name.starts_with("DYLD_")
        || name.starts_with("DOCKER_")
        || name.starts_with("CONTAINER_");
    if denied {
        return Err(Error::Invalid(format!(
            "environment variable {name} is not permitted in a command recipe"
        )));
    }
    Ok(())
}

fn aggregate_bytes<'a>(values: impl Iterator<Item = &'a str>, kind: &str) -> Result<usize> {
    let mut total = 0usize;
    for value in values {
        if value.contains('\0') {
            return Err(Error::Invalid(format!("{kind} contains NUL")));
        }
        total = total
            .checked_add(value.len())
            .ok_or_else(|| Error::Limit(format!("{kind} byte count overflow")))?;
    }
    Ok(total)
}

fn bounded(kind: &str, value: u64, minimum: u64, maximum: u64) -> Result<()> {
    if !(minimum..=maximum).contains(&value) {
        return Err(Error::Limit(format!(
            "{kind} must be between {minimum} and {maximum}"
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context::ChangeId;

    fn recipe() -> CommandRecipe {
        let environment = BTreeMap::from([("LANG".into(), "C.UTF-8".into())]);
        CommandRecipe {
            version: 1,
            image: format!("sha256:{}", "1".repeat(64)),
            argv: vec!["/bin/test".into(), "--version".into()],
            environment_sha256: environment_digest(&environment).unwrap(),
            environment,
            toolchain_sha256: "2".repeat(64),
            limits: ExecutionLimits {
                timeout_ms: 1_000,
                max_output_bytes: 1_024,
                memory_bytes: 16 * 1024 * 1024,
                scratch_bytes: 1024 * 1024,
                pids: 8,
                cpu_millis: 1_000,
            },
            syscall_policy: SyscallPolicy::DockerBuiltin,
            no_new_privileges: true,
        }
    }

    fn request() -> ExecutionRequest {
        let recipe = recipe();
        ExecutionRequest {
            attempt_id: "attempt_1".into(),
            subject: CheckSubject {
                repository: "repo".into(),
                change: ChangeId {
                    repository: "repo".into(),
                    id: "change".into(),
                },
                change_version: 1,
                context_digest: "3".repeat(64),
                revision: "4".repeat(64),
                recipe_sha256: recipe.digest().unwrap(),
                toolchain_sha256: recipe.toolchain_sha256.clone(),
                environment_sha256: recipe.environment_sha256.clone(),
            },
            run_generation: 1,
            recipe,
        }
    }

    fn record() -> ExecutionRecord {
        let request = request();
        ExecutionRecord {
            version: 1,
            request_sha256: request.digest().unwrap(),
            request,
            outcome: ExecutionOutcome::Success,
            exit_code: Some(0),
            stdout: Vec::new(),
            stderr: Vec::new(),
            output_truncated: false,
            cleanup_confirmed: true,
            cleanup_handle: None,
            isolation_observed: true,
            runtime_identity: Some("docker:test".into()),
            detail: "complete".into(),
        }
    }

    #[test]
    fn request_binds_subject_to_recipe() {
        let request = request();
        assert!(request.validate().is_ok());
        assert_eq!(request.canonical_bytes().unwrap()[0], b'{');
    }

    #[test]
    fn recipe_rejects_sensitive_environment() {
        let mut recipe = recipe();
        recipe.environment.insert("LD_PRELOAD".into(), "x".into());
        recipe.environment_sha256 = environment_digest(&recipe.environment).unwrap();
        assert!(recipe.validate().is_err());
    }

    #[test]
    fn cleanup_handle_binds_request_and_container_nonce() {
        let mut record = record();
        let nonce = "a".repeat(64);
        record.outcome = ExecutionOutcome::Indeterminate;
        record.exit_code = None;
        record.cleanup_confirmed = false;
        record.cleanup_handle = Some(CleanupHandle {
            container_name: format!("agentscm-{nonce}"),
            owner_label: format!("{}:{nonce}", record.request.digest().unwrap()),
            creation_confirmed: true,
            runtime_identity: "docker:fixture".into(),
            container_id: Some("a".repeat(64)),
        });
        record.runtime_identity = Some("docker:fixture".into());
        assert!(record.validate().is_ok());

        let mut wrong_name = record.clone();
        wrong_name.cleanup_handle.as_mut().unwrap().container_name =
            format!("agentscm-{}", "b".repeat(64));
        assert!(wrong_name.validate().is_err());
        let mut wrong_owner = record;
        wrong_owner.cleanup_handle.as_mut().unwrap().owner_label =
            format!("{}:{nonce}", "f".repeat(64));
        assert!(wrong_owner.validate().is_err());
    }

    #[test]
    fn cleanup_handle_rejects_forged_container_or_runtime_identity() {
        let mut record = record();
        let nonce = "a".repeat(64);
        record.outcome = ExecutionOutcome::Indeterminate;
        record.exit_code = None;
        record.cleanup_confirmed = false;
        record.runtime_identity = Some("docker:fixture".into());
        record.cleanup_handle = Some(CleanupHandle {
            container_name: format!("agentscm-{nonce}"),
            owner_label: format!("{}:{nonce}", record.request.digest().unwrap()),
            creation_confirmed: true,
            runtime_identity: "docker:fixture".into(),
            container_id: Some("not-a-digest".into()),
        });
        assert!(record.validate().is_err());

        record.cleanup_handle.as_mut().unwrap().container_id = Some("b".repeat(64));
        record.cleanup_handle.as_mut().unwrap().runtime_identity = "docker:other".into();
        assert!(record.validate().is_err());
    }

    #[test]
    fn success_requires_confirmed_cleanup_without_handle() {
        let mut record = record();
        let nonce = "a".repeat(64);
        record.cleanup_confirmed = false;
        record.cleanup_handle = Some(CleanupHandle {
            container_name: format!("agentscm-{nonce}"),
            owner_label: format!("{}:{nonce}", record.request.digest().unwrap()),
            creation_confirmed: true,
            runtime_identity: "docker:test".into(),
            container_id: Some("a".repeat(64)),
        });
        assert!(record.validate().is_err());
    }

    #[test]
    fn canonical_record_roundtrip_preserves_digest() {
        let record = record();
        let digest = record.digest().unwrap();
        let decoded: ExecutionRecord =
            serde_json::from_slice(&record.canonical_bytes().unwrap()).unwrap();
        assert_eq!(decoded.digest().unwrap(), digest);
        assert_eq!(decoded, record);
    }
}
