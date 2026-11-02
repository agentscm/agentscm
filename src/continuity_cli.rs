//! Explicit local continuity preparation. No subcommand publishes source or forge state.
use std::path::PathBuf;

use agentscm::continuity::{ForgeIdentity, ForgeProvider, Journal};
use agentscm::{Error, Result};
use clap::{Args, Subcommand, ValueEnum};
use serde_json::json;

#[derive(Debug, Args)]
pub struct ContinuityArgs {
    /// Existing ordinary Git checkout.
    #[arg(long, default_value = ".")]
    repo: PathBuf,
    /// Private directory outside the checkout. Defaults to the user configuration directory.
    #[arg(long)]
    state_dir: Option<PathBuf>,
    #[command(subcommand)]
    command: ContinuityCommand,
}

#[derive(Clone, Debug, ValueEnum)]
pub enum Provider {
    Github,
    Gitlab,
}

#[derive(Debug, Subcommand)]
pub enum ContinuityCommand {
    /// Save bounded task context and local source identity without contacting a server.
    Start {
        #[arg(long)]
        id: String,
        #[arg(long)]
        base: String,
        #[arg(
            long,
            conflicts_with = "context_file",
            required_unless_present = "context_file"
        )]
        context: Option<String>,
        #[arg(long, conflicts_with = "context")]
        context_file: Option<PathBuf>,
        #[arg(long, value_enum)]
        provider: Provider,
        /// Defaults to github.com or gitlab.com for the selected provider.
        #[arg(long)]
        host: Option<String>,
        /// Owner/repository (GitHub) or group/project (GitLab).
        #[arg(long)]
        repository: String,
    },
    /// Inspect retained context, checks and immutable publication drafts.
    Status {
        #[arg(long)]
        id: String,
    },
    /// Run an explicit bounded local command once. Receipts are caller asserted, not CI or approval.
    Check {
        #[arg(long)]
        id: String,
        #[arg(long)]
        operation: String,
        #[arg(long, default_value_t = 300, value_parser = clap::value_parser!(u64).range(1..=300))]
        timeout_seconds: u64,
        #[arg(last = true, required = true)]
        argv: Vec<String>,
    },
    /// Retain an exact clean commit and PR/MR text for later normal reviewed publication.
    Draft {
        #[arg(long)]
        id: String,
        #[arg(long)]
        draft: String,
        #[arg(long)]
        source_branch: String,
        #[arg(long)]
        target_branch: String,
        #[arg(long)]
        title: String,
        #[arg(long, conflicts_with = "body_file")]
        body: Option<String>,
        #[arg(long, conflicts_with = "body")]
        body_file: Option<PathBuf>,
    },
    /// Mark a retained draft cancelled. Replacement requires a new draft ID.
    CancelDraft {
        #[arg(long)]
        id: String,
        #[arg(long)]
        draft: String,
    },
    /// Recheck local applicability. Remote freshness stays unknown unless --live succeeds.
    Revalidate {
        #[arg(long)]
        id: String,
        /// Explicitly read the connected Hub and forge. Performs no remote mutations.
        #[arg(long, requires = "draft")]
        live: bool,
        /// Exact saved draft to observe remotely; never substitutes the current HEAD.
        #[arg(long)]
        draft: Option<String>,
    },
    /// Delete one task's private context, completed evidence and drafts. Unknown attempts are retained.
    Delete {
        #[arg(long)]
        id: String,
        /// Explicitly discard unknown attempt evidence after inspecting status. Never retries it.
        #[arg(long)]
        discard_unknown: bool,
    },
}

