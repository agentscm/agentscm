use std::path::{Path, PathBuf};

use agentscm::attached_worktree::{AttachRequest, AttachmentJournal, SourceMode};
use agentscm::authority::Authority;
use agentscm::store::Engine;
use agentscm::{Error, Result};
use clap::Subcommand;
use serde_json::{Value, json};

#[derive(Debug, Subcommand)]
pub enum AttachedWorktreeCommand {
    /// Bind an actor to this worktree and capture its target baseline and selected source.
    Attach {
        #[arg(long, default_value = "/usr/bin/git")]
        git: PathBuf,
        #[arg(long)]
        repository: PathBuf,
        #[arg(long)]
        actor: String,
        #[arg(long, default_value = "origin")]
        remote: String,
        #[arg(long)]
        target: String,
        #[arg(long)]
        output: String,
        #[arg(long, value_enum, default_value = "committed")]
        source: CliSourceMode,
        #[arg(long = "path")]
        paths: Vec<String>,
        #[arg(long)]
        operation_id: String,
    },
    /// Inspect retained attachment identity and exact Git/native mappings.
    State {
        #[arg(long)]
        id: String,
    },
    /// Capture the frozen configured source without modifying Git state.
    Capture {
        #[arg(long)]
        expected_generation: u64,
        #[arg(long)]
        id: String,
        #[arg(long)]
        operation_id: String,
    },
    /// Import an advanced target and merge it into the native candidate if disjoint.
    Refresh {
        #[arg(long)]
        expected_generation: u64,
        #[arg(long)]
        id: String,
        #[arg(long)]
        operation_id: String,
    },
}

#[derive(Clone, Copy, Debug, clap::ValueEnum)]
pub enum CliSourceMode {
    Committed,
    Index,
    /// Raw selected filesystem bytes, without Git text/EOL/encoding conversion.
    Worktree,
}
impl From<CliSourceMode> for SourceMode {
    fn from(value: CliSourceMode) -> Self {
        match value {
            CliSourceMode::Committed => Self::Committed,
            CliSourceMode::Index => Self::Index,
            CliSourceMode::Worktree => Self::Worktree,
        }
    }
}

pub fn run(data_dir: &Path, command: AttachedWorktreeCommand) -> Result<Value> {
    let data = data_dir.canonicalize()?;
    if !data.join("state.sqlite").is_file() {
        return Err(Error::Invalid(
            "attached-worktree commands require an initialized repository data directory".into(),
        ));
    }
    if let AttachedWorktreeCommand::Attach {
        repository: source, ..
    } = &command
    {
        let source = source.canonicalize()?;
        if source.starts_with(&data) || data.starts_with(&source) {
            return Err(Error::Invalid(
                "source and native data directories must be separate".into(),
            ));
        }
    }
    let repository = Authority::open(&data)?.repository_id()?;
    let engine = Engine::open(&data)?;
    match command {
        AttachedWorktreeCommand::Attach {
            git,
            repository: worktree,
            actor,
            remote,
            target,
            output,
            source,
            paths,
            operation_id,
        } => {
            let journal = AttachmentJournal::open(&data, &repository)?;
            let state = journal.attach(
                &engine,
                &operation_id,
                &AttachRequest {
                    git,
                    worktree,
                    actor,
                    remote,
                    target,
                    output,
                    source: source.into(),
                    paths,
                },
            )?;
            Ok(json!({"ok":true,"attachment":state}))
        }
        AttachedWorktreeCommand::State { id } => {
            let journal = AttachmentJournal::open_existing(&data, &repository)?;
            Ok(
                json!({"ok":true,"attachment":journal.state(&id)?,"mappings":journal.mappings(&id,None,256)?}),
            )
        }
        AttachedWorktreeCommand::Capture {
            id,
            operation_id,
            expected_generation,
        } => {
            let journal = AttachmentJournal::open_existing(&data, &repository)?;
            Ok(
                json!({"ok":true,"capture":journal.capture(&engine,&operation_id,&id,expected_generation)?}),
            )
        }
        AttachedWorktreeCommand::Refresh {
            id,
            operation_id,
            expected_generation,
        } => {
            let journal = AttachmentJournal::open_existing(&data, &repository)?;
            Ok(
                json!({"ok":true,"attachment":journal.refresh(&engine,&operation_id,&id,expected_generation)?}),
            )
        }
    }
}
