//! Local-only HTTP adapter. Directories and credentials are fixed at startup.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;

use agentscm::repo_explorer::{Explorer, evidence::EvidenceExplorer, git::GitExplorer};
use agentscm::{Error, Result};
use axum::Router;
use axum::extract::{Path, RawQuery, State};
use axum::http::StatusCode;
use axum::response::Response;
use axum::routing::get;
use serde_json::Value;

use super::{
    ApiError, AppState, api_response, blocking, guard_response, named_fields, optional_number,
    read_error,
};

const MAX_RESPONSE_BYTES: usize = 4 * 1024 * 1024;

pub(super) fn open(
    root: Option<PathBuf>,
    repository: Option<String>,
    spool: Option<PathBuf>,
) -> Result<Option<Arc<Explorer>>> {
    match (root, repository, spool) {
        (None, None, None) => Ok(None),
        (Some(root), Some(repository), spool) => {
            if !root.is_absolute() || spool.as_ref().is_some_and(|path| !path.is_absolute()) {
                return Err(Error::Invalid(
                    "explorer paths must be absolute operator-selected directories".into(),
                ));
            }
            let git = GitExplorer::open(&root, &repository)?;
            let evidence = spool
                .map(|path| EvidenceExplorer::open(path, repository))
                .transpose()?;
            Ok(Some(Arc::new(Explorer { git, evidence })))
        }
        _ => Err(Error::Invalid(
            "--explorer-repo and --explorer-repository must be supplied together".into(),
        )),
    }
}

pub(super) fn routes() -> Router<AppState> {
    Router::new().route("/v1/local/explorer/{operation}", get(read))
}

#[derive(Clone, Copy)]
enum Operation {
    Summary,
    Tree,
    File,
    History,
    Diff,
    Search,
    Evidence,
    Sessions,
    Conversation,
}

impl Operation {
    fn parse(value: &str) -> Option<Self> {
        Some(match value {
            "summary" => Self::Summary,
            "tree" => Self::Tree,
            "file" => Self::File,
            "history" => Self::History,
            "diff" => Self::Diff,
            "search" => Self::Search,
            "evidence" => Self::Evidence,
            "sessions" => Self::Sessions,
            "conversation" => Self::Conversation,
            _ => return None,
        })
    }

    fn fields(self) -> &'static [&'static str] {
        match self {
            Self::Summary => &["ref"],
            Self::Tree => &["commit", "path", "after", "limit"],
            Self::File => &["commit", "path"],
            Self::History => &["commit", "path", "cursor", "limit"],
            Self::Diff => &["left", "right", "path", "cursor", "limit"],
            Self::Search => &["commit", "q", "cursor", "limit"],
            Self::Evidence => &["commit", "path", "work_id", "cursor", "limit"],
            Self::Sessions => &["cursor", "limit"],
            Self::Conversation => &["work_id", "session_id", "cursor", "limit"],
        }
    }

    fn default_limit(self) -> usize {
        match self {
            Self::Tree | Self::Diff => 100,
            Self::Search => 50,
            _ => 25,
        }
    }
}

fn required<'a>(fields: &'a BTreeMap<String, String>, name: &str) -> Result<&'a str> {
    fields
        .get(name)
        .filter(|value| !value.is_empty())
        .map(String::as_str)
        .ok_or_else(|| Error::Invalid(format!("query requires {name}")))
}

