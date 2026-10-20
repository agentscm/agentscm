use std::fs::File;
use std::io::{self, Read};
use std::path::{Path, PathBuf};

use clap::Subcommand;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};

use agentscm::authority::Authority;
use agentscm::merge::{BaseChoice, MergeJournal, Resolution};
use agentscm::store::Engine;
use agentscm::workspace::Workspace;
use agentscm::{Error, Result};

const MAX_JSON_INPUT_BYTES: u64 = 16 * 1024 * 1024;
const MAX_CURSOR_BYTES: u64 = 1_024;
const MAX_PAGE: usize = 1_000;

#[derive(Debug, Subcommand)]
pub enum MergeCommand {
    /// Begin or retry a merge of two local source revisions.
    Begin {
        operation_id: String,
        left: String,
        right: String,
    },
    /// Find the durable merge session associated with an operation.
    Operation { operation: String },
    /// Inspect a merge session.
    Show { session: String },
    /// Page the best common ancestor candidates.
    Bases {
        session: String,
        /// Cursor file containing one JSON string, or - for standard input.
        #[arg(long)]
        cursor: Option<PathBuf>,
        #[arg(long, default_value_t = 100)]
        limit: usize,
    },
    /// Page the merge conflicts without their detail previews.
    Conflicts {
        session: String,
        /// Cursor file containing one JSON string, or - for standard input.
        #[arg(long)]
        cursor: Option<PathBuf>,
        #[arg(long, default_value_t = 100)]
        limit: usize,
    },
    /// Inspect one conflict, including its bounded preview when present.
    Conflict {
        session: String,
        conflict_id: String,
    },
    /// Select an explicit common ancestor or the unrelated empty baseline.
    Base {
        session: String,
        operation_id: String,
        expected_generation: u64,
        /// BaseChoice JSON file, or - for standard input.
        #[arg(default_value = "-")]
        input: PathBuf,
    },
    /// Resolve one durable conflict.
    Resolve {
        session: String,
        operation_id: String,
        expected_generation: u64,
        conflict_id: String,
        /// Resolution JSON file, or - for standard input.
        #[arg(default_value = "-")]
        input: PathBuf,
    },
    /// Publish a fully resolved merge revision.
    Complete {
        session: String,
        operation_id: String,
        expected_generation: u64,
    },
    /// Abort a nonterminal merge session.
    Abort {
        session: String,
        operation_id: String,
        expected_generation: u64,
    },
    /// Resume a durable pending merge operation.
    Recover { session: String },
    /// Apply a completed merge to an existing local workspace.
    Apply {
        root: PathBuf,
        session: String,
        merge_generation: u64,
        operation_id: String,
        expected_workspace_generation: u64,
    },
}

pub(crate) fn telemetry_action(command: &MergeCommand) -> bool {
    matches!(
        command,
        MergeCommand::Begin { .. }
            | MergeCommand::Base { .. }
            | MergeCommand::Resolve { .. }
            | MergeCommand::Complete { .. }
            | MergeCommand::Abort { .. }
            | MergeCommand::Recover { .. }
            | MergeCommand::Apply { .. }
    )
}

