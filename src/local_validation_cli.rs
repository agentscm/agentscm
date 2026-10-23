//! `local-validation run|show|verify`: produce, display and independently verify
//! a Git-bound local validation attestation. Keys come from `agent-task key-generate`.

use std::fs::{self, DirBuilder, File, OpenOptions};
use std::io::{Read, Write};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use agentscm::evidence::{EvaluationClock, EvidenceOutcome, sign_evidence};
use agentscm::local_validation::comment::{self, MAX_COMMENT_BYTES, envelope_sha256};
use agentscm::local_validation::run::{observe, produce};
use agentscm::local_validation::{
    Issuance, MAX_RECORD_BYTES, Observed, ValidationPolicy, ValidationRecipe, Verdict,
    VerdictState, assess, evidence_payload,
};
use agentscm::policy::issuers::{IssuerRegistrySnapshot, issuer_key_id};
use agentscm::{Error, Result};
use clap::{Args, Subcommand};
use serde_json::json;

use crate::agent_task_cli::{public_hex, read_signing_key, validate_private_directory_metadata};

const MAX_INPUT_BYTES: u64 = 2 * 1024 * 1024;
const MAX_EXPIRES_SECONDS: u64 = 366 * 24 * 3600;

#[derive(Debug, Subcommand)]
pub enum LocalValidationCommand {
    /// Run the recipe on a clean tree, sign the result and retain it before printing.
    Run(RunArgs),
    /// Print the PR-comment body for a retained attestation.
    Show {
        #[arg(long)]
        state_dir: PathBuf,
        /// Envelope SHA-256 printed by `run`.
        #[arg(long)]
        attestation: String,
    },
    /// Decide whether an attestation satisfies the exact head. Exit 0 success, 1 failure, 2 error.
    Verify(VerifyArgs),
}

#[derive(Debug, Args)]
pub struct RunArgs {
    #[arg(long, default_value = ".")]
    repo: PathBuf,
    #[arg(long)]
    recipe: PathBuf,
    #[arg(long)]
    policy: PathBuf,
    /// IssuerRegistrySnapshot JSON.
    #[arg(long)]
    registry: PathBuf,
    /// Absolute owner-private raw Ed25519 key file (see `agent-task key-generate`).
    #[arg(long)]
    key_file: PathBuf,
    #[arg(long)]
    issuer_id: String,
    /// Absolute directory for retained attestations; created 0700.
    #[arg(long)]
    state_dir: PathBuf,
    /// Forge base branch name recorded in the context.
    #[arg(long, default_value = "main")]
    base_ref: String,
    /// Local revision resolved for the base tip and merge-base.
    #[arg(long, default_value = "origin/main")]
    base_rev: String,
    /// Defaults to the policy maximum age.
    #[arg(long, value_parser = clap::value_parser!(u64).range(1..=MAX_EXPIRES_SECONDS))]
    expires_in_seconds: Option<u64>,
}

#[derive(Debug, Args)]
pub struct VerifyArgs {
    /// Comment body produced by `show`.
    #[arg(long, conflicts_with_all = ["envelope", "record"])]
    comment_body: Option<PathBuf>,
    #[arg(long, requires = "record")]
    envelope: Option<PathBuf>,
    #[arg(long, requires = "envelope")]
    record: Option<PathBuf>,
    #[arg(long)]
    policy: PathBuf,
    #[arg(long)]
    registry: PathBuf,
    #[arg(long, default_value = ".")]
    repo: PathBuf,
    #[arg(long, default_value = "HEAD")]
    head: String,
    #[arg(long, default_value = "main")]
    base_ref: String,
    #[arg(long, default_value = "origin/main")]
    base_rev: String,
    #[arg(long)]
    repository: String,
}

/// Returns the process exit code.
pub fn run(command: LocalValidationCommand) -> Result<i32> {
    match command {
        LocalValidationCommand::Run(args) => run_recipe(args),
        LocalValidationCommand::Show {
            state_dir,
            attestation,
        } => {
            show(&state_dir, &attestation)?;
            Ok(0)
        }
        LocalValidationCommand::Verify(args) => verify(args),
    }
}

fn read_bounded(path: &Path, max: u64) -> Result<Vec<u8>> {
    let mut bytes = Vec::new();
    File::open(path)?.take(max + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > max {
        return Err(Error::Limit(format!(
            "{} exceeds {max} bytes",
            path.display()
        )));
    }
    Ok(bytes)
}

fn read_registry(path: &Path) -> Result<IssuerRegistrySnapshot> {
    IssuerRegistrySnapshot::from_slice(&read_bounded(path, MAX_INPUT_BYTES)?)
}

fn now_ms() -> Option<u64> {
    let elapsed = SystemTime::now().duration_since(UNIX_EPOCH).ok()?;
    u64::try_from(elapsed.as_millis()).ok()
}

fn retain(dir: &Path, name: &str, bytes: &[u8]) -> Result<()> {
    let temporary = dir.join(format!(".{name}.tmp"));
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&temporary)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    fs::rename(&temporary, dir.join(name))?;
    File::open(dir)?.sync_all()?;
    Ok(())
}

fn private_state_dir(path: &Path) -> Result<()> {
    if !path.is_absolute() {
        return Err(Error::Invalid("state directory must be absolute".into()));
    }
    match DirBuilder::new().mode(0o700).create(path) {
        Err(e) if e.kind() != std::io::ErrorKind::AlreadyExists => return Err(e.into()),
        _ => {}
    }
    validate_private_directory_metadata(&fs::symlink_metadata(path)?)
}

