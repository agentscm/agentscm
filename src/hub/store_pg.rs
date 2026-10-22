use std::collections::BTreeSet;
use std::net::IpAddr;
use std::str::FromStr;
use std::sync::{Mutex, PoisonError};
use std::time::Duration;

use postgres::config::{Host, SslMode};
use postgres::{Client, Config, IsolationLevel, NoTls};
use serde_json::json;

use super::connection::{
    CONNECTION_SELECT, ConnectedRepository, Connection as ForgeConnection, DELIVERY_KEEP_SECONDS,
    ForgeCheck, ForgeCheckReserve, NewConnection, NewForgeCheck, Observation, Observed, Outbound,
    REPOSITORY_SELECT, gone_event, observation_kinds, validate_check_key, validate_check_phase,
    validate_observation, validate_repository_row, validate_status,
};
use super::coordination::{
    CoordinationAck, CoordinationAcknowledgement, CoordinationAgent, CoordinationInbox,
    CoordinationInboxQuery, CoordinationListQuery, CoordinationProposal, CoordinationReply,
    CoordinationRequest, CoordinationResolve, CoordinationResponse, CoordinationStatus,
    CoordinationTarget, CoordinationWithdraw, MAX_ACTIVE_COORDINATION_REQUESTS,
    MAX_ACTIVE_COORDINATION_REQUESTS_PER_OWNER, MAX_ACTIVE_COORDINATION_REQUESTS_PER_TOKEN,
    MAX_COORDINATION_REASONS_PER_TARGET, MAX_COORDINATION_UPDATES,
    MAX_PROJECTED_COORDINATION_UPDATES, parse_response_kind, parse_status, response_kind_name,
    status_name,
};
use super::external_runs::{
    EXTERNAL_RUN_COLUMNS, ExternalKind, ExternalObserved, ExternalOutcome, ExternalRun, supersedes,
};
use super::handoff::{
    Answered, Asked, HandoffNote, MAX_OPEN, PER_TOUCH, Request, Verdict, answer_kind, clean_note,
    cutoff, file_key, grant_call, judge, remaining, still_holds,
};
use super::insights::{
    CostGroup, HistoryPoint, InsightCursor, InsightInput, InsightPage, InsightQuery, MAX_HISTORY,
    MAX_OBSERVATIONS, MAX_PUBLICATIONS, PublicationEvidence, RecordedCheck, TokenGroup,
    UsageReceipt, UsageRecord, UsageSummary, WorkInsight,
};
use super::intent::{
    Announced, Existing, INTENT_SELECT, Intent, NewIntent, Plan, Who, announced, display_all,
    finish, notice_events, plan, probe_set, validate_ttl,
};
use super::publish::{
    Create, MAX_UNFINISHED, NewPublication, PUBLICATION_SELECT, Publication, Update,
};
use super::session::{
    Checked, DEFAULT_SESSION, NOTICES_PER_TOUCH, Row, SESSION_TTL_SECONDS, STALE_SESSION_SECONDS,
    SessionCall, Touched, check, check_name, expiry_cause, guard, held, plan as plan_touch,
};
use super::store::{
    ACTIVITY_KINDS, ActivityEvent, AgentToken, Event, ForgeIdentity, HubEvent, LIST_LIMIT,
    NAMED_MIGRATIONS, NewAgentToken, Principal, SessionView, Store, TokenDigest, User, event_limit,
    lower_hex_of, new_id, pending, plan_named, session_key, unix_now, validate_login,
};
use super::work::{
    Briefing, BriefingAck, BriefingQuery, CLAIM_STALE_SECONDS, Checkpoint, CheckpointReceipt,
    ClaimStatus, MAX_BRIEFING_BYTES, SourceRef, WORK_DELIVERY_RESERVATION_BYTES,
    WORK_PAYLOAD_CAPACITY_BYTES, WorkCheckpoint, WorkCheckpointResult, WorkCompaction,
    WorkConflictKind, WorkEvent, WorkItem, WorkOutcome, WorkOutcomeKind, WorkOverview,
    WorkOverviewQuery, WorkParticipant, WorkRepositoryScope, WorkRole, WorkScopeCheck,
    WorkScopeOverlap, WorkSource, WorkStart, WorkStartConflict, WorkStartResult, WorkState,
    WorkWho, check_work_delivery_page, check_work_payload_capacity, normalized_scope,
    scope_overlap_reasons, uncovered_scope, work_overlap_pairs, work_payload_row_bytes,
};
use super::{HubError, Result};

/// Held for the whole migration transaction so hubs starting together serialize.
const MIGRATION_LOCK: i64 = 0x6167_656e_7473_636d;
/// Serializes event appends so `seq` order is commit order across hub processes.
const EVENT_LOCK: i64 = 0x6167_656e_7473_6576;
/// First key of the advisory lock that serializes intent announces per repository.
const INTENT_LOCK_CLASS: i32 = 0x6167_656e;
/// First key of the advisory lock that serializes publication creates per token, so the
/// open-publication cap holds across hub processes.
const PUBLICATION_LOCK_CLASS: i32 = 0x6167_6570;
/// Serializes briefing delivery creation and acknowledgements per owner and repository scope.
const WORK_DELIVERY_LOCK_CLASS: i32 = 0x6167_6578;
/// Serializes every payload-growing effect in a complete repository identity.
const WORK_CAPACITY_LOCK_CLASS: i32 = 0x6167_6579;
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
const TCP_USER_TIMEOUT: Duration = Duration::from_secs(15);
const KEEPALIVES_IDLE: Duration = Duration::from_secs(30);

// shortcut: one serialized connection, no pool; upgrade when hub load needs concurrent DB work
pub(super) struct PgStore {
    config: Config,
    /// None until connected, and after a poisoned lock.
    client: Mutex<Option<Client>>,
}

impl PgStore {
    pub(super) fn open(url: &str) -> Result<Self> {
        let config = config(url)?;
        let mut client = connect(&config)?;
        migrate(&mut client)?;
        Ok(Self {
            config,
            client: Mutex::new(Some(client)),
        })
    }

    /// Runs `op` on a live connection, reconnecting first if it is known closed.
    /// With `retry`, an error that leaves the connection closed is retried once
    /// on a new connection; only use that for reads, since a lost reply to a
    /// write may have committed.
    fn run<T>(&self, retry: bool, mut op: impl FnMut(&mut Client) -> Result<T>) -> Result<T> {
        let mut slot = match self.client.lock() {
            Ok(slot) => slot,
            Err(poisoned) => {
                self.client.clear_poison();
                let mut slot = poisoned.into_inner();
                *slot = None;
                slot
            }
        };
        match op(live(&mut slot, &self.config)?) {
            Err(_) if retry && slot.as_ref().is_some_and(Client::is_closed) => {
                op(live(&mut slot, &self.config)?)
            }
            result => result,
        }
    }

    fn set_disabled(&self, actor: Option<&str>, user: &str, disabled: bool) -> Result<()> {
        self.run(false, |client| {
            let mut tx = client.transaction()?;
            let changed = tx.execute(
                "UPDATE hub_users SET disabled = $2 WHERE id = $1 AND disabled <> $2",
                &[&user, &disabled],
            )?;
            if changed > 0 {
                append(&mut tx, &Event::user_toggled(user, disabled, actor)?)?;
            } else if tx
                .query_opt("SELECT 1 FROM hub_users WHERE id = $1", &[&user])?
                .is_none()
            {
                return Err(HubError::Invalid("unknown user".into()));
            }
            Ok(tx.commit()?)
        })
    }

    fn repository_query(
        &self,
        tail: &str,
        args: &[&(dyn postgres::types::ToSql + Sync)],
    ) -> Result<Vec<ConnectedRepository>> {
        self.run(true, |client| {
            let rows = client.query(&format!("{REPOSITORY_SELECT} {tail}"), args)?;
            Ok(rows.iter().map(repository_row).collect())
        })
    }

    fn revoke(&self, owner: Option<&str>, token: &str) -> Result<bool> {
        self.run(false, |client| {
            let mut tx = client.transaction()?;
            let changed = tx.execute(
                "UPDATE hub_agent_tokens SET revoked = TRUE
                 WHERE id = $1 AND NOT revoked AND ($2::text IS NULL OR owner = $2)",
                &[&token, &owner],
            )?;
            if changed > 0 {
                append(&mut tx, &Event::token_revoked(token, owner)?)?;
            }
            tx.commit()?;
            Ok(changed > 0)
        })
    }
}

impl Drop for PgStore {
    fn drop(&mut self) {
        let Some(client) = self
            .client
            .get_mut()
            .unwrap_or_else(PoisonError::into_inner)
            .take()
        else {
            return;
        };
        // The sync client panics when dropped inside a tokio runtime; a plain thread has none.
        // ManuallyDrop leaks the socket, rather than panicking here, if the thread can't start.
        let client = std::mem::ManuallyDrop::new(client);
        match std::thread::Builder::new()
            .spawn(move || drop(std::mem::ManuallyDrop::into_inner(client)))
        {
            Ok(thread) => {
                let _ = thread.join();
            }
            Err(error) => eprintln!("hub: postgres client not closed: {error}"),
        }
    }
}

fn live<'a>(slot: &'a mut Option<Client>, config: &Config) -> Result<&'a mut Client> {
    if slot.as_ref().is_none_or(Client::is_closed) {
        *slot = Some(connect(config)?);
    }
    slot.as_mut()
        .ok_or_else(|| HubError::Unavailable("postgres connection".into()))
}

/// Parse and vet the URL. Errors are fixed text: the URL may hold a password.
fn config(url: &str) -> Result<Config> {
    let mut config = Config::from_str(url)
        .map_err(|_| HubError::Invalid("invalid postgres connection string".into()))?;
    // NoTls with the default sslmode=prefer silently falls back to plaintext.
    let plaintext_ok = config.get_ssl_mode() == SslMode::Disable
        || (all_loopback(&config) && config.get_hostaddrs().iter().all(IpAddr::is_loopback));
    if !plaintext_ok {
        return Err(HubError::Invalid(
            "postgres over TCP needs TLS, which this build lacks; use a unix socket or a \
             loopback IP, or set sslmode=disable to accept plaintext on a private network"
                .into(),
        ));
    }
    config.connect_timeout(CONNECT_TIMEOUT);
    // Bound a black-holed server, which statement_timeout cannot detect.
    if config.get_tcp_user_timeout().is_none() {
        config.tcp_user_timeout(TCP_USER_TIMEOUT);
    }
    config.keepalives_idle(KEEPALIVES_IDLE);
    if config.get_application_name().is_none() {
        config.application_name("agentscm-hub");
    }
    Ok(config)
}

fn all_loopback(config: &Config) -> bool {
    config.get_hosts().iter().all(|host| match host {
        #[cfg(unix)]
        Host::Unix(_) => true,
        // Names, even `localhost`, resolve through the OS and may leave the machine.
        Host::Tcp(name) => name.parse::<IpAddr>().is_ok_and(|ip| ip.is_loopback()),
    })
}

fn connect(config: &Config) -> Result<Client> {
    // The driver's error may echo connection parameters, so keep it off the error path.
    let mut client = config
        .connect(NoTls)
        .map_err(|_| HubError::Unavailable("could not connect to postgres".into()))?;
    client.batch_execute("SET statement_timeout = '10s'; SET lock_timeout = '5s'")?;
    Ok(client)
}

fn migrate(client: &mut Client) -> Result<()> {
    let mut tx = client
        .build_transaction()
        .isolation_level(IsolationLevel::ReadCommitted)
        .start()?;
    tx.execute("SELECT pg_advisory_xact_lock($1)", &[&MIGRATION_LOCK])?;
    tx.batch_execute(
        "CREATE TABLE IF NOT EXISTS hub_schema_migrations (
            version BIGINT PRIMARY KEY, applied_at BIGINT NOT NULL)",
    )?;
    let applied: Vec<i64> = tx
        .query("SELECT version FROM hub_schema_migrations", &[])?
        .iter()
        .map(|row| row.get(0))
        .collect();
    for migration in pending(&applied)? {
        tx.batch_execute(migration.postgres)?;
        tx.execute(
            "INSERT INTO hub_schema_migrations (version, applied_at) VALUES ($1, $2)",
            &[&migration.version, &unix_now()],
        )?;
    }
    tx.batch_execute(
        "CREATE TABLE IF NOT EXISTS hub_named_migrations (
            name TEXT PRIMARY KEY, applied_at BIGINT NOT NULL)",
    )?;
    let applied_names: Vec<String> = tx
        .query("SELECT name FROM hub_named_migrations", &[])?
        .iter()
        .map(|row| row.get(0))
        .collect();
    let applied_refs: Vec<&str> = applied_names.iter().map(String::as_str).collect();
    for migration in plan_named(NAMED_MIGRATIONS, &applied_refs)? {
        tx.batch_execute(migration.postgres)?;
        tx.execute(
            "INSERT INTO hub_named_migrations (name, applied_at) VALUES ($1, $2)",
            &[&migration.name, &unix_now()],
        )?;
    }
    Ok(tx.commit()?)
}

fn append(tx: &mut postgres::Transaction<'_>, event: &Event) -> Result<i64> {
    tx.execute("SELECT pg_advisory_xact_lock($1)", &[&EVENT_LOCK])?;
    let row = tx.query_one(
        "INSERT INTO hub_events (id, at, kind, subject, data) VALUES ($1, $2, $3, $4, $5)
         RETURNING seq",
        &[
            &event.id,
            &event.at,
            &event.kind,
            &event.subject,
            &event.data,
        ],
    )?;
    Ok(row.get(0))
}

/// Appends `events` in order with one statement, so a wide fan-out is one round trip.
fn append_all(tx: &mut postgres::Transaction<'_>, events: &[Event]) -> Result<()> {
    if events.is_empty() {
        return Ok(());
    }
    tx.execute("SELECT pg_advisory_xact_lock($1)", &[&EVENT_LOCK])?;
    let column = |field: fn(&Event) -> &str| events.iter().map(field).collect::<Vec<_>>();
    let ats: Vec<i64> = events.iter().map(|event| event.at).collect();
    tx.execute(
        "INSERT INTO hub_events (id, at, kind, subject, data)
         SELECT id, at, kind, subject, data
         FROM UNNEST($1::text[], $2::bigint[], $3::text[], $4::text[], $5::text[])
             WITH ORDINALITY AS e (id, at, kind, subject, data, n)
         ORDER BY n",
        &[
            &column(|event| &event.id),
            &ats,
            &column(|event| event.kind),
            &column(|event| &event.subject),
            &column(|event| &event.data),
        ],
    )?;
    Ok(())
}

fn work_who(tx: &mut postgres::Transaction<'_>, token: &str, now: i64) -> Result<WorkWho> {
    let row = tx
        .query_opt(
            "SELECT t.repository, t.repository_id, t.name, t.read_only,
                    u.id, u.provider, u.host, u.forge_user_id, u.login
             FROM hub_agent_tokens t JOIN hub_users u ON u.id = t.owner
             WHERE t.id = $1 AND NOT t.revoked AND t.expires_at > $2 AND NOT u.disabled
             FOR SHARE",
            &[&token, &now],
        )?
        .ok_or(HubError::Unauthorized)?;
    Ok(WorkWho {
        token: token.to_owned(),
        repository: row.get(0),
        repository_id: row.get(1),
        agent: row.get(2),
        read_only: row.get(3),
        owner: User {
            id: row.get(4),
            identity: ForgeIdentity {
                provider: row.get(5),
                host: row.get(6),
                user_id: row.get(7),
            },
            login: row.get(8),
            disabled: false,
        },
    })
}

fn role_name(role: WorkRole) -> &'static str {
    match role {
        WorkRole::Lead => "lead",
        WorkRole::Helper => "helper",
        WorkRole::Reviewer => "reviewer",
    }
}

fn state_name(state: WorkState) -> &'static str {
    match state {
        WorkState::Active => "active",
        WorkState::Blocked => "blocked",
        WorkState::Paused => "paused",
        WorkState::Finished => "finished",
        WorkState::Handover => "handover",
    }
}

fn json_string<T: serde::Serialize>(value: &T) -> Result<String> {
    serde_json::to_string(value).map_err(|error| HubError::Unavailable(error.to_string()))
}

fn json_value<T: serde::de::DeserializeOwned>(value: &str) -> Result<T> {
    serde_json::from_str(value).map_err(|error| HubError::Unavailable(error.to_string()))
}

fn enum_name<T: serde::Serialize>(value: &T) -> Result<String> {
    Ok(json_string(value)?.trim_matches('"').to_owned())
}

fn pg_usage_summary(
    tx: &mut postgres::Transaction<'_>,
    work: &str,
    group_limit: usize,
) -> Result<UsageSummary> {
    let r=tx.query_one("SELECT COUNT(*)::bigint,SUM(input_tokens)::bigint,SUM(output_tokens)::bigint,COUNT(input_tokens)::bigint,COUNT(output_tokens)::bigint,COUNT(*) FILTER (WHERE cost_amount_micros IS NULL)::bigint,COUNT(*) FILTER (WHERE outcome='failed')::bigint FROM hub_work_model_usage WHERE work=$1",&[&work])?;
    let observations: i64 = r.get(0);
    let mut token_groups = Vec::new();
    for r in tx.query("SELECT agent,model_provider,model,usage_basis,COUNT(*)::bigint,SUM(input_tokens)::bigint,SUM(output_tokens)::bigint,COUNT(input_tokens)::bigint,COUNT(output_tokens)::bigint FROM hub_work_model_usage WHERE work=$1 GROUP BY agent,model_provider,model,usage_basis ORDER BY agent COLLATE \"C\",model_provider COLLATE \"C\",model COLLATE \"C\",usage_basis COLLATE \"C\" LIMIT $2",&[&work,&((group_limit+1)as i64)])? {
        let basis:String=r.get(3); token_groups.push(TokenGroup { agent:r.get(0),provider:r.get(1),model:r.get(2),usage_basis:serde_json::from_str(&format!("\"{basis}\"")).map_err(|e|HubError::Unavailable(e.to_string()))?,observations:r.get::<_,i64>(4) as usize,input_tokens:r.get(5),output_tokens:r.get(6),input_known_observations:r.get::<_,i64>(7) as usize,output_known_observations:r.get::<_,i64>(8) as usize });
    }
    let mut cost_groups = Vec::new();
    for r in tx.query("SELECT cost_kind,cost_currency,cost_basis,usage_basis,agent,model_provider,model,SUM(cost_amount_micros)::bigint,COUNT(*)::bigint FROM hub_work_model_usage WHERE work=$1 AND cost_amount_micros IS NOT NULL GROUP BY cost_kind,cost_currency,cost_basis,usage_basis,agent,model_provider,model ORDER BY agent COLLATE \"C\",model_provider COLLATE \"C\",model COLLATE \"C\",usage_basis COLLATE \"C\",cost_kind COLLATE \"C\",cost_currency COLLATE \"C\",cost_basis COLLATE \"C\" LIMIT $2",&[&work,&((group_limit+1)as i64)])? {
        let kind:String=r.get(0); let usage_basis:String=r.get(3); cost_groups.push(CostGroup { kind:serde_json::from_str(&format!("\"{kind}\"")).map_err(|e|HubError::Unavailable(e.to_string()))?,currency:r.get(1),basis:r.get(2),usage_basis:serde_json::from_str(&format!("\"{usage_basis}\"")).map_err(|e|HubError::Unavailable(e.to_string()))?,agent:r.get(4),provider:r.get(5),model:r.get(6),amount_micros:r.get(7),observations:r.get::<_,i64>(8) as usize });
    }
    let groups_complete = token_groups.len() <= group_limit && cost_groups.len() <= group_limit;
    token_groups.truncate(group_limit);
    cost_groups.truncate(group_limit);
    Ok(UsageSummary {
        observations: observations as usize,
        coverage: if observations == 0 {
            "unknown"
        } else {
            "partial"
        }
        .into(),
        input_tokens: r.get(1),
        output_tokens: r.get(2),
        input_known_observations: r.get::<_, i64>(3) as usize,
        output_known_observations: r.get::<_, i64>(4) as usize,
        missing_cost_observations: r.get::<_, i64>(5) as usize,
        reported_failed_attempts: r.get::<_, i64>(6) as usize,
        groups_complete,
        cost_groups,
        token_groups,
    })
}

fn pg_insight_input(
    tx: &mut postgres::Transaction<'_>,
    mut item: WorkItem,
    now: i64,
    group_limit: usize,
) -> Result<InsightInput> {
    let rows=tx.query("WITH history AS MATERIALIZED (SELECT event_seq,snapshot::jsonb AS doc FROM hub_work_versions WHERE work=$1 ORDER BY event_seq LIMIT 257) SELECT event_seq,(doc->>'updated_at')::bigint,doc->>'state',(doc->>'generation')::bigint,(doc->>'last_checkin_at')::bigint,COALESCE(doc#>'{current,source_refs}','[]'::jsonb)::text FROM history ORDER BY event_seq",&[&item.id])?;
    let mut history = Vec::with_capacity(rows.len().min(MAX_HISTORY));
    for r in rows.iter().take(MAX_HISTORY) {
        let state: String = r.get(2);
        history.push(HistoryPoint {
            seq: r.get(0),
            at: r.get(1),
            state: serde_json::from_str(&format!("\"{state}\""))
                .map_err(|e| HubError::Unavailable(e.to_string()))?,
            generation: r.get(3),
            last_checkin_at: r.get(4),
        });
    }
    let floors=tx.query_opt("SELECT event_floor_seq,snapshot_floor_seq FROM hub_work_retention WHERE provider=(SELECT provider FROM hub_work_items WHERE id=$1) AND host=(SELECT host FROM hub_work_items WHERE id=$1) AND repository=(SELECT repository FROM hub_work_items WHERE id=$1) AND repository_id IS NOT DISTINCT FROM (SELECT repository_id FROM hub_work_items WHERE id=$1)",&[&item.id])?.map(|r|(r.get::<_,i64>(0),r.get::<_,i64>(1))).unwrap_or((0,0));
    let retention_gap =
        item.created_seq <= 0 || floors.0 >= item.created_seq || floors.1 >= item.created_seq;
    let mut checkpoint_commits = Vec::new();
    let mut seen_commits = BTreeSet::new();
    let mut checkpoint_overflow = false;
    for source in item
        .current
        .as_ref()
        .into_iter()
        .flat_map(|c| &c.source_refs)
    {
        if let SourceRef::Commit { oid, .. } = source
            && super::insights::valid_oid(oid)
            && seen_commits.insert(oid.clone())
        {
            checkpoint_commits.push(oid.clone());
            if checkpoint_commits.len() > MAX_PUBLICATIONS {
                checkpoint_overflow = true;
                break;
            }
        }
    }
    'history_commits: for row in rows.iter().take(MAX_HISTORY) {
        let refs: String = row.get(5);
        for source in json_value::<Vec<SourceRef>>(&refs)? {
            if let SourceRef::Commit { oid, .. } = source
                && super::insights::valid_oid(&oid)
                && seen_commits.insert(oid.clone())
            {
                checkpoint_commits.push(oid);
                if checkpoint_commits.len() > MAX_PUBLICATIONS {
                    checkpoint_overflow = true;
                    break 'history_commits;
                }
            }
        }
    }
    checkpoint_overflow |= rows.len() > MAX_HISTORY;
    checkpoint_commits.truncate(MAX_PUBLICATIONS);
    let mut commits = checkpoint_commits.clone();
    for r in tx.query("SELECT source_commit FROM hub_work_model_usage WHERE work=$1 AND source_commit IS NOT NULL GROUP BY source_commit ORDER BY MIN(received_at),source_commit LIMIT 17",&[&item.id])? { let oid:String=r.get(0); if !commits.contains(&oid){commits.push(oid);} }
    let commits_truncated = checkpoint_overflow || commits.len() > MAX_PUBLICATIONS;
    commits.truncate(MAX_PUBLICATIONS);
    let scope = tx.query_one(
        "SELECT provider,host,repository_id FROM hub_work_items WHERE id=$1",
        &[&item.id],
    )?;
    let provider: String = scope.get(0);
    let host: String = scope.get(1);
    let repository_id: Option<i64> = scope.get(2);
    let mut publications = Vec::new();
    let mut publications_complete = !commits_truncated;
    if let Some(repository_id) = repository_id {
        let connected=tx.query_opt("SELECT r.id FROM hub_repositories r JOIN hub_connections c ON c.id=r.connection WHERE r.host=$1 AND r.repository_id=$2 AND c.provider=$3",&[&host,&repository_id,&provider])?.map(|r|r.get::<_,String>(0)).unwrap_or_default();
        for commit in &commits {
            let remaining = MAX_PUBLICATIONS.saturating_sub(publications.len());
            if remaining == 0 {
                publications_complete = false;
                break;
            }
            let found=tx.query("SELECT p.id,p.branch_state,p.pr_number,p.merge_commit,p.updated_at FROM hub_publications p JOIN hub_agent_tokens t ON t.id=p.token JOIN hub_users u ON u.id=t.owner WHERE p.commit_sha=$1 AND p.repository=$2 AND t.repository=$2 AND t.repository_id=$3 AND u.provider=$4 AND u.host=$5 ORDER BY p.updated_at,p.id LIMIT $6",&[commit,&item.repository,&repository_id,&provider,&host,&((remaining+1)as i64)])?;
            if found.len() > remaining {
                publications_complete = false;
            }
            for row in found.into_iter().take(remaining) {
                let pr_number: Option<i64> = row.get(2);
                if pr_number.is_some() && connected.is_empty() {
                    publications_complete = false;
                }
                let mut checks = if let Some(number) = pr_number.filter(|_| !connected.is_empty()) {
                    tx.query("SELECT head_sha,state,phase,binding_digest,updated_at FROM hub_forge_checks WHERE repository=$1 AND number=$2 AND head_sha=$3 ORDER BY context LIMIT 17",&[&connected,&number,commit])?.into_iter().map(|r|RecordedCheck{head_sha:r.get(0),state:r.get(1),phase:r.get(2),binding_digest:r.get(3),observed_at:r.get(4)}).collect::<Vec<_>>()
                } else {
                    Vec::new()
                };
                if checks.len() > 16 {
                    publications_complete = false;
                    checks.truncate(16);
                }
                publications.push(PublicationEvidence {
                    publication_id: row.get(0),
                    commit: commit.clone(),
                    branch_state: row.get(1),
                    pr_number,
                    merge_commit: row.get(3),
                    observed_at: row.get(4),
                    checks,
                    association: if checkpoint_commits.contains(commit) {
                        "claimed_by_checkpoint"
                    } else {
                        "claimed_by_usage"
                    }
                    .into(),
                });
            }
        }
    } else {
        publications_complete = false;
    }
    project_claim(tx, &mut item, now)?;
    Ok(InsightInput {
        usage: pg_usage_summary(tx, &item.id, group_limit)?,
        item,
        history,
        history_complete: rows.len() <= MAX_HISTORY && !retention_gap,
        retention_gap,
        publications,
        publications_complete,
        commits_truncated,
        as_of: now,
    })
}

fn project_claim(tx: &mut postgres::Transaction<'_>, item: &mut WorkItem, now: i64) -> Result<()> {
    if now < item.last_checkin_at {
        item.claim_status = ClaimStatus::Unknown;
        return Ok(());
    }
    let active = tx
        .query_opt(
            "SELECT 1 FROM hub_agent_tokens t JOIN hub_users u ON u.id = t.owner
             WHERE t.id = $1 AND NOT t.revoked AND t.expires_at > $2 AND NOT u.disabled",
            &[&item.lead_token, &now],
        )?
        .is_some();
    item.claim_status = if active && item.last_checkin_at >= now - CLAIM_STALE_SECONDS {
        ClaimStatus::Active
    } else {
        ClaimStatus::Stale
    };
    Ok(())
}

fn scoped_work(
    tx: &mut postgres::Transaction<'_>,
    who: &WorkWho,
    id: &str,
    now: i64,
    lock: bool,
) -> Result<Option<WorkItem>> {
    let tail = if lock { " FOR UPDATE" } else { "" };
    let row = tx.query_opt(
        &format!(
            "SELECT snapshot FROM hub_work_items
             WHERE id = $1 AND provider = $2 AND host = $3 AND repository = $4
               AND repository_id IS NOT DISTINCT FROM $5{tail}"
        ),
        &[
            &id,
            &who.owner.identity.provider,
            &who.owner.identity.host,
            &who.repository,
            &who.repository_id,
        ],
    )?;
    row.map(|row| {
        let snapshot: String = row.get(0);
        let mut item: WorkItem = json_value(&snapshot)?;
        project_claim(tx, &mut item, now)?;
        Ok(item)
    })
    .transpose()
}

fn relation_targets(
    tx: &mut postgres::Transaction<'_>,
    who: &WorkWho,
    ids: &[String],
) -> Result<()> {
    for id in ids {
        if scoped_work(tx, who, id, 0, false)?.is_none() {
            return Err(HubError::Invalid(format!(
                "related work {id} is outside the repository or unknown"
            )));
        }
    }
    Ok(())
}

fn work_scope_overlaps(
    tx: &mut postgres::Transaction<'_>,
    who: &WorkWho,
    scope: &[String],
    exclude: Option<&str>,
    now: i64,
) -> Result<Vec<WorkScopeOverlap>> {
    if scope.is_empty() {
        return Ok(Vec::new());
    }
    let rows = tx.query(
        "SELECT snapshot FROM hub_work_items
         WHERE provider = $1 AND host = $2 AND repository = $3
           AND repository_id IS NOT DISTINCT FROM $4 AND state <> 'finished'
           AND ($5::text IS NULL OR id <> $5)
         ORDER BY id LIMIT 101",
        &[
            &who.owner.identity.provider,
            &who.owner.identity.host,
            &who.repository,
            &who.repository_id,
            &exclude,
        ],
    )?;
    if rows.len() > 100 {
        return Err(HubError::WorkOverlapIncomplete);
    }
    let mut found = Vec::new();
    for row in rows {
        let snapshot: String = row.get(0);
        let mut item: WorkItem = json_value(&snapshot)?;
        project_claim(tx, &mut item, now)?;
        let reasons = scope_overlap_reasons(scope, &item.scope);
        if !reasons.is_empty() {
            let their_scope = item.scope.clone();
            found.push(WorkScopeOverlap {
                item,
                your_scope: scope.to_vec(),
                their_scope,
                reasons,
            });
        }
    }
    Ok(found)
}

