//! Agent-facing shared work commands. The hub remains the source of truth.
use std::fs::File;
use std::io::{self, Read};
use std::path::PathBuf;
use std::time::Duration;

use agentscm::hub::catchup::{CatchupAck, CatchupPublicationRepair, CatchupQuery};
use agentscm::hub::coordination::{
    CoordinationAck, CoordinationReply, CoordinationResolve, CoordinationWithdraw,
};
use agentscm::hub::insights::UsageRecord;
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
    /// Inspect exact Work subjects, scope overlaps and the merge watermark without claiming work.
    Preflight {
        /// Strict WorkStart JSON, including any source revision and checked merge identities.
        #[arg(long, default_value = "-")]
        request: PathBuf,
    },
    /// Read a fixed return-to-work delivery without marking it reviewed.
    Catchup {
        /// Strict CatchupQuery JSON for a subscriber or page continuation; - reads stdin.
        #[arg(long)]
        request: Option<PathBuf>,
    },
    /// Explicitly mark a fully delivered catch-up window reviewed.
    CatchupAck {
        /// Strict CatchupAck JSON using the exact delivered identity and starting cursors.
        #[arg(long, default_value = "-")]
        request: PathBuf,
    },
    /// Submit one caller-reported usage observation for durable work.
    Usage {
        /// Strict UsageRecord JSON file, or - for standard input.
        #[arg(long, default_value = "-")]
        request: PathBuf,
    },
    /// Read the conservative insight projection for one work item.
    Insight { id: String },
    /// List conservative work insight projections in a bounded time window.
    Insights {
        #[arg(long)]
        since: Option<i64>,
        #[arg(long)]
        until: Option<i64>,
        #[arg(long)]
        after: Option<String>,
        #[arg(long)]
        through: Option<i64>,
        #[arg(long, default_value_t = 20)]
        limit: usize,
    },
    /// Read durable requests addressed to an agent session; does not acknowledge receipt.
    Inbox {
        #[arg(long)]
        session: String,
        #[arg(long)]
        after: Option<i64>,
        #[arg(long)]
        through: Option<i64>,
        #[arg(long, default_value_t = 100)]
        limit: usize,
        #[arg(long, default_value_t = 0)]
        wait_seconds: u64,
    },
    /// Acknowledge receipt of a specific coordination request version.
    CoordinationAck {
        #[arg(long, default_value = "-")]
        request: PathBuf,
    },
    /// Report a split, join, handover or decline as the affected lead.
    CoordinationReply {
        #[arg(long, default_value = "-")]
        request: PathBuf,
    },
    /// Validate and record resolution after work scope or participation changes.
    CoordinationResolve {
        #[arg(long, default_value = "-")]
        request: PathBuf,
    },
    /// Withdraw your proposal, or clean up an unavailable requester as a current affected lead.
    CoordinationWithdraw {
        #[arg(long, default_value = "-")]
        request: PathBuf,
    },
    /// Read ongoing assignments, recent outcomes and scope overlaps without acknowledging.
    Overview {
        /// Include reported outcomes since this Unix timestamp; defaults to the last 24 hours.
        #[arg(long)]
        recent_since: Option<i64>,
        /// Maximum records per section. Inspect completeness flags in the response.
        #[arg(long, default_value_t = 100)]
        limit: usize,
    },
    /// Read unfinished shared work and changes since the last acknowledged delivery.
    Briefing {
        /// Independent subscriber/session identity. Repeat it with each continuation.
        #[arg(long)]
        subscriber: Option<String>,
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
    /// Repair Work after a verified repository rename; local operator database access required.
    RepairRepository {
        #[arg(
            long,
            conflicts_with = "database_env",
            required_unless_present = "database_env"
        )]
        database: Option<String>,
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
        /// Previous owner/name namespace.
        #[arg(long)]
        repository: String,
        #[arg(long)]
        repository_id: i64,
        /// Current connected owner/name namespace.
        #[arg(long)]
        destination: String,
        /// Apply atomically; omission previews all changes and collision checks then rolls back.
        #[arg(long)]
        apply: bool,
    },
    /// Preview or apply guarded repair of one retained merged-publication projection failure.
    RepairMerge {
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
        /// Exact canonical owner/name repository retained on the publication.
        #[arg(long)]
        repository: String,
        /// Exact retained publication id.
        #[arg(long)]
        publication: String,
        /// Replacement merge commit: 40 or 64 lowercase hexadecimal characters.
        #[arg(long)]
        merge_commit: String,
        /// Apply the guarded repair. Omit for a read-only proposal preview.
        #[arg(long)]
        apply: bool,
    },
}

