//! Agent-facing shared work commands. The hub remains the source of truth.
use std::fs::File;
use std::io::{self, Read};
use std::path::PathBuf;
use std::time::Duration;

use agentscm::hub::catchup::{CatchupAck, CatchupPublicationRepair, CatchupQuery};
use agentscm::hub::coordination::{
    CoordinationAck, CoordinationReply, CoordinationResolve, CoordinationWithdraw,
};
use agentscm::hub::error_envelope::{FieldProblem, InputProblem};
use agentscm::hub::insights::UsageRecord;
use agentscm::hub::reply::{self, Seen};
use agentscm::hub::store::{self, unix_now};
use agentscm::hub::work::{
    BriefingAck, WorkCheckpoint, WorkRepositoryScope, WorkStart, WorkStartResult, WorkState,
};
use agentscm::hub::work_links::WorkPrLinkUpdate;
use agentscm::hub::work_reconciliation::WorkDeclarationsUpdate;
use agentscm::hub::work_recovery::{
    BatchOutcome, MAX_BATCH_TRANSITIONS, RecoveryQuery, RemedyKind, RemedyPreviewRequest,
    RepairPlanV1, RepairReportV1, RepairResultV1, WorkRecoveryV1, WorkTransitionBatch,
    WorkTransitionBatchV1, render_repair, repair_plan, require_findings,
};
use agentscm::{Error, Result};
use clap::Subcommand;
use reqwest::Method;
use serde::Deserialize;
use serde::de::{DeserializeOwned, IgnoredAny};
use serde_json::json;

use crate::hub_cli::map_hub_error;
use crate::intent_cli::Hub;

const LIMIT: Duration = Duration::from_secs(10);
const REQUEST_BYTES: u64 = 1024 * 1024;

