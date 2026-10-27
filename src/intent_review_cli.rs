//! JSON transport for the trusted-local intent review adapter.

use std::io::{self, Read};
use std::path::PathBuf;

use agentscm::intent_review_host::{IntentReviewHost, IntentReviewRequest, MAX_REQUEST_BYTES};
use agentscm::{Error, Result};
use clap::Args;
use serde_json::Value;

#[derive(Debug, Args)]
pub struct IntentReviewArgs {
    /// Git checkout to capture. The repository identity comes from its origin.
    #[arg(long, default_value = ".")]
    root: PathBuf,
    /// Permit explicit human operations using cited caller-supplied authorization excerpts.
    /// This trusted-local capability is not independent proof of a human identity.
    #[arg(long)]
    human_authority: bool,
    /// Local intent and workflow-library store; defaults to the store used by agentscm open.
    #[arg(long)]
    store: Option<PathBuf>,
}

pub fn run(args: IntentReviewArgs) -> Result<Value> {
    let data_dir = args
        .store
        .unwrap_or(crate::local_service::dir()?.join("store"));
    let root = crate::intent_cli::repo_root(&args.root)
        .ok_or_else(|| Error::Invalid("intent review requires a Git worktree".into()))?;
    let (forge, repository) = crate::client_config::origin_identity(&root)?;
    let repository = format!("{forge}/{repository}").to_ascii_lowercase();
    let mut bytes = Vec::new();
    io::stdin()
        .take(MAX_REQUEST_BYTES as u64 + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() > MAX_REQUEST_BYTES {
        return Err(Error::Limit("intent request exceeds 256 KiB".into()));
    }
    let request: IntentReviewRequest = serde_json::from_slice(&bytes)?;
    let host = IntentReviewHost::new(root, data_dir, repository)?;
    let host = if args.human_authority {
        host.with_human_authority()
    } else {
        host
    };
    host.dispatch(request)
}