pub fn merge(data_dir: &Path, command: MergeCommand) -> Result<Value> {
    let data = existing_data_directory(data_dir)?;
    let authority = Authority::open(&data)?;
    let repository = authority.repository_id()?;
    let engine = Engine::open(&data)?;
    let mut journal = MergeJournal::open(&data, &repository)?;

    match command {
        MergeCommand::Begin {
            operation_id,
            left,
            right,
        } => Ok(json!({
            "ok": true,
            "outcome": journal.begin(&engine, &operation_id, &left, &right)?,
        })),
        MergeCommand::Operation { operation } => Ok(json!({
            "ok": true,
            "session": journal.operation_session(&operation)?,
        })),
        MergeCommand::Show { session } => Ok(json!({
            "ok": true,
            "state": journal.inspect(&session)?,
        })),
        MergeCommand::Bases {
            session,
            cursor,
            limit,
        } => {
            require_page_limit(limit, "merge bases")?;
            let cursor: Option<String> = cursor
                .as_deref()
                .map(|path| read_json(path, MAX_CURSOR_BYTES))
                .transpose()?;
            Ok(json!({
                "ok": true,
                "page": journal.bases(&session, cursor.as_deref(), limit)?,
            }))
        }
        MergeCommand::Conflicts {
            session,
            cursor,
            limit,
        } => {
            require_page_limit(limit, "merge conflicts")?;
            let cursor: Option<String> = cursor
                .as_deref()
                .map(|path| read_json(path, MAX_CURSOR_BYTES))
                .transpose()?;
            Ok(json!({
                "ok": true,
                "page": journal.conflicts(&session, cursor.as_deref(), limit)?,
            }))
        }
        MergeCommand::Conflict {
            session,
            conflict_id,
        } => Ok(json!({
            "ok": true,
            "conflict": journal.conflict(&session, &conflict_id)?,
        })),
        MergeCommand::Base {
            session,
            operation_id,
            expected_generation,
            input,
        } => {
            let choice: BaseChoice = read_json(&input, MAX_JSON_INPUT_BYTES)?;
            Ok(json!({
                "ok": true,
                "outcome": journal.select_base(
                    &engine,
                    &session,
                    &operation_id,
                    expected_generation,
                    choice,
                )?,
            }))
        }
        MergeCommand::Resolve {
            session,
            operation_id,
            expected_generation,
            conflict_id,
            input,
        } => {
            let resolution: Resolution = read_json(&input, MAX_JSON_INPUT_BYTES)?;
            Ok(json!({
                "ok": true,
                "outcome": journal.resolve(
                    &engine,
                    &session,
                    &operation_id,
                    expected_generation,
                    &conflict_id,
                    resolution,
                )?,
            }))
        }
        MergeCommand::Complete {
            session,
            operation_id,
            expected_generation,
        } => Ok(json!({
            "ok": true,
            "outcome": journal.complete(
                &engine,
                &session,
                &operation_id,
                expected_generation,
            )?,
        })),
        MergeCommand::Abort {
            session,
            operation_id,
            expected_generation,
        } => Ok(json!({
            "ok": true,
            "outcome": journal.abort(&session, &operation_id, expected_generation)?,
        })),
        MergeCommand::Recover { session } => Ok(json!({
            "ok": true,
            "outcome": journal.recover(&engine, &session)?,
        })),
        MergeCommand::Apply {
            root,
            session,
            merge_generation,
            operation_id,
            expected_workspace_generation,
        } => {
            let root = separate_existing_workspace(&data, &root)?;
            let mut workspace = Workspace::open(&root)?;
            if workspace.state()?.change.repository != repository {
                return Err(Error::Conflict(
                    "workspace belongs to another repository".into(),
                ));
            }
            let completion = journal.completion(&session, merge_generation)?;
            Ok(json!({
                "ok": true,
                "outcome": workspace.apply(
                    &engine,
                    &operation_id,
                    expected_workspace_generation,
                    &completion,
                )?,
            }))
        }
    }
}

fn existing_data_directory(path: &Path) -> Result<PathBuf> {
    let data = path.canonicalize()?;
    if !data.join("state.sqlite").is_file() {
        return Err(Error::Invalid(
            "merge commands require an initialized repository data directory".into(),
        ));
    }
    Ok(data)
}

fn separate_existing_workspace(data: &Path, root: &Path) -> Result<PathBuf> {
    let root = root.canonicalize()?;
    if data.starts_with(&root) || root.starts_with(data) {
        return Err(Error::Invalid(
            "repository data directory and workspace must be separate, non-nested directories"
                .into(),
        ));
    }
    Ok(root)
}

fn require_page_limit(limit: usize, operation: &str) -> Result<()> {
    if !(1..=MAX_PAGE).contains(&limit) {
        return Err(Error::Limit(format!(
            "{operation} limit must be between 1 and {MAX_PAGE}"
        )));
    }
    Ok(())
}

fn read_json<T: DeserializeOwned>(path: &Path, maximum: u64) -> Result<T> {
    let mut reader: Box<dyn Read> = if path == Path::new("-") {
        Box::new(io::stdin().lock())
    } else {
        Box::new(File::open(path)?)
    };
    let mut bytes = Vec::new();
    reader.by_ref().take(maximum + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > maximum {
        return Err(Error::Limit(format!(
            "merge JSON input exceeds {maximum} bytes"
        )));
    }
    Ok(serde_json::from_slice(&bytes)?)
}