#[derive(Debug, Subcommand)]
pub enum WorkCommand {
    /// Read shared deterministic status, recipients and next actions.
    Status {
        /// Print the versioned shared JSON response.
        #[arg(long)]
        json: bool,
    },
    /// Inspect exact Work subjects, scope overlaps and the merge watermark without claiming work.
    Preflight {
        /// Strict WorkStart JSON, including any source revision and checked merge identities.
        #[arg(long, default_value = "-")]
        request: PathBuf,
    },
    /// Grant named-file overlap consent as the current target lead.
    OverlapConsent {
        #[arg(long, default_value = "-")]
        request: PathBuf,
    },
    /// Read current consent state for one coordination request.
    OverlapConsents {
        request_id: String,
        #[arg(long)]
        session: String,
    },
    /// Inspect explicit requester acceptance without claiming Work.
    OverlapPreflight {
        #[arg(long, default_value = "-")]
        request: PathBuf,
    },
    /// Accept consent IDs and atomically admit the requesting Work.
    OverlapStart {
        #[arg(long, default_value = "-")]
        request: PathBuf,
    },
    /// Accept consent IDs and atomically update the requesting Work scope.
    OverlapCheckpoint {
        #[arg(long, default_value = "-")]
        request: PathBuf,
    },
    /// Read the latest dependency notice versions addressed to the current lead.
    DependencyNotices {
        #[arg(long)]
        session: String,
        #[arg(long, default_value_t = 0)]
        after: i64,
        #[arg(long)]
        through: Option<i64>,
        #[arg(long, default_value_t = 20)]
        limit: usize,
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
    /// Read bounded recorded intent, freshness and completeness for one Work item.
    Intent { id: String },
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
    /// Read an explicitly linked PR and its recorded source revision for a Work item.
    PrLink {
        id: String,
        #[arg(long)]
        expected_updated_seq: Option<i64>,
        #[arg(long)]
        expected_head: Option<String>,
    },
    /// Read durable caller-declared PR links and prerequisite conditions.
    Declarations { id: String },
    /// Replace durable declarations as the fenced Work lead.
    DeclarationsSet {
        #[arg(long, default_value = "-")]
        request: PathBuf,
    },
    /// Read the read-only PR evidence and prerequisite report for a Work item.
    Reconcile { id: String },
    /// Read the retained durable evaluation, separating current from stale history.
    Reconciliation { id: String },
    /// Persist a server-evaluated reconciliation with Work and declaration fences.
    ReconcileApply {
        #[arg(long, default_value = "-")]
        request: PathBuf,
    },
    /// Set or remove a native Work issue link as the current lead.
    IssueLinkSet {
        /// Strict WorkIssueLinkUpdate JSON; link:null removes the caller assertion.
        #[arg(long, default_value = "-")]
        request: PathBuf,
    },
    /// Explicitly set or remove a PR relationship as the current Work lead.
    PrLinkSet {
        /// Strict WorkPrLinkUpdate JSON; link:null removes. Use - for standard input.
        #[arg(long, default_value = "-")]
        request: PathBuf,
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
    /// Show what you can recover: your owner's unfinished Work (active, needs you, can resume),
    /// why each still exists, and every remedy typed as permitted or refused with its exact call.
    /// With --path, also the live holds over those paths and your owner's resumable Work there.
    Recovery {
        /// One Work item of the repository, whoever owns it.
        #[arg(long)]
        work_id: Option<String>,
        /// A path you mean to edit; repeat for more (at most 32).
        #[arg(long = "path")]
        paths: Vec<String>,
        /// The next_after of the previous page.
        #[arg(long)]
        after: Option<String>,
        /// Work items per page, 1 to 50; defaults to 20.
        #[arg(long)]
        limit: Option<usize>,
        /// Your agent session, which tells its lead from your owner's other sessions.
        #[arg(long)]
        session: Option<String>,
        /// Print the versioned JSON response.
        #[arg(long)]
        json: bool,
    },
    /// Preview what one remedy would change and the exact call that applies it. Changes nothing.
    RecoveryPreview {
        #[arg(long)]
        work_id: String,
        /// release, retain, complete, abandon, supersede, reopen, prune_participants, resume or handover.
        #[arg(long)]
        remedy: String,
        /// Your agent session.
        #[arg(long)]
        session: Option<String>,
        /// For a handover: a path the next lead takes; repeat for more. The rest is freed.
        #[arg(long = "scope")]
        scope: Vec<String>,
        /// For a supersede: the Work that replaces this one.
        #[arg(long)]
        superseded_by: Option<String>,
    },
    /// Plan the repairs your owner's inventory proposes for retained claims: release a lapsed hold
    /// or prune a full participant list. Dry run by default; --apply sends the plan with fixed
    /// operation ids; a rerun reads a fresh inventory and applies nothing twice. Never completes, resumes or hands over.
    Repair {
        /// Your agent session.
        #[arg(long)]
        session: Option<String>,
        /// Apply the plan; without it nothing changes.
        #[arg(long)]
        apply: bool,
        /// Print one JSON document.
        #[arg(long)]
        json: bool,
    },
    /// Apply up to 25 Work transitions in order, each on its own, with one outcome per item.
    /// Exits non-zero when any item was not applied; earlier items stay applied.
    Transitions {
        /// Strict WorkTransitionBatch JSON file, or - for standard input.
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
        WorkCommand::Status { .. }
        | WorkCommand::Inbox { .. }
        | WorkCommand::Overview { .. }
        | WorkCommand::Briefing { .. }
        | WorkCommand::Catchup { .. }
        | WorkCommand::OverlapPreflight { .. }
        | WorkCommand::OverlapConsents { .. }
        | WorkCommand::DependencyNotices { .. }
        | WorkCommand::Preflight { .. }
        | WorkCommand::PrLink { .. }
        | WorkCommand::Reconcile { .. }
        | WorkCommand::Reconciliation { .. }
        | WorkCommand::Declarations { .. }
        | WorkCommand::Insight { .. }
        | WorkCommand::Intent { .. }
        | WorkCommand::Recovery { .. }
        | WorkCommand::RecoveryPreview { .. }
        | WorkCommand::Insights { .. } => false,
        WorkCommand::Compact { apply, .. }
        | WorkCommand::Repair { apply, .. }
        | WorkCommand::RepairMerge { apply, .. }
        | WorkCommand::RepairRepository { apply, .. } => *apply,
        WorkCommand::CoordinationAck { .. }
        | WorkCommand::CoordinationReply { .. }
        | WorkCommand::CoordinationResolve { .. }
        | WorkCommand::CoordinationWithdraw { .. }
        | WorkCommand::OverlapConsent { .. }
        | WorkCommand::OverlapStart { .. }
        | WorkCommand::OverlapCheckpoint { .. }
        | WorkCommand::Start { .. }
        | WorkCommand::Usage { .. }
        | WorkCommand::PrLinkSet { .. }
        | WorkCommand::IssueLinkSet { .. }
        | WorkCommand::DeclarationsSet { .. }
        | WorkCommand::ReconcileApply { .. }
        | WorkCommand::Checkpoint { .. }
        | WorkCommand::Transitions { .. }
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
    let intent_suffix = if matches!(&command, WorkCommand::Intent { .. }) {
        "intent"
    } else {
        "insights"
    };
    let hub = Hub::connect(timeout)?;
    let (reply, claim_conflict) = match command {
        WorkCommand::Status { json } => {
            let reply = hub
                .call(Method::GET, "/v1/hub/standup/status", None)
                .await?;
            reply::print(
                &reply,
                json,
                agentscm::hub::work_http::actionable_status::render_text,
            )?;
            return Ok(());
        }
        WorkCommand::OverlapConsent { request } => {
            let request: agentscm::hub::work_overlap::WorkOverlapConsentRequest =
                read_request(&request)?;
            (
                hub.call(
                    Method::POST,
                    "/v1/hub/standup/overlap/consent",
                    Some(json!(request)),
                )
                .await?,
                false,
            )
        }
        WorkCommand::OverlapConsents {
            request_id,
            session,
        } => {
            let path = query_path(
                "/v1/hub/standup/overlap/consents",
                json!({"request_id":request_id,"session":session}),
            )?;
            (hub.call(Method::GET, &path, None).await?, false)
        }
        WorkCommand::OverlapPreflight { request } => {
            let request: agentscm::hub::work_overlap::WorkOverlapStart = read_request(&request)?;
            (
                hub.call(
                    Method::POST,
                    "/v1/hub/standup/overlap/preflight?intent=1",
                    Some(json!(request)),
                )
                .await?,
                false,
            )
        }
        WorkCommand::OverlapStart { request } => {
            let request: agentscm::hub::work_overlap::WorkOverlapStart = read_request(&request)?;
            let reply = hub
                .call(
                    Method::POST,
                    "/v1/hub/standup/overlap/start?intent=1",
                    Some(json!(request)),
                )
                .await?;
            let conflict = reply.get("conflict").is_some_and(|v| !v.is_null());
            (reply, conflict)
        }
        WorkCommand::OverlapCheckpoint { request } => {
            let request: agentscm::hub::work_overlap::WorkOverlapCheckpoint =
                read_request(&request)?;
            (
                hub.call(
                    Method::POST,
                    "/v1/hub/standup/overlap/checkpoint?intent=1",
                    Some(json!(request)),
                )
                .await?,
                false,
            )
        }
        WorkCommand::DependencyNotices {
            session,
            after,
            through,
            limit,
        } => {
            let path = query_path(
                "/v1/hub/standup/dependency-notices",
                json!({"session":session,"after":after,"through":through,"limit":limit}),
            )?;
            (hub.call(Method::GET, &path, None).await?, false)
        }
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
        WorkCommand::PrLink {
            id,
            expected_updated_seq,
            expected_head,
        } => {
            let mut url = reqwest::Url::parse("http://hub/v1/hub/standup/work")
                .map_err(|_| invalid("cannot construct Work PR link URL"))?;
            url.path_segments_mut()
                .map_err(|_| invalid("cannot construct Work PR link URL"))?
                .push(&id)
                .push("pr-link");
            let path = url
                .as_str()
                .strip_prefix("http://hub")
                .ok_or_else(|| invalid("cannot construct Work PR link URL"))?;
            let path = query_path(
                path,
                json!({"expected_updated_seq":expected_updated_seq,"expected_head":expected_head}),
            )?;
            (hub.call(Method::GET, &path, None).await?, false)
        }
        WorkCommand::Declarations { id } => {
            let mut url = reqwest::Url::parse("http://hub/v1/hub/standup/work")
                .map_err(|_| invalid("cannot construct declaration URL"))?;
            url.path_segments_mut()
                .map_err(|_| invalid("cannot construct declaration URL"))?
                .push(&id)
                .push("declarations");
            let path = url
                .as_str()
                .strip_prefix("http://hub")
                .ok_or_else(|| invalid("cannot construct declaration URL"))?;
            (hub.call(Method::GET, path, None).await?, false)
        }
        WorkCommand::DeclarationsSet { request } => {
            let request: WorkDeclarationsUpdate = read_request(&request)?;
            (
                hub.call(
                    Method::POST,
                    "/v1/hub/standup/declarations",
                    Some(json!(request)),
                )
                .await?,
                false,
            )
        }
        WorkCommand::Reconciliation { id } => {
            let mut url = reqwest::Url::parse("http://hub/v1/hub/standup/work")
                .map_err(|_| invalid("cannot construct reconciliation URL"))?;
            url.path_segments_mut()
                .map_err(|_| invalid("cannot construct reconciliation URL"))?
                .push(&id)
                .push("reconciliation-v2");
            let path = url
                .as_str()
                .strip_prefix("http://hub")
                .ok_or_else(|| invalid("cannot construct reconciliation URL"))?;
            (hub.call(Method::GET, path, None).await?, false)
        }
        WorkCommand::ReconcileApply { request } => {
            let request: agentscm::hub::work_reconciliation::ReconciliationApply =
                read_request(&request)?;
            (
                hub.call(
                    Method::POST,
                    "/v1/hub/standup/reconciliation",
                    Some(json!(request)),
                )
                .await?,
                false,
            )
        }
        WorkCommand::Reconcile { id } => {
            let mut url = reqwest::Url::parse("http://hub/v1/hub/standup/work")
                .map_err(|_| invalid("cannot construct reconciliation URL"))?;
            url.path_segments_mut()
                .map_err(|_| invalid("cannot construct reconciliation URL"))?
                .push(&id)
                .push("reconciliation");
            let path = url
                .as_str()
                .strip_prefix("http://hub")
                .ok_or_else(|| invalid("cannot construct reconciliation URL"))?;
            (hub.call(Method::GET, path, None).await?, false)
        }
        WorkCommand::IssueLinkSet { request } => {
            let request: agentscm::hub::work_issues::WorkIssueLinkUpdate = read_request(&request)?;
            (
                hub.call(
                    Method::POST,
                    "/v1/hub/standup/issue-link",
                    Some(json!(request)),
                )
                .await?,
                false,
            )
        }
        WorkCommand::PrLinkSet { request } => {
            let request: WorkPrLinkUpdate = read_request(&request)?;
            (
                hub.call(
                    Method::POST,
                    "/v1/hub/standup/pr-link",
                    Some(json!(request)),
                )
                .await?,
                false,
            )
        }
        WorkCommand::Insight { id } | WorkCommand::Intent { id } => {
            let suffix = intent_suffix;
            let mut url = reqwest::Url::parse("http://hub/v1/hub/standup/work")
                .map_err(|_| invalid("cannot construct insight URL"))?;
            url.path_segments_mut()
                .map_err(|_| invalid("cannot construct insight URL"))?
                .push(&id)
                .push(suffix);
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
                    .append_pair("wait_seconds", &wait_seconds.to_string())
                    .append_pair("intent", "1");
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
                    "/v1/hub/standup/preflight?dependencies=1&intent=1",
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
                    "/v1/hub/standup/start?reclaimed=1&dependencies=1&intent=1",
                    Some(serde_json::to_value(request)?),
                )
                .await?;
            reply::check::<WorkStartResult>(&reply)?;
            let started: Started = reply::decode(&reply)?;
            if let Seen::Unknown(state) = &started.item.state {
                eprintln!(
                    "The Hub reports this Work in a state this version of agentscm does not know ({state:?}), so the claim is not confirmed."
                );
            }
            (reply, !started.claimed())
        }
        WorkCommand::Recovery {
            work_id,
            paths,
            after,
            limit,
            session,
            json,
        } => {
            let query = RecoveryQuery {
                work_id,
                paths,
                after,
                limit,
                session,
                all_owners: false,
            };
            let reply = hub
                .call(
                    Method::POST,
                    "/v1/hub/standup/recovery",
                    Some(serde_json::to_value(query)?),
                )
                .await?;
            reply::print::<WorkRecoveryV1>(
                &reply,
                json,
                agentscm::hub::work_recovery::render_text,
            )?;
            return Ok(());
        }
        WorkCommand::RecoveryPreview {
            work_id,
            remedy,
            session,
            scope,
            superseded_by,
        } => {
            let remedy: RemedyKind = serde_json::from_value(json!(remedy)).map_err(|_| {
                invalid("unknown remedy; see agentscm work recovery-preview --help")
            })?;
            let request = RemedyPreviewRequest {
                work_id,
                remedy,
                session,
                scope: (!scope.is_empty()).then_some(scope),
                superseded_by,
            };
            (
                hub.call(
                    Method::POST,
                    "/v1/hub/standup/recovery/preview",
                    Some(serde_json::to_value(request)?),
                )
                .await?,
                false,
            )
        }
        WorkCommand::Repair {
            session,
            apply,
            json,
        } => {
            let report = repair(&hub, session, apply).await?;
            if json {
                println!("{}", serde_json::to_value(&report)?);
            } else {
                println!("{}", render_repair(&report));
            }
            if report.failed() > 0 {
                return Err(Error::Conflict(format!(
                    "{} repairs were not applied; rerun the command: it reads a fresh inventory, plans only what is still needed and applies nothing twice",
                    report.failed()
                )));
            }
            return Ok(());
        }
        WorkCommand::Transitions { request } => {
            let request: WorkTransitionBatch = read_request(&request)?;
            let reply = hub
                .call(
                    Method::POST,
                    "/v1/hub/standup/work-transitions?intent=1",
                    Some(serde_json::to_value(request)?),
                )
                .await?;
            reply::check::<WorkTransitionBatchV1>(&reply)?;
            let result: BatchCounts = reply::decode(&reply)?;
            println!("{reply}");
            if result.not_applied > 0 {
                return Err(Error::Conflict(format!(
                    "{} of {} transitions were not applied; each item's outcome, code and retry are in the JSON result",
                    result.not_applied,
                    result.items.len()
                )));
            }
            return Ok(());
        }
        WorkCommand::Checkpoint { request } => {
            let request: WorkCheckpoint = read_request(&request)?;
            (
                hub.call(
                    Method::POST,
                    "/v1/hub/standup/checkpoint?reclaimed=1&intent=1",
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

/// What `work start` reads from the Hub's answer to choose its exit status. The answer itself is
/// printed as the Hub sent it.
#[derive(Deserialize)]
struct Started {
    item: StartedItem,
    conflict: Option<IgnoredAny>,
    #[serde(default)]
    coordination_request: Option<IgnoredAny>,
}

#[derive(Deserialize)]
struct StartedItem {
    state: Seen<WorkState>,
}

impl Started {
    /// A state this build does not know is not a confirmed claim.
    fn claimed(&self) -> bool {
        self.conflict.is_none()
            && self.coordination_request.is_none()
            && matches!(
                self.item.state,
                Seen::Known(WorkState::Active | WorkState::Blocked)
            )
    }
}

/// What `work transitions` reads from the Hub's answer to choose its exit status.
#[derive(Deserialize)]
struct BatchCounts {
    items: Vec<IgnoredAny>,
    not_applied: usize,
}

/// Pages of the inventory a repair reads before it says the listing is incomplete.
const REPAIR_PAGES: usize = 20;

/// Reads the caller's owner inventory, plans repairs and, with `apply`, sends them.
async fn repair(hub: &Hub, session: Option<String>, apply: bool) -> Result<RepairReportV1> {
    let mut plan = RepairPlanV1::default();
    let mut after = None;
    let mut complete = false;
    for _ in 0..REPAIR_PAGES {
        let query = RecoveryQuery {
            work_id: None,
            paths: Vec::new(),
            after: after.take(),
            limit: Some(50),
            session: session.clone(),
            all_owners: false,
        };
        let reply = hub
            .call(
                Method::POST,
                "/v1/hub/standup/recovery",
                Some(serde_json::to_value(query)?),
            )
            .await?;
        let page: WorkRecoveryV1 = reply::decode(&reply).map_err(|e| invalid(&e.to_string()))?;
        require_findings(&page).map_err(invalid)?;
        let part = repair_plan(&page);
        plan.planned.extend(part.planned);
        plan.skipped.extend(part.skipped);
        match page.next_after {
            Some(next) if !page.complete => after = Some(next),
            _ => {
                complete = true;
                break;
            }
        }
    }
    let mut results = Vec::new();
    if apply {
        for (index, chunk) in plan.planned.chunks(MAX_BATCH_TRANSITIONS).enumerate() {
            let batch = WorkTransitionBatch {
                session: session.clone(),
                items: chunk.iter().map(|p| p.item.clone()).collect(),
            };
            let sent = hub
                .call(
                    Method::POST,
                    "/v1/hub/standup/work-transitions?intent=1",
                    Some(serde_json::to_value(batch)?),
                )
                .await
                .map_err(|e| e.to_string())
                .and_then(|reply| {
                    reply::decode::<WorkTransitionBatchV1>(&reply).map_err(|e| e.to_string())
                });
            match sent {
                Ok(done) => results.extend(done.items.iter().map(RepairResultV1::from)),
                Err(message) => {
                    // Whether these applied is unknown; the fixed ids make a rerun settle it.
                    results.extend(
                        plan.planned[index * MAX_BATCH_TRANSITIONS..]
                            .iter()
                            .map(|p| RepairResultV1 {
                                work_id: p.work_id.clone(),
                                operation_id: p.item.operation_id.clone(),
                                outcome: BatchOutcome::Unavailable,
                                code: Some("unavailable".to_owned()),
                                message: Some(message.clone()),
                            }),
                    );
                    break;
                }
            }
        }
    }
    Ok(RepairReportV1 {
        applied: apply,
        planned: plan.planned,
        skipped: plan.skipped,
        results,
        complete,
    })
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
    serde_json::from_str(&text).map_err(|error| {
        match InputProblem::from_json_error(&error.to_string()) {
            Some(problem) => invalid(&format!(
                "work request field `{}` is {}: {}",
                problem.name,
                match problem.problem {
                    FieldProblem::Missing => "missing",
                    FieldProblem::Unknown => "unknown",
                    FieldProblem::WrongType => "the wrong type",
                    _ => "invalid",
                },
                problem.hint
            )),
            None => invalid(&format!(
                "work request is not valid strict JSON at line {} column {}",
                error.line(),
                error.column()
            )),
        }
    })
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

#[cfg(test)]
mod tests {
    use super::*;

    fn started(reply: serde_json::Value) -> Started {
        reply::decode(&reply).unwrap()
    }

    #[test]
    fn start_reads_its_exit_status_from_a_newer_hubs_answer() {
        let answer = |state: &str| {
            json!({
                "item": {"id": "wrk_1", "state": state, "added_later": [1]},
                "created": true, "conflict": null, "added_later": {"state": "paused"},
            })
        };
        assert!(started(answer("active")).claimed());
        assert!(started(answer("blocked")).claimed());
        for state in ["paused", "finished", "handover"] {
            assert!(!started(answer(state)).claimed(), "{state}");
        }
        // A state added later is shown as sent and never taken for a held claim.
        let newer = started(answer("snoozed"));
        assert_eq!(newer.item.state, Seen::Unknown("snoozed".into()));
        assert!(!newer.claimed());
        let mut refused = answer("active");
        refused["conflict"] = json!({"kind": "scope_overlap", "added_later": 1});
        assert!(!started(refused).claimed());
        let mut asked = answer("active");
        asked["coordination_request"] = json!({"id": "crq_1", "status": "added_later"});
        assert!(!started(asked).claimed());
        assert!(reply::decode::<Started>(&json!({"created": true})).is_err());
    }

    #[test]
    fn transitions_count_items_whatever_a_newer_hub_adds_to_them() {
        let counts: BatchCounts = reply::decode(&json!({
            "schema_version": 1, "applied": 1, "not_applied": 1, "added_later": true,
            "items": [{"outcome": "applied"}, {"outcome": "added_later", "more": 1}],
        }))
        .unwrap();
        assert_eq!((counts.items.len(), counts.not_applied), (2, 1));
    }
}