fn work_event(kind: &'static str, item: &WorkItem) -> Result<Event> {
    let mut event = Event::new(
        kind,
        &item.id,
        json!({"generation": item.generation, "owner": item.owner,
            "repository": item.repository, "state": state_name(item.state)}),
    )?;
    event.at = item.updated_at;
    Ok(event)
}

fn save_work_version(tx: &mut postgres::Transaction<'_>, item: &WorkItem, seq: i64) -> Result<()> {
    let snapshot = json_string(item)?;
    let current = item.current.as_ref().map(json_string).transpose()?;
    let scope = json_string(&item.scope)?;
    tx.execute(
        "UPDATE hub_work_items SET lead_token = $2, lead_agent = $3, lead_session = $4,
             generation = $5, state = $6, next_action = $7, current_checkpoint = $8,
             snapshot = $9, updated_at = $10, updated_seq = $11, scope = $12,
             branch = $13, worktree = $14 WHERE id = $1",
        &[
            &item.id,
            &item.lead_token,
            &item.lead_agent,
            &item.lead_session,
            &item.generation,
            &state_name(item.state),
            &item.next_action,
            &current,
            &snapshot,
            &item.updated_at,
            &seq,
            &scope,
            &item.branch,
            &item.worktree,
        ],
    )?;
    tx.execute(
        "INSERT INTO hub_work_versions (work, event_seq, state, snapshot)
         VALUES ($1, $2, $3, $4)",
        &[&item.id, &seq, &state_name(item.state), &snapshot],
    )?;
    Ok(())
}

fn work_payload_bytes(
    tx: &mut postgres::Transaction<'_>,
    provider: &str,
    host: &str,
    repository: &str,
    repository_id: Option<i64>,
) -> Result<i64> {
    Ok(tx
        .query_one(
            "SELECT COALESCE(SUM(bytes), 0)::bigint FROM (
                 SELECT octet_length(snapshot)::bigint + 256 AS bytes FROM hub_work_items
                  WHERE provider = $1 AND host = $2 AND repository = $3
                    AND repository_id IS NOT DISTINCT FROM $4
                 UNION ALL
                 SELECT octet_length(v.snapshot)::bigint + 256 FROM hub_work_versions v
                  JOIN hub_work_items w ON w.id = v.work
                  WHERE w.provider = $1 AND w.host = $2 AND w.repository = $3
                    AND w.repository_id IS NOT DISTINCT FROM $4
                 UNION ALL
                 SELECT octet_length(o.request)::bigint + 256 FROM hub_work_checkpoint_ops o
                  JOIN hub_work_items w ON w.id = o.work
                  WHERE w.provider = $1 AND w.host = $2 AND w.repository = $3
                    AND w.repository_id IS NOT DISTINCT FROM $4
                 UNION ALL
                 SELECT octet_length(u.request)::bigint + 256 FROM hub_work_model_usage u
                  JOIN hub_work_items w ON w.id = u.work
                  WHERE w.provider = $1 AND w.host = $2 AND w.repository = $3
                    AND w.repository_id IS NOT DISTINCT FROM $4
                 UNION ALL
                 SELECT CASE WHEN NOT acknowledged THEN 4194304::bigint
                             ELSE octet_length(page)::bigint + 256 END
                   FROM hub_work_deliveries
                  WHERE provider = $1 AND host = $2 AND repository = $3
                    AND repository_id IS NOT DISTINCT FROM $4
                 UNION ALL
                 SELECT octet_length(e.data)::bigint + octet_length(e.kind)::bigint
                        + octet_length(e.subject)::bigint + 256
                   FROM hub_events e JOIN hub_work_items w ON w.id = e.subject
                  WHERE w.provider = $1 AND w.host = $2 AND w.repository = $3
                    AND w.repository_id IS NOT DISTINCT FROM $4
                    AND e.kind IN ('work_started', 'work_joined', 'work_takeover', 'work_checkpoint')
                 UNION ALL
                 SELECT octet_length(r.proposed_scope)::bigint
                        + octet_length(r.proposed_title)::bigint + 256
                   FROM hub_coordination_requests r
                  WHERE r.provider=$1 AND r.host=$2 AND r.repository=$3
                    AND r.repository_id IS NOT DISTINCT FROM $4
                 UNION ALL
                 SELECT octet_length(t.reasons)::bigint + 256
                   FROM hub_coordination_targets t
                   JOIN hub_coordination_requests r ON r.id=t.request
                  WHERE r.provider=$1 AND r.host=$2 AND r.repository=$3
                    AND r.repository_id IS NOT DISTINCT FROM $4
                 UNION ALL
                 SELECT octet_length(COALESCE(s.note,''))::bigint + 256
                   FROM hub_coordination_responses s
                   JOIN hub_coordination_requests r ON r.id=s.request
                  WHERE r.provider=$1 AND r.host=$2 AND r.repository=$3
                    AND r.repository_id IS NOT DISTINCT FROM $4
                 UNION ALL
                 SELECT octet_length(o.payload)::bigint + 256
                   FROM hub_coordination_operations o
                   JOIN hub_coordination_requests r ON r.id=o.request
                  WHERE r.provider=$1 AND r.host=$2 AND r.repository=$3
                    AND r.repository_id IS NOT DISTINCT FROM $4
                 UNION ALL
                 SELECT 256::bigint FROM hub_coordination_acknowledgements a
                   JOIN hub_coordination_requests r ON r.id=a.request
                  WHERE r.provider=$1 AND r.host=$2 AND r.repository=$3
                    AND r.repository_id IS NOT DISTINCT FROM $4
                 UNION ALL
                 SELECT 256::bigint FROM hub_coordination_versions v
                   JOIN hub_coordination_requests r ON r.id=v.request
                  WHERE r.provider=$1 AND r.host=$2 AND r.repository=$3
                    AND r.repository_id IS NOT DISTINCT FROM $4
                 UNION ALL
                 SELECT octet_length(e.data)::bigint + octet_length(e.kind)::bigint
                        + octet_length(e.subject)::bigint + 256
                   FROM hub_events e JOIN hub_coordination_requests r ON r.id=e.subject
                  WHERE r.provider=$1 AND r.host=$2 AND r.repository=$3
                    AND r.repository_id IS NOT DISTINCT FROM $4
             ) payloads",
            &[
                &provider,
                &host,
                &repository,
                &repository_id,
            ],
        )?
        .get(0))
}

fn work_event_payload_bytes(event: &Event) -> Result<i64> {
    work_payload_row_bytes(
        event
            .data
            .len()
            .saturating_add(event.kind.len())
            .saturating_add(event.subject.len()),
    )
}

fn work_event_high_water(
    tx: &mut postgres::Transaction<'_>,
    who: &WorkWho,
    minimum: i64,
) -> Result<i64> {
    Ok(tx
        .query_one(
            "SELECT MAX(value) FROM (
                 SELECT COALESCE(MAX(seq),0)::bigint AS value FROM hub_events
                 UNION ALL SELECT $5::bigint
                 UNION ALL SELECT event_floor_seq FROM hub_work_retention
                   WHERE provider=$1 AND host=$2 AND repository=$3
                     AND repository_id IS NOT DISTINCT FROM $4
                 UNION ALL SELECT snapshot_floor_seq FROM hub_work_retention
                   WHERE provider=$1 AND host=$2 AND repository=$3
                     AND repository_id IS NOT DISTINCT FROM $4
                 UNION ALL SELECT event_floor_seq FROM hub_coordination_retention
                   WHERE provider=$1 AND host=$2 AND repository=$3
                     AND repository_id IS NOT DISTINCT FROM $4) watermarks",
            &[
                &who.owner.identity.provider,
                &who.owner.identity.host,
                &who.repository,
                &who.repository_id,
                &minimum,
            ],
        )?
        .get(0))
}

fn work_transition_payload_bytes(snapshot: &str, event: &Event) -> Result<i64> {
    let snapshot = work_payload_row_bytes(snapshot.len())?;
    let event = work_event_payload_bytes(event)?;
    snapshot
        .checked_mul(2)
        .and_then(|bytes| bytes.checked_add(event))
        .ok_or_else(|| HubError::Invalid("work payload size is too large".into()))
}

fn stored_work_item_bytes(tx: &mut postgres::Transaction<'_>, id: &str) -> Result<i64> {
    Ok(tx
        .query_one(
            "SELECT octet_length(snapshot)::bigint + 256 FROM hub_work_items WHERE id = $1",
            &[&id],
        )?
        .get(0))
}

fn adjust_work_payload_capacity(
    tx: &mut postgres::Transaction<'_>,
    who: &WorkWho,
    delta: i64,
    now: i64,
) -> Result<()> {
    let used: Option<i64> = tx
        .query_opt(
            "SELECT charged_bytes FROM hub_work_usage
             WHERE provider = $1 AND host = $2 AND repository = $3
               AND repository_id IS NOT DISTINCT FROM $4 FOR UPDATE",
            &[
                &who.owner.identity.provider,
                &who.owner.identity.host,
                &who.repository,
                &who.repository_id,
            ],
        )?
        .map(|row| row.get(0));
    let used = used.unwrap_or(0);
    check_work_payload_capacity(used, delta, WORK_PAYLOAD_CAPACITY_BYTES)?;
    let charged = used
        .checked_add(delta)
        .filter(|charged| *charged >= 0)
        .ok_or_else(|| HubError::Unavailable("work payload accounting is inconsistent".into()))?;
    let changed = tx.execute(
        "UPDATE hub_work_usage SET charged_bytes = $5, updated_at = $6
         WHERE provider = $1 AND host = $2 AND repository = $3
           AND repository_id IS NOT DISTINCT FROM $4",
        &[
            &who.owner.identity.provider,
            &who.owner.identity.host,
            &who.repository,
            &who.repository_id,
            &charged,
            &now,
        ],
    )?;
    if changed == 0 {
        tx.execute(
            "INSERT INTO hub_work_usage
                (provider, host, repository, repository_id, charged_bytes, updated_at)
             VALUES ($1, $2, $3, $4, $5, $6)",
            &[
                &who.owner.identity.provider,
                &who.owner.identity.host,
                &who.repository,
                &who.repository_id,
                &charged,
                &now,
            ],
        )?;
    }
    Ok(())
}

fn work_retention_floors(tx: &mut postgres::Transaction<'_>, who: &WorkWho) -> Result<(i64, i64)> {
    Ok(tx
        .query_opt(
            "SELECT event_floor_seq, snapshot_floor_seq FROM hub_work_retention
             WHERE provider = $1 AND host = $2 AND repository = $3
               AND repository_id IS NOT DISTINCT FROM $4",
            &[
                &who.owner.identity.provider,
                &who.owner.identity.host,
                &who.repository,
                &who.repository_id,
            ],
        )?
        .map_or((0, 0), |row| (row.get(0), row.get(1))))
}

fn replace_relations(
    tx: &mut postgres::Transaction<'_>,
    who: &WorkWho,
    work: &str,
    depends_on: &[String],
    related: &[String],
) -> Result<()> {
    relation_targets(tx, who, depends_on)?;
    relation_targets(tx, who, related)?;
    if depends_on.iter().chain(related).any(|id| id == work) {
        return Err(HubError::Invalid("work cannot relate to itself".into()));
    }
    tx.execute("DELETE FROM hub_work_relations WHERE work = $1", &[&work])?;
    for (kind, ids) in [("depends_on", depends_on), ("related", related)] {
        for id in ids {
            tx.execute(
                "INSERT INTO hub_work_relations (work, related, kind) VALUES ($1, $2, $3)",
                &[&work, id, &kind],
            )?;
        }
    }
    Ok(())
}

fn unfinished_for_session(
    tx: &mut postgres::Transaction<'_>,
    who: &WorkWho,
    session: &str,
    now: i64,
) -> Result<Option<WorkItem>> {
    let rows = tx.query(
        "SELECT snapshot FROM hub_work_items
         WHERE provider = $1 AND host = $2 AND repository = $3
           AND repository_id IS NOT DISTINCT FROM $4 AND lead_token = $5
           AND lead_session = $6 AND state <> 'finished'
         ORDER BY id LIMIT 2",
        &[
            &who.owner.identity.provider,
            &who.owner.identity.host,
            &who.repository,
            &who.repository_id,
            &who.token,
            &session,
        ],
    )?;
    if rows.len() > 1 {
        return Err(HubError::Invalid(
            "session is lead for more than one unfinished work item".into(),
        ));
    }
    rows.first()
        .map(|row| {
            let snapshot: String = row.get(0);
            let mut item: WorkItem = json_value(&snapshot)?;
            project_claim(tx, &mut item, now)?;
            Ok(item)
        })
        .transpose()
}

fn resolve_work(
    tx: &mut postgres::Transaction<'_>,
    who: &WorkWho,
    start: &WorkStart,
    now: i64,
) -> Result<Option<WorkItem>> {
    if let Some(id) = &start.work_id {
        return scoped_work(tx, who, id, now, true);
    }
    let row = if let Some(source) = &start.source {
        tx.query_opt(
            "SELECT snapshot FROM hub_work_items
             WHERE provider = $1 AND host = $2 AND repository = $3
               AND repository_id IS NOT DISTINCT FROM $4 AND source_provider = $5
               AND source_host = $6 AND source_repository = $7 AND source_issue = $8
             FOR UPDATE",
            &[
                &who.owner.identity.provider,
                &who.owner.identity.host,
                &who.repository,
                &who.repository_id,
                &source.provider,
                &source.host,
                &source.repository,
                &source.issue,
            ],
        )?
    } else {
        tx.query_opt(
            "SELECT snapshot FROM hub_work_items
             WHERE provider = $1 AND host = $2 AND repository = $3
               AND repository_id IS NOT DISTINCT FROM $4 AND owner = $5 AND request_key = $6
             FOR UPDATE",
            &[
                &who.owner.identity.provider,
                &who.owner.identity.host,
                &who.repository,
                &who.repository_id,
                &who.owner.id,
                &start.request_key,
            ],
        )?
    };
    row.map(|row| {
        let snapshot: String = row.get(0);
        let mut item: WorkItem = json_value(&snapshot)?;
        project_claim(tx, &mut item, now)?;
        Ok(item)
    })
    .transpose()
}

fn coordination_event(
    kind: &'static str,
    request: &str,
    now: i64,
    data: serde_json::Value,
) -> Result<Event> {
    let mut event = Event::new(kind, request, data)?;
    event.at = now;
    Ok(event)
}

fn coordination_lead(item: &WorkItem) -> CoordinationAgent {
    let participant = item.participants.iter().find(|participant| {
        participant.token == item.lead_token && participant.session == item.lead_session
    });
    CoordinationAgent {
        token: item.lead_token.clone(),
        agent: item.lead_agent.clone(),
        session: item.lead_session.clone(),
        owner: participant.map_or_else(
            || item.owner.clone(),
            |participant| participant.owner.clone(),
        ),
        login: participant.map_or_else(
            || item.owner_login.clone(),
            |participant| participant.login.clone(),
        ),
    }
}

fn coordination_request_at(
    tx: &mut postgres::Transaction<'_>,
    who: &WorkWho,
    request: &str,
    through: i64,
    now: i64,
) -> Result<Option<CoordinationRequest>> {
    let Some(row) = tx.query_opt(
        "SELECT r.requester_owner, r.requester_token, r.requester_agent,
                r.requester_session, u.login, v.proposed_work, r.source_provider,
                r.source_host, r.source_repository, r.source_issue, r.request_key,
                r.proposed_title, r.proposed_scope, v.status, r.created_at, v.updated_at,
                r.created_seq, v.event_seq
         FROM hub_coordination_requests r
         JOIN hub_users u ON u.id = r.requester_owner
         JOIN hub_coordination_versions v ON v.request = r.id
         WHERE r.id = $1 AND r.provider = $2 AND r.host = $3
           AND r.repository = $4 AND r.repository_id IS NOT DISTINCT FROM $5
           AND v.event_seq = (SELECT MAX(v2.event_seq) FROM hub_coordination_versions v2
                              WHERE v2.request = r.id AND v2.event_seq <= $6)",
        &[
            &request,
            &who.owner.identity.provider,
            &who.owner.identity.host,
            &who.repository,
            &who.repository_id,
            &through,
        ],
    )?
    else {
        return Ok(None);
    };
    let requester_owner: String = row.get(0);
    let requester_token: String = row.get(1);
    let requester_agent: String = row.get(2);
    let requester_session: String = row.get(3);
    let requester_login: String = row.get(4);
    let proposed_work: Option<String> = row.get(5);
    let source_provider: Option<String> = row.get(6);
    let source_host: Option<String> = row.get(7);
    let source_repository: Option<String> = row.get(8);
    let source_issue: Option<String> = row.get(9);
    let source = match (
        source_provider,
        source_host,
        source_repository,
        source_issue,
    ) {
        (Some(provider), Some(host), Some(repository), Some(issue)) => {
            Some(super::work::ForgeWorkRef {
                provider,
                host,
                repository,
                issue,
            })
        }
        (None, None, None, None) => None,
        _ => {
            return Err(HubError::Unavailable(
                "invalid coordination proposal source".into(),
            ));
        }
    };
    let mut targets = Vec::new();
    for target in tx.query(
        "SELECT work, reasons, reasons_complete FROM hub_coordination_targets
         WHERE request = $1 ORDER BY work",
        &[&request],
    )? {
        let work: String = target.get(0);
        let reasons: String = target.get(1);
        let reasons_complete: bool = target.get(2);
        let item = scoped_work(tx, who, &work, now, false)?
            .ok_or_else(|| HubError::Unavailable("coordination target work is missing".into()))?;
        targets.push(CoordinationTarget {
            work_id: work,
            lead: coordination_lead(&item),
            generation: item.generation,
            reasons: json_value(&reasons)?,
            reasons_complete,
        });
    }
    let acknowledgements = tx
        .query(
            "SELECT target_work, token, agent, session, owner, login, generation, through_seq,
                acknowledged_at, event_seq
         FROM hub_coordination_acknowledgements a
         WHERE request = $1 AND event_seq <= $2
           AND event_seq = (SELECT MAX(a2.event_seq) FROM hub_coordination_acknowledgements a2
                            WHERE a2.request = a.request AND a2.target_work = a.target_work
                              AND a2.token = a.token AND a2.session = a.session
                              AND a2.generation = a.generation AND a2.event_seq <= $2)
         ORDER BY CASE WHEN
             (a.target_work='' AND a.token=$4 AND a.session=$5 AND a.generation=0)
             OR EXISTS (SELECT 1 FROM hub_coordination_targets t
                        JOIN hub_work_items w ON w.id=t.work
                        WHERE t.request=a.request AND t.work=a.target_work
                          AND w.lead_token=a.token AND w.lead_session=a.session
                          AND w.generation=a.generation)
             THEN 0 ELSE 1 END, event_seq DESC LIMIT $3",
            &[
                &request,
                &through,
                &(MAX_PROJECTED_COORDINATION_UPDATES + 1),
                &requester_token,
                &requester_session,
            ],
        )?
        .into_iter()
        .map(|row| {
            let target: String = row.get(0);
            CoordinationAcknowledgement {
                target_work_id: (!target.is_empty()).then_some(target),
                agent: CoordinationAgent {
                    token: row.get(1),
                    agent: row.get(2),
                    session: row.get(3),
                    owner: row.get(4),
                    login: row.get(5),
                },
                generation: row.get(6),
                through_seq: row.get(7),
                acknowledged_at: row.get(8),
                seq: row.get(9),
            }
        })
        .collect::<Vec<_>>();
    let acknowledgements_complete =
        acknowledgements.len() <= MAX_PROJECTED_COORDINATION_UPDATES as usize;
    let acknowledgements = acknowledgements
        .into_iter()
        .take(MAX_PROJECTED_COORDINATION_UPDATES as usize)
        .collect::<Vec<_>>();
    let response_rows = tx.query(
        "SELECT id, operation_id, target_work, token, agent, session, owner, login,
                target_generation, kind, note, created_at, event_seq
         FROM hub_coordination_responses WHERE request = $1 AND event_seq <= $2
         ORDER BY CASE WHEN EXISTS (
             SELECT 1 FROM hub_coordination_targets t JOIN hub_work_items w ON w.id=t.work
             WHERE t.request=hub_coordination_responses.request
               AND t.work=hub_coordination_responses.target_work
               AND w.lead_token=hub_coordination_responses.token
               AND w.lead_session=hub_coordination_responses.session
               AND w.generation=hub_coordination_responses.target_generation)
             THEN 0 ELSE 1 END, event_seq DESC LIMIT $3",
        &[
            &request,
            &through,
            &(MAX_PROJECTED_COORDINATION_UPDATES + 1),
        ],
    )?;
    let responses_complete = response_rows.len() <= MAX_PROJECTED_COORDINATION_UPDATES as usize;
    let mut responses = Vec::new();
    for row in response_rows
        .into_iter()
        .take(MAX_PROJECTED_COORDINATION_UPDATES as usize)
    {
        let kind: String = row.get(9);
        responses.push(CoordinationResponse {
            id: row.get(0),
            operation_id: row.get(1),
            target_work_id: row.get(2),
            responder: CoordinationAgent {
                token: row.get(3),
                agent: row.get(4),
                session: row.get(5),
                owner: row.get(6),
                login: row.get(7),
            },
            target_generation: row.get(8),
            kind: parse_response_kind(&kind)?,
            note: row.get(10),
            created_at: row.get(11),
            seq: row.get(12),
        });
    }
    let stored_status: String = row.get(13);
    let status = parse_status(&stored_status)?;
    Ok(Some(CoordinationRequest {
        id: request.to_owned(),
        requester: CoordinationAgent {
            token: requester_token,
            agent: requester_agent,
            session: requester_session,
            owner: requester_owner,
            login: requester_login,
        },
        proposal: CoordinationProposal {
            work_id: proposed_work,
            source,
            request_key: row.get(10),
            title: row.get(11),
            scope: json_value(&row.get::<_, String>(12))?,
        },
        targets,
        status,
        acknowledgements,
        acknowledgements_complete,
        responses,
        responses_complete,
        created_at: row.get(14),
        updated_at: row.get(15),
        created_seq: row.get(16),
        updated_seq: row.get(17),
    }))
}

fn coordination_all_targets_responded(
    tx: &mut postgres::Transaction<'_>,
    request: &str,
) -> Result<bool> {
    Ok(tx
        .query_one(
            "SELECT NOT EXISTS (
                 SELECT 1 FROM hub_coordination_targets t
                 JOIN hub_work_items w ON w.id=t.work
                 WHERE t.request=$1 AND NOT EXISTS (
                     SELECT 1 FROM hub_coordination_responses r
                     WHERE r.request=t.request AND r.target_work=t.work
                       AND r.token=w.lead_token AND r.session=w.lead_session
                       AND r.target_generation=w.generation
                 )
             )",
            &[&request],
        )?
        .get(0))
}

fn coordination_was_initially_bound(
    tx: &mut postgres::Transaction<'_>,
    request: &str,
) -> Result<bool> {
    Ok(tx
        .query_one(
            "SELECT proposed_work IS NOT NULL FROM hub_coordination_versions
             WHERE request=$1 ORDER BY event_seq LIMIT 1",
            &[&request],
        )?
        .get(0))
}

fn coordination_payload_bytes(parts: &[&str], event: &Event) -> Result<i64> {
    parts
        .iter()
        .try_fold(work_event_payload_bytes(event)?, |total, part| {
            total
                .checked_add(work_payload_row_bytes(part.len())?)
                .ok_or_else(|| HubError::Invalid("work payload size is too large".into()))
        })
}

fn create_coordination_request(
    tx: &mut postgres::Transaction<'_>,
    who: &WorkWho,
    requester_session: &str,
    proposal: CoordinationProposal,
    overlaps: &[WorkScopeOverlap],
    now: i64,
) -> Result<CoordinationRequest> {
    let mut target_ids = overlaps
        .iter()
        .map(|overlap| overlap.item.id.clone())
        .collect::<Vec<_>>();
    target_ids.sort();
    target_ids.dedup();
    let identity = if let Some(work) = &proposal.work_id {
        json_string(&("work", work))?
    } else if let Some(source) = &proposal.source {
        json_string(&("source", source))?
    } else if let Some(key) = &proposal.request_key
        && !super::coordination::is_automatic_request_key(key)
    {
        json_string(&("request", key))?
    } else {
        json_string(&("automatic", &proposal.title))?
    };
    let fingerprint = lower_hex_of(
        json_string(&(
            (&who.token, requester_session),
            &identity,
            &proposal.title,
            &target_ids,
            &proposal.scope,
        ))?
        .as_bytes(),
    );
    if let Some(row) = tx.query_opt(
        "SELECT id FROM hub_coordination_requests
         WHERE provider = $1 AND host = $2 AND repository = $3
           AND repository_id IS NOT DISTINCT FROM $4 AND fingerprint = $5
           AND status IN ('pending', 'responded')",
        &[
            &who.owner.identity.provider,
            &who.owner.identity.host,
            &who.repository,
            &who.repository_id,
            &fingerprint,
        ],
    )? {
        let id: String = row.get(0);
        return coordination_request_at(tx, who, &id, i64::MAX, now)?
            .ok_or_else(|| HubError::Unavailable("coordination request disappeared".into()));
    }
    let row = tx.query_one(
        "SELECT COUNT(*),
                    COUNT(*) FILTER (WHERE requester_owner = $5),
                    COUNT(*) FILTER (WHERE requester_token = $6)
             FROM hub_coordination_requests
         WHERE provider = $1 AND host = $2 AND repository = $3
           AND repository_id IS NOT DISTINCT FROM $4 AND status IN ('pending', 'responded')",
        &[
            &who.owner.identity.provider,
            &who.owner.identity.host,
            &who.repository,
            &who.repository_id,
            &who.owner.id,
            &who.token,
        ],
    )?;
    let active: i64 = row.get(0);
    let owner_active: i64 = row.get(1);
    let token_active: i64 = row.get(2);
    if active >= MAX_ACTIVE_COORDINATION_REQUESTS
        || owner_active >= MAX_ACTIVE_COORDINATION_REQUESTS_PER_OWNER
        || token_active >= MAX_ACTIVE_COORDINATION_REQUESTS_PER_TOKEN
    {
        return Err(HubError::WorkCapacity);
    }
    let id = new_id("crq")?;
    // Automatic requests are built from work scopes that are already in their
    // canonical storage form (directory entries end in `/`). Re-normalizing
    // that internal form would reject a valid scope expansion such as `ui/**`.
    let scope = json_string(&proposal.scope)?;
    let event = coordination_event(
        "coordination_requested",
        &id,
        now,
        json!({"requester": who.token, "session": requester_session,
            "proposal": &proposal, "targets": &target_ids}),
    )?;
    let seq = append(tx, &event)?;
    let (source_provider, source_host, source_repository, source_issue) = proposal
        .source
        .as_ref()
        .map_or((None, None, None, None), |source| {
            (
                Some(source.provider.as_str()),
                Some(source.host.as_str()),
                Some(source.repository.as_str()),
                Some(source.issue.as_str()),
            )
        });
    tx.execute(
        "INSERT INTO hub_coordination_requests
            (id, provider, host, repository, repository_id, requester_owner, requester_token,
             requester_agent, requester_session, proposed_work, source_provider, source_host,
             source_repository, source_issue, request_key, proposed_title, proposed_scope,
             fingerprint, status, created_at, updated_at, created_seq, updated_seq)
         VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13,$14,$15,$16,$17,$18,
                 'pending',$19,$19,$20,$20)",
        &[
            &id,
            &who.owner.identity.provider,
            &who.owner.identity.host,
            &who.repository,
            &who.repository_id,
            &who.owner.id,
            &who.token,
            &who.agent,
            &requester_session,
            &proposal.work_id,
            &source_provider,
            &source_host,
            &source_repository,
            &source_issue,
            &proposal.request_key,
            &proposal.title,
            &scope,
            &fingerprint,
            &now,
            &seq,
        ],
    )?;
    let mut target_payloads = Vec::new();
    for overlap in overlaps {
        let reasons_complete = overlap.reasons.len() <= MAX_COORDINATION_REASONS_PER_TARGET;
        let reasons = json_string(
            &overlap
                .reasons
                .iter()
                .take(MAX_COORDINATION_REASONS_PER_TARGET)
                .cloned()
                .collect::<Vec<_>>(),
        )?;
        tx.execute(
            "INSERT INTO hub_coordination_targets
             (request,work,original_generation,reasons,reasons_complete) VALUES ($1,$2,$3,$4,$5)",
            &[
                &id,
                &overlap.item.id,
                &overlap.item.generation,
                &reasons,
                &reasons_complete,
            ],
        )?;
        target_payloads.push(reasons);
    }
    tx.execute(
        "INSERT INTO hub_coordination_versions
            (request,event_seq,status,proposed_work,updated_at) VALUES ($1,$2,'pending',$3,$4)",
        &[&id, &seq, &proposal.work_id, &now],
    )?;
    let mut parts = vec![scope.as_str(), proposal.title.as_str()];
    parts.extend(target_payloads.iter().map(String::as_str));
    adjust_work_payload_capacity(tx, who, coordination_payload_bytes(&parts, &event)?, now)?;
    coordination_request_at(tx, who, &id, seq, now)?
        .ok_or_else(|| HubError::Unavailable("coordination request was not stored".into()))
}

fn coordination_is_requester(request: &CoordinationRequest, who: &WorkWho, session: &str) -> bool {
    request.requester.token == who.token && request.requester.session == session
}

fn coordination_target_for<'a>(
    request: &'a CoordinationRequest,
    work: &str,
    who: &WorkWho,
    session: &str,
) -> Option<&'a CoordinationTarget> {
    request.targets.iter().find(|target| {
        target.work_id == work && target.lead.token == who.token && target.lead.session == session
    })
}

