//! Operator-local walkthrough authoring and shared read projections.

use std::path::PathBuf;

use agentscm::repo_explorer::evidence::EvidenceExplorer;
use agentscm::repo_explorer::git::GitExplorer;
use agentscm::repo_explorer::walkthrough::{Store, read_draft};
use agentscm::{Error, Result};
use clap::{Args, Subcommand};
use serde_json::Value;

#[derive(Debug, Args)]
pub struct Checkout {
    /// Existing Git checkout. Walkthroughs are stored outside its source tree.
    #[arg(long)]
    root: PathBuf,
    /// Logical repository namespace used by Repository explorer.
    #[arg(long)]
    repository: String,
}

#[derive(Debug, Args)]
pub struct Read {
    #[command(flatten)]
    checkout: Checkout,
    /// Immutable walkthrough SHA-256 identity.
    #[arg(long)]
    id: String,
    /// Exact selected commit to compare against the walkthrough head.
    #[arg(long)]
    current_head: Option<String>,
    /// Optional operator-selected retained capture spool; no bodies are read.
    #[arg(long)]
    spool: Option<PathBuf>,
}

#[derive(Debug, Subcommand)]
pub enum WalkthroughCommand {
    /// Emit a bounded deterministic source map without authoring or storage.
    Map {
        #[command(flatten)]
        checkout: Checkout,
        #[arg(long)]
        base: String,
        #[arg(long)]
        head: String,
        /// Exact selected source; omission leaves applicability unknown.
        #[arg(long)]
        current_head: Option<String>,
    },
    /// Bind an authored draft (or empty manifest) to exact committed source.
    Create {
        #[command(flatten)]
        checkout: Checkout,
        #[arg(long)]
        base: String,
        #[arg(long)]
        head: String,
        #[arg(long)]
        change_id: String,
        /// Bounded UTF-8 JSON draft. Omit for an observed manifest without prose.
        #[arg(long)]
        draft: Option<PathBuf>,
        /// Required current walkthrough digest for amending an existing change.
        #[arg(long)]
        expected_parent: Option<String>,
    },
    /// List immutable versions, newest first, with explicit omission counts.
    List {
        #[command(flatten)]
        checkout: Checkout,
        #[arg(long, default_value_t = 100)]
        limit: usize,
    },
    /// Read an immutable walkthrough and currently available retained action facts.
    Show(Read),
    /// Export a bounded agent packet with source handles and explicit omissions.
    Context(Read),
    /// Retrieve exact source lines for one authored anchor.
    Source {
        #[command(flatten)]
        checkout: Checkout,
        #[arg(long)]
        id: String,
        #[arg(long)]
        anchor: String,
    },
}

pub fn run(command: WalkthroughCommand) -> Result<Value> {
    match command {
        WalkthroughCommand::Map {
            checkout,
            base,
            head,
            current_head,
        } => {
            let root = std::fs::canonicalize(&checkout.root)?;
            let git = GitExplorer::open(&root, &checkout.repository)?;
            agentscm::repo_explorer::walkthrough_map::map(
                &git,
                &base,
                &head,
                current_head.as_deref(),
            )
        }
        WalkthroughCommand::Create {
            checkout,
            base,
            head,
            change_id,
            draft,
            expected_parent,
        } => {
            let (git, store) = open(&checkout)?;
            let supplied_draft = draft.is_some();
            let mut draft = draft
                .as_deref()
                .map(read_draft)
                .transpose()?
                .unwrap_or_default();
            if draft.author.is_empty() {
                draft.author = "unspecified".into();
            }
            if draft.title.is_empty() {
                draft.title = change_id.clone();
            }
            if !supplied_draft && draft.steps.is_empty() && draft.purpose.is_empty() {
                draft.open_questions.push("No behavioral explanation has been authored; inspect the changed-file manifest and source.".into());
            }
            serde_json::to_value(store.create(
                &git,
                &change_id,
                &base,
                &head,
                draft,
                expected_parent.as_deref(),
            )?)
            .map_err(Into::into)
        }
        WalkthroughCommand::List { checkout, limit } => {
            let (_, store) = open(&checkout)?;
            serde_json::to_value(store.list(limit)?).map_err(Into::into)
        }
        WalkthroughCommand::Show(read) => {
            let (git, store) = open(&read.checkout)?;
            let evidence = evidence(&read)?;
            serde_json::to_value(store.show(
                &git,
                &read.id,
                read.current_head.as_deref(),
                evidence.as_ref(),
            )?)
            .map_err(Into::into)
        }
        WalkthroughCommand::Context(read) => {
            let (git, store) = open(&read.checkout)?;
            let evidence = evidence(&read)?;
            store.context(
                &git,
                &read.id,
                read.current_head.as_deref(),
                evidence.as_ref(),
            )
        }
        WalkthroughCommand::Source {
            checkout,
            id,
            anchor,
        } => {
            let (git, store) = open(&checkout)?;
            serde_json::to_value(store.source(&git, &id, &anchor)?).map_err(Into::into)
        }
    }
}

fn open(checkout: &Checkout) -> Result<(GitExplorer, Store)> {
    let root = std::fs::canonicalize(&checkout.root)?;
    let git = GitExplorer::open(&root, &checkout.repository)?;
    let store = Store::for_checkout(&git)?;
    Ok((git, store))
}

fn evidence(read: &Read) -> Result<Option<EvidenceExplorer>> {
    read.spool
        .as_ref()
        .map(|path| {
            if !path.is_absolute() {
                return Err(Error::Invalid(
                    "capture spool must be an absolute operator-selected path".into(),
                ));
            }
            EvidenceExplorer::open(path, read.checkout.repository.clone())
        })
        .transpose()
}
