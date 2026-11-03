//! `local-ci device key-generate|enroll-request`: create a trusted local CI device key and a
//! signed enrollment request for an administrator to submit. Neither command prints private
//! key bytes. A key location inside the current Git checkout is refused.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

use agentscm::local_ci::{
    EnrollmentStatement, KeyLocationRefusal, OwnerIdentity, key_location_refused,
};
use agentscm::policy::issuer_key_id;
use agentscm::{Error, Result};
use clap::{Args, Subcommand};
use ed25519_dalek::Signer;
use serde_json::{Value, json};

use crate::agent_task_cli::{generate_key, public_hex, read_signing_key};

#[derive(Debug, Subcommand)]
pub enum LocalCiCommand {
    /// Trusted local CI devices (experimental).
    Device {
        #[command(subcommand)]
        command: DeviceCommand,
    },
}

#[derive(Debug, Subcommand)]
pub enum DeviceCommand {
    /// Create a raw Ed25519 device key (mode 0600) and print its public key and key ID.
    KeyGenerate {
        /// Absolute path outside any checkout or job mount.
        #[arg(long)]
        key: PathBuf,
    },
    /// Sign an enrollment request for an administrator to submit; prints JSON, no secret.
    EnrollRequest(EnrollRequestArgs),
}

#[derive(Debug, Args)]
pub struct EnrollRequestArgs {
    #[arg(long)]
    key: PathBuf,
    /// `lcd_` device ID.
    #[arg(long)]
    device_id: String,
    /// `host/owner/name` of the connected repository.
    #[arg(long)]
    repository: String,
    #[arg(long)]
    owner_provider: String,
    #[arg(long)]
    owner_host: String,
    #[arg(long)]
    owner_person: String,
    /// 1 to enroll; the current generation plus 1 to rotate.
    #[arg(long)]
    generation: u64,
}

pub fn run(command: LocalCiCommand) -> Result<Value> {
    let LocalCiCommand::Device { command } = command;
    match command {
        DeviceCommand::KeyGenerate { key } => {
            check_location(&key, false)?;
            generate_key(&key)
        }
        DeviceCommand::EnrollRequest(args) => enroll_request(args),
    }
}

fn enroll_request(args: EnrollRequestArgs) -> Result<Value> {
    check_location(&args.key, true)?;
    let key = read_signing_key(&args.key)?;
    let public_key_hex = public_hex(&key);
    let issued_at_ms = u64::try_from(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|_| Error::Invalid("system clock is before 1970".into()))?
            .as_millis(),
    )
    .map_err(|_| Error::Invalid("system clock is out of range".into()))?;
    let statement = EnrollmentStatement {
        version: 1,
        repository: args.repository,
        owner: OwnerIdentity {
            provider: args.owner_provider,
            host: args.owner_host,
            person_id: args.owner_person,
        },
        device_id: args.device_id,
        key_id: issuer_key_id(&public_key_hex)?,
        public_key_hex,
        generation: args.generation,
        issued_at_ms,
    };
    let signature: String = key
        .sign(&statement.signing_message()?)
        .to_bytes()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    Ok(json!({"statement": statement, "signature": signature}))
}

/// Refuses a key path inside the Git checkout enclosing the current directory, if any.
/// Declared mounts and environment belong to an execution profile, which does not call this yet.
fn check_location(key: &Path, exists: bool) -> Result<()> {
    if !key.is_absolute() {
        return Err(Error::Invalid("key path must be absolute".into()));
    }
    let canonical = if exists {
        key.canonicalize()?
    } else {
        let name = key
            .file_name()
            .ok_or_else(|| Error::Invalid("key path has no file name".into()))?;
        key.parent()
            .ok_or_else(|| Error::Invalid("key path has no parent".into()))?
            .canonicalize()?
            .join(name)
    };
    let root = checkout_root()?;
    let roots: Vec<&Path> = root.iter().map(PathBuf::as_path).collect();
    match key_location_refused(&canonical, &roots, &[], &[]) {
        None => Ok(()),
        Some(KeyLocationRefusal::InsideSourceRoot) => Err(Error::Invalid(
            "key path is inside the current Git checkout; choose a path outside it".into(),
        )),
        Some(_) => Err(Error::Invalid("key path is not acceptable".into())),
    }
}

fn checkout_root() -> Result<Option<PathBuf>> {
    let out = Command::new("git")
        .args(["rev-parse", "--show-toplevel"])
        .output()?;
    if !out.status.success() {
        return Ok(None);
    }
    let root = String::from_utf8_lossy(&out.stdout).trim().to_owned();
    Ok(Some(PathBuf::from(root).canonicalize()?))
}