pub async fn run(args: ContinuityArgs) -> Result<i32> {
    let root = crate::client_config::canonical_root(&args.repo)?;
    let state = match args.state_dir {
        Some(path) => path,
        None => crate::hub_cli::config_dir()?
            .join("agentscm")
            .join("continuity")
            .join(agentscm::checks::sha256(
                root.as_os_str().as_encoded_bytes(),
            )),
    };
    let mut journal = Journal::open(&state, &root)?;
    let mut exit_code = 0;
    let output = match args.command {
        ContinuityCommand::Start {
            id,
            base,
            context,
            context_file,
            provider,
            host,
            repository,
        } => {
            let context = read_text(context, context_file)?;
            let provider = match provider {
                Provider::Github => ForgeProvider::Github,
                Provider::Gitlab => ForgeProvider::Gitlab,
            };
            let host = host.unwrap_or_else(|| {
                match provider {
                    ForgeProvider::Github => "github.com",
                    ForgeProvider::Gitlab => "gitlab.com",
                }
                .into()
            });
            let forge = ForgeIdentity {
                provider,
                host,
                repository,
            };
            forge.validate()?;
            // A declared provider is explicit; generic/custom Git origins are not inferred.
            let origin = crate::client_config::origin_identity(&root)?;
            if origin != (forge.host.clone(), forge.repository.clone()) {
                return Err(Error::Invalid(
                    "declared forge does not match checkout origin".into(),
                ));
            }
            serde_json::to_value(journal.start(&id, &base, &context, forge)?)?
        }
        ContinuityCommand::Status { id } => serde_json::to_value(journal.status(&id)?)?,
        ContinuityCommand::Check {
            id,
            operation,
            timeout_seconds,
            argv,
        } => {
            let record = journal.check(&id, &operation, argv, timeout_seconds)?;
            if record.state != "finished"
                || record.exit_code != Some(0)
                || record.stop.as_deref() != Some("Exited")
            {
                exit_code = 1;
            }
            serde_json::to_value(record)?
        }
        ContinuityCommand::Draft {
            id,
            draft,
            source_branch,
            target_branch,
            title,
            body,
            body_file,
        } => serde_json::to_value(journal.draft(
            &id,
            &draft,
            &source_branch,
            &target_branch,
            &title,
            &read_text(body, body_file)?,
        )?)?,
        ContinuityCommand::CancelDraft { id, draft } => {
            journal.cancel_draft(&id, &draft)?;
            json!({"cancelled": draft})
        }
        ContinuityCommand::Delete {
            id,
            discard_unknown,
        } => {
            if discard_unknown {
                journal.delete_discarding_unknown(&id)?;
            } else {
                journal.delete(&id)?;
            }
            json!({"deleted": id})
        }
        ContinuityCommand::Revalidate { id, live, draft } => {
            let local = journal.revalidate(&id)?;
            if live {
                let status = journal.status(&id)?;
                let draft = status
                    .drafts
                    .iter()
                    .find(|d| Some(&d.id) == draft.as_ref() && !d.cancelled)
                    .ok_or_else(|| Error::NotFound("active publication draft".into()))?;
                if draft.forge != status.task.forge {
                    return Err(Error::Invalid(
                        "draft forge identity differs from task".into(),
                    ));
                }
                let provider = match draft.forge.provider {
                    ForgeProvider::Github => "github",
                    ForgeProvider::Gitlab => "gitlab",
                };
                match crate::continuity_remote::observe(
                    &root,
                    provider,
                    &draft.forge.host,
                    &draft.forge.repository,
                    &draft.source_branch,
                    &draft.target_branch,
                    &draft.commit,
                    &draft.base_tip,
                )
                .await
                {
                    Ok(observation) => {
                        json!({"local": local, "remote": observation, "publication_authorized": false})
                    }
                    Err(error) => {
                        json!({"local": local, "remote": {"freshness": "unknown", "reason": remote_failure_reason(&error), "blockers": ["live observation failed; remote state remains unverified"]}, "publication_authorized": false})
                    }
                }
            } else {
                serde_json::to_value(local)?
            }
        }
    };
    println!("{}", serde_json::to_string_pretty(&output)?);
    Ok(exit_code)
}

fn read_text(inline: Option<String>, path: Option<PathBuf>) -> Result<String> {
    use std::io::Read;
    let text = if let Some(path) = path {
        let mut bytes = Vec::new();
        std::fs::File::open(path)?
            .take(32 * 1024 + 1)
            .read_to_end(&mut bytes)?;
        String::from_utf8(bytes)
            .map_err(|_| Error::Invalid("continuity text must be UTF-8".into()))?
    } else {
        inline.unwrap_or_default()
    };
    if text.len() > 32 * 1024 || text.contains('\0') {
        return Err(Error::Limit(
            "continuity text exceeds 32 KiB or contains NUL".into(),
        ));
    }
    Ok(text)
}

// Only approved static categories cross the private/authenticated read boundary.
// Never echo an upstream response, filesystem path or credential error verbatim.
fn remote_failure_reason(error: &Error) -> &'static str {
    match error {
        Error::Invalid(message) => match message.as_str() {
            "continuity agent token expired or revoked" => "authentication_expired_or_revoked",
            "continuity forge authentication failed" => "forge_authentication_failed",
            "continuity forge access denied" => "forge_access_denied",
            "continuity forge rate limited" => "rate_limited",
            "continuity forge observation timed out" => "timed_out",
            "continuity repository access denied" => "access_denied",
            "continuity Hub rate limited" => "rate_limited",
            "continuity requires a saved checkout connection" => "saved_connection_required",
            "continuity checkout or origin identity changed"
            | "continuity origin changed during observation" => "local_identity_changed",
            "continuity Hub identity or response unverified" => "remote_identity_unverified",
            "continuity forge observation freshness unverified" => "remote_freshness_unverified",
            "continuity Hub observation unavailable" => "connection_unavailable",
            _ => "observation_unverified",
        },
        Error::Io(_) => "local_connection_or_token_unavailable",
        _ => "observation_unverified",
    }
}
