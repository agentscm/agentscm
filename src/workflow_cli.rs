use std::path::{Path, PathBuf};

use agentscm::authority::Authority;
use agentscm::context::ChangeId;
use agentscm::store::Engine;
use agentscm::sync;
use agentscm::workspace::{RecoveryChoice, Workspace};
use agentscm::{Error, Result};
use clap::{Subcommand, ValueEnum};
use serde_json::{Value, json};

#[derive(Debug, Subcommand)]
pub enum WorkspaceCommand {
    /// Create a new directory for an existing local Change and revision.
    Create {
        root: PathBuf,
        change: String,
        revision: String,
        /// Materialize this path and its descendants; repeat for configuration/dependencies.
        /// With no paths, materialize the complete supported revision.
        #[arg(long = "path")]
        paths: Vec<String>,
    },
    /// Add paths from the current immutable base without replacing local occupants.
    Expand {
        root: PathBuf,
        operation_id: String,
        expected_generation: u64,
        /// Path root to add; repeat as needed. A sole '.' requests the complete revision.
        #[arg(long = "path", required = true)]
        paths: Vec<String>,
    },
    /// Inspect base membership and projection state without fetching file contents.
    PathState {
        root: PathBuf,
        path: String,
    },
    State {
        root: PathBuf,
    },
    Status {
        root: PathBuf,
        #[arg(long, default_value_t = 1000)]
        limit: usize,
    },
    /// List what would be captured and what exclusion rules leave out, with the rule for each.
    Preview {
        root: PathBuf,
        #[arg(long, default_value_t = 1000)]
        limit: usize,
    },
    Checkpoint {
        root: PathBuf,
        operation_id: String,
        expected_generation: u64,
    },
    Undo {
        root: PathBuf,
        operation_id: String,
        expected_generation: u64,
        checkpoint_operation_id: String,
    },
    Recover {
        root: PathBuf,
        #[arg(long, value_enum)]
        choice: Choice,
    },
}

pub(crate) fn workspace_telemetry_action(command: &WorkspaceCommand) -> bool {
    matches!(
        command,
        WorkspaceCommand::Create { .. }
            | WorkspaceCommand::Expand { .. }
            | WorkspaceCommand::Checkpoint { .. }
            | WorkspaceCommand::Undo { .. }
            | WorkspaceCommand::Recover { .. }
    )
}

#[derive(Clone, Copy, Debug, ValueEnum)]
pub enum Choice {
    Complete,
    RollBack,
}

#[derive(Debug, Subcommand)]
pub enum SyncCommand {
    /// Write a new bounded source offer for one or more roots.
    Offer {
        output: PathBuf,
        #[arg(required = true)]
        roots: Vec<String>,
    },
    /// Request objects missing from this receiver.
    Request { offer: PathBuf, output: PathBuf },
    /// Write a new bundle with the requested source objects.
    Bundle {
        offer: PathBuf,
        request: PathBuf,
        output: PathBuf,
    },
    /// Validate and import source; no target or approval is imported.
    Import {
        bundle: PathBuf,
        state_directory: PathBuf,
    },
}

pub fn sync(data_dir: &Path, command: SyncCommand) -> Result<Value> {
    let engine = Engine::open(data_dir)?;
    match command {
        SyncCommand::Offer { output, roots } => {
            let authority = Authority::open(data_dir)?;
            let offer = sync::write_offer(&engine, &authority.repository_id()?, &roots, &output)?;
            Ok(json!({"ok":true,"output":output,"roots":offer.roots,"objects":offer.objects.len()}))
        }
        SyncCommand::Request { offer, output } => {
            let request = sync::write_request(&engine, &offer, &output)?;
            Ok(
                json!({"ok":true,"output":output,"missing":request.missing.len(),"offer_sha256":request.offer_sha256}),
            )
        }
        SyncCommand::Bundle {
            offer,
            request,
            output,
        } => {
            sync::write_bundle(&engine, &offer, &request, &output)?;
            Ok(json!({"ok":true,"output":output}))
        }
        SyncCommand::Import {
            bundle,
            state_directory,
        } => {
            let outcome = sync::import_bundle(&engine, &bundle, &state_directory)?;
            Ok(json!({"ok":true,"outcome":outcome}))
        }
    }
}