fn save_coordination_version(
    tx: &mut postgres::Transaction<'_>,
    request: &str,
    status: CoordinationStatus,
    proposed_work: Option<&str>,
    now: i64,
    seq: i64,
) -> Result<()> {
    tx.execute(
        "UPDATE hub_coordination_requests SET status=$2, proposed_work=$3,
         source_provider=CASE WHEN $3::text IS NULL THEN source_provider ELSE NULL END,
         source_host=CASE WHEN $3::text IS NULL THEN source_host ELSE NULL END,
         source_repository=CASE WHEN $3::text IS NULL THEN source_repository ELSE NULL END,
         source_issue=CASE WHEN $3::text IS NULL THEN source_issue ELSE NULL END,
         request_key=CASE WHEN $3::text IS NULL THEN request_key ELSE NULL END,
         updated_at=$4, updated_seq=$5 WHERE id=$1",
        &[&request, &status_name(status), &proposed_work, &now, &seq],
    )?;
    tx.execute(
        "INSERT INTO hub_coordination_versions
         (request,event_seq,status,proposed_work,updated_at) VALUES ($1,$2,$3,$4,$5)",
        &[&request, &seq, &status_name(status), &proposed_work, &now],
    )?;
    Ok(())
}

fn bind_coordination_request(
    tx: &mut postgres::Transaction<'_>,
    who: &WorkWho,
    session: &str,
    item: &WorkItem,
    now: i64,
) -> Result<()> {
    let mut row = match &item.source {
        WorkSource::Native { request_key } => tx.query_opt(
            "SELECT id FROM hub_coordination_requests
             WHERE provider=$1 AND host=$2 AND repository=$3
               AND repository_id IS NOT DISTINCT FROM $4 AND requester_token=$5
               AND requester_session=$6 AND proposed_work IS NULL
               AND status IN ('pending','responded') AND request_key=$7
             ORDER BY created_seq DESC LIMIT 1",
            &[
                &who.owner.identity.provider,
                &who.owner.identity.host,
                &who.repository,
                &who.repository_id,
                &who.token,
                &session,
                &request_key,
            ],
        )?,
        WorkSource::Forge {
            provider,
            host,
            repository,
            issue,
        } => tx.query_opt(
            "SELECT id FROM hub_coordination_requests
             WHERE provider=$1 AND host=$2 AND repository=$3
               AND repository_id IS NOT DISTINCT FROM $4 AND requester_token=$5
               AND requester_session=$6 AND proposed_work IS NULL
               AND status IN ('pending','responded') AND source_provider=$7
               AND source_host=$8 AND source_repository=$9 AND source_issue=$10
             ORDER BY created_seq DESC LIMIT 1",
            &[
                &who.owner.identity.provider,
                &who.owner.identity.host,
                &who.repository,
                &who.repository_id,
                &who.token,
                &session,
                &provider,
                &host,
                &repository,
                &issue,
            ],
        )?,
    };
    if row.is_none()
        && matches!(&item.source, WorkSource::Native { request_key }
            if super::coordination::is_automatic_request_key(request_key))
    {
        row = tx
            .query(
                "SELECT id,proposed_scope FROM hub_coordination_requests
             WHERE provider=$1 AND host=$2 AND repository=$3
               AND repository_id IS NOT DISTINCT FROM $4 AND requester_token=$5
               AND requester_session=$6 AND proposed_work IS NULL
               AND status IN ('pending','responded') AND proposed_title=$7
             ORDER BY created_seq DESC",
                &[
                    &who.owner.identity.provider,
                    &who.owner.identity.host,
                    &who.repository,
                    &who.repository_id,
                    &who.token,
                    &session,
                    &item.title,
                ],
            )?
            .into_iter()
            .find_map(|candidate| {
                let scope: String = candidate.get(1);
                json_value::<Vec<String>>(&scope)
                    .ok()
                    .filter(|proposal_scope| {
                        uncovered_scope(proposal_scope, &item.scope).is_empty()
                    })
                    .map(|_| candidate)
            });
    }
    let Some(row) = row else {
        return Ok(());
    };
    let request: String = row.get(0);
    let event = coordination_event(
        "coordination_bound",
        &request,
        now,
        json!({"work_id": item.id}),
    )?;
    let seq = append(tx, &event)?;
    save_coordination_version(
        tx,
        &request,
        CoordinationStatus::Pending,
        Some(&item.id),
        now,
        seq,
    )?;
    adjust_work_payload_capacity(
        tx,
        who,
        coordination_payload_bytes(&[&item.id], &event)?,
        now,
    )
}

fn retarget_coordination_requests(
    tx: &mut postgres::Transaction<'_>,
    who: &WorkWho,
    work: &str,
    generation: i64,
    lead_session: &str,
    now: i64,
) -> Result<()> {
    let rows = tx.query(
        "SELECT r.id,r.proposed_work FROM hub_coordination_requests r
         JOIN hub_coordination_targets t ON t.request=r.id
         WHERE t.work=$1 AND r.status IN ('pending','responded')
         ORDER BY r.id LIMIT 101",
        &[&work],
    )?;
    if rows.len() > MAX_ACTIVE_COORDINATION_REQUESTS as usize {
        return Err(HubError::WorkCapacity);
    }
    for row in rows {
        let request: String = row.get(0);
        let proposed_work: Option<String> = row.get(1);
        let event = coordination_event(
            "coordination_retargeted",
            &request,
            now,
            json!({"work_id": work, "lead_token": who.token,
                "lead_session": lead_session, "generation": generation}),
        )?;
        let seq = append(tx, &event)?;
        save_coordination_version(
            tx,
            &request,
            CoordinationStatus::Pending,
            proposed_work.as_deref(),
            now,
            seq,
        )?;
        adjust_work_payload_capacity(tx, who, coordination_payload_bytes(&[work], &event)?, now)?;
    }
    Ok(())
}

fn coordination_operation_replay(
    tx: &mut postgres::Transaction<'_>,
    request: &str,
    operation: &str,
    kind: &str,
    payload: &str,
) -> Result<bool> {
    match tx.query_opt(
        "SELECT kind,payload FROM hub_coordination_operations
         WHERE request=$1 AND operation_id=$2",
        &[&request, &operation],
    )? {
        None => Ok(false),
        Some(row) if row.get::<_, String>(0) == kind && row.get::<_, String>(1) == payload => {
            Ok(true)
        }
        Some(_) => Err(HubError::Invalid(
            "coordination operation_id was already used for another request".into(),
        )),
    }
}

fn coordination_page(
    tx: &mut postgres::Transaction<'_>,
    who: &WorkWho,
    session: Option<&str>,
    cursor: (i64, Option<i64>, bool),
    limit: usize,
    now: i64,
) -> Result<CoordinationInbox> {
    let (after, requested_through, pending_only) = cursor;
    let retention_floor: i64 = tx
        .query_opt(
            "SELECT event_floor_seq FROM hub_coordination_retention
             WHERE provider=$1 AND host=$2 AND repository=$3
               AND repository_id IS NOT DISTINCT FROM $4",
            &[
                &who.owner.identity.provider,
                &who.owner.identity.host,
                &who.repository,
                &who.repository_id,
            ],
        )?
        .map_or(0, |row| row.get(0));
    let through = match requested_through {
        Some(through) => through,
        None => tx
            .query_one("SELECT COALESCE(MAX(seq),0) FROM hub_events", &[])?
            .get::<_, i64>(0)
            .max(retention_floor)
            .max(after),
    };
    let status = if pending_only {
        "AND v.status IN ('pending', 'responded')"
    } else {
        ""
    };
    let rows = if let Some(session) = session {
        tx.query(
            &format!(
                "SELECT r.id,v.event_seq FROM hub_coordination_requests r
             JOIN hub_coordination_versions v ON v.request=r.id
             WHERE r.provider=$1 AND r.host=$2 AND r.repository=$3
               AND r.repository_id IS NOT DISTINCT FROM $4 AND v.event_seq>$5 AND v.event_seq<=$6
               AND v.event_seq=(SELECT MAX(v2.event_seq) FROM hub_coordination_versions v2
                                WHERE v2.request=r.id AND v2.event_seq<=$6)
               {status}
               AND (r.requester_token=$7 AND r.requester_session=$8 OR EXISTS (
                    SELECT 1 FROM hub_coordination_targets t JOIN hub_work_items w ON w.id=t.work
                    WHERE t.request=r.id AND w.lead_token=$7 AND w.lead_session=$8))
             ORDER BY v.event_seq,r.id LIMIT $9"
            ),
            &[
                &who.owner.identity.provider,
                &who.owner.identity.host,
                &who.repository,
                &who.repository_id,
                &after,
                &through,
                &who.token,
                &session,
                &i64::try_from(limit + 1).unwrap_or(i64::MAX),
            ],
        )?
    } else {
        tx.query(
            &format!(
                "SELECT r.id,v.event_seq FROM hub_coordination_requests r
             JOIN hub_coordination_versions v ON v.request=r.id
             WHERE r.provider=$1 AND r.host=$2 AND r.repository=$3
               AND r.repository_id IS NOT DISTINCT FROM $4 AND v.event_seq>$5 AND v.event_seq<=$6
               AND v.event_seq=(SELECT MAX(v2.event_seq) FROM hub_coordination_versions v2
                                WHERE v2.request=r.id AND v2.event_seq<=$6)
               {status}
             ORDER BY v.event_seq,r.id LIMIT $7"
            ),
            &[
                &who.owner.identity.provider,
                &who.owner.identity.host,
                &who.repository,
                &who.repository_id,
                &after,
                &through,
                &i64::try_from(limit + 1).unwrap_or(i64::MAX),
            ],
        )?
    };
    let mut complete = rows.len() <= limit;
    let rows = rows.into_iter().take(limit).collect::<Vec<_>>();
    let mut next_after = after;
    let mut requests = Vec::new();
    let mut bytes = 0_usize;
    for row in rows {
        let id: String = row.get(0);
        let seq: i64 = row.get(1);
        let request = coordination_request_at(tx, who, &id, through, now)?
            .ok_or_else(|| HubError::Unavailable("coordination page changed during read".into()))?;
        let request_bytes = json_string(&request)?.len();
        if !requests.is_empty() && bytes.saturating_add(request_bytes) > MAX_BRIEFING_BYTES {
            complete = false;
            break;
        }
        if request_bytes > MAX_BRIEFING_BYTES {
            return Err(HubError::Unavailable(
                "one coordination request exceeds the page byte limit".into(),
            ));
        }
        bytes = bytes.saturating_add(request_bytes);
        next_after = seq;
        requests.push(request);
    }
    Ok(CoordinationInbox {
        from: after,
        through,
        next_after,
        requests,
        complete,
        retention_gap: after < retention_floor,
    })
}

fn start_in(
    tx: &mut postgres::Transaction<'_>,
    who: &WorkWho,
    start: &WorkStart,
    now: i64,
) -> Result<WorkStartResult> {
    if who.read_only {
        return Err(HubError::Unauthorized);
    }
    let scope = start.validate(who)?;
    if let Some(mut item) = resolve_work(tx, who, start, now)? {
        if start.role != WorkRole::Lead {
            let previous_item_bytes = stored_work_item_bytes(tx, &item.id)?;
            let participant_generation = tx
                .query_opt(
                    "SELECT generation FROM hub_work_participants
                     WHERE work=$1 AND token=$2 AND session=$3",
                    &[&item.id, &who.token, &start.session],
                )?
                .map(|row| row.get::<_, i64>(0));
            if let Some(existing) = item
                .participants
                .iter()
                .find(|p| p.token == who.token && p.session == start.session)
            {
                if existing.role == WorkRole::Lead {
                    return Err(HubError::Invalid(
                        "the lead role can only change through takeover".into(),
                    ));
                }
                if existing.role == start.role && participant_generation == Some(item.generation) {
                    return Ok(WorkStartResult {
                        item,
                        created: false,
                        joined: false,
                        took_over: false,
                        conflict: None,
                        context: None,
                        coordination_request: None,
                    });
                }
                item.participants
                    .retain(|p| p.token != who.token || p.session != start.session);
            } else if item.participants.len() >= 100 {
                return Err(HubError::Invalid(
                    "work has the maximum of 100 participants".into(),
                ));
            }
            item.participants.push(WorkParticipant {
                owner: who.owner.id.clone(),
                login: who.owner.login.clone(),
                token: who.token.clone(),
                agent: who.agent.clone(),
                session: start.session.clone(),
                role: start.role,
            });
            item.participants.sort_by(|a, b| {
                (&a.owner, &a.token, &a.session).cmp(&(&b.owner, &b.token, &b.session))
            });
            item.updated_at = now;
            let event = work_event("work_joined", &item)?;
            let seq = append(tx, &event)?;
            item.updated_seq = seq;
            tx.execute(
                "INSERT INTO hub_work_participants
                    (work, token, owner, agent, session, role, generation, created_at, updated_at)
                 VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $8)
                 ON CONFLICT (work, token, session) DO UPDATE SET role = excluded.role,
                    agent = excluded.agent, generation = excluded.generation,
                    updated_at = excluded.updated_at",
                &[
                    &item.id,
                    &who.token,
                    &who.owner.id,
                    &who.agent,
                    &start.session,
                    &role_name(start.role),
                    &item.generation,
                    &now,
                ],
            )?;
            save_work_version(tx, &item, seq)?;
            let snapshot = json_string(&item)?;
            let delta = work_transition_payload_bytes(&snapshot, &event)?
                .checked_sub(previous_item_bytes)
                .ok_or_else(|| HubError::Invalid("work payload size is too large".into()))?;
            adjust_work_payload_capacity(tx, who, delta, now)?;
            return Ok(WorkStartResult {
                item,
                created: false,
                joined: true,
                took_over: false,
                conflict: None,
                context: None,
                coordination_request: None,
            });
        }
        if item.lead_token == who.token
            && item.lead_session == start.session
            && start.takeover_generation.is_none()
        {
            if item.title != start.title
                || item.scope != scope
                || item.branch != start.branch
                || item.worktree != start.worktree
                || item.next_action != start.next_action
                || item.depends_on != start.depends_on
                || item.related_work != start.related_work
            {
                return Ok(WorkStartResult {
                    conflict: Some(WorkStartConflict {
                        kind: WorkConflictKind::MetadataChanged,
                        current_generation: item.generation,
                        overlaps: Vec::new(),
                    }),
                    item,
                    created: false,
                    joined: false,
                    took_over: false,
                    context: None,
                    coordination_request: None,
                });
            }
            let overlaps = work_scope_overlaps(tx, who, &item.scope, Some(&item.id), now)?;
            let coordination_request = if overlaps.is_empty() {
                None
            } else {
                Some(create_coordination_request(
                    tx,
                    who,
                    &start.session,
                    CoordinationProposal {
                        work_id: Some(item.id.clone()),
                        source: None,
                        request_key: None,
                        title: item.title.clone(),
                        scope: item.scope.clone(),
                    },
                    &overlaps,
                    now,
                )?)
            };
            return Ok(WorkStartResult {
                item,
                created: false,
                joined: false,
                took_over: false,
                conflict: None,
                context: None,
                coordination_request,
            });
        }
        if start.takeover_generation != Some(item.generation) {
            let kind = if start.takeover_generation.is_some() {
                WorkConflictKind::GenerationChanged
            } else if item.claim_status == ClaimStatus::Stale {
                WorkConflictKind::StaleTakeoverRequired
            } else {
                WorkConflictKind::ClaimHeld
            };
            return Ok(WorkStartResult {
                conflict: Some(WorkStartConflict {
                    kind,
                    current_generation: item.generation,
                    overlaps: Vec::new(),
                }),
                item,
                created: false,
                joined: false,
                took_over: false,
                context: None,
                coordination_request: None,
            });
        }
        let resuming_own_paused = item.lead_token == who.token
            && item.state == WorkState::Paused
            && start.takeover_generation == Some(item.generation);
        if !resuming_own_paused
            && item.claim_status != ClaimStatus::Stale
            && item.state != WorkState::Handover
        {
            return Ok(WorkStartResult {
                conflict: Some(WorkStartConflict {
                    kind: WorkConflictKind::ClaimHeld,
                    current_generation: item.generation,
                    overlaps: Vec::new(),
                }),
                item,
                created: false,
                joined: false,
                took_over: false,
                context: None,
                coordination_request: None,
            });
        }
        let previous_item_bytes = stored_work_item_bytes(tx, &item.id)?;
        let previous_lead_token = item.lead_token.clone();
        let previous_lead_session = item.lead_session.clone();
        for participant in &mut item.participants {
            if participant.role == WorkRole::Lead {
                participant.role = WorkRole::Helper;
            }
        }
        if !item.participants.iter().any(|participant| {
            participant.token == who.token && participant.session == start.session
        }) && item.participants.len() >= 100
        {
            return Err(HubError::Invalid(
                "work has the maximum of 100 participants".into(),
            ));
        }
        item.participants.retain(|participant| {
            participant.token != who.token || participant.session != start.session
        });
        item.participants.push(WorkParticipant {
            owner: who.owner.id.clone(),
            login: who.owner.login.clone(),
            token: who.token.clone(),
            agent: who.agent.clone(),
            session: start.session.clone(),
            role: WorkRole::Lead,
        });
        item.lead_token = who.token.clone();
        item.lead_agent = who.agent.clone();
        item.lead_session = start.session.clone();
        item.generation += 1;
        item.state = WorkState::Active;
        item.claim_status = ClaimStatus::Active;
        item.last_checkin_at = now;
        item.updated_at = now;
        let event = work_event("work_takeover", &item)?;
        let seq = append(tx, &event)?;
        item.updated_seq = seq;
        tx.execute(
            "UPDATE hub_work_participants SET role = 'helper', updated_at = $2
             WHERE work = $1 AND role = 'lead'",
            &[&item.id, &now],
        )?;
        tx.execute(
            "INSERT INTO hub_work_participants
                (work, token, owner, agent, session, role, generation, created_at, updated_at)
             VALUES ($1, $2, $3, $4, $5, 'lead', $6, $7, $7)
             ON CONFLICT (work, token, session) DO UPDATE SET role = 'lead',
                owner = excluded.owner, agent = excluded.agent, generation = excluded.generation,
                updated_at = excluded.updated_at",
            &[
                &item.id,
                &who.token,
                &who.owner.id,
                &who.agent,
                &start.session,
                &item.generation,
                &now,
            ],
        )?;
        save_work_version(tx, &item, seq)?;
        release_session_intent(tx, &previous_lead_token, &previous_lead_session, now)?;
        retarget_coordination_requests(tx, who, &item.id, item.generation, &start.session, now)?;
        let snapshot = json_string(&item)?;
        let delta = work_transition_payload_bytes(&snapshot, &event)?
            .checked_sub(previous_item_bytes)
            .ok_or_else(|| HubError::Invalid("work payload size is too large".into()))?;
        adjust_work_payload_capacity(tx, who, delta, now)?;
        return Ok(WorkStartResult {
            item,
            created: false,
            joined: false,
            took_over: true,
            conflict: None,
            context: None,
            coordination_request: None,
        });
    }
    if start.work_id.is_some() {
        return Err(HubError::Invalid("unknown work item".into()));
    }
    if let Some(item) = unfinished_for_session(tx, who, &start.session, now)? {
        return Ok(WorkStartResult {
            conflict: Some(WorkStartConflict {
                kind: WorkConflictKind::SessionGoalChanged,
                current_generation: item.generation,
                overlaps: Vec::new(),
            }),
            item,
            created: false,
            joined: false,
            took_over: false,
            context: None,
            coordination_request: None,
        });
    }
    let overlaps = work_scope_overlaps(tx, who, &scope, None, now)?;
    if let Some(first) = overlaps.first() {
        let coordination_request = create_coordination_request(
            tx,
            who,
            &start.session,
            CoordinationProposal {
                work_id: None,
                source: start.source.clone(),
                request_key: start.request_key.clone(),
                title: start.title.clone(),
                scope: scope.clone(),
            },
            &overlaps,
            now,
        )?;
        return Ok(WorkStartResult {
            item: first.item.clone(),
            created: false,
            joined: false,
            took_over: false,
            conflict: Some(WorkStartConflict {
                kind: WorkConflictKind::ScopeOverlap,
                current_generation: first.item.generation,
                overlaps,
            }),
            context: None,
            coordination_request: Some(coordination_request),
        });
    }
    relation_targets(tx, who, &start.depends_on)?;
    relation_targets(tx, who, &start.related_work)?;
    let id = new_id("wrk")?;
    let source = match (&start.source, &start.request_key) {
        (Some(source), None) => WorkSource::Forge {
            provider: source.provider.clone(),
            host: source.host.clone(),
            repository: source.repository.clone(),
            issue: source.issue.clone(),
        },
        (None, Some(request_key)) => WorkSource::Native {
            request_key: request_key.clone(),
        },
        _ => return Err(HubError::Invalid("invalid work resolver".into())),
    };
    let mut item = WorkItem {
        id: id.clone(),
        repository: who.repository.clone(),
        owner: who.owner.id.clone(),
        owner_login: who.owner.login.clone(),
        source,
        title: start.title.clone(),
        state: WorkState::Active,
        claim_status: ClaimStatus::Active,
        last_checkin_at: now,
        generation: 1,
        lead_token: who.token.clone(),
        lead_agent: who.agent.clone(),
        lead_session: start.session.clone(),
        participants: vec![WorkParticipant {
            owner: who.owner.id.clone(),
            login: who.owner.login.clone(),
            token: who.token.clone(),
            agent: who.agent.clone(),
            session: start.session.clone(),
            role: WorkRole::Lead,
        }],
        scope,
        branch: start.branch.clone(),
        worktree: start.worktree.clone(),
        next_action: start.next_action.clone(),
        current: None,
        blocker: None,
        depends_on: start.depends_on.clone(),
        related_work: start.related_work.clone(),
        created_at: now,
        updated_at: now,
        created_seq: 0,
        updated_seq: 0,
    };
    let event = work_event("work_started", &item)?;
    let seq = append(tx, &event)?;
    item.created_seq = seq;
    item.updated_seq = seq;
    let (source_provider, source_host, source_repository, source_issue, request_key) =
        match &item.source {
            WorkSource::Native { request_key } => (None, None, None, None, Some(request_key)),
            WorkSource::Forge {
                provider,
                host,
                repository,
                issue,
            } => (
                Some(provider),
                Some(host),
                Some(repository),
                Some(issue),
                None,
            ),
        };
    let scope_json = json_string(&item.scope)?;
    let snapshot = json_string(&item)?;
    tx.execute(
        "INSERT INTO hub_work_items
            (id, provider, host, repository, repository_id, owner, lead_token, lead_agent,
             lead_session, generation, source_provider, source_host, source_repository,
             source_issue, request_key, title, state, scope, branch, worktree, next_action,
             current_checkpoint, snapshot, created_at, updated_at, created_seq, updated_seq)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, 1, $10, $11, $12, $13, $14,
                 $15, 'active', $16, $17, $18, $19, NULL, $20, $21, $21, $22, $22)",
        &[
            &item.id,
            &who.owner.identity.provider,
            &who.owner.identity.host,
            &who.repository,
            &who.repository_id,
            &who.owner.id,
            &who.token,
            &who.agent,
            &start.session,
            &source_provider,
            &source_host,
            &source_repository,
            &source_issue,
            &request_key,
            &item.title,
            &scope_json,
            &item.branch,
            &item.worktree,
            &item.next_action,
            &snapshot,
            &now,
            &seq,
        ],
    )?;
    tx.execute(
        "INSERT INTO hub_work_participants
            (work, token, owner, agent, session, role, generation, created_at, updated_at)
         VALUES ($1, $2, $3, $4, $5, 'lead', 1, $6, $6)",
        &[
            &id,
            &who.token,
            &who.owner.id,
            &who.agent,
            &start.session,
            &now,
        ],
    )?;
    replace_relations(tx, who, &id, &item.depends_on, &item.related_work)?;
    tx.execute(
        "INSERT INTO hub_work_versions (work, event_seq, state, snapshot)
         VALUES ($1, $2, 'active', $3)",
        &[&id, &seq, &snapshot],
    )?;
    adjust_work_payload_capacity(
        tx,
        who,
        work_transition_payload_bytes(&snapshot, &event)?,
        now,
    )?;
    bind_coordination_request(tx, who, &start.session, &item, now)?;
    Ok(WorkStartResult {
        item,
        created: true,
        joined: false,
        took_over: false,
        conflict: None,
        context: None,
        coordination_request: None,
    })
}

#[allow(clippy::too_many_arguments)]
fn briefing_page(
    tx: &mut postgres::Transaction<'_>,
    who: &WorkWho,
    delivery: Option<String>,
    subscriber: Option<String>,
    from: i64,
    through: i64,
    after: i64,
    work_after: Option<String>,
    work_done: bool,
    limit: usize,
    now: i64,
) -> Result<Briefing> {
    if after < from || after > through {
        return Err(HubError::Invalid(
            "briefing event cursor is outside its delivery".into(),
        ));
    }
    let (event_floor, snapshot_floor) = work_retention_floors(tx, who)?;
    if through < snapshot_floor {
        return Err(HubError::Invalid(
            "briefing watermark is older than retained work history".into(),
        ));
    }
    let mut items = Vec::new();
    let mut more_work = false;
    let mut bytes = 0_usize;
    if !work_done {
        let rows = tx.query(
            "SELECT v.snapshot FROM hub_work_items w JOIN hub_work_versions v ON v.work = w.id
             WHERE w.provider = $1 AND w.host = $2 AND w.repository = $3
               AND w.repository_id IS NOT DISTINCT FROM $4 AND w.id > $5
               AND v.event_seq = (SELECT MAX(v2.event_seq) FROM hub_work_versions v2
                   WHERE v2.work = w.id AND v2.event_seq <= $6)
               AND v.state <> 'finished'
             ORDER BY w.id LIMIT $7",
            &[
                &who.owner.identity.provider,
                &who.owner.identity.host,
                &who.repository,
                &who.repository_id,
                &work_after.as_deref().unwrap_or(""),
                &through,
                &((limit + 1) as i64),
            ],
        )?;
        more_work = rows.len() > limit;
        for row in rows.iter().take(limit) {
            let snapshot: String = row.get(0);
            if !items.is_empty() && bytes.saturating_add(snapshot.len()) > MAX_BRIEFING_BYTES {
                more_work = true;
                break;
            }
            bytes = bytes.saturating_add(snapshot.len());
            let mut item: WorkItem = json_value(&snapshot)?;
            project_claim(tx, &mut item, now)?;
            items.push(item);
        }
    }
    let next_work_after = items
        .last()
        .map(|item| item.id.clone())
        .or_else(|| work_after.clone());
    let next_work_done = work_done || !more_work;
    let rows = tx.query(
        "SELECT e.seq, e.at, e.kind, e.subject, v.snapshot
         FROM hub_events e JOIN hub_work_versions v ON v.event_seq = e.seq
         JOIN hub_work_items w ON w.id = v.work
         WHERE e.seq > $1 AND e.seq <= $2 AND w.provider = $3 AND w.host = $4
           AND w.repository = $5 AND w.repository_id IS NOT DISTINCT FROM $6
         ORDER BY e.seq LIMIT $7",
        &[
            &after,
            &through,
            &who.owner.identity.provider,
            &who.owner.identity.host,
            &who.repository,
            &who.repository_id,
            &((limit + 1) as i64),
        ],
    )?;
    let mut more_events = rows.len() > limit;
    let mut events = Vec::new();
    for row in rows.iter().take(limit) {
        let snapshot: String = row.get(4);
        if !events.is_empty() && bytes.saturating_add(snapshot.len()) > MAX_BRIEFING_BYTES {
            more_events = true;
            break;
        }
        bytes = bytes.saturating_add(snapshot.len());
        let mut item: WorkItem = json_value(&snapshot)?;
        project_claim(tx, &mut item, now)?;
        events.push(WorkEvent {
            seq: row.get(0),
            at: row.get(1),
            kind: row.get(2),
            work_id: row.get(3),
            item,
        });
    }
    let next_after = if more_events {
        events.last().map_or(after, |event| event.seq)
    } else {
        through
    };
    let minimum: Option<i64> = tx.query_one("SELECT MIN(seq) FROM hub_events", &[])?.get(0);
    let retention_gap =
        from < event_floor || minimum.is_some_and(|minimum| from < minimum.saturating_sub(1));
    Ok(Briefing {
        subscriber,
        delivery,
        from,
        through,
        after,
        work_after,
        next_after,
        next_work_after,
        next_work_done,
        items,
        events,
        retention_gap,
        complete: next_after == through && next_work_done,
    })
}

/// Rechecks inside the write transaction that the token is still usable; the
/// share lock makes a concurrent revoke or disable wait for this transaction.
fn active_token(tx: &mut postgres::Transaction<'_>, token: &str, now: i64) -> Result<Who> {
    let row = tx
        .query_opt(
            "SELECT t.repository, t.name, u.id, u.provider, u.host, u.forge_user_id, u.login
             FROM hub_agent_tokens t JOIN hub_users u ON u.id = t.owner
             WHERE t.id = $1 AND NOT t.revoked AND t.expires_at > $2 AND NOT u.disabled
             FOR SHARE",
            &[&token, &now],
        )?
        .ok_or(HubError::Unauthorized)?;
    Ok(Who {
        token: token.to_owned(),
        repository: row.get(0),
        agent: row.get(1),
        owner: User {
            id: row.get(2),
            identity: ForgeIdentity {
                provider: row.get(3),
                host: row.get(4),
                user_id: row.get(5),
            },
            login: row.get(6),
            disabled: false,
        },
    })
}

fn external_run_row(row: &postgres::Row) -> Result<ExternalRun> {
    let (Some(kind), Some(outcome)) = (
        ExternalKind::parse(row.get(1)),
        ExternalOutcome::parse(row.get(8)),
    ) else {
        return Err(HubError::Unavailable(
            "stored external run is invalid".into(),
        ));
    };
    Ok(ExternalRun {
        provider: row.get(0),
        kind,
        run_id: row.get(2),
        attempt: row.get(3),
        commit_sha: row.get(4),
        branch: row.get(5),
        environment: row.get(6),
        name: row.get(7),
        outcome,
        provider_updated_at: row.get(9),
        html_url: row.get(10),
    })
}