async fn read(
    State(state): State<AppState>,
    Path(operation): Path<String>,
    RawQuery(raw): RawQuery,
) -> Response {
    let Some(operation) = Operation::parse(&operation) else {
        return failure(StatusCode::NOT_FOUND, "Unknown repository explorer read.");
    };
    let Some(explorer) = state.explorer.clone() else {
        return failure(
            StatusCode::NOT_FOUND,
            "Repository explorer is off. Configure --explorer-repo and --explorer-repository on this computer.",
        );
    };
    let fields = match named_fields(raw.as_deref(), operation.fields()) {
        Ok(fields) => fields,
        Err(error) => return read_error(error, "repository explorer"),
    };
    let limit = match optional_number::<usize>(&fields, "limit") {
        Ok(value) => value.unwrap_or(operation.default_limit()),
        Err(error) => return read_error(error, "repository explorer"),
    };
    if matches!(
        operation,
        Operation::Evidence | Operation::Sessions | Operation::Conversation
    ) && explorer.evidence.is_none()
    {
        return failure(
            StatusCode::NOT_FOUND,
            "Private capture is not connected. Configure --explorer-spool to read retained evidence on this computer.",
        );
    }
    let result = blocking(state, move |_| {
        let value = project(&explorer, operation, &fields, limit)?;
        let body = serde_json::to_string(&value)?;
        if body.len() > MAX_RESPONSE_BYTES {
            return Err(Error::Limit(
                "explorer response exceeds its byte limit".into(),
            ));
        }
        Ok(body)
    })
    .await;
    match result {
        Ok((body, permit)) => guard_response(
            api_response(StatusCode::OK, body, "application/json"),
            permit,
        ),
        Err(error) => {
            // Git and filesystem diagnostics can contain local paths or private
            // data. Their detailed text stays in the local service diagnostics.
            eprintln!("repository explorer read failed: {:?}", error.message);
            let message = match error.status {
                StatusCode::BAD_REQUEST => {
                    "The explorer request is invalid or exceeds a read limit. Check the selected source, path and page size."
                }
                StatusCode::NOT_FOUND => {
                    "The requested source or retained capture was not found on this computer."
                }
                StatusCode::FORBIDDEN => {
                    "The requested capture is outside the configured repository namespace."
                }
                StatusCode::CONFLICT => {
                    "The local source or capture changed during this read. Refresh and retry."
                }
                StatusCode::SERVICE_UNAVAILABLE => {
                    "The local service is busy or this read is unavailable. Retry after the current read finishes."
                }
                _ => {
                    "The repository explorer read failed. Inspect local service diagnostics before retrying."
                }
            };
            failure(error.status, message)
        }
    }
}

fn project(
    explorer: &Explorer,
    operation: Operation,
    fields: &BTreeMap<String, String>,
    limit: usize,
) -> Result<Value> {
    let optional = |name| fields.get(name).map(String::as_str);
    let value = match operation {
        Operation::Summary => serde_json::to_value(explorer.git.summary(optional("ref"))?)?,
        Operation::Tree => serde_json::to_value(explorer.git.tree(
            required(fields, "commit")?,
            optional("path"),
            optional("after"),
            limit,
        )?)?,
        Operation::File => serde_json::to_value(
            explorer
                .git
                .file(required(fields, "commit")?, required(fields, "path")?)?,
        )?,
        Operation::History => serde_json::to_value(explorer.git.history(
            required(fields, "commit")?,
            optional("path"),
            optional("cursor"),
            limit,
        )?)?,
        Operation::Diff => serde_json::to_value(explorer.git.diff(
            required(fields, "left")?,
            required(fields, "right")?,
            optional("path"),
            optional("cursor"),
            limit,
        )?)?,
        Operation::Search => serde_json::to_value(explorer.git.search(
            required(fields, "commit")?,
            required(fields, "q")?,
            optional("cursor"),
            limit,
        )?)?,
        Operation::Evidence => {
            let commit = required(fields, "commit")?;
            explorer.git.validate_commit(commit)?;
            let evidence = explorer.evidence.as_ref().ok_or(Error::SourceDenied)?;
            serde_json::to_value(evidence.evidence(
                commit,
                optional("path"),
                optional("work_id"),
                optional("cursor"),
                limit,
            )?)?
        }
        Operation::Sessions => {
            let evidence = explorer.evidence.as_ref().ok_or(Error::SourceDenied)?;
            serde_json::to_value(evidence.sessions(optional("cursor"), limit)?)?
        }
        Operation::Conversation => {
            let evidence = explorer.evidence.as_ref().ok_or(Error::SourceDenied)?;
            serde_json::to_value(evidence.conversation(
                required(fields, "work_id")?,
                required(fields, "session_id")?,
                optional("cursor"),
                limit,
            )?)?
        }
    };
    Ok(value)
}

fn failure(status: StatusCode, message: &str) -> Response {
    read_error(
        ApiError {
            status,
            message: message.into(),
        },
        "repository explorer",
    )
}
