use std::io::Read;
use std::path::{Path, PathBuf};

use agentscm::git_reconcile::{self, ExitRequest, InitializeRequest, ObserveRequest, Reconciler};
use agentscm::{Error, Result};
use clap::Subcommand;
use serde_json::{Value, json};

#[derive(Debug, Subcommand)]
pub enum GitReconcileCommand {
    /// Verify an initial complete clean Git/native pair; Git stays authoritative.
    Init {
        #[arg(default_value = "-")]
        input: PathBuf,
    },
    /// Observe one exact pair and retain candidates, conflicts or uncertainty.
    Observe {
        #[arg(default_value = "-")]
        input: PathBuf,
    },
    /// Read historical journal state, without Git, capture or schema creation.
    Status,
    /// Inspect a retained operation; a missing result does not prove nonexecution.
    Lookup { operation_id: String },
    /// Verify a local mirror handoff; does not disconnect or change authority.
    PrepareExit {
        #[arg(default_value = "-")]
        input: PathBuf,
    },
}

fn read<T: serde::de::DeserializeOwned>(input: &Path) -> Result<T> {
    let mut reader: Box<dyn Read> = if input == Path::new("-") {
        Box::new(std::io::stdin())
    } else {
        Box::new(std::fs::File::open(input)?)
    };
    let mut bytes = Vec::new();
    reader.by_ref().take(16385).read_to_end(&mut bytes)?;
    if bytes.len() > 16384 {
        return Err(Error::Limit("reconciliation request exceeds 16 KiB".into()));
    }
    Ok(serde_json::from_slice(&bytes)?)
}

pub fn run(command: GitReconcileCommand, data_dir: &Path) -> Result<Value> {
    match command {
        GitReconcileCommand::Init { input } => {
            let request: InitializeRequest = read(&input)?;
            let reconciliation = Reconciler::initialize(data_dir, &request)?.status()?;
            Ok(json!({"ok":true,"reconciliation":reconciliation}))
        }
        GitReconcileCommand::Observe { input } => {
            let request: ObserveRequest = read(&input)?;
            let reconciliation = Reconciler::open(data_dir)?.observe(&request)?;
            Ok(json!({"ok":true,"reconciliation":reconciliation}))
        }
        GitReconcileCommand::Status => {
            Ok(json!({"ok":true,"reconciliation":git_reconcile::read_status(data_dir)?}))
        }
        GitReconcileCommand::Lookup { operation_id } => {
            Ok(json!({"ok":true,"operation":git_reconcile::lookup(data_dir,&operation_id)?}))
        }
        GitReconcileCommand::PrepareExit { input } => {
            let request: ExitRequest = read(&input)?;
            let reconciliation = Reconciler::open(data_dir)?.prepare_exit(&request)?;
            Ok(json!({"ok":true,"reconciliation":reconciliation}))
        }
    }
}