pub fn workspace(data_dir: &Path, command: WorkspaceCommand) -> Result<Value> {
    let root = match &command {
        WorkspaceCommand::Create { root, .. }
        | WorkspaceCommand::Expand { root, .. }
        | WorkspaceCommand::PathState { root, .. }
        | WorkspaceCommand::State { root }
        | WorkspaceCommand::Status { root, .. }
        | WorkspaceCommand::Preview { root, .. }
        | WorkspaceCommand::Checkpoint { root, .. }
        | WorkspaceCommand::Undo { root, .. }
        | WorkspaceCommand::Recover { root, .. } => root,
    };
    // Require an existing external store before opening/creating anything in a workspace.
    let data = data_dir.canonicalize()?;
    if !data.join("state.sqlite").is_file() {
        return Err(Error::Invalid(
            "workspace commands require an initialized repository data directory".into(),
        ));
    }
    let absolute_root = if root.exists() {
        root.canonicalize()?
    } else {
        let parent = root
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or(Path::new("."));
        let name = root
            .file_name()
            .ok_or_else(|| Error::Invalid("workspace root needs a directory name".into()))?;
        parent.canonicalize()?.join(name)
    };
    if data.starts_with(&absolute_root) || absolute_root.starts_with(&data) {
        return Err(Error::Invalid(
            "repository data directory and workspace must be separate, non-nested directories"
                .into(),
        ));
    }
    let authority = Authority::open(&data)?;
    let repository = authority.repository_id()?;
    let engine = Engine::open(&data)?;
    if let WorkspaceCommand::Create {
        root,
        change,
        revision,
        paths,
    } = command
    {
        let id = ChangeId {
            repository,
            id: change,
        };
        let state = authority.change(&id)?;
        let context = authority.review_context(&state.context_digest)?;
        if context.revision != revision {
            return Err(Error::Conflict(
                "workspace revision must match the Change's current context at creation".into(),
            ));
        }
        let workspace = if paths.is_empty() {
            Workspace::create(&engine, root, id, &revision)?
        } else {
            Workspace::create_scoped(&engine, root, id, &revision, &paths)?
        };
        return Ok(json!({"ok":true,"workspace":workspace.state()?}));
    }
    let mut workspace = Workspace::open(root)?;
    if workspace.state()?.change.repository != repository {
        return Err(Error::Conflict(
            "workspace belongs to another repository".into(),
        ));
    }
    match command {
        WorkspaceCommand::Expand {
            operation_id,
            expected_generation,
            paths,
            ..
        } => Ok(
            json!({"ok":true,"outcome":workspace.expand(&engine, &operation_id, expected_generation, &paths)?}),
        ),
        WorkspaceCommand::PathState { path, .. } => {
            Ok(json!({"ok":true,"path":path,"state":workspace.path_state(&engine, &path)?}))
        }
        WorkspaceCommand::State { .. } => Ok(json!({"ok":true,"workspace":workspace.state()?})),
        WorkspaceCommand::Status { limit, .. } => {
            Ok(json!({"ok":true,"status":workspace.status(&engine, limit)?}))
        }
        WorkspaceCommand::Preview { limit, .. } => {
            Ok(json!({"ok":true,"preview":workspace.preview(&engine, limit)?}))
        }
        WorkspaceCommand::Checkpoint {
            operation_id,
            expected_generation,
            ..
        } => Ok(
            json!({"ok":true,"outcome":workspace.checkpoint(&engine, &operation_id, expected_generation)?}),
        ),
        WorkspaceCommand::Undo {
            operation_id,
            expected_generation,
            checkpoint_operation_id,
            ..
        } => Ok(
            json!({"ok":true,"outcome":workspace.undo(&engine, &operation_id, expected_generation, &checkpoint_operation_id)?}),
        ),
        WorkspaceCommand::Recover { choice, .. } => Ok(
            json!({"ok":true,"outcome":workspace.recover(&engine, match choice { Choice::Complete=>RecoveryChoice::Complete, Choice::RollBack=>RecoveryChoice::RollBack })?}),
        ),
        WorkspaceCommand::Create { .. } => unreachable!("create handled before open"),
    }
}