fn publication_row(row: &postgres::Row) -> Result<Publication> {
    Ok(Publication {
        id: row.get(0),
        repository: row.get(1),
        token: row.get(2),
        owner: row.get(20),
        authorization_binding: row.get(21),
        agent: row.get(3),
        owner_login: row.get(4),
        revision: row.get(5),
        event_seq: row.get(6),
        commit: row.get(7),
        branch: row.get(8),
        base: row.get(9),
        manifest: row.get::<_, Option<String>>(10).ok_or_else(|| {
            HubError::Invalid("retained publication exceeds manifest budget".into())
        })?,
        branch_state: row.get(11),
        branch_message: row.get(12),
        pr_number: row.get(13),
        pr_node_id: row.get(14),
        pr_url: row.get(15),
        merge_commit: row.get(16),
        error: row.get(17),
        created_at: row.get(18),
        updated_at: row.get(19),
    })
}

/// `lock` adds `FOR UPDATE OF p` so a concurrent update waits for this transaction.
fn publication_by_id(
    tx: &mut postgres::Transaction<'_>,
    id: &str,
    lock: bool,
) -> Result<Option<Publication>> {
    let tail = if lock { " FOR UPDATE OF p" } else { "" };
    let row = tx.query_opt(
        &format!("{PUBLICATION_SELECT} WHERE p.id = $1{tail}"),
        &[&id],
    )?;
    row.as_ref().map(publication_row).transpose()
}

fn connection_row(row: &postgres::Row) -> ForgeConnection {
    ForgeConnection {
        id: row.get(0),
        provider: row.get(1),
        host: row.get(2),
        kind: row.get(3),
        app_id: row.get(4),
        app_slug: row.get(5),
        acts_as: row.get(6),
        outbound: row.get(7),
        status: row.get(8),
        status_detail: row.get(9),
        next_poll_at: row.get(10),
    }
}

fn connection_by_id(
    client: &mut impl postgres::GenericClient,
    id: &str,
) -> Result<Option<ForgeConnection>> {
    let row = client.query_opt(&format!("{CONNECTION_SELECT} WHERE id = $1"), &[&id])?;
    Ok(row.as_ref().map(connection_row))
}

fn repository_row(row: &postgres::Row) -> ConnectedRepository {
    ConnectedRepository {
        id: row.get(0),
        connection: row.get(1),
        host: row.get(2),
        repository_id: row.get(3),
        full_name: row.get(4),
        node_id: row.get(5),
        installation_id: row.get(6),
        pulls_etag: row.get(7),
    }
}

/// Locks the row: a repository's writes serialize on it.
fn repository_by_rep(tx: &mut postgres::Transaction<'_>, id: &str) -> Result<ConnectedRepository> {
    let row = tx.query_opt(
        &format!("{REPOSITORY_SELECT} WHERE id = $1 FOR UPDATE"),
        &[&id],
    )?;
    row.as_ref()
        .map(repository_row)
        .ok_or_else(|| HubError::Invalid("unknown repository".into()))
}

fn intent_row(row: &postgres::Row) -> (Intent, String, Option<String>) {
    (
        Intent {
            id: row.get(0),
            repository: row.get(1),
            task: row.get(2),
            branch: row.get(3),
            base: row.get(4),
            machine: row.get(5),
            paths: Vec::new(),
            expires_at: row.get(6),
            created_at: row.get(14),
            updated_at: row.get(15),
            token: row.get(7),
            agent: row.get(8),
            owner: User {
                id: row.get(9),
                identity: ForgeIdentity {
                    provider: row.get(10),
                    host: row.get(11),
                    user_id: row.get(12),
                },
                login: row.get(13),
                disabled: false,
            },
            via: Vec::new(),
        },
        row.get(16),
        row.get(17),
    )
}

/// Intents by id, whether or not they are live.
fn load_intents(tx: &mut postgres::Transaction<'_>, ids: &[String]) -> Result<Vec<Intent>> {
    let rows = tx.query(&format!("{INTENT_SELECT} WHERE i.id = ANY($1)"), &[&ids])?;
    finish(rows.iter().map(intent_row).collect())
}

/// Ids of the live intents of other holders in the repository that overlap any of `keys`.
fn probe(
    tx: &mut postgres::Transaction<'_>,
    who: &Who,
    own: &str,
    keys: &[String],
    now: i64,
) -> Result<BTreeSet<String>> {
    let (points, ranges) = probe_set(keys);
    let live = "repository = $1 AND intent <> $2 AND expires_at > $3";
    let mut rows = Vec::new();
    if !points.is_empty() {
        let sql =
            format!("SELECT DISTINCT intent FROM hub_intent_paths WHERE {live} AND key = ANY($4)");
        rows.extend(tx.query(&sql, &[&who.repository, &own, &now, &points])?);
    }
    let sql = format!(
        "SELECT DISTINCT intent FROM hub_intent_paths WHERE {live} AND key >= $4 AND key < $5"
    );
    for (start, end) in &ranges {
        rows.extend(tx.query(&sql, &[&who.repository, &own, &now, start, end])?);
    }
    Ok(rows.iter().map(|row| row.get(0)).collect())
}

/// Announces in one repository serialize, so two racing overlapping ones see each other.
fn lock_repository(tx: &mut postgres::Transaction<'_>, repository: &str) -> Result<()> {
    tx.execute(
        "SELECT pg_advisory_xact_lock($1, hashtext($2))",
        &[&INTENT_LOCK_CLASS, &repository],
    )?;
    Ok(())
}

fn lock_work_delivery(tx: &mut postgres::Transaction<'_>, who: &WorkWho) -> Result<()> {
    let scope = json_string(&(
        &who.owner.id,
        &who.owner.identity.provider,
        &who.owner.identity.host,
        &who.repository,
        who.repository_id,
    ))?;
    tx.execute(
        "SELECT pg_advisory_xact_lock($1, hashtext($2))",
        &[&WORK_DELIVERY_LOCK_CLASS, &scope],
    )?;
    Ok(())
}

fn lock_work_repository(
    tx: &mut postgres::Transaction<'_>,
    provider: &str,
    host: &str,
    repository: &str,
    repository_id: Option<i64>,
) -> Result<()> {
    let scope = json_string(&(provider, host, repository, repository_id))?;
    tx.execute(
        "SELECT pg_advisory_xact_lock($1, hashtext($2))",
        &[&WORK_CAPACITY_LOCK_CLASS, &scope],
    )?;
    Ok(())
}

fn lock_work_repository_shared(
    tx: &mut postgres::Transaction<'_>,
    provider: &str,
    host: &str,
    repository: &str,
    repository_id: Option<i64>,
) -> Result<()> {
    let scope = json_string(&(provider, host, repository, repository_id))?;
    tx.execute(
        "SELECT pg_advisory_xact_lock_shared($1, hashtext($2))",
        &[&WORK_CAPACITY_LOCK_CLASS, &scope],
    )?;
    Ok(())
}

fn lock_work_capacity(tx: &mut postgres::Transaction<'_>, who: &WorkWho) -> Result<()> {
    lock_work_repository(
        tx,
        &who.owner.identity.provider,
        &who.owner.identity.host,
        &who.repository,
        who.repository_id,
    )
}

/// The announce of `intent` (validated into `keys`) inside the caller's transaction.
fn announce_in(
    tx: &mut postgres::Transaction<'_>,
    who: &Who,
    intent: &NewIntent,
    keys: &[String],
    via: &[String],
    now: i64,
) -> Result<Announced> {
    let token = who.token.as_str();
    let paths = display_all(keys);
    let paths_json = serde_json::json!(paths).to_string();
    let expires_at = now + intent.ttl_seconds;
    lock_repository(tx, &who.repository)?;
    tx.execute(
        "DELETE FROM hub_intent_paths WHERE repository = $1 AND expires_at <= $2",
        &[&who.repository, &now],
    )?;
    let existing = tx
        .query_opt(
            "SELECT token, released_at IS NOT NULL, expires_at, task, branch, base, machine, paths
             FROM hub_intents WHERE id = $1",
            &[&intent.id],
        )?
        .map(|row| Existing {
            token: row.get(0),
            released: row.get(1),
            expires_at: row.get(2),
            task: row.get(3),
            branch: row.get(4),
            base: row.get(5),
            machine: row.get(6),
            paths: row.get(7),
        });
    let plan = plan(
        existing.as_ref(),
        token,
        intent,
        &paths_json,
        expires_at,
        now,
    )?;
    // Read before the first append takes the event lock, so the lock is held briefly.
    let mut ids = probe(tx, who, &intent.id, keys, now)?;
    match &plan {
        Plan::Unchanged => {}
        Plan::Renew => {
            tx.execute(
                "UPDATE hub_intents SET expires_at = $2, updated_at = $3 WHERE id = $1",
                &[&intent.id, &expires_at, &now],
            )?;
            tx.execute(
                "UPDATE hub_intent_paths SET expires_at = $2 WHERE intent = $1",
                &[&intent.id, &expires_at],
            )?;
            append(tx, &Event::intent_renewed(&intent.id, expires_at)?)?;
        }
        Plan::Write { .. } => {
            let written = tx.execute(
                "INSERT INTO hub_intents (id, repository, token, task, branch, base, machine,
                     paths, expires_at, created_at, updated_at)
                 VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $10)
                 ON CONFLICT (id) DO UPDATE SET task = excluded.task,
                     branch = excluded.branch, base = excluded.base,
                     machine = excluded.machine, paths = excluded.paths,
                     expires_at = excluded.expires_at, updated_at = excluded.updated_at
                 WHERE hub_intents.token = excluded.token AND hub_intents.released_at IS NULL",
                &[
                    &intent.id,
                    &who.repository,
                    &token,
                    &intent.task,
                    &intent.branch,
                    &intent.base,
                    &intent.machine,
                    &paths_json,
                    &expires_at,
                    &now,
                ],
            )?;
            // Another repository's announce may have inserted this id after our read.
            if written == 0 {
                return Err(HubError::Invalid("intent id is in use".into()));
            }
            tx.execute(
                "DELETE FROM hub_intent_paths WHERE intent = $1",
                &[&intent.id],
            )?;
            tx.execute(
                "INSERT INTO hub_intent_paths (intent, repository, key, expires_at)
                 SELECT $1, $2, unnest($3::text[]), $4::bigint",
                &[&intent.id, &who.repository, &keys, &expires_at],
            )?;
            append(
                tx,
                &Event::intent_announced(who, intent, &paths, expires_at)?,
            )?;
        }
    }
    ids.insert(intent.id.clone());
    let ids: Vec<String> = ids.into_iter().collect();
    let announced = announced(&intent.id, keys, load_intents(tx, &ids)?)?;
    if let Plan::Write { previous } = &plan {
        append_all(tx, &notice_events(who, intent, via, previous, &announced)?)?;
    }
    Ok(announced)
}

/// Releases the token's own intent; true if it was not already released.
fn release_in(
    tx: &mut postgres::Transaction<'_>,
    token: &str,
    intent: &str,
    now: i64,
) -> Result<bool> {
    let changed = tx.execute(
        "UPDATE hub_intents SET released_at = $3, updated_at = $3
         WHERE id = $1 AND token = $2 AND released_at IS NULL",
        &[&intent, &token, &now],
    )?;
    if changed > 0 {
        tx.execute("DELETE FROM hub_intent_paths WHERE intent = $1", &[&intent])?;
        append(tx, &Event::intent_released(intent)?)?;
    }
    Ok(changed > 0)
}

fn release_session_intent(
    tx: &mut postgres::Transaction<'_>,
    token: &str,
    session: &str,
    now: i64,
) -> Result<bool> {
    let intent = tx
        .query_opt(
            "SELECT intent FROM hub_sessions WHERE token=$1 AND session=$2",
            &[&token, &session],
        )?
        .and_then(|row| row.get::<_, Option<String>>(0));
    intent
        .as_deref()
        .map(|intent| release_in(tx, token, intent, now))
        .transpose()
        .map(Option::unwrap_or_default)
}

/// The named session, else the token's only recent hook-connected one, else the default.
fn session_name(
    tx: &mut postgres::Transaction<'_>,
    token: &str,
    requested: Option<&str>,
    now: i64,
) -> Result<String> {
    if let Some(name) = requested {
        if name == DEFAULT_SESSION {
            return Ok(name.to_owned());
        }
        if let Some(row) = tx.query_opt(
            "SELECT canonical_session FROM hub_session_aliases
             WHERE token=$1 AND observed_session=$2",
            &[&token, &name],
        )? {
            return Ok(row.get(0));
        }
        return Ok(name.to_owned());
    }
    let recent = tx.query(
        "SELECT session FROM hub_sessions
         WHERE token = $1 AND updated_at > $2 AND via LIKE '%\"hook:%' LIMIT 2",
        &[&token, &(now - SESSION_TTL_SECONDS)],
    )?;
    Ok(match recent.as_slice() {
        [only] => only.get(0),
        _ => DEFAULT_SESSION.to_owned(),
    })
}

fn alias_hook_session(
    tx: &mut postgres::Transaction<'_>,
    token: &str,
    observed: &str,
    now: i64,
) -> Result<String> {
    if observed == DEFAULT_SESSION {
        return Ok(observed.to_owned());
    }
    if let Some(row) = tx.query_opt(
        "SELECT canonical_session FROM hub_session_aliases
         WHERE token=$1 AND observed_session=$2",
        &[&token, &observed],
    )? {
        return Ok(row.get(0));
    }
    if session_row(tx, token, observed)?.is_some() {
        return Ok(observed.to_owned());
    }
    if tx
        .query_opt(
            "SELECT 1 FROM hub_session_aliases
             WHERE token=$1 AND canonical_session=$2",
            &[&token, &DEFAULT_SESSION],
        )?
        .is_some()
    {
        return Err(HubError::Invalid(
            "durable default work is already bound to another hook session".into(),
        ));
    }
    let eligible = tx
        .query_opt(
            "SELECT 1 WHERE EXISTS (
                 SELECT 1 FROM hub_work_participants p
                 WHERE p.token=$1 AND p.session=$2)",
            &[&token, &DEFAULT_SESSION],
        )?
        .is_some();
    if !eligible {
        return Ok(observed.to_owned());
    }
    let changed = tx.execute(
        "INSERT INTO hub_session_aliases
            (token,observed_session,canonical_session,created_at) VALUES ($1,$2,$3,$4)
         ON CONFLICT DO NOTHING",
        &[&token, &observed, &DEFAULT_SESSION, &now],
    )?;
    if changed == 0 {
        return Err(HubError::Invalid(
            "durable default work is already bound to another hook session".into(),
        ));
    }
    Ok(DEFAULT_SESSION.to_owned())
}

fn session_row(
    tx: &mut postgres::Transaction<'_>,
    token: &str,
    session: &str,
) -> Result<Option<Row>> {
    Ok(tx
        .query_opt(
            "SELECT intent, task, via, notice_cursor, denied, deny_times FROM hub_sessions
             WHERE token = $1 AND session = $2",
            &[&token, &session],
        )?
        .map(|row| Row {
            intent: row.get(0),
            task: row.get(1),
            via: row.get(2),
            cursor: row.get(3),
            denied: row.get(4),
            deny_times: row.get(5),
        }))
}

/// One touch of the session inside the caller's transaction. A call without `report` (made on
/// the session's behalf, or answered without text) tells and consumes nothing.
/// The shared work lock remains held through the intent write, fencing takeovers,
/// finishes and scope changes without a check/announce transaction gap.
fn guard_managed_touch(
    tx: &mut postgres::Transaction<'_>,
    who: &WorkWho,
    name: &str,
    keys: &[String],
) -> Result<()> {
    if keys.is_empty() {
        return Ok(());
    }
    let rows = tx.query(
        "SELECT w.snapshot,p.generation FROM hub_work_items w
         JOIN hub_work_participants p ON p.work=w.id
         WHERE w.provider=$1 AND w.host=$2 AND w.repository=$3
           AND w.repository_id IS NOT DISTINCT FROM $4 AND p.token=$5 AND p.session=$6",
        &[
            &who.owner.identity.provider,
            &who.owner.identity.host,
            &who.repository,
            &who.repository_id,
            &who.token,
            &name,
        ],
    )?;
    if rows.is_empty() {
        return Ok(());
    }
    let mut eligible = Vec::new();
    for row in rows {
        let item: WorkItem = json_value(&row.get::<_, String>(0))?;
        if matches!(item.state, WorkState::Active | WorkState::Blocked)
            && item.generation == row.get::<_, i64>(1)
        {
            eligible.push(item);
        }
    }
    if eligible.len() != 1 || !uncovered_scope(&eligible[0].scope, keys).is_empty() {
        return Err(HubError::Invalid("These files are not covered by a current unfinished assignment. Start or explicitly rejoin the work, or checkpoint the scope before editing.".into()));
    }
    Ok(())
}

fn touch_in(
    tx: &mut postgres::Transaction<'_>,
    who: &Who,
    call: &Checked,
    now: i64,
) -> Result<Touched> {
    let token = who.token.as_str();
    let work = work_who(tx, token, now)?;
    lock_work_repository_shared(
        tx,
        &work.owner.identity.provider,
        &work.owner.identity.host,
        &work.repository,
        work.repository_id,
    )?;
    // Two calls of one session must not each start an intent.
    lock_repository(tx, &who.repository)?;
    tx.execute(
        "DELETE FROM hub_sessions WHERE token = $1 AND updated_at < $2",
        &[&token, &(now - STALE_SESSION_SECONDS)],
    )?;
    let mut name = session_name(tx, token, call.session.as_deref(), now)?;
    if call.via.starts_with("hook:")
        && let Some(observed) = call.session.as_deref()
        && name == observed
    {
        name = alias_hook_session(tx, token, observed, now)?;
    }
    guard_managed_touch(tx, &work, &name, &call.keys)?;
    let mut row = session_row(tx, token, &name)?;
    if row.is_none() && name != DEFAULT_SESSION && call.via.starts_with("hook:") {
        // An agent that called agentscm_start before its first hook keeps that session.
        let adopted = tx.execute(
            "UPDATE hub_sessions SET session = $3
             WHERE token = $1 AND session = $4 AND via = '[\"mcp\"]' AND updated_at > $2",
            &[
                &token,
                &(now - SESSION_TTL_SECONDS),
                &name,
                &DEFAULT_SESSION,
            ],
        )?;
        if adopted > 0 {
            // Requests to and from the session follow it.
            for column in ["holder", "requester"] {
                tx.execute(
                    &format!(
                        "UPDATE hub_handoffs SET {column}_session = $2
                         WHERE {column}_token = $1 AND {column}_session = $3"
                    ),
                    &[&token, &name, &DEFAULT_SESSION],
                )?;
            }
            row = session_row(tx, token, &name)?;
        }
    }
    let (current, lapsed) = match row.as_ref().and_then(|row| row.intent.as_deref()) {
        Some(id) => tx
            .query_opt(
                "SELECT released_at IS NOT NULL, expires_at, created_at, paths
                 FROM hub_intents WHERE id = $1",
                &[&id],
            )?
            .map(|found| {
                let (released, expires, created) = (found.get(0), found.get(1), found.get(2));
                let cause = expiry_cause(released, expires, created, now);
                held(
                    id,
                    released,
                    expires,
                    created,
                    now,
                    &found.get::<_, String>(3),
                )
                .map(|held| (held, cause.map(|cause| (id.to_owned(), cause))))
            })
            .transpose()?
            .unwrap_or((None, None)),
        None => (None, None),
    };
    let step = plan_touch(row.as_ref(), current.as_ref(), call, now)?;
    let own = current.as_ref().map_or("", |held| held.id.as_str());
    let guarded = guard(
        token,
        row.as_ref(),
        current.as_ref(),
        step,
        call,
        now,
        |keys| {
            let ids: Vec<String> = probe(tx, who, own, keys, now)?.into_iter().collect();
            load_intents(tx, &ids)
        },
    )?;
    let step = &guarded.step;
    let cursor = row.as_ref().map_or(0, |row| row.cursor);
    // A refused call reports nothing else, so the notices wait for the next one.
    let notices: Vec<HubEvent> = match &current {
        Some(held) if call.report && !guarded.refused() => tx
            .query(
                "SELECT seq, id, at, kind, subject, data FROM hub_events
                 WHERE seq > $1 AND subject = $2 AND kind IN ('intent_notice', 'pr_notice')
                 ORDER BY seq LIMIT $3",
                &[&cursor, &held.id, &(NOTICES_PER_TOUCH as i64)],
            )?
            .iter()
            .map(|row| HubEvent {
                seq: row.get(0),
                id: row.get(1),
                at: row.get(2),
                kind: row.get(3),
                subject: row.get(4),
                data: row.get(5),
            })
            .collect(),
        _ => Vec::new(),
    };
    let announced = match &step.new {
        Some(new) => Some(announce_in(tx, who, new, &step.keys, &step.via, now)?),
        None => None,
    };
    if let Some(new) = &step.new {
        // Every touch is a sign of life, also when the hold cap leaves the expiry as it was.
        tx.execute(
            "UPDATE hub_intents SET updated_at = $2 WHERE id = $1",
            &[&new.id, &now],
        )?;
    }
    // Not told behind a refusal, which reports nothing else.
    let handoffs = if call.report && !guarded.refused() {
        handoffs_in(tx, token, &name, now)?
    } else {
        Vec::new()
    };
    let cursor = notices.last().map_or(cursor, |event| event.seq);
    tx.execute(
        "INSERT INTO hub_sessions (token, session, intent, task, via, notice_cursor,
             created_at, updated_at, denied, deny_times)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $7, $8, $9)
         ON CONFLICT (token, session) DO UPDATE SET intent = excluded.intent,
             task = excluded.task, via = excluded.via,
             notice_cursor = excluded.notice_cursor, updated_at = excluded.updated_at,
             denied = excluded.denied, deny_times = excluded.deny_times",
        &[
            &token,
            &name,
            &step.new.as_ref().map(|new| &new.id),
            &step.task,
            &serde_json::json!(step.via).to_string(),
            &cursor,
            &now,
            &guarded.denied_json,
            &guarded.times_json,
        ],
    )?;
    if row.is_none() {
        append(tx, &Event::session_started(token, &name, &step.via)?)?;
    } else if step.gained {
        append(tx, &Event::session_connected(token, &name, &step.via)?)?;
    }
    if let Some((intent, cause)) = &lapsed {
        append(tx, &Event::hold_expired(token, &name, intent, cause)?)?;
    }
    for event in guarded.events(token, &name)? {
        append(tx, &event)?;
    }
    let requested = auto_ask(tx, who, &name, &guarded.denied_keys(), now);
    Ok(guarded.touched(token, &name, announced, notices, handoffs, requested))
}

/// Files a handoff request from the session for `key`, if the limits and the holders allow.
#[allow(clippy::too_many_arguments)]
fn ask_in(
    tx: &mut postgres::Transaction<'_>,
    who: &Who,
    name: &str,
    row: &Row,
    key: &str,
    note: Option<&str>,
    auto: bool,
    now: i64,
) -> Result<Asked> {
    let token = who.token.as_str();
    let ids: Vec<String> = tx
        .query(
            "SELECT DISTINCT intent FROM hub_intent_paths
             WHERE repository = $1 AND key = $2 AND expires_at > $3",
            &[&who.repository, &key, &now],
        )?
        .iter()
        .map(|row| row.get(0))
        .collect();
    let holders = load_intents(tx, &ids)?;
    let open: Vec<(String, String)> = tx
        .query(
            "SELECT path, holder_intent FROM hub_handoffs
             WHERE requester_token = $1 AND requester_session = $2
               AND state = 'pending' AND created_at > $3",
            &[&token, &name, &cutoff(now)],
        )?
        .iter()
        .map(|row| (row.get(0), row.get(1)))
        .collect();
    let asked: Vec<String> = open
        .iter()
        .filter(|(asked, _)| asked == key)
        .map(|(_, intent)| intent.clone())
        .collect();
    let queued: Vec<String> = tx
        .query(
            "SELECT holder_intent FROM hub_handoffs
             WHERE repository = $1 AND path = $2 AND state = 'pending'
               AND created_at > $3",
            &[&who.repository, &key, &cutoff(now)],
        )?
        .iter()
        .map(|row| row.get(0))
        .collect();
    let verdict = judge(
        holders,
        (token, &row.via),
        row.intent.as_deref(),
        (&asked, open.len(), &queued),
    );
    let holder = match verdict {
        Verdict::Yours => return Ok(Asked::Yours),
        Verdict::NotHeld => return Ok(Asked::NotHeld),
        Verdict::AlreadyAsked(holder) => return Ok(Asked::AlreadyAsked { holder }),
        Verdict::TooMany => return Ok(Asked::TooMany),
        Verdict::Request(holder) => holder,
    };
    let Some(holder_session) = tx
        .query_opt(
            "SELECT session FROM hub_sessions WHERE intent = $1",
            &[&holder.id],
        )?
        .map(|row| row.get::<_, String>(0))
    else {
        return Ok(Asked::NotHeld);
    };
    tx.execute(
        "DELETE FROM hub_handoffs WHERE repository = $1 AND created_at < $2",
        &[&who.repository, &(now - STALE_SESSION_SECONDS)],
    )?;
    let id = new_id("hof")?;
    tx.execute(
        "INSERT INTO hub_handoffs (id, repository, path, requester_token,
             requester_session, holder_token, holder_session, holder_intent, note,
             state, created_at)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, 'pending', $10)",
        &[
            &id,
            &who.repository,
            &key,
            &token,
            &name,
            &holder.token,
            &holder_session,
            &holder.id,
            &note,
            &now,
        ],
    )?;
    let event = Event::handoff_request(token, &id, name, key, note, &holder, auto)?;
    append(tx, &event)?;
    Ok(Asked::Requested { holder })
}

/// A refused call asks the holder of each contested file to hand it over, as `agentscm_ask`
/// would. Best effort: a failure is logged and rolled back to its savepoint (so the failed
/// statement does not abort the transaction), and the touch and its refusal stand. Returns the
/// files a request was filed for.
fn auto_ask(
    tx: &mut postgres::Transaction<'_>,
    who: &Who,
    name: &str,
    keys: &[String],
    now: i64,
) -> Vec<String> {
    let mut requested = Vec::new();
    for key in keys.iter().take(MAX_OPEN) {
        let mut ask = || -> Result<Asked> {
            tx.batch_execute("SAVEPOINT auto_ask")?;
            let row = session_row(tx, &who.token, name)?
                .ok_or_else(|| HubError::Invalid("unknown session".into()))?;
            let note = clean_note(row.task.as_deref());
            let asked = ask_in(tx, who, name, &row, key, note.as_deref(), true, now)?;
            tx.batch_execute("RELEASE auto_ask")?;
            Ok(asked)
        };
        match ask() {
            Ok(Asked::Requested { .. }) => requested.push(key.clone()),
            Ok(_) => {}
            Err(error) => {
                eprintln!("hub: handoff request for a refused edit: {error}");
                let _ = tx.batch_execute("ROLLBACK TO auto_ask; RELEASE auto_ask");
            }
        }
    }
    requested
}

/// The session's open requests and unseen answers, marked told.
fn handoffs_in(
    tx: &mut postgres::Transaction<'_>,
    token: &str,
    session: &str,
    now: i64,
) -> Result<Vec<HandoffNote>> {
    let requests = tx.query(
        "SELECT h.id, h.path, h.note, t.name, u.login
         FROM hub_handoffs h JOIN hub_agent_tokens t ON t.id = h.requester_token
         JOIN hub_users u ON u.id = t.owner
         WHERE h.holder_token = $1 AND h.holder_session = $2 AND h.state = 'pending'
           AND NOT h.holder_told AND h.created_at > $3
         ORDER BY h.created_at, h.id LIMIT $4",
        &[&token, &session, &cutoff(now), &PER_TOUCH],
    )?;
    let answers = tx.query(
        "SELECT h.id, h.state, h.path, t.name, u.login, h.reply
         FROM hub_handoffs h JOIN hub_agent_tokens t ON t.id = h.holder_token
         JOIN hub_users u ON u.id = t.owner
         WHERE h.requester_token = $1 AND h.requester_session = $2
           AND h.state IN ('granted', 'declined') AND NOT h.requester_told
         ORDER BY h.decided_at, h.id LIMIT $3",
        &[&token, &session, &PER_TOUCH],
    )?;
    let told = |tx: &mut postgres::Transaction<'_>, column: &str, rows: &[postgres::Row]| {
        let ids: Vec<String> = rows.iter().map(|row| row.get(0)).collect();
        tx.execute(
            &format!("UPDATE hub_handoffs SET {column} = TRUE WHERE id = ANY($1)"),
            &[&ids],
        )
    };
    told(tx, "holder_told", &requests)?;
    told(tx, "requester_told", &answers)?;
    let note = |kind, path, note, reason, agent, login| HandoffNote {
        kind,
        path,
        note,
        reason,
        agent,
        login,
        session: session.to_owned(),
    };
    Ok(requests
        .iter()
        .map(|row| {
            note(
                "request",
                row.get(1),
                row.get(2),
                None,
                row.get(3),
                row.get(4),
            )
        })
        .chain(answers.iter().map(|row| {
            let kind = answer_kind(row.get(1));
            note(kind, row.get(2), None, row.get(5), row.get(3), row.get(4))
        }))
        .collect())
}

/// Ends a request without an answer.
fn lapse(tx: &mut postgres::Transaction<'_>, request: &str, now: i64) -> Result<()> {
    tx.execute(
        "UPDATE hub_handoffs SET state = 'lapsed', decided_at = $2 WHERE id = $1",
        &[&request, &now],
    )?;
    Ok(())
}

/// The tokens of one repository as the forge knows it, owned by users of one host and provider.
const TOKEN_SCOPE: &str = "t.repository = $1 AND t.repository_id IS NOT DISTINCT FROM $2
    AND u.host = $3 AND u.provider = $4";