fn run_recipe(args: RunArgs) -> Result<i32> {
    let recipe: ValidationRecipe =
        ValidationRecipe::from_slice(&read_bounded(&args.recipe, MAX_INPUT_BYTES)?)?;
    let policy = ValidationPolicy::from_slice(&read_bounded(&args.policy, MAX_INPUT_BYTES)?)?;
    let registry = read_registry(&args.registry)?;
    if registry.repository != policy.repository {
        return Err(Error::Invalid(
            "registry repository differs from the policy repository".into(),
        ));
    }
    let key = read_signing_key(&args.key_file)?;
    private_state_dir(&args.state_dir)?;
    let produced = produce(&args.repo, &recipe, &policy, &args.base_ref, &args.base_rev)?;
    let mut attempt = [0_u8; 16];
    getrandom::fill(&mut attempt)
        .map_err(|_| Error::Invalid("random source unavailable".into()))?;
    let attempt_id: String = attempt.iter().map(|b| format!("{b:02x}")).collect();
    let issued_at_ms = now_ms().ok_or_else(|| Error::Invalid("system clock unavailable".into()))?;
    let lifetime = args.expires_in_seconds.unwrap_or(policy.max_age_seconds);
    let key_id = issuer_key_id(&public_hex(&key))?;
    let (payload, record_bytes) = evidence_payload(
        &produced.record,
        &policy,
        &Issuance {
            issuer_id: &args.issuer_id,
            key_id: &key_id,
            attempt_id: &attempt_id,
            run_generation: 1,
            issued_at_ms,
            expires_at_ms: issued_at_ms.saturating_add(lifetime.saturating_mul(1000)),
            issuer_registry_sha256: &registry.digest()?,
            cargo_lock_sha256: &produced.cargo_lock_sha256,
            toolchain_file_sha256: &produced.toolchain_file_sha256,
        },
    )?;
    let pass = payload.outcome == EvidenceOutcome::Pass;
    let envelope_bytes = serde_json::to_vec(&sign_evidence(payload, &key)?)?;
    let sha = envelope_sha256(&envelope_bytes);
    // The record first: an envelope on disk always has its record.
    retain(
        &args.state_dir,
        &format!("{sha}.record.json"),
        &record_bytes,
    )?;
    retain(
        &args.state_dir,
        &format!("{sha}.envelope.json"),
        &envelope_bytes,
    )?;
    println!(
        "{}",
        json!({
            "envelope_sha256": sha,
            "outcome": if pass { "pass" } else { "fail" },
            "commit": produced.record.subject.commit,
        })
    );
    Ok(i32::from(!pass))
}

fn show(state_dir: &Path, sha: &str) -> Result<()> {
    if sha.len() != 64 || !sha.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')) {
        return Err(Error::Invalid(
            "attestation must be a lowercase SHA-256".into(),
        ));
    }
    let envelope = read_bounded(
        &state_dir.join(format!("{sha}.envelope.json")),
        MAX_RECORD_BYTES as u64,
    )?;
    let record = read_bounded(
        &state_dir.join(format!("{sha}.record.json")),
        MAX_RECORD_BYTES as u64,
    )?;
    if envelope_sha256(&envelope) != sha {
        return Err(Error::Invalid(
            "retained envelope does not match its name".into(),
        ));
    }
    print!("{}", comment::render(&envelope, &record)?);
    Ok(())
}

fn verdict_exit(
    state: VerdictState,
    reason: &str,
    detail: &str,
    envelope_sha256: &str,
    commit: &str,
) -> i32 {
    let (name, code) = match state {
        VerdictState::Success => ("success", 0),
        VerdictState::Failure => ("failure", 1),
        VerdictState::Error => ("error", 2),
    };
    println!(
        "{}",
        json!({"state": name, "reason": reason, "detail": detail,
               "envelope_sha256": envelope_sha256, "commit": commit})
    );
    code
}

fn verify(args: VerifyArgs) -> Result<i32> {
    let policy = ValidationPolicy::from_slice(&read_bounded(&args.policy, MAX_INPUT_BYTES)?)?;
    let registry = read_registry(&args.registry)?;
    let inputs = match (&args.comment_body, &args.envelope, &args.record) {
        (Some(body), _, _) => {
            let raw = read_bounded(body, MAX_COMMENT_BYTES as u64)?;
            String::from_utf8(raw)
                .map_err(|_| Error::Invalid("comment body is not UTF-8".into()))
                .and_then(|text| comment::parse(&text))
        }
        (None, Some(envelope), Some(record)) => Ok((
            read_bounded(envelope, MAX_RECORD_BYTES as u64)?,
            read_bounded(record, MAX_RECORD_BYTES as u64)?,
        )),
        _ => {
            return Err(Error::Invalid(
                "pass --comment-body or --envelope with --record".into(),
            ));
        }
    };
    let (envelope, record) = match inputs {
        Ok(pair) => pair,
        Err(error) => {
            return Ok(verdict_exit(
                VerdictState::Failure,
                "comment body invalid",
                &error.to_string(),
                "",
                "",
            ));
        }
    };
    let sha = envelope_sha256(&envelope);
    let git = match observe(&args.repo, &args.head, &args.base_rev) {
        Ok(git) => git,
        Err(error) => {
            return Ok(verdict_exit(
                VerdictState::Error,
                "git unavailable",
                &error.to_string(),
                &sha,
                "",
            ));
        }
    };
    let clock = now_ms().map_or(EvaluationClock::Uncertain, |unix_ms| {
        EvaluationClock::Known { unix_ms }
    });
    let Verdict {
        state,
        reason,
        detail,
    } = assess(
        &envelope,
        &record,
        &registry,
        &policy,
        Observed {
            repository: &args.repository,
            commit: &git.commit,
            tree: &git.tree,
            base_ref: &args.base_ref,
            merge_base: &git.merge_base,
        },
        clock,
    );
    Ok(verdict_exit(state, reason, &detail, &sha, &git.commit))
}