pub(crate) fn telemetry_action(command: &WorkCommand) -> bool {
    match command {
        WorkCommand::Inbox { .. }
        | WorkCommand::Overview { .. }
        | WorkCommand::Briefing { .. }
        | WorkCommand::Catchup { .. }
        | WorkCommand::Preflight { .. }
        | WorkCommand::Insight { .. }
        | WorkCommand::Insights { .. } => false,
        WorkCommand::Compact { apply, .. }
        | WorkCommand::RepairMerge { apply, .. }
        | WorkCommand::RepairRepository { apply, .. } => *apply,
        WorkCommand::CoordinationAck { .. }
        | WorkCommand::CoordinationReply { .. }
        | WorkCommand::CoordinationResolve { .. }
        | WorkCommand::CoordinationWithdraw { .. }
        | WorkCommand::Start { .. }
        | WorkCommand::Usage { .. }
        | WorkCommand::Checkpoint { .. }
        | WorkCommand::Ack { .. } => true,
        WorkCommand::CatchupAck { .. } => true,
    }
}

pub async fn run(command: WorkCommand) -> Result<()> {
    let command = match command {
        WorkCommand::RepairRepository {
            database,
            database_env,
            provider,
            host,
            repository,
            repository_id,
            destination,
            apply,
        } => {
            let database = database_url(database, database_env)?;
            let store = store::open(&database).map_err(map_hub_error)?;
            let result = store
                .work_repair_repository(
                    &agentscm::hub::work::WorkRepositoryRename {
                        scope: WorkRepositoryScope {
                            provider,
                            host,
                            repository,
                            repository_id: Some(repository_id),
                        },
                        destination,
                        apply,
                    },
                    unix_now(),
                )
                .map_err(map_hub_error)?;
            println!("{}", serde_json::to_value(result)?);
            return Ok(());
        }
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
        WorkCommand::RepairMerge {
            database,
            database_env,
            repository,
            publication,
            merge_commit,
            apply,
        } => {
            let database = database_url(database, database_env)?;
            let store = store::open(&database).map_err(map_hub_error)?;
            let result = store
                .repair_catchup_publication_merge(
                    &CatchupPublicationRepair {
                        repository,
                        publication,
                        merge_commit,
                        apply,
                    },
                    unix_now(),
                )
                .map_err(map_hub_error)?;
            println!("{}", serde_json::to_value(result)?);
            return Ok(());
        }
        command => command,
    };

    let timeout = match &command {
        WorkCommand::Inbox { wait_seconds, .. } => {
            LIMIT + Duration::from_secs((*wait_seconds).min(25))
        }
        _ => LIMIT,
    };
    let hub = Hub::connect(timeout)?;
    let (reply, claim_conflict) = match command {
        WorkCommand::Usage { request } => {
            let request: UsageRecord = read_request(&request)?;
            (
                hub.call(
                    Method::POST,
                    "/v1/hub/standup/usage",
                    Some(serde_json::to_value(request)?),
                )
                .await?,
                false,
            )
        }
        WorkCommand::Insight { id } => {
            let mut url = reqwest::Url::parse("http://hub/v1/hub/standup/work")
                .map_err(|_| invalid("cannot construct insight URL"))?;
            url.path_segments_mut()
                .map_err(|_| invalid("cannot construct insight URL"))?
                .push(&id)
                .push("insights");
            let path = url
                .as_str()
                .strip_prefix("http://hub")
                .ok_or_else(|| invalid("cannot construct insight URL"))?
                .to_owned();
            (hub.call(Method::GET, &path, None).await?, false)
        }
        WorkCommand::Insights {
            since,
            until,
            after,
            through,
            limit,
        } => {
            let mut url = reqwest::Url::parse("http://hub/v1/hub/standup/insights")
                .map_err(|_| invalid("cannot construct insights URL"))?;
            {
                let mut query = url.query_pairs_mut();
                query.append_pair("limit", &limit.to_string());
                if let Some(value) = since {
                    query.append_pair("since", &value.to_string());
                }
                if let Some(value) = until {
                    query.append_pair("until", &value.to_string());
                }
                if let Some(value) = after {
                    query.append_pair("after", &value);
                }
                if let Some(value) = through {
                    query.append_pair("through", &value.to_string());
                }
            }
            let path = url
                .as_str()
                .strip_prefix("http://hub")
                .ok_or_else(|| invalid("cannot construct insights URL"))?
                .to_owned();
            (hub.call(Method::GET, &path, None).await?, false)
        }
        WorkCommand::Inbox {
            session,
            after,
            through,
            limit,
            wait_seconds,
        } => {
            let mut url = reqwest::Url::parse("http://hub/v1/hub/standup/inbox")
                .map_err(|_| invalid("cannot construct inbox URL"))?;
            {
                let mut params = url.query_pairs_mut();
                params
                    .append_pair("session", &session)
                    .append_pair("limit", &limit.to_string())
                    .append_pair("wait_seconds", &wait_seconds.to_string());
                if let Some(after) = after {
                    params.append_pair("after", &after.to_string());
                }
                if let Some(through) = through {
                    params.append_pair("through", &through.to_string());
                }
            }
            let path = url
                .as_str()
                .strip_prefix("http://hub")
                .ok_or_else(|| invalid("cannot construct inbox URL"))?;
            (hub.call(Method::GET, path, None).await?, false)
        }
        WorkCommand::CoordinationAck { request } => {
            let request: CoordinationAck = read_request(&request)?;
            (
                hub.call(
                    Method::POST,
                    "/v1/hub/standup/coordination/ack",
                    Some(serde_json::to_value(request)?),
                )
                .await?,
                false,
            )
        }
        WorkCommand::CoordinationReply { request } => {
            let request: CoordinationReply = read_request(&request)?;
            (
                hub.call(
                    Method::POST,
                    "/v1/hub/standup/coordination/reply",
                    Some(serde_json::to_value(request)?),
                )
                .await?,
                false,
            )
        }
        WorkCommand::CoordinationResolve { request } => {
            let request: CoordinationResolve = read_request(&request)?;
            (
                hub.call(
                    Method::POST,
                    "/v1/hub/standup/coordination/resolve",
                    Some(serde_json::to_value(request)?),
                )
                .await?,
                false,
            )
        }
        WorkCommand::CoordinationWithdraw { request } => {
            let request: CoordinationWithdraw = read_request(&request)?;
            (
                hub.call(
                    Method::POST,
                    "/v1/hub/standup/coordination/withdraw",
                    Some(serde_json::to_value(request)?),
                )
                .await?,
                false,
            )
        }
        WorkCommand::Preflight { request } => {
            let request: WorkStart = read_request(&request)?;
            (
                hub.call(
                    Method::POST,
                    "/v1/hub/standup/preflight",
                    Some(json!(request)),
                )
                .await?,
                false,
            )
        }
        WorkCommand::Catchup { request } => {
            let query: CatchupQuery = match request {
                Some(path) => read_request(&path)?,
                None => CatchupQuery::default(),
            };
            let path = query_path("/v1/hub/standup/catchup", serde_json::to_value(query)?)?;
            (hub.call(Method::GET, &path, None).await?, false)
        }
        WorkCommand::CatchupAck { request } => {
            let request: CatchupAck = read_request(&request)?;
            (
                hub.call(
                    Method::POST,
                    "/v1/hub/standup/catchup/ack",
                    Some(json!(request)),
                )
                .await?,
                false,
            )
        }
        WorkCommand::Overview {
            recent_since,
            limit,
        } => {
            let mut query = format!("/v1/hub/standup/overview?limit={limit}");
            if let Some(since) = recent_since {
                query.push_str(&format!("&recent_since={since}"));
            }
            (hub.call(Method::GET, &query, None).await?, false)
        }
        WorkCommand::Briefing {
            subscriber,
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
                if let Some(subscriber) = subscriber {
                    query.append_pair("subscriber", &subscriber);
                }
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
            let result = serde_json::from_value::<WorkStartResult>(reply.clone())?;
            let claim_conflict = result.conflict.is_some()
                || result.coordination_request.is_some()
                || matches!(
                    result.item.state,
                    agentscm::hub::work::WorkState::Paused
                        | agentscm::hub::work::WorkState::Finished
                        | agentscm::hub::work::WorkState::Handover
                );
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
        WorkCommand::Compact { .. }
        | WorkCommand::RepairMerge { .. }
        | WorkCommand::RepairRepository { .. } => {
            unreachable!("local maintenance returned before hub connection")
        }
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

fn query_path(path: &str, query: serde_json::Value) -> Result<String> {
    let mut url = reqwest::Url::parse(&format!("http://hub{path}"))
        .map_err(|_| invalid("cannot construct work query"))?;
    let fields = query
        .as_object()
        .ok_or_else(|| invalid("invalid work query"))?;
    {
        let mut pairs = url.query_pairs_mut();
        for (key, value) in fields {
            match value {
                serde_json::Value::Null => {}
                serde_json::Value::String(value) => {
                    pairs.append_pair(key, value);
                }
                serde_json::Value::Number(value) => {
                    pairs.append_pair(key, &value.to_string());
                }
                _ => return Err(invalid("invalid work query field")),
            }
        }
    }
    url.as_str()
        .strip_prefix("http://hub")
        .map(str::to_owned)
        .ok_or_else(|| invalid("cannot construct work query"))
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
