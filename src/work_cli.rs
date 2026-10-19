//! Agent-facing shared work commands. The hub remains the source of truth.
use std::fs::File;
use std::io::{self, Read};
use std::path::PathBuf;
use std::time::Duration;

use agentscm::hub::store::{self, unix_now};
use agentscm::hub::work::{
    BriefingAck, WorkCheckpoint, WorkRepositoryScope, WorkStart, WorkStartResult,
};
use agentscm::{Error, Result};
use clap::Subcommand;
use reqwest::Method;
use serde::de::DeserializeOwned;
use serde_json::json;

use crate::hub_cli::map_hub_error;
use crate::intent_cli::Hub;

const LIMIT: Duration = Duration::from_secs(10);
const REQUEST_BYTES: u64 = 1024 * 1024;

#[derive(Debug, Subcommand)]
pub enum WorkCommand {
    /// Read unfinished shared work and changes since the last acknowledged delivery.
    Briefing {
        /// Retry or acknowledge one durable writer delivery.
        #[arg(long)]
        delivery: Option<String>,
        /// Event cursor returned by a previous briefing page.
        #[arg(long)]
        after: Option<i64>,
        /// Work-item cursor returned by a previous briefing page.
        #[arg(long)]
        work_after: Option<String>,
        /// Keep true after next_work_done becomes true while paging remaining events.
        #[arg(long)]
        work_done: bool,
        /// Fixed upper event sequence returned by the first page.
        #[arg(long)]
        through: Option<i64>,
        /// Maximum items and events on this page.
        #[arg(long, default_value_t = 20)]
        limit: usize,
    },
    /// Resolve or create durable work and claim the requested role.
    Start {
        /// Strict WorkStart JSON file, or - for standard input.
        #[arg(long, default_value = "-")]
        request: PathBuf,
    },
    /// Record work and print its committed-operation receipt plus the current item.
    Checkpoint {
        /// Strict WorkCheckpoint JSON; new operations require expected_updated_seq. Use - for stdin.
        #[arg(long, default_value = "-")]
        request: PathBuf,
    },
    /// Acknowledge a completely delivered writer briefing.
    Ack { delivery: String },
    /// Preview or apply safe pruning of old checkpoint receipts and versions.
    Compact {
        /// Explicit SQLite or passwordless PostgreSQL URL. Use --database-env for secret URLs.
        #[arg(
            long,
            conflicts_with = "database_env",
            required_unless_present = "database_env"
        )]
        database: Option<String>,
        /// Name of an environment variable containing the database URL.
        #[arg(
            long,
            conflicts_with = "database",
            required_unless_present = "database"
        )]
        database_env: Option<String>,
        #[arg(long)]
        provider: String,
        #[arg(long)]
        host: String,
        /// Canonical owner/name repository.
        #[arg(long)]
        repository: String,
        /// Stable forge repository id, when the scope uses one.
        #[arg(long)]
        repository_id: Option<i64>,
        /// Delete eligible history strictly before this Unix timestamp.
        #[arg(long)]
        before: i64,
        /// Apply the reported deletions. Omit for a read-only preview.
        #[arg(long)]
        apply: bool,
    },
}

pub async fn run(command: WorkCommand) -> Result<()> {
    let command = match command {
        WorkCommand::Compact {
            database,
            database_env,
            provider,
            host,
            repository,
            repository_id,
            before,
            apply,
        } => {
            let database = database_url(database, database_env)?;
            let store = store::open(&database).map_err(map_hub_error)?;
            let scope = WorkRepositoryScope {
                provider,
                host,
                repository,
                repository_id,
            };
            let result = store
                .work_compact(&scope, before, apply, unix_now())
                .map_err(map_hub_error)?;
            println!("{}", serde_json::to_value(result)?);
            return Ok(());
        }
        command => command,
    };

    let hub = Hub::connect(LIMIT)?;
    let (reply, claim_conflict) = match command {
        WorkCommand::Briefing {
            delivery,
            after,
            work_after,
            work_done,
            through,
            limit,
        } => {
            let mut url = reqwest::Url::parse("http://hub/v1/hub/standup/briefing")
                .map_err(|_| invalid("cannot construct briefing URL"))?;
            {
                let mut query = url.query_pairs_mut();
                if let Some(delivery) = delivery {
                    query.append_pair("delivery", &delivery);
                }
                if let Some(after) = after {
                    query.append_pair("after", &after.to_string());
                }
                if let Some(work_after) = work_after {
                    query.append_pair("work_after", &work_after);
                }
                if work_done {
                    query.append_pair("work_done", "true");
                }
                if let Some(through) = through {
                    query.append_pair("through", &through.to_string());
                }
                query.append_pair("limit", &limit.to_string());
            }
            let path = url
                .as_str()
                .strip_prefix("http://hub")
                .ok_or_else(|| invalid("cannot construct briefing URL"))?
                .to_owned();
            (hub.call(Method::GET, &path, None).await?, false)
        }
        WorkCommand::Start { request } => {
            let request: WorkStart = read_request(&request)?;
            let reply = hub
                .call(
                    Method::POST,
                    "/v1/hub/standup/start",
                    Some(serde_json::to_value(request)?),
                )
                .await?;
            let claim_conflict = serde_json::from_value::<WorkStartResult>(reply.clone())?
                .conflict
                .is_some();
            (reply, claim_conflict)
        }
        WorkCommand::Checkpoint { request } => {
            let request: WorkCheckpoint = read_request(&request)?;
            (
                hub.call(
                    Method::POST,
                    "/v1/hub/standup/checkpoint",
                    Some(serde_json::to_value(request)?),
                )
                .await?,
                false,
            )
        }
        WorkCommand::Ack { delivery } => {
            let request = BriefingAck { delivery };
            (
                hub.call(Method::POST, "/v1/hub/standup/ack", Some(json!(request)))
                    .await?,
                false,
            )
        }
        WorkCommand::Compact { .. } => unreachable!("compaction returned before hub connection"),
    };
    println!("{reply}");
    if claim_conflict {
        return Err(Error::Conflict(
            "durable work claim was not acquired; inspect the JSON result".into(),
        ));
    }
    Ok(())
}

fn read_request<T: DeserializeOwned>(path: &PathBuf) -> Result<T> {
    let mut text = String::new();
    if path.as_os_str() == "-" {
        io::stdin()
            .take(REQUEST_BYTES + 1)
            .read_to_string(&mut text)?;
    } else {
        File::open(path)?
            .take(REQUEST_BYTES + 1)
            .read_to_string(&mut text)?;
    }
    if text.len() as u64 > REQUEST_BYTES {
        return Err(invalid("work request exceeds 1 MiB"));
    }
    serde_json::from_str(&text).map_err(|_| invalid("work request is not valid strict JSON"))
}

fn invalid(message: &str) -> Error {
    Error::Invalid(message.to_owned())
}

fn database_url(database: Option<String>, database_env: Option<String>) -> Result<String> {
    match (database, database_env) {
        (Some(database), None) => Ok(database),
        (None, Some(name)) => {
            if name.is_empty() || name.contains('=') || name.contains('\0') {
                return Err(invalid("database environment variable name is invalid"));
            }
            let value = std::env::var_os(&name)
                .ok_or_else(|| invalid("database environment variable is not set"))?;
            value
                .into_string()
                .map_err(|_| invalid("database environment variable is not valid Unicode"))
        }
        _ => Err(invalid(
            "exactly one of --database or --database-env is required",
        )),
    }
}
