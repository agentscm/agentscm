use std::io::{IsTerminal, Write};
use std::path::{Path, PathBuf};

use agentscm::authority::Authority;
use agentscm::git_snapshot::{self, SnapshotRequest};
use agentscm::store::Engine;
use agentscm::{Error, Result};
use clap::{Args, Subcommand};
use serde_json::{Value, json};

#[derive(Debug, Args)]
pub struct Source {
    /// Absolute path of the Git executable.
    #[arg(long, default_value = "/usr/bin/git")]
    git: PathBuf,
    #[arg(long)]
    repository: PathBuf,
    /// Full lowercase commit object ID.
    #[arg(long)]
    revision: String,
    /// Tracked symlink or gitlink path to leave out. Repeatable. A symlink not listed is captured.
    #[arg(long)]
    exclude: Vec<String>,
}

#[derive(Debug, Subcommand)]
pub enum GitSnapshotCommand {
    /// Report what a capture would read. Never opens the data directory.
    Preflight {
        #[command(flatten)]
        source: Source,
    },
    /// Publish one native revision for a clean committed tree and write a receipt.
    Capture {
        #[command(flatten)]
        source: Source,
        /// New file outside the repository and the data directory.
        #[arg(long)]
        receipt: PathBuf,
        /// Print `progress: ` lines on stderr (default when stderr is a terminal).
        #[arg(long)]
        progress: bool,
    },
}

pub(crate) fn telemetry_action(command: &GitSnapshotCommand) -> bool {
    matches!(command, GitSnapshotCommand::Capture { .. })
}

fn request(source: Source) -> SnapshotRequest {
    SnapshotRequest {
        git: source.git,
        repository: source.repository,
        revision: source.revision,
        exclude: source.exclude,
    }
}

pub fn run(data_dir: &Path, command: GitSnapshotCommand) -> Result<Value> {
    match command {
        GitSnapshotCommand::Preflight { source } => {
            let report = git_snapshot::preflight(&request(source))?;
            Ok(json!({"ok":true,"preflight":report}))
        }
        GitSnapshotCommand::Capture {
            source,
            receipt,
            progress,
        } => {
            let request = request(source);
            let data = data_dir.canonicalize()?;
            if !data.join("state.sqlite").is_file() {
                return Err(Error::Invalid(
                    "git-snapshot capture requires an initialized repository data directory".into(),
                ));
            }
            let name = receipt
                .file_name()
                .ok_or_else(|| Error::Invalid("receipt needs a file name".into()))?;
            let parent = receipt
                .parent()
                .filter(|p| !p.as_os_str().is_empty())
                .unwrap_or(Path::new("."));
            let receipt_path = parent.canonicalize()?.join(name);
            // Read-only: nothing is written until the paths are known to be separate.
            let report = git_snapshot::preflight(&request)?;
            let toplevel = Path::new(&report.repository).canonicalize()?;
            if data.starts_with(&toplevel) || toplevel.starts_with(&data) {
                return Err(Error::Invalid(
                    "repository data directory and Git repository must be separate, non-nested directories".into(),
                ));
            }
            let git_dir = Path::new(&report.git_dir).canonicalize()?;
            if data.starts_with(&git_dir) || git_dir.starts_with(&data) {
                return Err(Error::Invalid(
                    "repository data directory and Git directory must be separate, non-nested directories".into(),
                ));
            }
            if [&toplevel, &git_dir, &data]
                .iter()
                .any(|root| receipt_path.starts_with(root))
            {
                return Err(Error::Invalid(
                    "receipt must be outside the Git repository, Git directory and the data directory".into(),
                ));
            }
            let repository = Authority::open(&data)?.repository_id()?;
            git_snapshot::check_receipt_path(&receipt_path, &report, &repository)?;
            let engine = Engine::open(&data)?;
            let show = progress || std::io::stderr().is_terminal();
            let mut on_progress = |line: &str| {
                if show {
                    let _ = writeln!(std::io::stderr(), "progress: {line}");
                }
            };
            let snapshot = git_snapshot::capture_with_progress(
                &engine,
                &repository,
                &request,
                report,
                &mut on_progress,
            )?;
            let existing = git_snapshot::write_receipt(&receipt_path, &snapshot.receipt)?;
            Ok(
                json!({"ok":true,"revision":snapshot.revision,"receipt":receipt_path,
                "existing":existing,"batches":snapshot.batches}),
            )
        }
    }
}
