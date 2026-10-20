use std::io::Read;
use std::path::{Path, PathBuf};

use agentscm::authority::Authority;
use agentscm::git_export::{ExportRequest, ExternalCheckRequest, Mirror};
use agentscm::store::Engine;
use agentscm::{Error, Result};
use clap::{Args, Subcommand};
use serde_json::{Value, json};

#[derive(Debug, Args)]
pub struct Destination {
    #[arg(long, default_value = "/usr/bin/git")]
    git: PathBuf,
    #[arg(long)]
    destination: PathBuf,
}

#[derive(Debug, Subcommand)]
pub enum GitExportCommand {
    /// Create a new owned bare local mirror. Never adopts an existing repository.
    Init {
        #[command(flatten)]
        destination: Destination,
        #[arg(long, default_value = "main")]
        target: String,
        /// Local Git repository containing the exact seed commit.
        #[arg(long, value_name = "LOCAL", requires = "seed_commit")]
        seed_repository: Option<PathBuf>,
        /// Full lowercase SHA-1 commit OID to retain as mirror ancestry.
        #[arg(long, value_name = "FULL_OID", requires = "seed_repository")]
        seed_commit: Option<String>,
    },
    /// Verify and export an exact integrated native event, with expected-ref CAS.
    Export {
        #[command(flatten)]
        destination: Destination,
        /// Fixed export request JSON, or - for stdin.
        #[arg(default_value = "-")]
        input: PathBuf,
    },
    /// Inspect retained state and the separately observed current Git ref.
    Status {
        #[command(flatten)]
        destination: Destination,
        #[arg(long)]
        event: u64,
    },
    /// Find retained event-to-Git mappings for a native revision.
    Lookup {
        #[command(flatten)]
        destination: Destination,
        #[arg(long)]
        revision: String,
    },
    /// Retain an unverified local result for one exact mirrored Git commit.
    RecordCheck {
        #[command(flatten)]
        destination: Destination,
        /// External check request JSON, or - for stdin.
        #[arg(default_value = "-")]
        input: PathBuf,
    },
    /// List retained unverified external results, optionally for one event.
    Checks {
        #[command(flatten)]
        destination: Destination,
        #[arg(long)]
        event: Option<u64>,
    },
}

pub(crate) fn telemetry_action(command: &GitExportCommand) -> bool {
    matches!(
        command,
        GitExportCommand::Init { .. }
            | GitExportCommand::Export { .. }
            | GitExportCommand::RecordCheck { .. }
    )
}

pub fn run(command: GitExportCommand, data_dir: &Path) -> Result<Value> {
    let data = data_dir.canonicalize()?;
    if !data.join("state.sqlite").is_file() {
        return Err(Error::Invalid(
            "git-export requires initialized native data".into(),
        ));
    }
    let destination = match &command {
        GitExportCommand::Init { destination, .. }
        | GitExportCommand::Export { destination, .. }
        | GitExportCommand::Status { destination, .. }
        | GitExportCommand::Lookup { destination, .. }
        | GitExportCommand::RecordCheck { destination, .. }
        | GitExportCommand::Checks { destination, .. } => destination,
    };
    let path = &destination.destination;
    let canonical = if path.exists() {
        path.canonicalize()?
    } else {
        let name = path
            .file_name()
            .ok_or_else(|| Error::Invalid("mirror needs a name".into()))?;
        path.parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or(Path::new("."))
            .canonicalize()?
            .join(name)
    };
    if canonical.starts_with(&data) || data.starts_with(&canonical) {
        return Err(Error::Invalid(
            "mirror and native data must be separate, non-nested directories".into(),
        ));
    }
    let authority = Authority::open_read_only(&data)?;
    let repository = authority.repository_id()?;
    match command {
        GitExportCommand::Init {
            destination,
            target,
            seed_repository,
            seed_commit,
        } => {
            let mirror = match (seed_repository, seed_commit) {
                (Some(seed_repository), Some(seed_commit)) => Mirror::initialize_with_seed(
                    &destination.git,
                    &destination.destination,
                    &repository,
                    &target,
                    &seed_repository,
                    &seed_commit,
                )?,
                (None, None) => Mirror::initialize(
                    &destination.git,
                    &destination.destination,
                    &repository,
                    &target,
                )?,
                _ => {
                    return Err(Error::Invalid(
                        "seed repository and seed commit must be supplied together".into(),
                    ));
                }
            };
            Ok(
                json!({"ok":true,"destination":canonical,"reference":"refs/heads/agentscm","profile":agentscm::git_export::PROFILE,"seed":mirror.seed()}),
            )
        }
        GitExportCommand::Export { destination, input } => {
            let request: ExportRequest = read_request(&input, "export")?;
            let mut mirror = Mirror::open(&destination.git, &canonical, &repository)?;
            let objects = std::fs::symlink_metadata(data.join("objects"))?;
            if !objects.file_type().is_dir() {
                return Err(Error::Invalid(
                    "git-export requires an existing native object directory".into(),
                ));
            }
            let engine = Engine::open(&data)?;
            let status = mirror.export(&engine, &authority, &request)?;
            Ok(json!({"ok":true,"export":status}))
        }
        GitExportCommand::Status { destination, event } => {
            let mirror = Mirror::open(&destination.git, &canonical, &repository)?;
            Ok(json!({"ok":true,"export":mirror.status(event)?}))
        }
        GitExportCommand::Lookup {
            destination,
            revision,
        } => {
            let mirror = Mirror::open(&destination.git, &canonical, &repository)?;
            Ok(json!({"ok":true,"exports":mirror.lookup_revision(&revision)?}))
        }
        GitExportCommand::RecordCheck { destination, input } => {
            let request: ExternalCheckRequest = read_request(&input, "external check")?;
            let mut mirror = Mirror::open(&destination.git, &canonical, &repository)?;
            Ok(json!({"ok":true,"check":mirror.record_external_check(&request)?}))
        }
        GitExportCommand::Checks { destination, event } => {
            let mirror = Mirror::open(&destination.git, &canonical, &repository)?;
            Ok(json!({"ok":true,"checks":mirror.external_checks(event)?}))
        }
    }
}

fn read_request<T: serde::de::DeserializeOwned>(path: &Path, kind: &str) -> Result<T> {
    let mut reader: Box<dyn Read> = if path == Path::new("-") {
        Box::new(std::io::stdin())
    } else {
        Box::new(std::fs::File::open(path)?)
    };
    let mut bytes = Vec::new();
    reader.by_ref().take(16385).read_to_end(&mut bytes)?;
    if bytes.len() > 16384 {
        return Err(Error::Limit(format!("{kind} request exceeds 16 KiB")));
    }
    Ok(serde_json::from_slice(&bytes)?)
}