impl Store for PgStore {
    fn backend(&self) -> &'static str {
        "postgres"
    }

    fn ping(&self) -> Result<()> {
        self.run(true, |client| {
            Ok(client.simple_query("SELECT 1").map(drop)?)
        })
    }

    fn upsert_user(&self, identity: &ForgeIdentity, login: &str) -> Result<User> {
        identity.validate()?;
        validate_login(login)?;
        self.run(false, |client| {
            let mut tx = client.transaction()?;
            let id = new_id("usr")?;
            let inserted = tx.execute(
                "INSERT INTO hub_users (id, provider, host, forge_user_id, login, created_at)
                 VALUES ($1, $2, $3, $4, $5, $6) ON CONFLICT DO NOTHING",
                &[
                    &id,
                    &identity.provider,
                    &identity.host,
                    &identity.user_id,
                    &login,
                    &unix_now(),
                ],
            )?;
            let (id, disabled) = if inserted > 0 {
                append(&mut tx, &Event::user_created(&id, identity, login)?)?;
                (id, false)
            } else {
                // The lock serializes concurrent logins for the same identity.
                let row = tx.query_one(
                    "SELECT id, login, disabled FROM hub_users
                     WHERE provider = $1 AND host = $2 AND forge_user_id = $3 FOR UPDATE",
                    &[&identity.provider, &identity.host, &identity.user_id],
                )?;
                let (id, previous): (String, String) = (row.get(0), row.get(1));
                if previous != login {
                    tx.execute(
                        "UPDATE hub_users SET login = $2 WHERE id = $1",
                        &[&id, &login],
                    )?;
                    append(&mut tx, &Event::user_login_changed(&id, &previous, login)?)?;
                }
                (id, row.get(2))
            };
            tx.commit()?;
            Ok(User {
                id,
                identity: identity.clone(),
                login: login.to_owned(),
                disabled,
            })
        })
    }

    fn set_user_disabled(&self, user: &str, disabled: bool) -> Result<()> {
        self.set_disabled(None, user, disabled)
    }

    fn set_user_disabled_by(&self, actor: &str, user: &str, disabled: bool) -> Result<()> {
        self.set_disabled(Some(actor), user, disabled)
    }

    fn insert_agent_token(&self, token: &NewAgentToken) -> Result<String> {
        let now = unix_now();
        token.validate(now)?;
        let id = new_id("tok")?;
        self.run(false, |client| {
            let mut tx = client.transaction()?;
            let inserted = tx.execute(
                "INSERT INTO hub_agent_tokens
                     (id, owner, name, repository, digest, expires_at, created_at, repository_id, read_only)
                 SELECT $1, id, $3, $4, $5, $6, $7, $8, $9 FROM hub_users WHERE id = $2 AND NOT disabled FOR SHARE",
                &[
                    &id,
                    &token.owner,
                    &token.name,
                    &token.repository,
                    &token.digest.as_str(),
                    &token.expires_at,
                    &now,
                    &token.repository_id,
                    &token.read_only,
                ],
            )?;
            if inserted == 0 {
                return Err(HubError::Invalid("unknown or disabled owner".into()));
            }
            append(&mut tx, &Event::token_created(&id, token)?)?;
            tx.commit()?;
            Ok(id.clone())
        })
    }

    fn revoke_agent_token(&self, token: &str) -> Result<bool> {
        self.revoke(None, token)
    }

    fn revoke_owned_agent_token(&self, owner: &str, token: &str) -> Result<bool> {
        self.revoke(Some(owner), token)
    }

    fn list_agent_tokens(&self, owner: &str) -> Result<Vec<AgentToken>> {
        self.run(true, |client| {
            let rows = client.query(
                "SELECT id, name, repository, repository_id, expires_at, revoked, created_at, read_only
                 FROM hub_agent_tokens WHERE owner = $1
                 ORDER BY created_at DESC, id DESC LIMIT $2",
                &[&owner, &LIST_LIMIT],
            )?;
            Ok(rows
                .iter()
                .map(|row| AgentToken {
                    id: row.get(0),
                    name: row.get(1),
                    repository: row.get(2),
                    repository_id: row.get(3),
                    expires_at: row.get(4),
                    revoked: row.get(5),
                    created_at: row.get(6),
                    read_only: row.get(7),
                })
                .collect())
        })
    }

    fn list_users(&self) -> Result<Vec<User>> {
        self.run(true, |client| {
            let rows = client.query(
                "SELECT id, provider, host, forge_user_id, login, disabled FROM hub_users
                 ORDER BY created_at, id LIMIT $1",
                &[&LIST_LIMIT],
            )?;
            Ok(rows
                .iter()
                .map(|row| User {
                    id: row.get(0),
                    identity: ForgeIdentity {
                        provider: row.get(1),
                        host: row.get(2),
                        user_id: row.get(3),
                    },
                    login: row.get(4),
                    disabled: row.get(5),
                })
                .collect())
        })
    }

    fn user_by_id(&self, id: &str) -> Result<Option<User>> {
        self.run(true, |client| {
            Ok(client.query_opt(
                "SELECT id, provider, host, forge_user_id, login, disabled FROM hub_users WHERE id = $1",
                &[&id],
            )?.map(|row| User {
                id: row.get(0),
                identity: ForgeIdentity { provider: row.get(1), host: row.get(2), user_id: row.get(3) },
                login: row.get(4), disabled: row.get(5),
            }))
        })
    }

    fn user_for_identity(&self, identity: &ForgeIdentity) -> Result<Option<User>> {
        identity.validate()?;
        self.run(true, |client| {
            Ok(client.query_opt(
                "SELECT id, provider, host, forge_user_id, login, disabled FROM hub_users WHERE provider=$1 AND host=$2 AND forge_user_id=$3",
                &[&identity.provider,&identity.host,&identity.user_id],
            )?.map(|row|User { id:row.get(0),identity:ForgeIdentity{provider:row.get(1),host:row.get(2),user_id:row.get(3)},login:row.get(4),disabled:row.get(5) }))
        })
    }

    fn record_sign_in(&self, user: &str, method: &str, admin: bool) -> Result<()> {
        self.run(false, |client| {
            let mut tx = client.transaction()?;
            if tx
                .query_opt("SELECT 1 FROM hub_users WHERE id = $1 FOR SHARE", &[&user])?
                .is_none()
            {
                return Err(HubError::Invalid("unknown user".into()));
            }
            append(&mut tx, &Event::user_signed_in(user, method, admin)?)?;
            Ok(tx.commit()?)
        })
    }

    fn authenticate(&self, digest: &TokenDigest, now: i64) -> Result<Option<Principal>> {
        self.run(true, |client| {
            let row = client.query_opt(
                "SELECT t.id, t.name, t.repository, t.expires_at,
                        u.id, u.provider, u.host, u.forge_user_id, u.login, t.repository_id,
                        t.read_only
                 FROM hub_agent_tokens t JOIN hub_users u ON u.id = t.owner
                 WHERE t.digest = $1 AND NOT t.revoked AND t.expires_at > $2 AND NOT u.disabled",
                &[&digest.as_str(), &now],
            )?;
            Ok(row.map(|row| Principal {
                token_id: row.get(0),
                name: row.get(1),
                repository: row.get(2),
                repository_id: row.get(9),
                expires_at: row.get(3),
                read_only: row.get(10),
                owner: User {
                    id: row.get(4),
                    identity: ForgeIdentity {
                        provider: row.get(5),
                        host: row.get(6),
                        user_id: row.get(7),
                    },
                    login: row.get(8),
                    disabled: false,
                },
            }))
        })
    }

    fn announce_intent(&self, token: &str, intent: &NewIntent, now: i64) -> Result<Announced> {
        let keys = intent.validate()?;
        self.run(false, |client| {
            let mut tx = client.transaction()?;
            let who = active_token(&mut tx, token, now)?;
            let announced = announce_in(&mut tx, &who, intent, &keys, &[], now)?;
            tx.commit()?;
            Ok(announced)
        })
    }

    fn renew_intent(
        &self,
        token: &str,
        intent: &str,
        ttl_seconds: i64,
        now: i64,
    ) -> Result<Option<Intent>> {
        validate_ttl(ttl_seconds)?;
        let expires_at = now + ttl_seconds;
        self.run(false, |client| {
            let mut tx = client.transaction()?;
            active_token(&mut tx, token, now)?;
            let changed = tx.execute(
                "UPDATE hub_intents SET expires_at = $3, updated_at = $4
                 WHERE id = $1 AND token = $2 AND released_at IS NULL AND expires_at > $4",
                &[&intent, &token, &expires_at, &now],
            )?;
            if changed == 0 {
                return Ok(None);
            }
            tx.execute(
                "UPDATE hub_intent_paths SET expires_at = $2 WHERE intent = $1",
                &[&intent, &expires_at],
            )?;
            append(&mut tx, &Event::intent_renewed(intent, expires_at)?)?;
            let loaded = load_intents(&mut tx, &[intent.to_owned()])?;
            tx.commit()?;
            Ok(loaded.into_iter().next())
        })
    }

    fn release_intent(&self, token: &str, intent: &str, now: i64) -> Result<bool> {
        self.run(false, |client| {
            let mut tx = client.transaction()?;
            active_token(&mut tx, token, now)?;
            let released = release_in(&mut tx, token, intent, now)?;
            tx.commit()?;
            Ok(released)
        })
    }

    fn touch_session(&self, token: &str, call: &SessionCall, now: i64) -> Result<Touched> {
        let call = check(call)?;
        self.run(false, |client| {
            let mut tx = client.transaction()?;
            let who = active_token(&mut tx, token, now)?;
            let touched = touch_in(&mut tx, &who, &call, now)?;
            tx.commit()?;
            Ok(touched)
        })
    }

    fn end_session(&self, token: &str, session: Option<&str>, now: i64) -> Result<bool> {
        if let Some(session) = session {
            check_name(session)?;
        }
        self.run(false, |client| {
            let mut tx = client.transaction()?;
            active_token(&mut tx, token, now)?;
            let name = session_name(&mut tx, token, session, now)?;
            let Some(row) = session_row(&mut tx, token, &name)? else {
                return Ok(false);
            };
            if let Some(intent) = &row.intent {
                release_in(&mut tx, token, intent, now)?;
            }
            tx.execute(
                "DELETE FROM hub_sessions WHERE token = $1 AND session = $2",
                &[&token, &name],
            )?;
            append(&mut tx, &Event::session_ended(token, &name)?)?;
            tx.commit()?;
            Ok(true)
        })
    }

    fn ask_handoff(
        &self,
        token: &str,
        session: Option<&str>,
        path: &str,
        note: Option<&str>,
        now: i64,
    ) -> Result<Asked> {
        let key = file_key(path)?;
        if let Some(session) = session {
            check_name(session)?;
        }
        let note = clean_note(note);
        self.run(false, |client| {
            let mut tx = client.transaction()?;
            let who = active_token(&mut tx, token, now)?;
            lock_repository(&mut tx, &who.repository)?;
            let name = session_name(&mut tx, token, session, now)?;
            let Some(row) = session_row(&mut tx, token, &name)? else {
                return Err(HubError::Invalid("unknown session".into()));
            };
            let asked = ask_in(
                &mut tx,
                &who,
                &name,
                &row,
                &key,
                note.as_deref(),
                false,
                now,
            )?;
            tx.commit()?;
            Ok(asked)
        })
    }

    fn answer_handoff(
        &self,
        token: &str,
        session: Option<&str>,
        path: &str,
        accept: bool,
        reason: Option<&str>,
        now: i64,
    ) -> Result<Answered> {
        let key = file_key(path)?;
        if let Some(session) = session {
            check_name(session)?;
        }
        let reason = clean_note(reason).filter(|_| !accept);
        self.run(false, |client| {
            let mut tx = client.transaction()?;
            let who = active_token(&mut tx, token, now)?;
            let name = session_name(&mut tx, token, session, now)?;
            let found = tx.query_opt(
                "SELECT h.id, h.requester_token, h.requester_session, h.holder_session,
                        t.name, u.login
                 FROM hub_handoffs h JOIN hub_agent_tokens t ON t.id = h.requester_token
                 JOIN hub_users u ON u.id = t.owner
                 WHERE h.repository = $1 AND h.path = $2 AND h.holder_token = $3
                   AND h.state = 'pending' AND h.created_at > $4
                   AND h.holder_session = $5
                 ORDER BY h.created_at, h.id LIMIT 1",
                &[&who.repository, &key, &token, &cutoff(now), &name],
            )?;
            let Some(request) = found.map(|row| Request {
                id: row.get(0),
                requester_token: row.get(1),
                requester_session: row.get(2),
                holder_session: row.get(3),
                agent: row.get(4),
                login: row.get(5),
            }) else {
                return Ok(Answered::NoRequest);
            };
            // A grant touches the requester's managed assignment. Acquire its
            // work lock before the intent repository lock, just like direct touches.
            let requester_available = if accept {
                match work_who(&mut tx, &request.requester_token, now) {
                    Ok(work) => {
                        lock_work_repository_shared(
                            &mut tx,
                            &work.owner.identity.provider,
                            &work.owner.identity.host,
                            &work.repository,
                            work.repository_id,
                        )?;
                        true
                    }
                    Err(HubError::Unauthorized) => false,
                    Err(error) => return Err(error),
                }
            } else {
                true
            };
            lock_repository(&mut tx, &who.repository)?;
            // The pending request was read before waiting for the locks. Another
            // answer or expiry may have committed while we waited.
            if tx
                .query_opt(
                    "SELECT id FROM hub_handoffs WHERE id=$1 AND state='pending' AND created_at>$2",
                    &[&request.id, &cutoff(now)],
                )?
                .is_none()
            {
                return Ok(Answered::NoRequest);
            }
            // Preserve this operation's unavailable decision even if an owner
            // is re-enabled concurrently: touch_in requires the lock above.
            if !requester_available {
                lapse(&mut tx, &request.id, now)?;
                tx.commit()?;
                return Ok(Answered::NoRequest);
            }
            let requester = (request.agent.clone(), request.login.clone());
            // The holder session's intent now: a hold cap may have renewed it since the request.
            let holder_intent = session_row(&mut tx, token, &request.holder_session)?
                .and_then(|row| row.intent)
                .unwrap_or_default();
            let current = match tx.query_opt(
                "SELECT released_at IS NOT NULL, expires_at, created_at, paths FROM hub_intents
                 WHERE id = $1",
                &[&holder_intent],
            )? {
                Some(found) => held(
                    &holder_intent,
                    found.get(0),
                    found.get(1),
                    found.get(2),
                    now,
                    &found.get::<_, String>(3),
                )?,
                None => None,
            };
            // A request for a file its holder let go of is moot.
            let Some(current) = current.filter(|held| still_holds(held, &key)) else {
                lapse(&mut tx, &request.id, now)?;
                tx.commit()?;
                return Ok(Answered::NotHeld);
            };
            if accept {
                let requester_who = match active_token(&mut tx, &request.requester_token, now) {
                    Ok(requester) => requester,
                    // The requester's token is gone: nobody to hand the file to.
                    Err(HubError::Unauthorized) => {
                        lapse(&mut tx, &request.id, now)?;
                        tx.commit()?;
                        return Ok(Answered::NoRequest);
                    }
                    Err(error) => return Err(error),
                };
                // The requester's session ended: nobody to hand the file to.
                let Some(requester_row) = session_row(
                    &mut tx,
                    &request.requester_token,
                    &request.requester_session,
                )?
                else {
                    lapse(&mut tx, &request.id, now)?;
                    tx.commit()?;
                    return Ok(Answered::NoRequest);
                };
                let left = remaining(&current, &key);
                if left.is_empty() {
                    release_in(&mut tx, token, &current.id, now)?;
                } else {
                    tx.execute(
                        "UPDATE hub_intents SET paths = $2, updated_at = $3 WHERE id = $1",
                        &[&current.id, &serde_json::json!(left).to_string(), &now],
                    )?;
                    tx.execute(
                        "DELETE FROM hub_intent_paths WHERE intent = $1 AND key = $2",
                        &[&current.id, &key],
                    )?;
                }
                let call = grant_call(&request.requester_session, &key, Some(&requester_row.via));
                touch_in(&mut tx, &requester_who, &check(&call)?, now)?;
            }
            tx.execute(
                "UPDATE hub_handoffs SET state = $2, decided_at = $3, reply = $4 WHERE id = $1",
                &[
                    &request.id,
                    &if accept { "granted" } else { "declined" },
                    &now,
                    &reason,
                ],
            )?;
            let kind = if accept {
                "handoff_grant"
            } else {
                "handoff_decline"
            };
            let event = Event::handoff_answer(
                kind,
                token,
                &request.holder_session,
                &key,
                &request,
                reason.as_deref(),
            )?;
            append(&mut tx, &event)?;
            tx.commit()?;
            Ok(if accept {
                Answered::Granted { requester }
            } else {
                Answered::Declined { requester }
            })
        })
    }

    fn live_intents(&self, repository: &str, now: i64, limit: usize) -> Result<Vec<Intent>> {
        self.run(true, |client| {
            let rows = client.query(
                &format!(
                    "{INTENT_SELECT} WHERE i.repository = $1 AND i.released_at IS NULL
                     AND i.expires_at > $2 ORDER BY i.id LIMIT $3"
                ),
                &[&repository, &now, &(limit as i64)],
            )?;
            finish(rows.iter().map(intent_row).collect())
        })
    }

    fn notices_after(&self, token: &str, after: i64, limit: usize) -> Result<Vec<HubEvent>> {
        self.run(true, |client| {
            let rows = client.query(
                "SELECT seq, id, at, kind, subject, data FROM hub_events
                 WHERE seq > $2 AND kind IN ('intent_notice', 'pr_notice')
                   AND subject IN (SELECT id FROM hub_intents WHERE token = $1)
                 ORDER BY seq LIMIT $3",
                &[&token, &after, &event_limit(limit)],
            )?;
            Ok(rows
                .iter()
                .map(|row| HubEvent {
                    seq: row.get(0),
                    id: row.get(1),
                    at: row.get(2),
                    kind: row.get(3),
                    subject: row.get(4),
                    data: row.get(5),
                })
                .collect())
        })
    }

    fn live_sessions(&self, who: &Principal, now: i64, limit: usize) -> Result<Vec<SessionView>> {
        self.run(true, |client| {
            let rows = client.query(
                &format!(
                    "SELECT s.token, t.name, u.login, s.intent, s.task, s.via, s.created_at,
                            s.updated_at, s.session
                     FROM hub_sessions s JOIN hub_agent_tokens t ON t.id = s.token
                     JOIN hub_users u ON u.id = t.owner
                     WHERE {TOKEN_SCOPE} AND NOT t.revoked AND t.expires_at > $5
                       AND NOT u.disabled AND s.updated_at >= $6
                     ORDER BY s.updated_at DESC, s.token, s.session LIMIT $7"
                ),
                &[
                    &who.repository,
                    &who.repository_id,
                    &who.owner.identity.host,
                    &who.owner.identity.provider,
                    &now,
                    &(now - SESSION_TTL_SECONDS),
                    &(limit as i64),
                ],
            )?;
            Ok(rows
                .iter()
                .map(|row| SessionView {
                    key: session_key(row.get(0), row.get(8)),
                    token: row.get(0),
                    agent: row.get(1),
                    owner: row.get(2),
                    intent: row.get(3),
                    task: row.get(4),
                    via: row.get(5),
                    created_at: row.get(6),
                    updated_at: row.get(7),
                })
                .collect())
        })
    }

    fn repository_events(&self, who: &Principal, limit: usize) -> Result<Vec<ActivityEvent>> {
        self.run(true, |client| {
            // Driven from the subject index: the scope's token and intent ids, not every event.
            let rows = client.query(
                &format!(
                    "SELECT e.seq, e.id, e.at, e.kind, e.subject, e.data, t.id, t.name, u.login
                     FROM hub_events e LEFT JOIN hub_intents i ON i.id = e.subject
                     JOIN hub_agent_tokens t ON t.id = COALESCE(i.token, e.subject)
                     JOIN hub_users u ON u.id = t.owner
                     WHERE e.kind IN ({ACTIVITY_KINDS}) AND e.subject IN (
                         SELECT t.id FROM hub_agent_tokens t JOIN hub_users u ON u.id = t.owner
                         WHERE {TOKEN_SCOPE}
                         UNION ALL
                         SELECT i.id FROM hub_intents i JOIN hub_agent_tokens t ON t.id = i.token
                         JOIN hub_users u ON u.id = t.owner WHERE {TOKEN_SCOPE})
                     ORDER BY e.seq DESC LIMIT $5"
                ),
                &[
                    &who.repository,
                    &who.repository_id,
                    &who.owner.identity.host,
                    &who.owner.identity.provider,
                    &event_limit(limit),
                ],
            )?;
            Ok(rows
                .iter()
                .map(|row| ActivityEvent {
                    event: HubEvent {
                        seq: row.get(0),
                        id: row.get(1),
                        at: row.get(2),
                        kind: row.get(3),
                        subject: row.get(4),
                        data: row.get(5),
                    },
                    token: row.get(6),
                    agent: row.get(7),
                    owner: row.get(8),
                })
                .collect())
        })
    }

    fn insert_connection(&self, new: &NewConnection, actor: &str) -> Result<ForgeConnection> {
        new.validate()?;
        self.run(false, |client| {
            let mut tx = client.transaction()?;
            let inserted = tx.execute(
                "INSERT INTO hub_connections
                     (id, provider, host, kind, app_id, app_slug, acts_as, status, created_at)
                 VALUES ($1, $2, $3, $4, $5, $6, $7, 'enabled', $8) ON CONFLICT DO NOTHING",
                &[
                    &new.id,
                    &new.provider,
                    &new.host,
                    &new.kind,
                    &new.app_id,
                    &new.app_slug,
                    &new.acts_as,
                    &unix_now(),
                ],
            )?;
            if inserted == 0 {
                return Err(HubError::Invalid("connection id is in use".into()));
            }
            append(&mut tx, &Event::connection_created(new, actor)?)?;
            let created = connection_by_id(&mut tx, &new.id)?;
            tx.commit()?;
            created.ok_or_else(|| HubError::Unavailable("connection not found".into()))
        })
    }

    fn connections(&self) -> Result<Vec<ForgeConnection>> {
        self.run(true, |client| {
            let rows = client.query(
                &format!("{CONNECTION_SELECT} ORDER BY created_at, id LIMIT $1"),
                &[&LIST_LIMIT],
            )?;
            Ok(rows.iter().map(connection_row).collect())
        })
    }

    fn connection(&self, id: &str) -> Result<Option<ForgeConnection>> {
        self.run(true, |client| connection_by_id(client, id))
    }

    fn pat_connection(&self, host: &str) -> Result<Option<ForgeConnection>> {
        self.run(true, |client| {
            let row = client.query_opt(
                &format!(
                    "{CONNECTION_SELECT} WHERE kind = 'pat' AND host = $1 ORDER BY created_at, id LIMIT 1"
                ),
                &[&host],
            )?;
            Ok(row.as_ref().map(connection_row))
        })
    }

    fn set_connection_status(&self, id: &str, status: &str, detail: Option<&str>) -> Result<()> {
        validate_status(status, detail)?;
        self.run(false, |client| {
            let mut tx = client.transaction()?;
            let changed = tx.execute(
                "UPDATE hub_connections SET status = $2, status_detail = $3
                 WHERE id = $1 AND (status <> $2 OR status_detail IS DISTINCT FROM $3)",
                &[&id, &status, &detail],
            )?;
            if changed > 0 {
                append(&mut tx, &Event::connection_status(id, status, detail)?)?;
            } else if connection_by_id(&mut tx, id)?.is_none() {
                return Err(HubError::Invalid("unknown connection".into()));
            }
            Ok(tx.commit()?)
        })
    }

    fn set_connection_outbound(&self, id: &str, enabled: bool, actor: &str) -> Result<()> {
        self.run(false, |client| {
            let mut tx = client.transaction()?;
            let changed = tx.execute(
                "UPDATE hub_connections SET outbound = $2 WHERE id = $1 AND outbound <> $2",
                &[&id, &enabled],
            )?;
            if changed > 0 {
                append(&mut tx, &Event::connection_outbound(id, enabled, actor)?)?;
            } else if connection_by_id(&mut tx, id)?.is_none() {
                return Err(HubError::Invalid("unknown connection".into()));
            }
            Ok(tx.commit()?)
        })
    }

    fn get_setting(&self, key: &str) -> Result<Option<String>> {
        self.run(true, |client| {
            Ok(client
                .query_opt("SELECT value FROM hub_settings WHERE key = $1", &[&key])?
                .map(|row| row.get(0)))
        })
    }

    fn set_setting(&self, key: &str, value: &str, actor: &str) -> Result<()> {
        let now = unix_now();
        self.run(false, |client| {
            client.execute(
                "INSERT INTO hub_settings (key, value, updated_at, updated_by) VALUES ($1, $2, $3, $4)
                 ON CONFLICT (key) DO UPDATE SET value = $2, updated_at = $3, updated_by = $4",
                &[&key, &value, &now, &actor],
            )?;
            Ok(())
        })
    }

    fn set_next_poll(&self, id: &str, at: i64) -> Result<()> {
        self.run(false, |client| {
            client.execute(
                "UPDATE hub_connections SET next_poll_at = $2 WHERE id = $1",
                &[&id, &at],
            )?;
            Ok(())
        })
    }

    fn upsert_repository(
        &self,
        connection: &str,
        host: &str,
        repository_id: i64,
        full_name: &str,
        node_id: Option<&str>,
        installation_id: Option<i64>,
    ) -> Result<ConnectedRepository> {
        validate_repository_row(repository_id, full_name)?;
        self.run(false, |client| {
            let mut tx = client.transaction()?;
            let connected_host = tx
                .query_opt("SELECT host FROM hub_connections WHERE id = $1", &[&connection])?
                .map(|row| row.get::<_, String>(0));
            if connected_host.as_deref() != Some(host) {
                return Err(HubError::Invalid("unknown connection for that host".into()));
            }
            let id = new_id("rep")?;
            let inserted = tx.execute(
                "INSERT INTO hub_repositories
                     (id, connection, host, repository_id, full_name, node_id, installation_id, created_at)
                 VALUES ($1, $2, $3, $4, $5, $6, $7, $8) ON CONFLICT (host, repository_id) DO NOTHING",
                &[&id, &connection, &host, &repository_id, &full_name, &node_id, &installation_id, &unix_now()],
            )?;
            let repo = if inserted > 0 {
                let repo = ConnectedRepository {
                    id,
                    connection: connection.to_owned(),
                    host: host.to_owned(),
                    repository_id,
                    full_name: full_name.to_owned(),
                    node_id: node_id.map(str::to_owned),
                    installation_id,
                    pulls_etag: None,
                };
                append(&mut tx, &Event::repository_connected(&repo)?)?;
                repo
            } else {
                let row = tx.query_one(
                    &format!("{REPOSITORY_SELECT} WHERE host = $1 AND repository_id = $2 FOR UPDATE"),
                    &[&host, &repository_id],
                )?;
                let mut repo = repository_row(&row);
                if repo.connection == connection {
                    if repo.full_name != full_name {
                        let previous = std::mem::replace(&mut repo.full_name, full_name.to_owned());
                        append(&mut tx, &Event::repository_renamed(&repo, &previous)?)?;
                    }
                    // Not events: they say how to reach the repository, not what it is.
                    repo.node_id = node_id.map(str::to_owned).or(repo.node_id);
                    repo.installation_id = installation_id.or(repo.installation_id);
                    tx.execute(
                        "UPDATE hub_repositories SET full_name = $2, node_id = $3, installation_id = $4 WHERE id = $1",
                        &[&repo.id, &repo.full_name, &repo.node_id, &repo.installation_id],
                    )?;
                }
                repo
            };
            tx.commit()?;
            Ok(repo)
        })
    }

    fn repositories(&self, connection: &str) -> Result<Vec<ConnectedRepository>> {
        self.repository_query(
            "WHERE connection = $1 ORDER BY created_at, id LIMIT $2",
            &[&connection, &LIST_LIMIT],
        )
    }

    fn repository_named(&self, host: &str, full_name: &str) -> Result<Option<ConnectedRepository>> {
        Ok(self
            .repository_query(
                "WHERE host = $1 AND lower(full_name) = lower($2) ORDER BY created_at, id LIMIT 1",
                &[&host, &full_name],
            )?
            .into_iter()
            .next())
    }

    fn repository_by_id(
        &self,
        host: &str,
        repository_id: i64,
    ) -> Result<Option<ConnectedRepository>> {
        Ok(self
            .repository_query(
                "WHERE host = $1 AND repository_id = $2",
                &[&host, &repository_id],
            )?
            .into_iter()
            .next())
    }

    fn repositories_on(&self, host: &str, limit: usize) -> Result<Vec<ConnectedRepository>> {
        self.repository_query(
            "WHERE host = $1 ORDER BY full_name COLLATE \"C\", id LIMIT $2",
            &[&host, &event_limit(limit)],
        )
    }

    fn set_pulls_etag(&self, repo: &str, etag: Option<&str>) -> Result<()> {
        self.run(false, |client| {
            client.execute(
                "UPDATE hub_repositories SET pulls_etag = $2 WHERE id = $1",
                &[&repo, &etag],
            )?;
            Ok(())
        })
    }

    fn record_delivery(&self, connection: &str, delivery: &str, now: i64) -> Result<bool> {
        self.run(false, |client| {
            client.execute(
                "DELETE FROM hub_webhook_deliveries WHERE received_at < $1",
                &[&(now - DELIVERY_KEEP_SECONDS)],
            )?;
            let inserted = client.execute(
                "INSERT INTO hub_webhook_deliveries (connection, delivery, received_at)
                 VALUES ($1, $2, $3) ON CONFLICT DO NOTHING",
                &[&connection, &delivery, &now],
            )?;
            Ok(inserted > 0)
        })
    }

    fn observe(
        &self,
        repo: &str,
        kind: &str,
        key: &str,
        data: &str,
        observed_at: i64,
    ) -> Result<Observed> {
        validate_observation(kind, key, data)?;
        let (observed_kind, _) = observation_kinds(kind)?;
        self.run(false, |client| {
            let mut tx = client.transaction()?;
            // The repository row lock serializes its observations.
            let repository = repository_by_rep(&mut tx, repo)?;
            let existing = tx
                .query_opt(
                    "SELECT data, observed_at, deleted FROM hub_observations
                     WHERE repository = $1 AND kind = $2 AND key = $3",
                    &[&repo, &kind, &key],
                )?
                .map(|row| {
                    (
                        row.get::<_, String>(0),
                        row.get::<_, i64>(1),
                        row.get::<_, bool>(2),
                    )
                });
            let previous = match existing {
                Some((_, at, _)) if at > observed_at => return Ok(Observed::Stale),
                Some((old, _, false)) if old == data => {
                    tx.execute(
                        "UPDATE hub_observations SET observed_at = $4
                         WHERE repository = $1 AND kind = $2 AND key = $3",
                        &[&repo, &kind, &key, &observed_at],
                    )?;
                    tx.commit()?;
                    return Ok(Observed::Unchanged);
                }
                Some((old, _, deleted)) => (!deleted).then_some(old),
                None => None,
            };
            tx.execute(
                "INSERT INTO hub_observations (repository, kind, key, data, observed_at, deleted)
                 VALUES ($1, $2, $3, $4, $5, FALSE)
                 ON CONFLICT (repository, kind, key) DO UPDATE SET data = excluded.data,
                     observed_at = excluded.observed_at, deleted = FALSE",
                &[&repo, &kind, &key, &data, &observed_at],
            )?;
            append(
                &mut tx,
                &Event::observation(observed_kind, &repository, data)?,
            )?;
            tx.commit()?;
            Ok(Observed::Changed { previous })
        })
    }

    fn forget(
        &self,
        repo: &str,
        kind: &str,
        key: &str,
        observed_at: i64,
        data: &str,
        event: &str,
    ) -> Result<bool> {
        validate_observation(kind, key, data)?;
        let gone_kind = gone_event(kind, event)?;
        self.run(false, |client| {
            let mut tx = client.transaction()?;
            let repository = repository_by_rep(&mut tx, repo)?;
            let existing = tx
                .query_opt(
                    "SELECT observed_at, deleted FROM hub_observations
                     WHERE repository = $1 AND kind = $2 AND key = $3",
                    &[&repo, &kind, &key],
                )?
                .map(|row| (row.get::<_, i64>(0), row.get::<_, bool>(1)));
            let gone = match existing {
                Some((at, _)) if at > observed_at => return Ok(false),
                Some((_, deleted)) => deleted,
                // Never seen: leave a marker so an older read cannot add it later.
                None => true,
            };
            tx.execute(
                "INSERT INTO hub_observations (repository, kind, key, data, observed_at, deleted)
                 VALUES ($1, $2, $3, $4, $5, TRUE)
                 ON CONFLICT (repository, kind, key) DO UPDATE SET data = excluded.data,
                     observed_at = excluded.observed_at, deleted = TRUE",
                &[&repo, &kind, &key, &data, &observed_at],
            )?;
            // An already-forgotten row only moves its read time forward.
            if !gone {
                append(&mut tx, &Event::observation(gone_kind, &repository, data)?)?;
            }
            tx.commit()?;
            Ok(!gone)
        })
    }

    fn observations(&self, repo: &str, kind: &str) -> Result<Vec<Observation>> {
        observation_kinds(kind)?;
        self.run(true, |client| {
            let rows = client.query(
                "SELECT kind, key, data, observed_at FROM hub_observations
                 WHERE repository = $1 AND kind = $2 AND NOT deleted
                 ORDER BY key COLLATE \"C\" LIMIT $3",
                &[&repo, &kind, &LIST_LIMIT],
            )?;
            Ok(rows
                .iter()
                .map(|row| Observation {
                    kind: row.get(0),
                    key: row.get(1),
                    data: row.get(2),
                    observed_at: row.get(3),
                })
                .collect())
        })
    }

    fn prune_tombstones(&self, before: i64) -> Result<usize> {
        self.run(false, |client| {
            let removed = client.execute(
                "DELETE FROM hub_observations WHERE deleted AND observed_at < $1",
                &[&before],
            )?;
            Ok(removed as usize)
        })
    }

    fn outbound(&self, repo: &str, number: i64) -> Result<Option<Outbound>> {
        self.run(true, |client| {
            let row = client.query_opt(
                "SELECT comment_id, body_sha256, status_sha, status_description
                 FROM hub_outbound WHERE repository = $1 AND number = $2",
                &[&repo, &number],
            )?;
            Ok(row.map(|row| Outbound {
                comment_id: row.get(0),
                body_sha256: row.get(1),
                status_sha: row.get(2),
                status_description: row.get(3),
            }))
        })
    }

    fn set_outbound_comment(
        &self,
        repo: &str,
        number: i64,
        comment_id: i64,
        body_sha256: &str,
    ) -> Result<()> {
        self.run(false, |client| {
            let mut tx = client.transaction()?;
            let repository = repository_by_rep(&mut tx, repo)?;
            tx.execute(
                "INSERT INTO hub_outbound (repository, number, comment_id, body_sha256)
                 VALUES ($1, $2, $3, $4)
                 ON CONFLICT (repository, number) DO UPDATE SET
                     comment_id = excluded.comment_id, body_sha256 = excluded.body_sha256",
                &[&repo, &number, &comment_id, &body_sha256],
            )?;
            append(
                &mut tx,
                &Event::pr_comment_written(&repository, number, comment_id)?,
            )?;
            Ok(tx.commit()?)
        })
    }

    fn set_outbound_status(
        &self,
        repo: &str,
        number: i64,
        sha: &str,
        description: &str,
    ) -> Result<()> {
        self.run(false, |client| {
            let mut tx = client.transaction()?;
            let repository = repository_by_rep(&mut tx, repo)?;
            tx.execute(
                "INSERT INTO hub_outbound (repository, number, status_sha, status_description)
                 VALUES ($1, $2, $3, $4)
                 ON CONFLICT (repository, number) DO UPDATE SET
                     status_sha = excluded.status_sha, status_description = excluded.status_description",
                &[&repo, &number, &sha, &description],
            )?;
            append(&mut tx, &Event::pr_status_written(&repository, number, sha, description)?)?;
            Ok(tx.commit()?)
        })
    }

    fn forge_check(&self, repo: &str, number: i64, context: &str) -> Result<Option<ForgeCheck>> {
        self.run(true, |client| {
            let row = client.query_opt(
                "SELECT head_sha, state, binding_digest, description, phase, generation, updated_at
                 FROM hub_forge_checks WHERE repository = $1 AND number = $2 AND context = $3",
                &[&repo, &number, &context],
            )?;
            Ok(row.map(|row| ForgeCheck {
                head_sha: row.get(0),
                state: row.get(1),
                binding_digest: row.get(2),
                description: row.get(3),
                phase: row.get(4),
                generation: row.get(5),
                updated_at: row.get(6),
            }))
        })
    }

    fn forge_checks_showing_success(
        &self,
        repo: &str,
        full_name: &str,
        enabled: &[String],
        limit: usize,
    ) -> Result<Vec<(i64, String, ForgeCheck)>> {
        self.run(true, |client| {
            let limit = limit as i64;
            Ok(client
                .query(
                    "SELECT number, context, head_sha, state, binding_digest, description, phase,
                            generation, updated_at
                     FROM hub_forge_checks
                     WHERE repository = $1 AND (state = 'success' OR phase = 'attempting')
                       AND context <> ALL($3)
                       AND (SELECT q.merge_commit FROM hub_publications q
                            WHERE lower(q.repository) = lower($2)
                              AND q.pr_number = hub_forge_checks.number
                            ORDER BY q.created_at DESC, q.id DESC LIMIT 1) IS NULL
                     ORDER BY number, context LIMIT $4",
                    &[&repo, &full_name, &enabled, &limit],
                )?
                .iter()
                .map(|row| {
                    (
                        row.get(0),
                        row.get(1),
                        ForgeCheck {
                            head_sha: row.get(2),
                            state: row.get(3),
                            binding_digest: row.get(4),
                            description: row.get(5),
                            phase: row.get(6),
                            generation: row.get(7),
                            updated_at: row.get(8),
                        },
                    )
                })
                .collect())
        })
    }

    fn reserve_forge_check(
        &self,
        repo: &str,
        number: i64,
        context: &str,
        expected: Option<i64>,
        new: &NewForgeCheck,
        now: i64,
    ) -> Result<ForgeCheckReserve> {
        validate_check_key(number, context)?;
        new.validate()?;
        self.run(false, |client| {
            let (changed, generation) = match expected {
                None => (
                    client.execute(
                        "INSERT INTO hub_forge_checks (repository, number, context, head_sha,
                             state, binding_digest, description, phase, generation, updated_at)
                         VALUES ($1, $2, $3, $4, $5, $6, $7, 'attempting', 1, $8)
                         ON CONFLICT (repository, number, context) DO NOTHING",
                        &[
                            &repo,
                            &number,
                            &context,
                            &new.head_sha,
                            &new.state,
                            &new.binding_digest,
                            &new.description,
                            &now,
                        ],
                    )?,
                    1,
                ),
                Some(expected) => (
                    client.execute(
                        "UPDATE hub_forge_checks SET head_sha = $5, state = $6,
                             binding_digest = $7, description = $8, phase = 'attempting',
                             generation = generation + 1, updated_at = $9
                         WHERE repository = $1 AND number = $2 AND context = $3
                             AND generation = $4",
                        &[
                            &repo,
                            &number,
                            &context,
                            &expected,
                            &new.head_sha,
                            &new.state,
                            &new.binding_digest,
                            &new.description,
                            &now,
                        ],
                    )?,
                    expected.saturating_add(1),
                ),
            };
            Ok(if changed == 1 {
                ForgeCheckReserve::Reserved(generation)
            } else {
                ForgeCheckReserve::Conflict
            })
        })
    }

    fn complete_forge_check(
        &self,
        repo: &str,
        number: i64,
        context: &str,
        generation: i64,
        phase: &str,
        now: i64,
    ) -> Result<bool> {
        validate_check_key(number, context)?;
        validate_check_phase(phase)?;
        self.run(false, |client| {
            Ok(client.execute(
                "UPDATE hub_forge_checks SET phase = $6, generation = generation + 1,
                     updated_at = $5
                 WHERE repository = $1 AND number = $2 AND context = $3 AND generation = $4",
                &[&repo, &number, &context, &generation, &now, &phase],
            )? == 1)
        })
    }

    fn observe_external_run(&self, repo: &str, run: &ExternalRun) -> Result<ExternalObserved> {
        run.validate()?;
        let kind = run.kind.as_str();
        let outcome = run.outcome.as_str();
        self.run(false, |client| {
            let mut tx = client.transaction()?;
            // The repository row lock serializes its observations.
            repository_by_rep(&mut tx, repo)?;
            let stored = tx
                .query_opt(
                    &format!(
                        "SELECT {EXTERNAL_RUN_COLUMNS} FROM hub_external_runs
                         WHERE repository = $1 AND provider = $2 AND kind = $3 AND run_id = $4
                             AND attempt = $5"
                    ),
                    &[&repo, &run.provider, &kind, &run.run_id, &run.attempt],
                )?
                .map(|row| external_run_row(&row))
                .transpose()?;
            match &stored {
                Some(stored) if stored == run => return Ok(ExternalObserved::Unchanged),
                Some(stored) if !supersedes(stored, run) => return Ok(ExternalObserved::Stale),
                _ => {}
            }
            tx.execute(
                "INSERT INTO hub_external_runs (repository, provider, kind, run_id, attempt,
                     commit_sha, branch, environment, name, outcome, provider_updated_at,
                     html_url)
                 VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12)
                 ON CONFLICT (repository, provider, kind, run_id, attempt) DO UPDATE SET
                     commit_sha = excluded.commit_sha, branch = excluded.branch,
                     environment = excluded.environment, name = excluded.name,
                     outcome = excluded.outcome,
                     provider_updated_at = excluded.provider_updated_at,
                     html_url = excluded.html_url",
                &[
                    &repo,
                    &run.provider,
                    &kind,
                    &run.run_id,
                    &run.attempt,
                    &run.commit_sha,
                    &run.branch,
                    &run.environment,
                    &run.name,
                    &outcome,
                    &run.provider_updated_at,
                    &run.html_url,
                ],
            )?;
            append(&mut tx, &Event::external_run_observed(repo, run)?)?;
            tx.commit()?;
            Ok(ExternalObserved::Changed)
        })
    }

    fn external_runs(
        &self,
        repo: &str,
        commit_sha: &str,
        kind: ExternalKind,
        limit: usize,
    ) -> Result<Vec<ExternalRun>> {
        self.run(true, |client| {
            let limit = limit as i64;
            let kind = kind.as_str();
            client
                .query(
                    &format!(
                        "SELECT {EXTERNAL_RUN_COLUMNS} FROM hub_external_runs
                         WHERE repository = $1 AND commit_sha = $2 AND kind = $3
                         ORDER BY COALESCE(environment, '') COLLATE \"C\", run_id, attempt
                         LIMIT $4"
                    ),
                    &[&repo, &commit_sha, &kind, &limit],
                )?
                .iter()
                .map(external_run_row)
                .collect()
        })
    }

    fn mark_external_reconciled(
        &self,
        repo: &str,
        commit_sha: &str,
        at: i64,
        complete: bool,
    ) -> Result<()> {
        self.run(false, |client| {
            client.execute(
                "INSERT INTO hub_external_reconciles
                     (repository, commit_sha, reconciled_at, complete)
                 VALUES ($1, $2, $3, $4)
                 ON CONFLICT (repository, commit_sha) DO UPDATE SET
                     reconciled_at = excluded.reconciled_at, complete = excluded.complete",
                &[&repo, &commit_sha, &at, &complete],
            )?;
            Ok(())
        })
    }

    fn external_reconciled(&self, repo: &str, commit_sha: &str) -> Result<Option<(i64, bool)>> {
        self.run(true, |client| {
            Ok(client
                .query_opt(
                    "SELECT reconciled_at, complete FROM hub_external_reconciles
                     WHERE repository = $1 AND commit_sha = $2",
                    &[&repo, &commit_sha],
                )?
                .map(|row| (row.get(0), row.get(1))))
        })
    }

    fn merged_publications(
        &self,
        repository: &str,
        after: &str,
        oldest: i64,
        limit: usize,
    ) -> Result<Vec<Publication>> {
        self.run(true, |client| {
            let limit = limit as i64;
            client
                .query(
                    &format!(
                        "{PUBLICATION_SELECT} WHERE lower(p.repository) = lower($1) AND p.id > $2
                         AND p.merge_commit IS NOT NULL AND p.updated_at >= $3
                         ORDER BY p.id LIMIT $4"
                    ),
                    &[&repository, &after, &oldest, &limit],
                )?
                .iter()
                .map(publication_row)
                .collect()
        })
    }

    fn append_pr_notices(&self, notices: &[(String, String)]) -> Result<()> {
        let events = notices
            .iter()
            .map(|(intent, data)| Event::pr_notice(intent, data))
            .collect::<Result<Vec<_>>>()?;
        self.run(false, |client| {
            let mut tx = client.transaction()?;
            append_all(&mut tx, &events)?;
            Ok(tx.commit()?)
        })
    }

    fn create_bound_publication(
        &self,
        token: &str,
        new: &NewPublication,
        binding: Option<&str>,
        now: i64,
    ) -> Result<Create> {
        if binding.is_some_and(|value| {
            value.len() > 65536 || serde_json::from_str::<serde_json::Value>(value).is_err()
        }) {
            return Err(HubError::Invalid(
                "invalid publication authorization binding".into(),
            ));
        }
        self.run(false, |client| {
            let mut tx = client.transaction()?;
            let who = active_token(&mut tx, token, now)?;
            new.validate(&who.repository)?;
            tx.execute(
                "SELECT pg_advisory_xact_lock($1, hashtext($2))",
                &[&PUBLICATION_LOCK_CLASS, &token],
            )?;
            let m = &new.manifest;
            let manifest = m.canonical();
            if let Some(found) = publication_by_id(&mut tx, &new.id, false)? {
                return Ok(if found.authorization_binding.as_deref() == binding { Create::of_existing(found, &who.repository, &manifest) } else { Create::Conflict });
            }
            // Serialize candidate/integrated reservations for this repository/branch,
            // including tokens owned by different actors.
            tx.execute("SELECT pg_advisory_xact_lock($1, hashtext($2))", &[&PUBLICATION_LOCK_CLASS, &format!("branch:{}:{}", who.repository, m.branch)])?;
            let head: Option<String> = tx.query_opt("SELECT publication FROM hub_candidate_heads WHERE repository=$1 AND branch=$2", &[&who.repository, &m.branch])?.map(|r| r.get(0));
            let previous = match &head { Some(id) => publication_by_id(&mut tx,id,false)?, None => None };
            if head.is_some() && previous.is_none() { return Err(HubError::Unavailable("candidate lineage record missing".into())); }
            if binding.is_none() && previous.as_ref().is_some_and(|p| p.authorization_binding.is_some()) {
                return Ok(Create::Conflict);
            }
            if !super::publish::candidate_continuation(new, previous.as_ref(), &who.owner.id)? { return Ok(Create::Conflict); }
            if m.candidate.is_some() && head.is_none() {
                let occupied: bool = tx.query_one("SELECT EXISTS(SELECT 1 FROM hub_publications WHERE repository=$1 AND branch=$2)", &[&who.repository, &m.branch])?.get(0);
                if occupied { return Ok(Create::Conflict); }
            }
            if m.candidate.is_some() {
                tx.execute("INSERT INTO hub_candidate_heads VALUES($1,$2,$3) ON CONFLICT(repository,branch) DO UPDATE SET publication=excluded.publication", &[&who.repository, &m.branch, &new.id])?;
            }
            let unfinished: i64 = tx
                .query_one(
                    "SELECT count(*) FROM hub_publications
                     WHERE token = $1 AND pr_number IS NULL AND branch_state <> 'rejected'",
                    &[&token],
                )?
                .get(0);
            if unfinished >= MAX_UNFINISHED {
                return Ok(Create::TooMany);
            }
            let inserted = tx.execute(
                "INSERT INTO hub_publications (id, repository, token, revision, event_seq,
                     commit_sha, branch, base, manifest, created_at, updated_at, authorization_binding)
                 VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $10, $11) ON CONFLICT (id) DO NOTHING",
                &[
                    &new.id,
                    &who.repository,
                    &token,
                    &m.revision,
                    &m.event_seq,
                    &m.commit,
                    &m.branch,
                    &m.base,
                    &manifest,
                    &now,
                    &binding,
                ],
            )?;
            if inserted > 0 {
                append(
                    &mut tx,
                    &Event::publication_requested(&who.repository, new)?,
                )?;
            }
            let found = publication_by_id(&mut tx, &new.id, false)?.ok_or_else(|| {
                HubError::Unavailable("publication not found after insert".into())
            })?;
            tx.commit()?;
            Ok(if found.authorization_binding.as_deref() == binding { Create::of_existing(found, &who.repository, &manifest) } else { Create::Conflict })
        })
    }

    fn publication(&self, repository: &str, id: &str) -> Result<Option<Publication>> {
        self.run(true, |client| {
            let mut tx = client.transaction()?;
            Ok(publication_by_id(&mut tx, id, false)?.filter(|p| p.repository == repository))
        })
    }

    fn publications(
        &self,
        repository: &str,
        before: Option<&str>,
        limit: usize,
    ) -> Result<Vec<Publication>> {
        self.run(true, |client| {
            let limit = limit as i64;
            client
                .query(
                    &format!(
                        "{PUBLICATION_SELECT} WHERE p.repository = $1
                         AND ($2::text IS NULL OR (p.created_at, p.id) < (SELECT q.created_at, q.id
                             FROM hub_publications q WHERE q.repository = $1 AND q.id = $2))
                         ORDER BY p.created_at DESC, p.id DESC LIMIT $3"
                    ),
                    &[&repository, &before, &limit],
                )?
                .iter()
                .map(publication_row)
                .collect()
        })
    }

    fn pull_publications(
        &self,
        repository: &str,
        after: i64,
        limit: usize,
    ) -> Result<Vec<Publication>> {
        self.run(true, |client| {
            let limit = limit as i64;
            client
                .query(
                    &format!(
                        "{PUBLICATION_SELECT} WHERE lower(p.repository) = lower($1) AND p.pr_number > $2
                         AND p.merge_commit IS NULL
                         AND p.id = (SELECT q.id FROM hub_publications q
                             WHERE lower(q.repository) = lower($1) AND q.pr_number = p.pr_number
                             ORDER BY q.created_at DESC, q.id DESC LIMIT 1)
                         ORDER BY p.pr_number LIMIT $3"
                    ),
                    &[&repository, &after, &limit],
                )?
                .iter()
                .map(publication_row)
                .collect()
        })
    }

    fn update_publication(&self, id: &str, update: &Update, now: i64) -> Result<Publication> {
        self.run(false, |client| {
            let mut tx = client.transaction()?;
            // Same branch lock and ordering as create_publication: a successor
            // reservation and an old candidate's dispatch fence are indivisible.
            if matches!(update, Update::PullRequestAttempt) {
                let current = publication_by_id(&mut tx, id, false)?
                    .ok_or_else(|| HubError::Invalid("unknown publication".into()))?;
                if current.event_seq.is_none() {
                    tx.execute("SELECT pg_advisory_xact_lock($1, hashtext($2))", &[&PUBLICATION_LOCK_CLASS, &format!("branch:{}:{}", current.repository, current.branch)])?;
                    let head: Option<String> = tx.query_opt("SELECT publication FROM hub_candidate_heads WHERE repository=$1 AND branch=$2", &[&current.repository, &current.branch])?.map(|r| r.get(0));
                    if head.as_deref() != Some(id) {
                        return Err(HubError::Invalid("candidate was superseded before PR attempt".into()));
                    }
                }
            }
            let mut p = publication_by_id(&mut tx, id, true)?
                .ok_or_else(|| HubError::Invalid("unknown publication".into()))?;
            let (changed, event) = p.apply(update, now)?;
            if changed {
                tx.execute(
                    "UPDATE hub_publications SET branch_state = $2, branch_message = $3,
                         pr_number = $4, pr_node_id = $5, pr_url = $6, merge_commit = $7,
                         error = $8, updated_at = $9
                     WHERE id = $1",
                    &[
                        &id,
                        &p.branch_state,
                        &p.branch_message,
                        &p.pr_number,
                        &p.pr_node_id,
                        &p.pr_url,
                        &p.merge_commit,
                        &p.error,
                        &p.updated_at,
                    ],
                )?;
            }
            if let Some(event) = event {
                append(&mut tx, &event)?;
            }
            tx.commit()?;
            Ok(p)
        })
    }

    fn work_start(&self, token: &str, start: &WorkStart, now: i64) -> Result<WorkStartResult> {
        self.run(false, |client| {
            let mut tx = client.transaction()?;
            let who = work_who(&mut tx, token, now)?;
            lock_work_capacity(&mut tx, &who)?;
            let resolver = match (&start.work_id, &start.source, &start.request_key) {
                (Some(id), None, None) => vec!["work", id],
                (None, Some(source), None) => vec![
                    "forge",
                    &source.provider,
                    &source.host,
                    &source.repository,
                    &source.issue,
                ],
                (None, None, Some(key)) => vec!["native", &who.owner.id, key],
                _ => vec!["invalid"],
            };
            let lock = json_string(&(
                &who.owner.identity.provider,
                &who.owner.identity.host,
                who.repository_id,
                &who.repository,
                resolver,
            ))?;
            tx.execute(
                "SELECT pg_advisory_xact_lock($1, hashtext($2))",
                &[&0x6167_6577_i32, &lock],
            )?;
            lock_repository(&mut tx, &who.repository)?;
            let result = start_in(&mut tx, &who, start, now)?;
            tx.commit()?;
            Ok(result)
        })
    }

    fn work_start_auto(&self, token: &str, start: &WorkStart, now: i64) -> Result<WorkStartResult> {
        if start.work_id.is_some() || start.source.is_some() || start.request_key.is_some() {
            return Err(HubError::Invalid(
                "automatic start does not accept a work resolver".into(),
            ));
        }
        if start.role != WorkRole::Lead || start.takeover_generation.is_some() {
            return Err(HubError::Invalid(
                "automatic start requires a new lead without takeover".into(),
            ));
        }
        self.run(false, |client| {
            let mut tx = client.transaction()?;
            let who = work_who(&mut tx, token, now)?;
            if who.read_only {
                return Err(HubError::Unauthorized);
            }
            lock_work_capacity(&mut tx, &who)?;
            let mut planned = start.clone();
            planned.request_key = Some("automatic-session-start".into());
            let planned_scope = planned.validate(&who)?;
            let lock = json_string(&(token, &start.session))?;
            tx.execute(
                "SELECT pg_advisory_xact_lock($1, hashtext($2))",
                &[&0x6167_6577_i32, &lock],
            )?;
            lock_repository(&mut tx, &who.repository)?;
            if let Some(item) = unfinished_for_session(&mut tx, &who, &start.session, now)? {
                let changed = item.title != start.title
                    || item.scope != planned_scope
                    || item.branch != start.branch
                    || item.worktree != start.worktree
                    || item.next_action != start.next_action
                    || item.depends_on != start.depends_on
                    || item.related_work != start.related_work;
                let conflict = changed.then_some(WorkStartConflict {
                    kind: WorkConflictKind::SessionGoalChanged,
                    current_generation: item.generation,
                    overlaps: Vec::new(),
                });
                tx.commit()?;
                return Ok(WorkStartResult {
                    item,
                    created: false,
                    joined: false,
                    took_over: false,
                    conflict,
                    context: None,
                    coordination_request: None,
                });
            }
            let mut created = start.clone();
            created.request_key = Some(new_id("req")?);
            let result = start_in(&mut tx, &who, &created, now)?;
            tx.commit()?;
            Ok(result)
        })
    }

    fn work_checkpoint(
        &self,
        token: &str,
        checkpoint: &WorkCheckpoint,
        now: i64,
    ) -> Result<WorkCheckpointResult> {
        checkpoint.validate()?;
        let request = json_string(checkpoint)?;
        self.run(false, |client| {
            let mut tx = client.transaction()?;
            let who = work_who(&mut tx, token, now)?;
            if who.read_only {
                return Err(HubError::Unauthorized);
            }
            lock_work_capacity(&mut tx, &who)?;
            if checkpoint.kind == super::work::CheckpointKind::Finish {
                lock_repository(&mut tx, &who.repository)?;
            }
            let mut item = scoped_work(&mut tx, &who, &checkpoint.work_id, now, true)?
                .ok_or_else(|| HubError::Invalid("unknown work item".into()))?;
            if let Some(row) = tx.query_opt(
                "SELECT o.request, v.snapshot, o.event_seq FROM hub_work_checkpoint_ops o
                 JOIN hub_work_versions v ON v.work = o.work AND v.event_seq = o.event_seq
                 WHERE o.work = $1 AND o.operation_id = $2",
                &[&checkpoint.work_id, &checkpoint.operation_id],
            )? {
                let previous: String = row.get(0);
                if previous != request {
                    return Err(HubError::Invalid(
                        "checkpoint operation_id was already used for another request".into(),
                    ));
                }
                let snapshot: String = row.get(1);
                let committed: WorkItem = json_value(&snapshot)?;
                if committed.lead_token != token
                    || committed.lead_session != checkpoint.session
                    || committed.generation != checkpoint.generation
                {
                    return Err(HubError::Invalid(
                        "checkpoint operation belongs to another lead session".into(),
                    ));
                }
                let event_seq: i64 = row.get(2);
                tx.commit()?;
                return Ok(WorkCheckpointResult {
                    item,
                    receipt: CheckpointReceipt {
                        work_id: checkpoint.work_id.clone(),
                        operation_id: checkpoint.operation_id.clone(),
                        generation: committed.generation,
                        event_seq,
                        replayed: true,
                    },
                });
            }
            if item.generation != checkpoint.generation {
                return Err(HubError::Invalid(format!(
                    "checkpoint generation is fenced; current generation is {}",
                    item.generation
                )));
            }
            if item.lead_token != token || item.lead_session != checkpoint.session {
                return Err(HubError::Invalid(
                    "only the current lead session may checkpoint work".into(),
                ));
            }
            let expected = checkpoint.expected_updated_seq.ok_or_else(|| {
                HubError::Invalid("expected_updated_seq is required for a new checkpoint".into())
            })?;
            if expected != item.updated_seq {
                return Err(HubError::Invalid(format!(
                    "checkpoint receipt expired or work changed; current updated_seq is {}",
                    item.updated_seq
                )));
            }
            if let Some(scope) = checkpoint.normalized_scope()? {
                let overlaps = work_scope_overlaps(&mut tx, &who, &scope, Some(&item.id), now)?;
                if !overlaps.is_empty() {
                    let request = create_coordination_request(
                        &mut tx,
                        &who,
                        &checkpoint.session,
                        CoordinationProposal {
                            work_id: Some(item.id.clone()),
                            source: None,
                            request_key: None,
                            title: item.title.clone(),
                            scope,
                        },
                        &overlaps,
                        now,
                    )?;
                    tx.commit()?;
                    return Err(HubError::WorkCoordinationRequired {
                        overlaps,
                        request: Box::new(request),
                    });
                }
            }
            let previous_item_bytes = stored_work_item_bytes(&mut tx, &item.id)?;
            let referenced = checkpoint
                .source_refs
                .iter()
                .filter_map(|source| match source {
                    SourceRef::Work { work_id } => Some(work_id.clone()),
                    _ => None,
                })
                .collect::<Vec<_>>();
            relation_targets(&mut tx, &who, &referenced)?;
            replace_relations(
                &mut tx,
                &who,
                &item.id,
                &checkpoint.depends_on,
                &checkpoint.related_work,
            )?;
            item.state = checkpoint.state();
            item.claim_status = ClaimStatus::Active;
            item.last_checkin_at = now;
            item.next_action = checkpoint.next_action.clone();
            if let Some(scope) = checkpoint.normalized_scope()? {
                item.scope = scope;
            }
            if checkpoint.branch.is_some() {
                item.branch = checkpoint.branch.clone();
            }
            if checkpoint.worktree.is_some() {
                item.worktree = checkpoint.worktree.clone();
            }
            let current = Checkpoint {
                kind: checkpoint.kind,
                summary: checkpoint.summary.clone(),
                next_action: checkpoint.next_action.clone(),
                source_refs: checkpoint.source_refs.clone(),
                at: now,
                token: token.to_owned(),
                agent: who.agent.clone(),
                session: checkpoint.session.clone(),
                generation: checkpoint.generation,
            };
            if checkpoint.kind == super::work::CheckpointKind::Blocker {
                item.blocker = Some(current.clone());
            } else if checkpoint.resolve_blocker {
                item.blocker = None;
            }
            item.current = Some(current);
            item.depends_on = checkpoint.depends_on.clone();
            item.related_work = checkpoint.related_work.clone();
            item.updated_at = now;
            let event = work_event("work_checkpoint", &item)?;
            let seq = append(&mut tx, &event)?;
            item.updated_seq = seq;
            save_work_version(&mut tx, &item, seq)?;
            tx.execute(
                "INSERT INTO hub_work_checkpoint_ops
                    (work, operation_id, request, event_seq)
                 VALUES ($1, $2, $3, $4)",
                &[&item.id, &checkpoint.operation_id, &request, &seq],
            )?;
            let snapshot = json_string(&item)?;
            let request_bytes = work_payload_row_bytes(request.len())?;
            let delta = work_transition_payload_bytes(&snapshot, &event)?
                .checked_sub(previous_item_bytes)
                .and_then(|bytes| bytes.checked_add(request_bytes))
                .ok_or_else(|| HubError::Invalid("work payload size is too large".into()))?;
            adjust_work_payload_capacity(&mut tx, &who, delta, now)?;
            if checkpoint.kind == super::work::CheckpointKind::Finish {
                release_session_intent(&mut tx, token, &checkpoint.session, now)?;
            }
            tx.commit()?;
            Ok(WorkCheckpointResult {
                item,
                receipt: CheckpointReceipt {
                    work_id: checkpoint.work_id.clone(),
                    operation_id: checkpoint.operation_id.clone(),
                    generation: checkpoint.generation,
                    event_seq: seq,
                    replayed: false,
                },
            })
        })
    }

    fn work_get(&self, token: &str, id: &str, now: i64) -> Result<Option<WorkItem>> {
        self.run(true, |client| {
            let mut tx = client.transaction()?;
            let who = work_who(&mut tx, token, now)?;
            let item = scoped_work(&mut tx, &who, id, now, false)?;
            tx.commit()?;
            Ok(item)
        })
    }

    fn work_usage(&self, token: &str, usage: &UsageRecord, now: i64) -> Result<UsageReceipt> {
        usage.validate()?;
        let request = json_string(usage)?;
        self.run(false,|client|{let mut tx=client.transaction()?;let who=work_who(&mut tx,token,now)?;if who.read_only{return Err(HubError::Unauthorized);} lock_work_capacity(&mut tx,&who)?;
            let item=scoped_work(&mut tx,&who,&usage.work_id,now,true)?.ok_or_else(||HubError::Invalid("unknown work".into()))?;
            if let Some(r)=tx.query_opt("SELECT author_token,request,received_at FROM hub_work_model_usage WHERE work=$1 AND observation_id=$2",&[&usage.work_id,&usage.observation_id])? {let author:String=r.get(0);let existing:String=r.get(1);let received:i64=r.get(2);if author==token&&existing==request{tx.commit()?;return Ok(UsageReceipt{work_id:usage.work_id.clone(),observation_id:usage.observation_id.clone(),received_at:received,replayed:true});}return Err(HubError::Invalid("observation_id is already used by different usage".into()));}
            let authorized=if item.state==WorkState::Finished{item.lead_token==token&&item.lead_session==usage.session&&item.generation==usage.generation}else{item.generation==usage.generation&&tx.query_opt("SELECT 1 FROM hub_work_participants WHERE work=$1 AND token=$2 AND session=$3 AND generation=$4",&[&usage.work_id,&token,&usage.session,&usage.generation])?.is_some()};if !authorized{return Err(HubError::Invalid("usage author is not a current work participant".into()));}
            let count:i64=tx.query_one("SELECT COUNT(*)::bigint FROM hub_work_model_usage WHERE work=$1",&[&usage.work_id])?.get(0);if count>=MAX_OBSERVATIONS as i64{return Err(HubError::Invalid("work has reached the usage observation limit".into()));}
            let phase=enum_name(&usage.phase)?;let outcome=enum_name(&usage.outcome)?;let basis=enum_name(&usage.usage_basis)?;let cost_kind=usage.cost.as_ref().map(|c|enum_name(&c.kind)).transpose()?;let currency=usage.cost.as_ref().map(|c|c.currency.as_str());let amount=usage.cost.as_ref().map(|c|c.amount_micros);let cost_basis=usage.cost.as_ref().map(|c|c.basis.as_str());
            tx.execute("INSERT INTO hub_work_model_usage (work,observation_id,author_token,request,generation,session,phase,outcome,agent,model_provider,model,usage_basis,source_commit,input_tokens,output_tokens,cost_kind,cost_currency,cost_amount_micros,cost_basis,received_at) VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13,$14,$15,$16,$17,$18,$19,$20)",&[&usage.work_id,&usage.observation_id,&token,&request,&usage.generation,&usage.session,&phase,&outcome,&usage.agent,&usage.provider,&usage.model,&basis,&usage.source_commit,&usage.input_tokens,&usage.output_tokens,&cost_kind,&currency,&amount,&cost_basis,&now])?;
            adjust_work_payload_capacity(&mut tx,&who,work_payload_row_bytes(request.len())?,now)?;tx.commit()?;Ok(UsageReceipt{work_id:usage.work_id.clone(),observation_id:usage.observation_id.clone(),received_at:now,replayed:false})})
    }

    fn work_insight(&self, token: &str, id: &str, now: i64) -> Result<Option<WorkInsight>> {
        self.run(true, |client| {
            let mut tx = client
                .build_transaction()
                .isolation_level(IsolationLevel::RepeatableRead)
                .start()?;
            let who = work_who(&mut tx, token, now)?;
            let Some(item) = scoped_work(&mut tx, &who, id, now, false)? else {
                tx.commit()?;
                return Ok(None);
            };
            let insight =
                super::insights::project(pg_insight_input(&mut tx, item, now, MAX_OBSERVATIONS)?);
            tx.commit()?;
            Ok(Some(insight))
        })
    }

    fn work_insights(&self, token: &str, query: &InsightQuery, now: i64) -> Result<InsightPage> {
        let (since, until) = query.window(now)?;
        self.run(true, |client| {
            let mut tx=client.build_transaction().isolation_level(IsolationLevel::RepeatableRead).start()?;
            let who=work_who(&mut tx,token,now)?;
            let through=query.through.unwrap_or(tx.query_one("SELECT COALESCE(MAX(seq),0)::bigint FROM hub_events",&[])?.get(0));
            let cursor=query.after.as_ref().map(|v|InsightCursor::decode(v,since,until,through)).transpose()?;
            let after_at=cursor.as_ref().map(|c|c.created_at); let after_id=cursor.as_ref().map(|c|c.id.as_str());
            let rows=tx.query("SELECT snapshot FROM hub_work_items WHERE provider=$1 AND host=$2 AND repository=$3 AND repository_id IS NOT DISTINCT FROM $4 AND created_at>=$5 AND created_at<=$6 AND (created_seq IS NULL OR created_seq<=0 OR created_seq<=$7) AND ($8::bigint IS NULL OR created_at>$8 OR (created_at=$8 AND id>$9)) ORDER BY created_at,id LIMIT $10",&[&who.owner.identity.provider,&who.owner.identity.host,&who.repository,&who.repository_id,&since,&until,&through,&after_at,&after_id,&((query.limit+1)as i64)])?;
            let complete=rows.len()<=query.limit; let mut items=Vec::new();
            for r in rows.iter().take(query.limit) { let snapshot:String=r.get(0); let item:WorkItem=json_value(&snapshot)?; items.push(super::insights::project(pg_insight_input(&mut tx,item,now,20)?)); }
            let next_after=if complete{None}else{items.last().map(|i|InsightCursor{since,until,through,created_at:i.created_at,id:i.work_id.clone()}.encode())};
            tx.commit()?; Ok(InsightPage{version:1,as_of:now,since,until,through,next_after,complete,items})
        })
    }

    fn work_for_session(&self, token: &str, session: &str, now: i64) -> Result<Option<WorkItem>> {
        super::intent::text("session", session, 255)?;
        self.run(true, |client| {
            let mut tx = client.transaction()?;
            let who = work_who(&mut tx, token, now)?;
            let item = unfinished_for_session(&mut tx, &who, session, now)?;
            tx.commit()?;
            Ok(item)
        })
    }

    fn session_name_for(&self, token: &str, session: Option<&str>, now: i64) -> Result<String> {
        self.run(true, |client| {
            let mut tx = client.transaction()?;
            work_who(&mut tx, token, now)?;
            let name = session_name(&mut tx, token, session, now)?;
            tx.commit()?;
            Ok(name)
        })
    }

    fn work_briefing(&self, token: &str, query: &BriefingQuery, now: i64) -> Result<Briefing> {
        query.validate()?;
        self.run(false, |client| {
            let mut tx = client.transaction()?;
            let who = work_who(&mut tx, token, now)?;
            if who.read_only {
                lock_work_repository_shared(
                    &mut tx,
                    &who.owner.identity.provider,
                    &who.owner.identity.host,
                    &who.repository,
                    who.repository_id,
                )?;
            } else {
                lock_work_capacity(&mut tx, &who)?;
                lock_work_delivery(&mut tx, &who)?;
            }
            let subscriber = query.subscriber.clone();
            let subscriber_key = subscriber.as_deref().unwrap_or("");
            let ack_cursor = tx
                .query_opt(
                    "SELECT cursor FROM hub_work_acks
                     WHERE owner = $1 AND provider = $2 AND host = $3 AND repository = $4
                       AND repository_id IS NOT DISTINCT FROM $5 AND subscriber_token = $6
                       AND subscriber_session = $7",
                    &[
                        &who.owner.id,
                        &who.owner.identity.provider,
                        &who.owner.identity.host,
                        &who.repository,
                        &who.repository_id,
                        &who.token,
                        &subscriber_key,
                    ],
                )?
                .map_or(0, |row| row.get(0));
            if who.read_only {
                if query.delivery.is_some() {
                    return Err(HubError::Invalid(
                        "read-only briefing does not accept a delivery".into(),
                    ));
                }
                let after = query.after.unwrap_or(ack_cursor);
                if query.after.is_none() && (query.work_after.is_some() || query.work_done) {
                    return Err(HubError::Invalid(
                        "first briefing page cannot have a work cursor".into(),
                    ));
                }
                let through = match query.through {
                    Some(through) => through,
                    None => work_event_high_water(&mut tx, &who, after)?,
                };
                let page = briefing_page(
                    &mut tx,
                    &who,
                    None,
                    subscriber.clone(),
                    after,
                    through,
                    after,
                    query.work_after.clone(),
                    query.work_done,
                    query.limit,
                    now,
                )?;
                tx.commit()?;
                return Ok(page);
            }
            if let Some(delivery) = &query.delivery {
                let row = tx
                    .query_opt(
                        "SELECT from_seq, through_seq, page_after, page_work_after, page_work_done,
                                next_after, next_work_after, next_work_done, page, complete
                         FROM hub_work_deliveries WHERE id = $1 AND owner = $2 AND provider = $3
                           AND host = $4 AND repository = $5
                           AND repository_id IS NOT DISTINCT FROM $6 AND subscriber_token = $7
                           AND subscriber_session = $8 FOR UPDATE",
                        &[
                            &delivery,
                            &who.owner.id,
                            &who.owner.identity.provider,
                            &who.owner.identity.host,
                            &who.repository,
                            &who.repository_id,
                            &who.token,
                            &subscriber_key,
                        ],
                    )?
                    .ok_or_else(|| HubError::Invalid("unknown briefing delivery".into()))?;
                let from: i64 = row.get(0);
                let through: i64 = row.get(1);
                let page_after: i64 = row.get(2);
                let page_work_after: Option<String> = row.get(3);
                let page_work_done: bool = row.get(4);
                let next_after: i64 = row.get(5);
                let next_work_after: Option<String> = row.get(6);
                let next_work_done: bool = row.get(7);
                let cached: String = row.get(8);
                let complete: bool = row.get(9);
                if query.through != Some(through) || query.after.is_none() {
                    return Err(HubError::Invalid(
                        "delivery requires its exact through and cursors".into(),
                    ));
                }
                let after = query.after.unwrap_or_default();
                if after == page_after
                    && query.work_after == page_work_after
                    && query.work_done == page_work_done
                {
                    let page = json_value(&cached)?;
                    tx.commit()?;
                    return Ok(page);
                }
                if complete
                    || after != next_after
                    || query.work_after != next_work_after
                    || query.work_done != next_work_done
                {
                    return Err(HubError::Invalid(
                        "briefing cursors must retry the previous page or continue exactly".into(),
                    ));
                }
                let page = briefing_page(
                    &mut tx,
                    &who,
                    Some(delivery.clone()),
                    subscriber.clone(),
                    from,
                    through,
                    after,
                    query.work_after.clone(),
                    query.work_done,
                    query.limit,
                    now,
                )?;
                let page_json = json_string(&page)?;
                check_work_delivery_page(&page_json)?;
                tx.execute(
                    "UPDATE hub_work_deliveries SET page_after = $2, page_work_after = $3,
                         page_work_done = $4, next_after = $5, next_work_after = $6,
                         next_work_done = $7, page = $8, complete = $9, updated_at = $10
                     WHERE id = $1",
                    &[
                        &delivery,
                        &page.after,
                        &page.work_after,
                        &query.work_done,
                        &page.next_after,
                        &page.next_work_after,
                        &page.next_work_done,
                        &page_json,
                        &page.complete,
                        &now,
                    ],
                )?;
                tx.commit()?;
                return Ok(page);
            }
            if query.through.is_some()
                || query.work_after.is_some()
                || query.work_done
                || query.after.is_some_and(|after| after != ack_cursor)
            {
                return Err(HubError::Invalid(
                    "a new delivery starts at the acknowledged cursor without paging cursors"
                        .into(),
                ));
            }
            let released: i64 = tx
                .query_one(
                    "SELECT COALESCE(SUM(CASE WHEN NOT acknowledged THEN 4194304::bigint
                                ELSE octet_length(page)::bigint + 256 END), 0)::bigint
                     FROM hub_work_deliveries WHERE updated_at < $1
                       AND provider = $2 AND host = $3 AND repository = $4
                       AND repository_id IS NOT DISTINCT FROM $5",
                    &[
                        &now.saturating_sub(86_400),
                        &who.owner.identity.provider,
                        &who.owner.identity.host,
                        &who.repository,
                        &who.repository_id,
                    ],
                )?
                .get(0);
            tx.execute(
                "DELETE FROM hub_work_deliveries WHERE updated_at < $1
                   AND provider = $2 AND host = $3 AND repository = $4
                   AND repository_id IS NOT DISTINCT FROM $5",
                &[
                    &now.saturating_sub(86_400),
                    &who.owner.identity.provider,
                    &who.owner.identity.host,
                    &who.repository,
                    &who.repository_id,
                ],
            )?;
            let row = tx.query_one(
                "SELECT COUNT(*),
                            COUNT(*) FILTER (WHERE subscriber_token = $6)
                     FROM hub_work_deliveries
                     WHERE owner = $1 AND provider = $2 AND host = $3 AND repository = $4
                       AND repository_id IS NOT DISTINCT FROM $5 AND NOT acknowledged",
                &[
                    &who.owner.id,
                    &who.owner.identity.provider,
                    &who.owner.identity.host,
                    &who.repository,
                    &who.repository_id,
                    &who.token,
                ],
            )?;
            let owner_open: i64 = row.get(0);
            let token_open: i64 = row.get(1);
            if owner_open >= 16 || token_open >= 8 {
                return Err(HubError::Invalid(
                    "too many unacknowledged briefing deliveries".into(),
                ));
            }
            let through = work_event_high_water(&mut tx, &who, ack_cursor)?;
            let delivery = new_id("dlv")?;
            let page = briefing_page(
                &mut tx,
                &who,
                Some(delivery.clone()),
                subscriber.clone(),
                ack_cursor,
                through,
                ack_cursor,
                None,
                false,
                query.limit,
                now,
            )?;
            let page_json = json_string(&page)?;
            check_work_delivery_page(&page_json)?;
            tx.execute(
                "INSERT INTO hub_work_deliveries
                    (id, owner, provider, host, repository, repository_id, subscriber_token,
                     subscriber_session,
                     from_seq, through_seq,
                     page_after, page_work_after, page_work_done, next_after, next_work_after,
                     next_work_done, page, complete, created_at, updated_at)
                 VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $9, NULL, FALSE, $11, $12,
                         $13, $14, $15, $16, $16)",
                &[
                    &delivery,
                    &who.owner.id,
                    &who.owner.identity.provider,
                    &who.owner.identity.host,
                    &who.repository,
                    &who.repository_id,
                    &who.token,
                    &subscriber_key,
                    &ack_cursor,
                    &through,
                    &page.next_after,
                    &page.next_work_after,
                    &page.next_work_done,
                    &page_json,
                    &page.complete,
                    &now,
                ],
            )?;
            adjust_work_payload_capacity(
                &mut tx,
                &who,
                WORK_DELIVERY_RESERVATION_BYTES
                    .checked_sub(released)
                    .ok_or_else(|| HubError::Invalid("work payload size is too large".into()))?,
                now,
            )?;
            tx.commit()?;
            Ok(page)
        })
    }

    fn work_ack(&self, token: &str, ack: &BriefingAck, now: i64) -> Result<()> {
        ack.validate()?;
        self.run(false, |client| {
            let mut tx = client.transaction()?;
            let who = work_who(&mut tx, token, now)?;
            if who.read_only {
                return Err(HubError::Unauthorized);
            }
            lock_work_capacity(&mut tx, &who)?;
            lock_work_delivery(&mut tx, &who)?;
            let row = tx
                .query_opt(
                    "SELECT through_seq, page, acknowledged, subscriber_session
                     FROM hub_work_deliveries
                     WHERE id = $1 AND owner = $2 AND provider = $3 AND host = $4
                       AND repository = $5 AND repository_id IS NOT DISTINCT FROM $6
                       AND subscriber_token = $7 AND complete FOR UPDATE",
                    &[
                        &ack.delivery,
                        &who.owner.id,
                        &who.owner.identity.provider,
                        &who.owner.identity.host,
                        &who.repository,
                        &who.repository_id,
                        &who.token,
                    ],
                )?
                .ok_or_else(|| HubError::Invalid("delivery is unknown or incomplete".into()))?;
            let through: i64 = row.get(0);
            let page: String = row.get(1);
            let acknowledged: bool = row.get(2);
            let subscriber_key: String = row.get(3);
            let changed = tx.execute(
                "UPDATE hub_work_acks SET cursor = GREATEST(cursor, $6), updated_at = $7
                 WHERE owner = $1 AND provider = $2 AND host = $3 AND repository = $4
                   AND repository_id IS NOT DISTINCT FROM $5 AND subscriber_token = $8
                   AND subscriber_session = $9",
                &[
                    &who.owner.id,
                    &who.owner.identity.provider,
                    &who.owner.identity.host,
                    &who.repository,
                    &who.repository_id,
                    &through,
                    &now,
                    &who.token,
                    &subscriber_key,
                ],
            )?;
            if changed == 0 {
                tx.execute(
                    "INSERT INTO hub_work_acks
                        (owner, provider, host, repository, repository_id, cursor, updated_at,
                         subscriber_token, subscriber_session)
                     VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)",
                    &[
                        &who.owner.id,
                        &who.owner.identity.provider,
                        &who.owner.identity.host,
                        &who.repository,
                        &who.repository_id,
                        &through,
                        &now,
                        &who.token,
                        &subscriber_key,
                    ],
                )?;
            }
            tx.execute(
                "UPDATE hub_work_deliveries SET acknowledged = TRUE, updated_at = $2
                 WHERE id = $1",
                &[&ack.delivery, &now],
            )?;
            if !acknowledged {
                adjust_work_payload_capacity(
                    &mut tx,
                    &who,
                    work_payload_row_bytes(page.len())?
                        .checked_sub(WORK_DELIVERY_RESERVATION_BYTES)
                        .ok_or_else(|| {
                            HubError::Invalid("work payload size is too large".into())
                        })?,
                    now,
                )?;
            }
            tx.commit()?;
            Ok(())
        })
    }

    fn work_compact(
        &self,
        scope: &WorkRepositoryScope,
        before: i64,
        apply: bool,
        now: i64,
    ) -> Result<WorkCompaction> {
        scope.validate()?;
        if before < 0 || before > now.saturating_sub(86_400) {
            return Err(HubError::Invalid(
                "work compaction cutoff must be at least 24 hours old".into(),
            ));
        }
        self.run(false, |client| {
            let mut tx = client.transaction()?;
            lock_work_repository(
                &mut tx,
                &scope.provider,
                &scope.host,
                &scope.repository,
                scope.repository_id,
            )?;
            let active_deliveries: i64 = tx
                .query_one(
                    "SELECT COUNT(*) FROM hub_work_deliveries
                     WHERE provider = $1 AND host = $2 AND repository = $3
                       AND repository_id IS NOT DISTINCT FROM $4
                       AND NOT acknowledged AND updated_at >= $5",
                    &[
                        &scope.provider,
                        &scope.host,
                        &scope.repository,
                        &scope.repository_id,
                        &now.saturating_sub(86_400),
                    ],
                )?
                .get(0);
            if active_deliveries > 0 {
                return Err(HubError::Invalid(
                    "work compaction is blocked by an unacknowledged briefing delivery".into(),
                ));
            }
            let operations: i64 = tx
                .query_one(
                    "SELECT COUNT(*) FROM hub_work_checkpoint_ops o
                     JOIN hub_work_items w ON w.id = o.work
                     JOIN hub_events e ON e.seq = o.event_seq
                     WHERE w.provider = $1 AND w.host = $2 AND w.repository = $3
                       AND w.repository_id IS NOT DISTINCT FROM $4 AND e.at < $5",
                    &[
                        &scope.provider,
                        &scope.host,
                        &scope.repository,
                        &scope.repository_id,
                        &before,
                    ],
                )?
                .get(0);
            let versions: i64 = tx
                .query_one(
                    "SELECT COUNT(*) FROM hub_work_versions v
                     JOIN hub_work_items w ON w.id = v.work
                     JOIN hub_events e ON e.seq = v.event_seq
                     WHERE w.provider = $1 AND w.host = $2 AND w.repository = $3
                       AND w.repository_id IS NOT DISTINCT FROM $4 AND e.at < $5
                       AND v.event_seq <> (SELECT MAX(v2.event_seq) FROM hub_work_versions v2
                           JOIN hub_events e2 ON e2.seq = v2.event_seq
                           WHERE v2.work = v.work AND e2.at < $5)",
                    &[
                        &scope.provider,
                        &scope.host,
                        &scope.repository,
                        &scope.repository_id,
                        &before,
                    ],
                )?
                .get(0);
            let candidate_snapshot_floor: Option<i64> = tx
                .query_one(
                    "SELECT MAX(baseline) FROM (
                         SELECT v.work, MAX(v.event_seq) AS baseline, COUNT(*) AS versions
                         FROM hub_work_versions v JOIN hub_work_items w ON w.id = v.work
                         JOIN hub_events e ON e.seq = v.event_seq
                         WHERE w.provider = $1 AND w.host = $2 AND w.repository = $3
                           AND w.repository_id IS NOT DISTINCT FROM $4 AND e.at < $5
                         GROUP BY v.work HAVING COUNT(*) > 1
                     ) compacted",
                    &[
                        &scope.provider,
                        &scope.host,
                        &scope.repository,
                        &scope.repository_id,
                        &before,
                    ],
                )?
                .get(0);
            let (current_event_floor, current_snapshot_floor): (i64, i64) = tx
                .query_opt(
                    "SELECT event_floor_seq, snapshot_floor_seq FROM hub_work_retention
                     WHERE provider = $1 AND host = $2 AND repository = $3
                       AND repository_id IS NOT DISTINCT FROM $4",
                    &[
                        &scope.provider,
                        &scope.host,
                        &scope.repository,
                        &scope.repository_id,
                    ],
                )?
                .map_or((0, 0), |row| (row.get(0), row.get(1)));
            let retention_floor =
                candidate_snapshot_floor.map_or(current_snapshot_floor, |floor| {
                    floor.max(current_snapshot_floor)
                });
            let current_coordination_floor: i64 = tx
                .query_opt(
                    "SELECT event_floor_seq FROM hub_coordination_retention
                     WHERE provider=$1 AND host=$2 AND repository=$3
                       AND repository_id IS NOT DISTINCT FROM $4",
                    &[
                        &scope.provider,
                        &scope.host,
                        &scope.repository,
                        &scope.repository_id,
                    ],
                )?
                .map_or(0, |row| row.get(0));
            let compacted_coordination = tx
                .query(
                    "SELECT r.id, (
                         SELECT MAX(e.seq) FROM hub_events e WHERE e.subject=r.id AND e.kind IN (
                           'coordination_requested','coordination_bound','coordination_retargeted',
                           'coordination_acknowledged','coordination_responded',
                           'coordination_resolved','coordination_withdrawn'))
                     FROM hub_coordination_requests r
                     WHERE r.provider=$1 AND r.host=$2 AND r.repository=$3
                       AND r.repository_id IS NOT DISTINCT FROM $4
                       AND r.status IN ('resolved','withdrawn') AND r.updated_at < $5
                       AND EXISTS (SELECT 1 FROM hub_events e WHERE e.subject=r.id AND e.kind IN (
                           'coordination_requested','coordination_bound','coordination_retargeted',
                           'coordination_acknowledged','coordination_responded',
                           'coordination_resolved','coordination_withdrawn'))
                       AND NOT EXISTS (SELECT 1 FROM hub_events e WHERE e.subject=r.id
                           AND e.kind IN ('coordination_requested','coordination_bound',
                             'coordination_retargeted','coordination_acknowledged',
                             'coordination_responded','coordination_resolved',
                             'coordination_withdrawn') AND e.at >= $5)",
                    &[
                        &scope.provider,
                        &scope.host,
                        &scope.repository,
                        &scope.repository_id,
                        &before,
                    ],
                )?
                .into_iter()
                .map(|row| (row.get::<_, String>(0), row.get::<_, i64>(1)))
                .collect::<Vec<_>>();
            let coordination_floor = compacted_coordination
                .iter()
                .map(|(_, floor)| *floor)
                .max()
                .unwrap_or(0)
                .max(current_coordination_floor);
            if apply {
                tx.execute(
                    "DELETE FROM hub_work_deliveries
                     WHERE provider = $1 AND host = $2 AND repository = $3
                       AND repository_id IS NOT DISTINCT FROM $4 AND updated_at < $5",
                    &[
                        &scope.provider,
                        &scope.host,
                        &scope.repository,
                        &scope.repository_id,
                        &now.saturating_sub(86_400),
                    ],
                )?;
                tx.execute(
                    "DELETE FROM hub_work_checkpoint_ops AS o USING hub_work_items AS w, hub_events AS e
                     WHERE w.id = o.work AND e.seq = o.event_seq
                       AND w.provider = $1 AND w.host = $2 AND w.repository = $3
                       AND w.repository_id IS NOT DISTINCT FROM $4 AND e.at < $5",
                    &[
                        &scope.provider,
                        &scope.host,
                        &scope.repository,
                        &scope.repository_id,
                        &before,
                    ],
                )?;
                tx.execute(
                    "DELETE FROM hub_work_versions AS v USING hub_work_items AS w, hub_events AS e
                     WHERE w.id = v.work AND e.seq = v.event_seq
                       AND w.provider = $1 AND w.host = $2 AND w.repository = $3
                       AND w.repository_id IS NOT DISTINCT FROM $4 AND e.at < $5
                       AND v.event_seq <> (SELECT MAX(v2.event_seq) FROM hub_work_versions v2
                           JOIN hub_events e2 ON e2.seq = v2.event_seq
                           WHERE v2.work = v.work AND e2.at < $5)",
                    &[
                        &scope.provider,
                        &scope.host,
                        &scope.repository,
                        &scope.repository_id,
                        &before,
                    ],
                )?;
                let removed_event_floor: Option<i64> = tx
                    .query_one(
                        "SELECT MAX(e.seq) FROM hub_events e
                         JOIN hub_work_items w ON w.id = e.subject
                         WHERE w.provider = $1 AND w.host = $2 AND w.repository = $3
                           AND w.repository_id IS NOT DISTINCT FROM $4 AND e.at < $5
                           AND e.kind IN ('work_started', 'work_joined', 'work_takeover', 'work_checkpoint')
                           AND NOT EXISTS (SELECT 1 FROM hub_work_versions v WHERE v.event_seq = e.seq)
                           AND NOT EXISTS (SELECT 1 FROM hub_work_checkpoint_ops o WHERE o.event_seq = e.seq)",
                        &[
                            &scope.provider,
                            &scope.host,
                            &scope.repository,
                            &scope.repository_id,
                            &before,
                        ],
                    )?
                    .get(0);
                tx.execute(
                    "DELETE FROM hub_events AS e USING hub_work_items AS w
                     WHERE w.id = e.subject
                       AND w.provider = $1 AND w.host = $2 AND w.repository = $3
                       AND w.repository_id IS NOT DISTINCT FROM $4 AND e.at < $5
                       AND e.kind IN ('work_started', 'work_joined', 'work_takeover', 'work_checkpoint')
                       AND NOT EXISTS (SELECT 1 FROM hub_work_versions v WHERE v.event_seq = e.seq)
                       AND NOT EXISTS (SELECT 1 FROM hub_work_checkpoint_ops o WHERE o.event_seq = e.seq)",
                    &[
                        &scope.provider,
                        &scope.host,
                        &scope.repository,
                        &scope.repository_id,
                        &before,
                    ],
                )?;
                let event_floor = removed_event_floor.map_or(current_event_floor, |floor| {
                    floor.max(current_event_floor)
                });
                if candidate_snapshot_floor.is_some() || removed_event_floor.is_some() {
                    let changed = tx.execute(
                        "UPDATE hub_work_retention
                         SET event_floor_seq = GREATEST(event_floor_seq, $5),
                             snapshot_floor_seq = GREATEST(snapshot_floor_seq, $6),
                             updated_at = $7
                         WHERE provider = $1 AND host = $2 AND repository = $3
                           AND repository_id IS NOT DISTINCT FROM $4",
                        &[
                            &scope.provider,
                            &scope.host,
                            &scope.repository,
                            &scope.repository_id,
                            &event_floor,
                            &retention_floor,
                            &now,
                        ],
                    )?;
                    if changed == 0 {
                        tx.execute(
                            "INSERT INTO hub_work_retention
                                (provider, host, repository, repository_id, event_floor_seq,
                                 snapshot_floor_seq, updated_at)
                             VALUES ($1, $2, $3, $4, $5, $6, $7)",
                            &[
                                &scope.provider,
                                &scope.host,
                                &scope.repository,
                                &scope.repository_id,
                                &event_floor,
                                &retention_floor,
                                &now,
                            ],
                        )?;
                    }
                }
                for (request, _) in &compacted_coordination {
                    tx.execute(
                        "DELETE FROM hub_coordination_operations WHERE request=$1",
                        &[request],
                    )?;
                    tx.execute(
                        "DELETE FROM hub_coordination_acknowledgements WHERE request=$1",
                        &[request],
                    )?;
                    tx.execute(
                        "DELETE FROM hub_coordination_responses WHERE request=$1",
                        &[request],
                    )?;
                    tx.execute(
                        "DELETE FROM hub_coordination_versions WHERE request=$1",
                        &[request],
                    )?;
                    tx.execute(
                        "DELETE FROM hub_coordination_targets WHERE request=$1",
                        &[request],
                    )?;
                    tx.execute(
                        "DELETE FROM hub_coordination_requests WHERE id=$1",
                        &[request],
                    )?;
                    tx.execute(
                        "DELETE FROM hub_events WHERE subject=$1 AND kind IN (
                           'coordination_requested','coordination_bound','coordination_retargeted',
                           'coordination_acknowledged','coordination_responded',
                           'coordination_resolved','coordination_withdrawn') AND at < $2",
                        &[request, &before],
                    )?;
                }
                if coordination_floor > current_coordination_floor {
                    let changed = tx.execute(
                        "UPDATE hub_coordination_retention
                         SET event_floor_seq=GREATEST(event_floor_seq,$5), updated_at=$6
                         WHERE provider=$1 AND host=$2 AND repository=$3
                           AND repository_id IS NOT DISTINCT FROM $4",
                        &[
                            &scope.provider,
                            &scope.host,
                            &scope.repository,
                            &scope.repository_id,
                            &coordination_floor,
                            &now,
                        ],
                    )?;
                    if changed == 0 {
                        tx.execute(
                            "INSERT INTO hub_coordination_retention
                                (provider,host,repository,repository_id,event_floor_seq,updated_at)
                             VALUES ($1,$2,$3,$4,$5,$6)",
                            &[
                                &scope.provider,
                                &scope.host,
                                &scope.repository,
                                &scope.repository_id,
                                &coordination_floor,
                                &now,
                            ],
                        )?;
                    }
                }
                let charged = work_payload_bytes(
                    &mut tx,
                    &scope.provider,
                    &scope.host,
                    &scope.repository,
                    scope.repository_id,
                )?;
                let changed = tx.execute(
                    "UPDATE hub_work_usage SET charged_bytes = $5, updated_at = $6
                     WHERE provider = $1 AND host = $2 AND repository = $3
                       AND repository_id IS NOT DISTINCT FROM $4",
                    &[
                        &scope.provider,
                        &scope.host,
                        &scope.repository,
                        &scope.repository_id,
                        &charged,
                        &now,
                    ],
                )?;
                if changed == 0 {
                    tx.execute(
                        "INSERT INTO hub_work_usage
                            (provider, host, repository, repository_id, charged_bytes, updated_at)
                         VALUES ($1, $2, $3, $4, $5, $6)",
                        &[
                            &scope.provider,
                            &scope.host,
                            &scope.repository,
                            &scope.repository_id,
                            &charged,
                            &now,
                        ],
                    )?;
                }
            }
            tx.commit()?;
            Ok(WorkCompaction {
                scope: scope.clone(),
                before,
                applied: apply,
                deleted_operations: operations as u64,
                deleted_versions: versions as u64,
                retention_floor,
                deleted_coordination_requests: compacted_coordination.len() as u64,
                coordination_retention_floor: coordination_floor,
            })
        })
    }

    fn work_briefing_preview_for(
        &self,
        token: &str,
        subscriber: Option<&str>,
        limit: usize,
        now: i64,
    ) -> Result<Briefing> {
        BriefingQuery {
            subscriber: subscriber.map(str::to_owned),
            limit,
            ..BriefingQuery::default()
        }
        .validate()?;
        self.run(true, |client| {
            let mut tx = client.transaction()?;
            let who = work_who(&mut tx, token, now)?;
            let subscriber_key = subscriber.unwrap_or("");
            lock_work_repository_shared(
                &mut tx,
                &who.owner.identity.provider,
                &who.owner.identity.host,
                &who.repository,
                who.repository_id,
            )?;
            let after = tx
                .query_opt(
                    "SELECT cursor FROM hub_work_acks
                     WHERE owner = $1 AND provider = $2 AND host = $3 AND repository = $4
                       AND repository_id IS NOT DISTINCT FROM $5 AND subscriber_token = $6
                       AND subscriber_session = $7",
                    &[
                        &who.owner.id,
                        &who.owner.identity.provider,
                        &who.owner.identity.host,
                        &who.repository,
                        &who.repository_id,
                        &who.token,
                        &subscriber_key,
                    ],
                )?
                .map_or(0, |row| row.get(0));
            let through = work_event_high_water(&mut tx, &who, after)?;
            let page = briefing_page(
                &mut tx,
                &who,
                None,
                subscriber.map(str::to_owned),
                after,
                through,
                after,
                None,
                false,
                limit,
                now,
            )?;
            tx.commit()?;
            Ok(page)
        })
    }

    fn work_overview(
        &self,
        token: &str,
        query: &WorkOverviewQuery,
        now: i64,
    ) -> Result<WorkOverview> {
        let recent_since = query.validate(now)?;
        self.run(true, |client| {
            let mut tx = client.transaction()?;
            let who = work_who(&mut tx, token, now)?;
            lock_work_repository_shared(
                &mut tx,
                &who.owner.identity.provider,
                &who.owner.identity.host,
                &who.repository,
                who.repository_id,
            )?;
            let fetch_limit = i64::try_from(query.limit + 1).unwrap_or(101);
            let mut current = tx
                .query(
                    "SELECT snapshot FROM hub_work_items
                     WHERE provider = $1 AND host = $2 AND repository = $3
                       AND repository_id IS NOT DISTINCT FROM $4 AND state <> 'finished'
                     ORDER BY id LIMIT $5",
                    &[
                        &who.owner.identity.provider,
                        &who.owner.identity.host,
                        &who.repository,
                        &who.repository_id,
                        &fetch_limit,
                    ],
                )?
                .into_iter()
                .map(|row| json_value::<WorkItem>(&row.get::<_, String>(0)))
                .collect::<Result<Vec<_>>>()?;
            let mut current_complete = current.len() <= query.limit;
            current.truncate(query.limit);
            for item in &mut current {
                project_claim(&mut tx, item, now)?;
            }
            let overview_part_budget = (MAX_BRIEFING_BYTES - 4096) / 3;
            while json_string(&current)?.len() > overview_part_budget {
                current.pop();
                current_complete = false;
            }

            let mut recent = tx
                .query(
                    "SELECT v.event_seq, e.at, v.snapshot FROM hub_work_versions v
                     JOIN hub_work_items w ON w.id = v.work
                     JOIN hub_events e ON e.seq = v.event_seq
                     WHERE w.provider = $1 AND w.host = $2 AND w.repository = $3
                       AND w.repository_id IS NOT DISTINCT FROM $4
                       AND e.at >= $5 AND e.at <= $6
                       AND e.kind = 'work_checkpoint'
                       AND v.snapshot::jsonb #>> '{current,kind}' IN ('decision', 'finish')
                     ORDER BY v.event_seq DESC LIMIT $7",
                    &[
                        &who.owner.identity.provider,
                        &who.owner.identity.host,
                        &who.repository,
                        &who.repository_id,
                        &recent_since,
                        &now,
                        &fetch_limit,
                    ],
                )?
                .into_iter()
                .map(|row| {
                    let item: WorkItem = json_value(&row.get::<_, String>(2))?;
                    let kind = if item.state == WorkState::Finished {
                        WorkOutcomeKind::Completion
                    } else {
                        WorkOutcomeKind::Decision
                    };
                    Ok(WorkOutcome {
                        seq: row.get(0),
                        at: row.get(1),
                        kind,
                        item,
                    })
                })
                .collect::<Result<Vec<_>>>()?;
            let (event_floor, snapshot_floor) = work_retention_floors(&mut tx, &who)?;
            let history_retention_gap = event_floor > 0 || snapshot_floor > 0;
            let mut recent_complete = recent.len() <= query.limit && !history_retention_gap;
            recent.truncate(query.limit);
            while json_string(&recent)?.len() > overview_part_budget {
                recent.pop();
                recent_complete = false;
            }
            let mut overlaps = work_overlap_pairs(&current);
            let mut overlaps_complete = current_complete && overlaps.len() <= query.limit;
            overlaps.truncate(query.limit);
            while json_string(&overlaps)?.len() > overview_part_budget {
                overlaps.pop();
                overlaps_complete = false;
            }
            tx.commit()?;
            Ok(WorkOverview {
                recent_since,
                until: now,
                current,
                recent,
                overlaps,
                current_complete,
                recent_complete,
                overlaps_complete,
                history_retention_gap,
            })
        })
    }

    fn work_scope_check(
        &self,
        token: &str,
        session: &str,
        scope: &[String],
        now: i64,
    ) -> Result<WorkScopeCheck> {
        super::intent::text("session", session, 255)?;
        let requested_scope = normalized_scope(scope)?;
        self.run(true, |client| {
            let mut tx = client.transaction()?;
            let who = work_who(&mut tx, token, now)?;
            lock_work_repository_shared(
                &mut tx,
                &who.owner.identity.provider,
                &who.owner.identity.host,
                &who.repository,
                who.repository_id,
            )?;
            let rows = tx.query(
                "SELECT w.snapshot FROM hub_work_items w
                 JOIN hub_work_participants p ON p.work = w.id
                 WHERE w.provider = $1 AND w.host = $2 AND w.repository = $3
                   AND w.repository_id IS NOT DISTINCT FROM $4 AND w.state IN ('active','blocked')
                   AND p.token = $5 AND p.session = $6 AND p.generation = w.generation
                 ORDER BY w.id LIMIT 2",
                &[
                    &who.owner.identity.provider,
                    &who.owner.identity.host,
                    &who.repository,
                    &who.repository_id,
                    &who.token,
                    &session,
                ],
            )?;
            if rows.len() > 1 {
                return Err(HubError::Invalid(
                    "session participates in more than one unfinished work item".into(),
                ));
            }
            let eligible = !rows.is_empty();
            let fallback = if eligible {
                None
            } else {
                tx.query_opt(
                    "SELECT w.snapshot FROM hub_work_items w
                     JOIN hub_work_participants p ON p.work=w.id
                     WHERE w.provider=$1 AND w.host=$2 AND w.repository=$3
                       AND w.repository_id IS NOT DISTINCT FROM $4
                       AND p.token=$5 AND p.session=$6
                     ORDER BY w.updated_at DESC,w.id LIMIT 1",
                    &[
                        &who.owner.identity.provider,
                        &who.owner.identity.host,
                        &who.repository,
                        &who.repository_id,
                        &who.token,
                        &session,
                    ],
                )?
                .map(|row| row.get::<_, String>(0))
            };
            let mut item = rows
                .into_iter()
                .next()
                .map(|row| row.get::<_, String>(0))
                .or(fallback)
                .map(|snapshot| json_value::<WorkItem>(&snapshot))
                .transpose()?;
            if let Some(item) = &mut item {
                project_claim(&mut tx, item, now)?;
            }
            let uncovered_scope = if eligible {
                item.as_ref().map_or_else(
                    || requested_scope.clone(),
                    |item| uncovered_scope(&item.scope, &requested_scope),
                )
            } else {
                requested_scope.clone()
            };
            tx.commit()?;
            Ok(WorkScopeCheck {
                item,
                requested_scope: requested_scope.clone(),
                uncovered_scope,
            })
        })
    }

    fn coordination_inbox(
        &self,
        token: &str,
        query: &CoordinationInboxQuery,
        now: i64,
    ) -> Result<CoordinationInbox> {
        query.validate()?;
        let session = query
            .session
            .as_deref()
            .ok_or_else(|| HubError::Invalid("coordination inbox requires a session".into()))?;
        self.run(true, |client| {
            let mut tx = client.transaction()?;
            let who = work_who(&mut tx, token, now)?;
            let page = coordination_page(
                &mut tx,
                &who,
                Some(session),
                (
                    query.after.unwrap_or(0),
                    query.through,
                    query.after.is_none(),
                ),
                query.limit,
                now,
            )?;
            tx.commit()?;
            Ok(page)
        })
    }

    fn coordination_list(
        &self,
        token: &str,
        query: &CoordinationListQuery,
        now: i64,
    ) -> Result<CoordinationInbox> {
        query.validate()?;
        self.run(true, |client| {
            let mut tx = client.transaction()?;
            let who = work_who(&mut tx, token, now)?;
            let page = coordination_page(
                &mut tx,
                &who,
                None,
                (query.after.unwrap_or(0), query.through, query.active_only),
                query.limit,
                now,
            )?;
            tx.commit()?;
            Ok(page)
        })
    }

    fn coordination_ack(
        &self,
        token: &str,
        ack: &CoordinationAck,
        now: i64,
    ) -> Result<CoordinationRequest> {
        ack.validate()?;
        self.run(false, |client| {
            let mut tx = client.transaction()?;
            let who = work_who(&mut tx, token, now)?;
            if who.read_only {
                return Err(HubError::Unauthorized);
            }
            lock_work_capacity(&mut tx, &who)?;
            let request =
                coordination_request_at(&mut tx, &who, &ack.request_id, i64::MAX, now)?
                    .ok_or_else(|| HubError::Invalid("unknown coordination request".into()))?;
            let (target, generation) = match &ack.target_work_id {
                None if coordination_is_requester(&request, &who, &ack.session) => ("", 0),
                Some(work) => {
                    let target = coordination_target_for(&request, work, &who, &ack.session)
                        .ok_or(HubError::Unauthorized)?;
                    (target.work_id.as_str(), target.generation)
                }
                None => return Err(HubError::Unauthorized),
            };
            if request.acknowledgements.iter().any(|receipt| {
                receipt.target_work_id.as_deref().unwrap_or("") == target
                    && receipt.agent.token == who.token
                    && receipt.agent.session == ack.session
                    && receipt.generation == generation
                    && receipt.through_seq >= ack.expected_updated_seq
            }) {
                tx.commit()?;
                return Ok(request);
            }
            if request.updated_seq != ack.expected_updated_seq {
                return Err(HubError::Invalid(format!(
                    "coordination request changed; current updated_seq is {}",
                    request.updated_seq
                )));
            }
            let updates: i64 = tx
                .query_one(
                    "SELECT COUNT(*) FROM hub_coordination_acknowledgements WHERE request=$1",
                    &[&request.id],
                )?
                .get(0);
            if updates >= MAX_COORDINATION_UPDATES {
                return Err(HubError::WorkCapacity);
            }
            let event = coordination_event(
                "coordination_acknowledged",
                &request.id,
                now,
                json!({"target_work_id": ack.target_work_id, "token": who.token,
                    "session": ack.session, "generation": generation,
                    "through_seq": request.updated_seq}),
            )?;
            let seq = append(&mut tx, &event)?;
            tx.execute(
                "INSERT INTO hub_coordination_acknowledgements
                 (request,target_work,token,agent,session,owner,login,generation,through_seq,
                  acknowledged_at,event_seq) VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11)",
                &[
                    &request.id,
                    &target,
                    &who.token,
                    &who.agent,
                    &ack.session,
                    &who.owner.id,
                    &who.owner.login,
                    &generation,
                    &seq,
                    &now,
                    &seq,
                ],
            )?;
            save_coordination_version(
                &mut tx,
                &request.id,
                request.status,
                request.proposal.work_id.as_deref(),
                now,
                seq,
            )?;
            let receipt = json_string(&(target, &who.token, &ack.session, generation, seq))?;
            adjust_work_payload_capacity(
                &mut tx,
                &who,
                coordination_payload_bytes(&[&receipt, ""], &event)?,
                now,
            )?;
            let updated = coordination_request_at(&mut tx, &who, &ack.request_id, i64::MAX, now)?
                .ok_or_else(|| {
                HubError::Unavailable("coordination request disappeared".into())
            })?;
            tx.commit()?;
            Ok(updated)
        })
    }

    fn coordination_reply(
        &self,
        token: &str,
        reply: &CoordinationReply,
        now: i64,
    ) -> Result<CoordinationRequest> {
        reply.validate()?;
        let payload = json_string(reply)?;
        self.run(false, |client| {
            let mut tx = client.transaction()?;
            let who = work_who(&mut tx, token, now)?;
            if who.read_only {
                return Err(HubError::Unauthorized);
            }
            lock_work_capacity(&mut tx, &who)?;
            let request = coordination_request_at(&mut tx, &who, &reply.request_id, i64::MAX, now)?
                .ok_or_else(|| HubError::Invalid("unknown coordination request".into()))?;
            let target =
                coordination_target_for(&request, &reply.target_work_id, &who, &reply.session)
                    .ok_or(HubError::Unauthorized)?;
            if coordination_operation_replay(
                &mut tx,
                &request.id,
                &reply.operation_id,
                "reply",
                &payload,
            )? {
                tx.commit()?;
                return Ok(request);
            }
            if !matches!(
                request.status,
                CoordinationStatus::Pending | CoordinationStatus::Responded
            ) {
                return Err(HubError::Invalid("coordination request is closed".into()));
            }
            if request.updated_seq != reply.expected_updated_seq {
                return Err(HubError::Invalid(format!(
                    "coordination request changed; current updated_seq is {}",
                    request.updated_seq
                )));
            }
            let updates: i64 = tx
                .query_one(
                    "SELECT COUNT(*) FROM hub_coordination_responses WHERE request=$1",
                    &[&request.id],
                )?
                .get(0);
            if updates >= MAX_COORDINATION_UPDATES {
                return Err(HubError::WorkCapacity);
            }
            let response_id = new_id("crs")?;
            let event = coordination_event(
                "coordination_responded",
                &request.id,
                now,
                json!({"response_id": response_id, "target_work_id": target.work_id,
                    "kind": response_kind_name(reply.kind), "token": who.token,
                    "session": reply.session, "generation": target.generation}),
            )?;
            let seq = append(&mut tx, &event)?;
            tx.execute(
                "INSERT INTO hub_coordination_responses
                 (id,request,operation_id,target_work,token,agent,session,owner,login,
                  target_generation,kind,note,created_at,event_seq)
                 VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13,$14)",
                &[
                    &response_id,
                    &request.id,
                    &reply.operation_id,
                    &target.work_id,
                    &who.token,
                    &who.agent,
                    &reply.session,
                    &who.owner.id,
                    &who.owner.login,
                    &target.generation,
                    &response_kind_name(reply.kind),
                    &reply.note,
                    &now,
                    &seq,
                ],
            )?;
            tx.execute(
                "INSERT INTO hub_coordination_operations
                 (request,operation_id,kind,payload,event_seq) VALUES ($1,$2,'reply',$3,$4)",
                &[&request.id, &reply.operation_id, &payload, &seq],
            )?;
            let all_responded = coordination_all_targets_responded(&mut tx, &request.id)?;
            let status = if all_responded {
                CoordinationStatus::Responded
            } else {
                CoordinationStatus::Pending
            };
            save_coordination_version(
                &mut tx,
                &request.id,
                status,
                request.proposal.work_id.as_deref(),
                now,
                seq,
            )?;
            let response = json_string(&(
                &response_id,
                &reply.operation_id,
                &target.work_id,
                response_kind_name(reply.kind),
                &reply.note,
            ))?;
            adjust_work_payload_capacity(
                &mut tx,
                &who,
                coordination_payload_bytes(&[&payload, &response, ""], &event)?,
                now,
            )?;
            let updated = coordination_request_at(&mut tx, &who, &reply.request_id, i64::MAX, now)?
                .ok_or_else(|| HubError::Unavailable("coordination request disappeared".into()))?;
            tx.commit()?;
            Ok(updated)
        })
    }

    fn coordination_resolve(
        &self,
        token: &str,
        resolve: &CoordinationResolve,
        now: i64,
    ) -> Result<CoordinationRequest> {
        resolve.validate()?;
        let payload = json_string(resolve)?;
        self.run(false, |client| {
            let mut tx = client.transaction()?;
            let who = work_who(&mut tx, token, now)?;
            if who.read_only {
                return Err(HubError::Unauthorized);
            }
            lock_work_capacity(&mut tx, &who)?;
            let request =
                coordination_request_at(&mut tx, &who, &resolve.request_id, i64::MAX, now)?
                    .ok_or_else(|| HubError::Invalid("unknown coordination request".into()))?;
            let authorized = coordination_is_requester(&request, &who, &resolve.session)
                || request.targets.iter().any(|target| {
                    target.lead.token == who.token && target.lead.session == resolve.session
                });
            if !authorized {
                return Err(HubError::Unauthorized);
            }
            if coordination_operation_replay(
                &mut tx,
                &request.id,
                &resolve.operation_id,
                "resolve",
                &payload,
            )? {
                tx.commit()?;
                return Ok(request);
            }
            if !matches!(
                request.status,
                CoordinationStatus::Pending | CoordinationStatus::Responded
            ) {
                return Err(HubError::Invalid("coordination request is closed".into()));
            }
            if request.updated_seq != resolve.expected_updated_seq {
                return Err(HubError::Invalid(format!(
                    "coordination request changed; current updated_seq is {}",
                    request.updated_seq
                )));
            }
            let proposal_scope = if let Some(work) = &request.proposal.work_id {
                let item = scoped_work(&mut tx, &who, work, now, true)?
                    .ok_or_else(|| HubError::Unavailable("proposed work item is missing".into()))?;
                if item.state == WorkState::Finished {
                    Vec::new()
                } else if coordination_was_initially_bound(&mut tx, &request.id)? {
                    if item.scope != request.proposal.scope {
                        return Err(HubError::Invalid(
                            "queued coordination proposal scope has not been applied".into(),
                        ));
                    }
                    request.proposal.scope.clone()
                } else {
                    item.scope
                }
            } else {
                request.proposal.scope.clone()
            };
            let mut unresolved = work_scope_overlaps(
                &mut tx,
                &who,
                &proposal_scope,
                request.proposal.work_id.as_deref(),
                now,
            )?;
            let mut needs_coordination = Vec::new();
            for overlap in unresolved.drain(..) {
                let participating = tx
                    .query_opt(
                        "SELECT 1 FROM hub_work_participants
                     WHERE work=$1 AND token=$2 AND session=$3 AND generation=$4",
                        &[
                            &overlap.item.id,
                            &request.requester.token,
                            &request.requester.session,
                            &overlap.item.generation,
                        ],
                    )?
                    .is_some();
                if !participating {
                    needs_coordination.push(overlap);
                }
            }
            if !needs_coordination.is_empty() {
                let requester_who = WorkWho {
                    token: request.requester.token.clone(),
                    repository: who.repository.clone(),
                    repository_id: who.repository_id,
                    agent: request.requester.agent.clone(),
                    read_only: false,
                    owner: User {
                        id: request.requester.owner.clone(),
                        identity: who.owner.identity.clone(),
                        login: request.requester.login.clone(),
                        disabled: false,
                    },
                };
                let refreshed = create_coordination_request(
                    &mut tx,
                    &requester_who,
                    &request.requester.session,
                    request.proposal.clone(),
                    &needs_coordination,
                    now,
                )?;
                tx.commit()?;
                return Err(HubError::WorkCoordinationRequired {
                    overlaps: needs_coordination,
                    request: Box::new(refreshed),
                });
            }
            let event = coordination_event(
                "coordination_resolved",
                &request.id,
                now,
                json!({"by": who.token, "session": resolve.session}),
            )?;
            let seq = append(&mut tx, &event)?;
            tx.execute(
                "INSERT INTO hub_coordination_operations
                 (request,operation_id,kind,payload,event_seq) VALUES ($1,$2,'resolve',$3,$4)",
                &[&request.id, &resolve.operation_id, &payload, &seq],
            )?;
            save_coordination_version(
                &mut tx,
                &request.id,
                CoordinationStatus::Resolved,
                request.proposal.work_id.as_deref(),
                now,
                seq,
            )?;
            adjust_work_payload_capacity(
                &mut tx,
                &who,
                coordination_payload_bytes(&[&payload, ""], &event)?,
                now,
            )?;
            let updated =
                coordination_request_at(&mut tx, &who, &resolve.request_id, i64::MAX, now)?
                    .ok_or_else(|| {
                        HubError::Unavailable("coordination request disappeared".into())
                    })?;
            tx.commit()?;
            Ok(updated)
        })
    }

    fn coordination_withdraw(
        &self,
        token: &str,
        withdraw: &CoordinationWithdraw,
        now: i64,
    ) -> Result<CoordinationRequest> {
        withdraw.validate()?;
        self.run(false, |client| {
            let mut tx = client.transaction()?;
            let who = work_who(&mut tx, token, now)?;
            if who.read_only {
                return Err(HubError::Unauthorized);
            }
            lock_work_capacity(&mut tx, &who)?;
            let request =
                coordination_request_at(&mut tx, &who, &withdraw.request_id, i64::MAX, now)?
                    .ok_or_else(|| HubError::Invalid("unknown coordination request".into()))?;
            let requester = coordination_is_requester(&request, &who, &withdraw.session);
            let requester_available = tx
                .query_opt(
                    "SELECT 1 FROM hub_agent_tokens t JOIN hub_users u ON u.id=t.owner
                     WHERE t.id=$1 AND NOT t.revoked AND t.expires_at>$2 AND NOT u.disabled",
                    &[&request.requester.token, &now],
                )?
                .is_some();
            let current_target_lead = request.targets.iter().any(|target| {
                target.lead.token == who.token && target.lead.session == withdraw.session
            });
            let cleanup = !requester && !requester_available && current_target_lead;
            if !requester && !cleanup {
                return Err(HubError::Unauthorized);
            }
            let reason = if cleanup {
                "requester_unavailable"
            } else {
                "requester_withdrawn"
            };
            let payload = json_string(&(withdraw, reason))?;
            if coordination_operation_replay(
                &mut tx,
                &request.id,
                &withdraw.operation_id,
                "withdraw",
                &payload,
            )? {
                tx.commit()?;
                return Ok(request);
            }
            if !matches!(
                request.status,
                CoordinationStatus::Pending | CoordinationStatus::Responded
            ) {
                return Err(HubError::Invalid("coordination request is closed".into()));
            }
            if request.updated_seq != withdraw.expected_updated_seq {
                return Err(HubError::Invalid(format!(
                    "coordination request changed; current updated_seq is {}",
                    request.updated_seq
                )));
            }
            let event = coordination_event(
                "coordination_withdrawn",
                &request.id,
                now,
                json!({"by": who.token, "session": withdraw.session, "reason": reason}),
            )?;
            let seq = append(&mut tx, &event)?;
            tx.execute(
                "INSERT INTO hub_coordination_operations
                 (request,operation_id,kind,payload,event_seq) VALUES ($1,$2,'withdraw',$3,$4)",
                &[&request.id, &withdraw.operation_id, &payload, &seq],
            )?;
            save_coordination_version(
                &mut tx,
                &request.id,
                CoordinationStatus::Withdrawn,
                request.proposal.work_id.as_deref(),
                now,
                seq,
            )?;
            adjust_work_payload_capacity(
                &mut tx,
                &who,
                coordination_payload_bytes(&[&payload, ""], &event)?,
                now,
            )?;
            let updated =
                coordination_request_at(&mut tx, &who, &withdraw.request_id, i64::MAX, now)?
                    .ok_or_else(|| {
                        HubError::Unavailable("coordination request disappeared".into())
                    })?;
            tx.commit()?;
            Ok(updated)
        })
    }

    fn events_after(&self, seq: i64, limit: usize) -> Result<Vec<HubEvent>> {
        self.run(true, |client| {
            let rows = client.query(
                "SELECT seq, id, at, kind, subject, data FROM hub_events
                 WHERE seq > $1 ORDER BY seq LIMIT $2",
                &[&seq, &event_limit(limit)],
            )?;
            Ok(rows
                .iter()
                .map(|row| HubEvent {
                    seq: row.get(0),
                    id: row.get(1),
                    at: row.get(2),
                    kind: row.get(3),
                    subject: row.get(4),
                    data: row.get(5),
                })
                .collect())
        })
    }
}

// The accepted URLs include a Unix socket path.
#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[test]
    fn plaintext_tcp_needs_loopback_or_explicit_sslmode_disable() {
        for url in [
            "postgres://u:pw@127.0.0.1/db",
            "postgres://u:pw@[::1]/db",
            "postgres://u:pw@/db?host=/var/run/postgresql",
            "postgres://u:pw@db.example.com/db?sslmode=disable",
        ] {
            assert!(config(url).is_ok(), "{url}");
        }
        for url in [
            "postgres://u:pw@db.example.com/db",
            "postgres://u:pw@10.0.0.5/db?sslmode=prefer",
            "postgres://u:pw@localhost/db?hostaddr=10.0.0.5",
            "postgres://u:pw@localhost/db",
            "postgres://u:pw@db.example.com/db?sslmode=require",
        ] {
            let message = config(url).err().unwrap().to_string();
            assert!(message.contains("needs TLS"), "{url}: {message}");
            assert!(!message.contains("pw"), "{message}");
        }
        let message = config("postgres://u:pw@h:notaport/db")
            .err()
            .unwrap()
            .to_string();
        assert!(!message.contains("pw"), "{message}");
    }
}
