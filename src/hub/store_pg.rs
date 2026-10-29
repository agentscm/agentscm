use std::collections::{BTreeMap, BTreeSet};
use std::net::IpAddr;
use std::str::FromStr;
use std::sync::{Mutex, PoisonError};
use std::time::Duration;

use postgres::config::{Host, SslMode};
use postgres::{Client, Config, IsolationLevel, NoTls};
use serde_json::json;

use crate::change_record::{
    ActionFinishV1, ActionMutationReceiptV1, ActionStartV1, ArtifactRefV1, CaptureBatchV1,
    CapturedEventV1, ChangeRecordSummaryV1, ConversationEntryV1, IngestReceiptV1,
    MAX_CONVERSATION_PAGE_ENTRIES, MAX_PROJECTED_SESSION_SETUPS, RetentionClassV1, SessionSetupV1,
    parse_conversation_cursor,
};

use super::catchup::{
    CATCHUP_AGENT_DELIVERY_CAPACITY_BYTES, CATCHUP_AGENT_TOKEN_DELIVERY_CAPACITY_BYTES,
    CATCHUP_DELIVERY_CAPACITY_BYTES, CATCHUP_PREVIEW_LIMIT, CatchupAck, CatchupAckResult,
    CatchupAckStatus, CatchupCoordinationEvent, CatchupMergeObservation, CatchupMergeSource,
    CatchupPage, CatchupPageWindow, CatchupPreview, CatchupPreviewQuery, CatchupPublicationRepair,
    CatchupPublicationRepairResult, CatchupQuery, CatchupRepositoryScope, CatchupRetention,
    CatchupSourceCoverage, CatchupSourceStatus, CatchupWorkEvent, HUMAN_SUBSCRIBER,
    MAX_ACTIVE_AGENT_DELIVERIES, MAX_ACTIVE_AGENT_DELIVERIES_PER_TOKEN,
    MAX_ACTIVE_CATCHUP_DELIVERIES, MAX_ACTIVE_HUMAN_DELIVERIES, MAX_CATCHUP_MERGE_BYTES,
    MAX_CATCHUP_MERGE_ROWS, MAX_CATCHUP_PAGE, MAX_CATCHUP_PAGE_PAYLOAD_BYTES, NewCatchupMerge,
    TrackedPublicationMergeInput, coverage_name, parse_coverage, parse_source,
    repository_namespace, source_name, tracked_publication_merge,
};
use super::change_record::{
    SessionStatsV1, SharedBodyAccess, SharedBodyRow, SharedBodyState, StoredActionV1,
    StoredDecisionV1, project as project_change_record,
};
use super::connection::{
    CONNECTION_SELECT, ConnectedRepository, Connection as ForgeConnection, DELIVERY_KEEP_SECONDS,
    ForgeCheck, ForgeCheckReserve, MergeGroup, NewConnection, NewForgeCheck, NewMergeGroup,
    Observation, Observed, Outbound, REPOSITORY_SELECT, gone_event, is_lower_hex,
    observation_kinds, validate_check_key, validate_check_phase, validate_group_note,
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
use super::device::{
    DEVICE_IDLE_SECONDS, DEVICE_TOUCH_SECONDS, Device, DevicePrincipal, MAX_DEVICES, MAX_REFUSALS,
    REFUSAL_SECONDS, Refusal, RepositoryUsage, validate_device_name,
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
use super::promise::{
    PromiseAdoptV1, PromiseChangeAuthority, PromiseChangeV1, PromiseCreateV1,
    PromiseCurrentAdmissionsV1, PromiseHistoryPageV1, PromiseInputPageV1, PromiseInputReceiptV1,
    PromiseInputV1, PromiseInputViewV1, PromisePageV1, PromiseReceiptV1,
    PromiseRepositoryAdmissionV1, PromiseStateV1, PromiseVersionKindV1, PromiseVersionV1,
    PromiseViewV1, admissions_current, current_admission, decode_current_scope, decode_input_view,
    decode_lifetime_scope, decode_version, encode as encode_promise, encoded_admissions_authorized,
    ensure_admissions_digest, extend_lifetime, operation_fingerprint,
    payload_admissions_authorized, scope_authorized, valid_promise_id, valid_promise_input_id,
};
use super::publish::{
    Create, MAX_UNFINISHED, NewPublication, PUBLICATION_SELECT, Publication, PublicationSearch,
    SEARCH_SCAN_ROWS, Update,
};
use super::session::{
    Checked, DEFAULT_SESSION, NOTICES_PER_TOUCH, Row, SESSION_TTL_SECONDS, STALE_SESSION_SECONDS,
    SessionCall, Touched, check, check_name, expiry_cause, guard, held, plan as plan_touch,
};
use super::skills::{EvidenceRow, RunHead, SkillAdoption};
use super::store::{
    ACTIVITY_KINDS, ActivityEvent, AgentToken, BROWSER_SESSION_MAX_SECONDS, Event, ForgeIdentity,
    HubEvent, LIST_LIMIT, MAX_BROWSER_SESSIONS, NAMED_MIGRATIONS, NewAgentToken, Principal,
    PromiseInputScope, SLIDE_STEP_SECONDS, SessionView, Store, TokenDigest, User, event_limit,
    like_pattern, lower_hex_of, new_id, pending, plan_named, search_number, session_key, unix_now,
    validate_login,
};
use super::work::{
    Briefing, BriefingAck, BriefingQuery, CLAIM_STALE_SECONDS, Checkpoint, CheckpointReceipt,
    ClaimStatus, MAX_BRIEFING_BYTES, SourceRef, WORK_DELIVERY_RESERVATION_BYTES,
    WORK_PAYLOAD_CAPACITY_BYTES, WorkCheckpoint, WorkCheckpointResult, WorkCompaction,
    WorkConflictKind, WorkEvent, WorkItem, WorkOutcome, WorkOutcomeKind, WorkOverview,
    WorkOverviewQuery, WorkParticipant, WorkRepositoryScope, WorkRole, WorkScopeCheck,
    WorkScopeOverlap, WorkSearchItem, WorkSearchPage, WorkSource, WorkStart, WorkStartConflict,
    WorkStartResult, WorkState, WorkWho, check_work_delivery_page, check_work_payload_capacity,
    normalized_scope, scope_overlap_reasons, uncovered_scope, work_overlap_pairs,
    work_payload_row_bytes,
};
use super::work_preflight::{WorkPreflightResult, WorkPreflightStatus};
use super::{HubError, Result};

mod preflight;

fn require_clear_preflight(
    who: &WorkWho,
    start: &WorkStart,
    result: &WorkPreflightResult,
) -> Result<()> {
    if start.preflight.is_none()
        || (result.status == WorkPreflightStatus::ClearWithinCheckedWindow && result.complete)
    {
        return Ok(());
    }
    let own_explicit_resume = result.subject_match.as_ref().is_some_and(|item| {
        start.role == WorkRole::Lead
            && start.work_id.as_deref() == Some(item.id.as_str())
            && item.lead_token == who.token
            && ((item.lead_session == start.session && start.takeover_generation.is_none())
                || (item.state == WorkState::Paused
                    && start.takeover_generation == Some(item.generation)))
    });
    if own_explicit_resume {
        Ok(())
    } else {
        Err(HubError::WorkPreflight(Box::new(result.clone())))
    }
}

/// Held for the whole migration transaction so hubs starting together serialize.
const MIGRATION_LOCK: i64 = 0x6167_656e_7473_636d;
/// Serializes event appends so `seq` order is commit order across hub processes.
const EVENT_LOCK: i64 = 0x6167_656e_7473_6576;
const PROMISE_OPERATION_LOCK_CLASS: i32 = 0x7072_6f6d;
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

const MAX_CHANGE_EVENTS_PER_SESSION: i64 = 100_000;
const MAX_CHANGE_ACTIONS_PER_WORK: i64 = 10_000;
const MAX_CHANGE_OPERATIONS_PER_WORK: i64 = 120_000;

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
            // Enabling someone again must not bring back a browser they were signed in on.
            if disabled {
                tx.execute(
                    "DELETE FROM hub_browser_sessions WHERE owner = $1",
                    &[&user],
                )?;
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

/// Revokes a live computer and every agent token it minted, in `tx`.
fn revoke_device_in(
    tx: &mut postgres::Transaction<'_>,
    device: &str,
    owner: Option<&str>,
    by: &str,
    now: i64,
) -> Result<bool> {
    let changed = tx.execute(
        "UPDATE hub_devices SET revoked_at = $3, revoked_by = $4
         WHERE id = $1 AND revoked_at IS NULL AND ($2::text IS NULL OR owner = $2)",
        &[&device, &owner, &now, &by],
    )?;
    if changed == 0 {
        return Ok(false);
    }
    append(tx, &Event::device_revoked(device, by)?)?;
    let tokens: Vec<String> = tx
        .query(
            "UPDATE hub_agent_tokens SET revoked = TRUE WHERE device_id = $1 AND NOT revoked
             RETURNING id",
            &[&device],
        )?
        .iter()
        .map(|row| row.get(0))
        .collect();
    for token in tokens {
        append(tx, &Event::token_revoked(&token, Some(by))?)?;
    }
    Ok(true)
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

const SHARED_BODY_COLUMNS: &str = "work,digest,size_bytes,session_id,generation,event_id,sequence,
    declared_token,declared_at,state,uploaded_at,expires_at,deleted_at,deleted_by,physical_deleted_at,
    author,delete_attempts";
/// Where a query that appends columns after `SHARED_BODY_COLUMNS` finds them.
const SHARED_BODY_COLUMN_COUNT: usize = 17;

fn shared_body_row(row: &postgres::Row) -> Result<SharedBodyRow> {
    Ok(SharedBodyRow {
        work: row.get(0),
        digest: row.get(1),
        size_bytes: row.get::<_, i64>(2) as u64,
        session_id: row.get(3),
        generation: row.get(4),
        event_id: row.get(5),
        sequence: row.get::<_, i64>(6) as u64,
        declared_token: row.get(7),
        declared_at: row.get(8),
        state: SharedBodyState::parse(&row.get::<_, String>(9))?,
        uploaded_at: row.get(10),
        expires_at: row.get(11),
        deleted_at: row.get(12),
        deleted_by: row.get(13),
        physical_deleted_at: row.get(14),
        author: row.get(15),
        delete_attempts: row.get(16),
    })
}

/// Records that an accepted event named a shared body for the human owning `token`. Each human's
/// declaration of a digest is its own row, and a later one must agree on size with the same
/// human's earlier one (never another's, which would leak that they declared it).
fn declare_shared_body(
    tx: &mut postgres::Transaction<'_>,
    batch: &CaptureBatchV1,
    token: &str,
    event: &CapturedEventV1,
    artifact: &ArtifactRefV1,
    now: i64,
) -> Result<()> {
    let size = artifact.size_bytes as i64;
    let sequence = event.sequence as i64;
    let declared = tx.query_opt(
        "SELECT size_bytes FROM hub_change_shared_bodies WHERE work=$1 AND digest=$2
           AND author=(SELECT owner FROM hub_agent_tokens WHERE id=$3)",
        &[&batch.work_id, &artifact.digest, &token],
    )?;
    if declared.is_some_and(|declared| declared.get::<_, i64>(0) != size) {
        return Err(HubError::Invalid(
            "shared body digest was already declared with a different size".into(),
        ));
    }
    tx.execute(
        "INSERT INTO hub_change_shared_bodies
         (work,digest,author,size_bytes,media,session_id,generation,event_id,sequence,
          declared_token,declared_at,state)
         VALUES ($1,$2,(SELECT owner FROM hub_agent_tokens WHERE id=$8),$3,'json',$4,$5,$6,$7,
                 $8,$9,'declared')
         ON CONFLICT (work,digest,author) DO NOTHING",
        &[
            &batch.work_id,
            &artifact.digest,
            &size,
            &batch.session.session_id,
            &batch.generation,
            &event.event_id,
            &sequence,
            &token,
            &now,
        ],
    )?;
    Ok(())
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

fn catchup_repository_key(repository_id: Option<i64>) -> i64 {
    repository_id.unwrap_or(-1)
}

fn pg_check_catchup_delivery_capacity(
    tx: &mut postgres::Transaction<'_>,
    who: &WorkWho,
    repository: &str,
    key: i64,
    subscriber_token: Option<&str>,
    previous_bytes: i64,
    new_bytes: i64,
) -> Result<()> {
    let total: i64 = tx
        .query_one(
            "SELECT COALESCE(SUM(octet_length(page)),0)::bigint FROM hub_catchup_deliveries
        WHERE owner=$1 AND provider=$2 AND host=$3 AND repository=$4 AND repository_key=$5",
            &[
                &who.owner.id,
                &who.owner.identity.provider,
                &who.owner.identity.host,
                &repository,
                &key,
            ],
        )?
        .get(0);
    if total
        .saturating_sub(previous_bytes)
        .saturating_add(new_bytes)
        > CATCHUP_DELIVERY_CAPACITY_BYTES
    {
        return Err(HubError::WorkCapacity);
    }
    if let Some(token) = subscriber_token {
        let agents: i64 = tx
            .query_one(
                "SELECT COALESCE(SUM(octet_length(page)),0)::bigint FROM hub_catchup_deliveries
            WHERE owner=$1 AND provider=$2 AND host=$3 AND repository=$4 AND repository_key=$5
              AND subscriber_token IS NOT NULL",
                &[
                    &who.owner.id,
                    &who.owner.identity.provider,
                    &who.owner.identity.host,
                    &repository,
                    &key,
                ],
            )?
            .get(0);
        let token_bytes: i64 = tx
            .query_one(
                "SELECT COALESCE(SUM(octet_length(page)),0)::bigint FROM hub_catchup_deliveries
            WHERE owner=$1 AND provider=$2 AND host=$3 AND repository=$4 AND repository_key=$5
              AND subscriber_token=$6",
                &[
                    &who.owner.id,
                    &who.owner.identity.provider,
                    &who.owner.identity.host,
                    &repository,
                    &key,
                    &token,
                ],
            )?
            .get(0);
        if agents
            .saturating_sub(previous_bytes)
            .saturating_add(new_bytes)
            > CATCHUP_AGENT_DELIVERY_CAPACITY_BYTES
            || token_bytes
                .saturating_sub(previous_bytes)
                .saturating_add(new_bytes)
                > CATCHUP_AGENT_TOKEN_DELIVERY_CAPACITY_BYTES
        {
            return Err(HubError::WorkCapacity);
        }
    }
    Ok(())
}

fn catchup_merge_id(
    scope: &CatchupRepositoryScope,
    pull_number: i64,
    merge_commit: &str,
) -> String {
    let input = format!(
        "agentscm.hub.catchup-merge.v1\0{}\0{}\0{}\0{}\0{}\0{}",
        scope.provider,
        scope.host,
        repository_namespace(&scope.repository, scope.repository_id),
        catchup_repository_key(scope.repository_id),
        pull_number,
        merge_commit
    );
    format!("mrg_{}", &lower_hex_of(input.as_bytes())[..32])
}

fn pg_catchup_merge(row: &postgres::Row) -> Result<CatchupMergeObservation> {
    Ok(CatchupMergeObservation {
        id: row.get(0),
        seq: row.get(1),
        source: parse_source(row.get::<_, String>(2).as_str())?,
        source_id: row.get(3),
        occurred_at: row.get(4),
        observed_at: row.get(5),
        title: row.get(6),
        branch: row.get(7),
        base: row.get(8),
        head_commit: row.get(9),
        merge_commit: row.get(10),
        pull_number: row.get(11),
        paths: json_value(row.get::<_, String>(12).as_str())?,
        paths_complete: row.get(13),
    })
}

const CATCHUP_MERGE_COLUMNS: &str = "id,seq,source,source_id,occurred_at,observed_at,title,branch,base,head_commit,merge_commit,pull_number,paths,paths_complete";

fn pg_record_catchup_merge(
    tx: &mut postgres::Transaction<'_>,
    scope: &CatchupRepositoryScope,
    merge: &NewCatchupMerge,
    observed_at: i64,
) -> Result<CatchupMergeObservation> {
    scope.validate()?;
    merge.validate(observed_at)?;
    lock_work_repository(
        tx,
        &scope.provider,
        &scope.host,
        &scope.repository,
        scope.repository_id,
    )?;
    let key = catchup_repository_key(scope.repository_id);
    let repository = repository_namespace(&scope.repository, scope.repository_id);
    tx.execute("INSERT INTO hub_catchup_repositories
        (provider,host,repository,repository_key,repository_id,next_merge_seq,merge_floor_seq,created_at,updated_at)
        VALUES ($1,$2,$3,$4,$5,1,0,$6,$6) ON CONFLICT DO NOTHING",
        &[&scope.provider,&scope.host,&repository,&key,&scope.repository_id,&observed_at])?;
    if merge.source == CatchupMergeSource::TrackedPublication {
        tx.execute(
            "INSERT INTO hub_catchup_sources
                (provider,host,repository,repository_key,repository_id,source,state,checked_at,unread_pages,deferred)
             VALUES ($1,$2,$3,$4,$5,'tracked_publication','incomplete',$6,1,0)
             ON CONFLICT(provider,host,repository,repository_key,source) DO UPDATE SET
                checked_at=GREATEST(hub_catchup_sources.checked_at,EXCLUDED.checked_at)",
            &[&scope.provider,&scope.host,&repository,&key,&scope.repository_id,&observed_at],
        )?;
    }
    if let Some(row) = tx.query_opt(
        &format!(
            "SELECT {CATCHUP_MERGE_COLUMNS} FROM hub_catchup_merges
        WHERE provider=$1 AND host=$2 AND repository=$3 AND repository_key=$4
          AND pull_number=$5 AND merge_commit=$6"
        ),
        &[
            &scope.provider,
            &scope.host,
            &repository,
            &key,
            &merge.pull_number,
            &merge.merge_commit,
        ],
    )? {
        return pg_catchup_merge(&row);
    }
    let seq: i64 = tx
        .query_one(
            "SELECT next_merge_seq FROM hub_catchup_repositories
        WHERE provider=$1 AND host=$2 AND repository=$3 AND repository_key=$4 FOR UPDATE",
            &[&scope.provider, &scope.host, &repository, &key],
        )?
        .get(0);
    if let Some(row) = tx.query_opt(
        &format!(
            "SELECT {CATCHUP_MERGE_COLUMNS} FROM hub_catchup_merges
        WHERE provider=$1 AND host=$2 AND repository=$3 AND repository_key=$4
          AND pull_number=$5 AND merge_commit=$6"
        ),
        &[
            &scope.provider,
            &scope.host,
            &repository,
            &key,
            &merge.pull_number,
            &merge.merge_commit,
        ],
    )? {
        return pg_catchup_merge(&row);
    }
    let id = catchup_merge_id(scope, merge.pull_number, &merge.merge_commit);
    let paths = json_string(&merge.paths)?;
    let payload_bytes = [
        merge.source_id.len(),
        merge.title.as_ref().map_or(0, String::len),
        merge.branch.len(),
        merge.base.len(),
        merge.head_commit.len(),
        merge.merge_commit.len(),
        paths.len(),
        128,
    ]
    .into_iter()
    .sum::<usize>() as i64;
    let usage=tx.query_one("SELECT COUNT(*)::bigint,COALESCE(SUM(payload_bytes),0)::bigint
        FROM hub_catchup_merges WHERE provider=$1 AND host=$2 AND repository=$3 AND repository_key=$4",
        &[&scope.provider,&scope.host,&repository,&key])?;
    let retained_rows: i64 = usage.get(0);
    let retained_bytes: i64 = usage.get(1);
    if retained_rows >= MAX_CATCHUP_MERGE_ROWS
        || retained_bytes.saturating_add(payload_bytes) > MAX_CATCHUP_MERGE_BYTES
    {
        return Err(HubError::WorkCapacity);
    }
    tx.execute(
        "INSERT INTO hub_catchup_merges
        (provider,host,repository,repository_key,repository_id,seq,id,source,source_id,occurred_at,
         observed_at,title,branch,base,head_commit,merge_commit,pull_number,paths,paths_complete,payload_bytes)
        VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13,$14,$15,$16,$17,$18,$19,$20)",
        &[
            &scope.provider,
            &scope.host,
            &repository,
            &key,
            &scope.repository_id,
            &seq,
            &id,
            &source_name(merge.source),
            &merge.source_id,
            &merge.occurred_at,
            &observed_at,
            &merge.title,
            &merge.branch,
            &merge.base,
            &merge.head_commit,
            &merge.merge_commit,
            &merge.pull_number,
            &paths,
            &merge.paths_complete,
            &payload_bytes,
        ],
    )?;
    tx.execute(
        "UPDATE hub_catchup_repositories SET next_merge_seq=$5,updated_at=$6
        WHERE provider=$1 AND host=$2 AND repository=$3 AND repository_key=$4",
        &[
            &scope.provider,
            &scope.host,
            &repository,
            &key,
            &(seq + 1),
            &observed_at,
        ],
    )?;
    pg_catchup_merge(&tx.query_one(
        &format!("SELECT {CATCHUP_MERGE_COLUMNS} FROM hub_catchup_merges WHERE id=$1"),
        &[&id],
    )?)
}

const CATCHUP_BACKFILL_BATCH: usize = 100;

fn pg_publication_catchup_merge(publication: &Publication, now: i64) -> Result<NewCatchupMerge> {
    let manifest = publication.manifest()?;
    let paths = manifest
        .changes
        .iter()
        .map(|change| change.path.clone())
        .collect::<Vec<_>>();
    tracked_publication_merge(TrackedPublicationMergeInput {
        source_id: publication.id.clone(),
        title: manifest.title.clone(),
        branch: publication.branch.clone(),
        base: publication.base.clone(),
        head_commit: publication.commit.clone(),
        merge_commit: publication.merge_commit.clone(),
        pull_number: publication.pr_number,
        paths,
        observed_at: now,
    })
}

fn pg_record_publication_failure(
    tx: &mut postgres::Transaction<'_>,
    scope: &CatchupRepositoryScope,
    publication: &str,
    now: i64,
) -> Result<()> {
    tx.execute(
        "INSERT INTO hub_catchup_publication_failures
            (provider,host,repository,repository_key,repository_id,publication,attempts,created_at,updated_at)
         VALUES ($1,$2,$3,$4,$5,$6,1,$7,$7)
         ON CONFLICT(provider,host,repository,repository_key,publication) DO UPDATE SET
            attempts=LEAST(1000000,hub_catchup_publication_failures.attempts+1),
            updated_at=EXCLUDED.updated_at",
        &[&scope.provider,&scope.host,&repository_namespace(&scope.repository,scope.repository_id),
          &catchup_repository_key(scope.repository_id),&scope.repository_id,&publication,&now],
    )?;
    Ok(())
}

fn pg_clear_publication_failure(
    tx: &mut postgres::Transaction<'_>,
    scope: &CatchupRepositoryScope,
    publication: &str,
) -> Result<()> {
    tx.execute(
        "DELETE FROM hub_catchup_publication_failures WHERE provider=$1 AND host=$2
           AND repository=$3 AND repository_key=$4 AND publication=$5",
        &[
            &scope.provider,
            &scope.host,
            &repository_namespace(&scope.repository, scope.repository_id),
            &catchup_repository_key(scope.repository_id),
            &publication,
        ],
    )?;
    Ok(())
}

fn pg_refresh_publication_coverage(
    tx: &mut postgres::Transaction<'_>,
    scope: &CatchupRepositoryScope,
    now: i64,
) -> Result<()> {
    let repository = repository_namespace(&scope.repository, scope.repository_id);
    let key = catchup_repository_key(scope.repository_id);
    let complete = tx
        .query_opt(
            "SELECT complete FROM hub_catchup_backfills WHERE provider=$1 AND host=$2
             AND repository=$3 AND repository_key=$4",
            &[&scope.provider, &scope.host, &repository, &key],
        )?
        .is_some_and(|row| row.get(0));
    let deferred: i64 = tx
        .query_one(
            "SELECT COUNT(*)::bigint FROM hub_catchup_publication_failures
             WHERE provider=$1 AND host=$2 AND repository=$3 AND repository_key=$4",
            &[&scope.provider, &scope.host, &repository, &key],
        )?
        .get(0);
    let state = if complete && deferred == 0 {
        "observed_only"
    } else {
        "incomplete"
    };
    tx.execute(
        "INSERT INTO hub_catchup_sources
            (provider,host,repository,repository_key,repository_id,source,state,checked_at,unread_pages,deferred)
         VALUES ($1,$2,$3,$4,$5,'tracked_publication',$6,$7,$8,$9)
         ON CONFLICT(provider,host,repository,repository_key,source) DO UPDATE SET
            state=EXCLUDED.state,checked_at=GREATEST(hub_catchup_sources.checked_at,EXCLUDED.checked_at),
            unread_pages=EXCLUDED.unread_pages,deferred=EXCLUDED.deferred",
        &[&scope.provider,&scope.host,&repository,&key,&scope.repository_id,&state,&now,
          &i64::from(!complete),&deferred],
    )?;
    Ok(())
}

fn pg_retain_publication_projection(
    tx: &mut postgres::Transaction<'_>,
    scope: &CatchupRepositoryScope,
    publication: &Publication,
    now: i64,
) -> Result<()> {
    let merge = match pg_publication_catchup_merge(publication, now) {
        Ok(merge) => merge,
        Err(_) => {
            pg_record_publication_failure(tx, scope, &publication.id, now)?;
            return Ok(());
        }
    };
    tx.batch_execute("SAVEPOINT catchup_publication_projection")?;
    match pg_record_catchup_merge(tx, scope, &merge, now) {
        Ok(_) => {
            tx.batch_execute("RELEASE catchup_publication_projection")?;
            pg_clear_publication_failure(tx, scope, &publication.id)?;
        }
        Err(_) => {
            tx.batch_execute(
                "ROLLBACK TO catchup_publication_projection; RELEASE catchup_publication_projection",
            )?;
            pg_record_publication_failure(tx, scope, &publication.id, now)?;
        }
    }
    Ok(())
}

fn pg_mark_backfill_unavailable(
    tx: &mut postgres::Transaction<'_>,
    who: &WorkWho,
    now: i64,
) -> Result<()> {
    tx.execute(
        "INSERT INTO hub_catchup_sources
            (provider,host,repository,repository_key,repository_id,source,state,checked_at,unread_pages,deferred)
         VALUES ($1,$2,$3,$4,$5,'tracked_publication','unavailable',$6,1,0)
         ON CONFLICT(provider,host,repository,repository_key,source) DO UPDATE SET
            state='unavailable',checked_at=GREATEST(hub_catchup_sources.checked_at,EXCLUDED.checked_at),
            unread_pages=1",
        &[&who.owner.identity.provider,&who.owner.identity.host,
          &repository_namespace(&who.repository,who.repository_id),&catchup_repository_key(who.repository_id),
          &who.repository_id,&now],
    )?;
    Ok(())
}

fn pg_try_backfill_catchup_publications(
    tx: &mut postgres::Transaction<'_>,
    who: &WorkWho,
    now: i64,
) -> Result<()> {
    tx.batch_execute("SAVEPOINT catchup_backfill")?;
    match pg_backfill_catchup_publications(tx, who, now) {
        Ok(()) => {
            tx.batch_execute("RELEASE catchup_backfill")?;
            Ok(())
        }
        Err(_) => {
            tx.batch_execute("ROLLBACK TO catchup_backfill; RELEASE catchup_backfill")?;
            pg_mark_backfill_unavailable(tx, who, now)
        }
    }
}

fn pg_backfill_catchup_publications(
    tx: &mut postgres::Transaction<'_>,
    who: &WorkWho,
    now: i64,
) -> Result<()> {
    lock_work_repository(
        tx,
        &who.owner.identity.provider,
        &who.owner.identity.host,
        &who.repository,
        who.repository_id,
    )?;
    let key = catchup_repository_key(who.repository_id);
    let repository = repository_namespace(&who.repository, who.repository_id);
    tx.execute(
        "INSERT INTO hub_catchup_backfills
            (provider,host,repository,repository_key,repository_id,after_publication,complete,deferred,updated_at)
         VALUES ($1,$2,$3,$4,$5,'',FALSE,0,$6) ON CONFLICT DO NOTHING",
        &[&who.owner.identity.provider,&who.owner.identity.host,&repository,&key,
            &who.repository_id,&now],
    )?;
    let row = tx.query_one(
        "SELECT after_publication,complete FROM hub_catchup_backfills
         WHERE provider=$1 AND host=$2 AND repository=$3 AND repository_key=$4 FOR UPDATE",
        &[
            &who.owner.identity.provider,
            &who.owner.identity.host,
            &repository,
            &key,
        ],
    )?;
    let after: String = row.get(0);
    let complete: bool = row.get(1);
    if complete {
        return Ok(());
    }
    let limit = (CATCHUP_BACKFILL_BATCH + 1) as i64;
    let mut rows = tx.query(
        "SELECT p.id,p.branch,p.base,p.commit_sha,p.merge_commit,p.pr_number
         FROM hub_publications p JOIN hub_agent_tokens t ON t.id=p.token
         JOIN hub_users u ON u.id=t.owner
         WHERE u.provider=$1 AND u.host=$2
           AND (($4::bigint IS NOT NULL AND t.repository_id=$4)
             OR ($4::bigint IS NULL AND t.repository_id IS NULL AND p.repository=$3))
           AND p.merge_commit IS NOT NULL
           AND p.pr_number IS NOT NULL AND p.id>$5 ORDER BY p.id LIMIT $6",
        &[
            &who.owner.identity.provider,
            &who.owner.identity.host,
            &who.repository,
            &who.repository_id,
            &after,
            &limit,
        ],
    )?;
    let has_more = rows.len() > CATCHUP_BACKFILL_BATCH;
    rows.truncate(CATCHUP_BACKFILL_BATCH);
    let mut last = after;
    let scope = CatchupRepositoryScope {
        provider: who.owner.identity.provider.clone(),
        host: who.owner.identity.host.clone(),
        repository: who.repository.clone(),
        repository_id: who.repository_id,
    };
    for row in rows {
        let id: String = row.get(0);
        last = id.clone();
        match tracked_publication_merge(TrackedPublicationMergeInput {
            source_id: id,
            title: String::new(),
            branch: row.get(1),
            base: row.get(2),
            head_commit: row.get(3),
            merge_commit: row.get(4),
            pull_number: row.get(5),
            paths: Vec::new(),
            observed_at: now,
        }) {
            Ok(mut merge) => {
                merge.title = None;
                merge.paths_complete = false;
                tx.batch_execute("SAVEPOINT catchup_backfill_publication")?;
                match pg_record_catchup_merge(tx, &scope, &merge, now) {
                    Ok(_) => {
                        tx.batch_execute("RELEASE catchup_backfill_publication")?;
                        pg_clear_publication_failure(tx, &scope, &merge.source_id)?;
                    }
                    Err(_) => {
                        tx.batch_execute("ROLLBACK TO catchup_backfill_publication; RELEASE catchup_backfill_publication")?;
                        pg_record_publication_failure(tx, &scope, &merge.source_id, now)?;
                    }
                }
            }
            Err(_) => pg_record_publication_failure(tx, &scope, &last, now)?,
        }
    }
    let deferred: i64 = tx
        .query_one(
            "SELECT COUNT(*)::bigint FROM hub_catchup_publication_failures
         WHERE provider=$1 AND host=$2 AND repository=$3 AND repository_key=$4",
            &[
                &who.owner.identity.provider,
                &who.owner.identity.host,
                &repository,
                &key,
            ],
        )?
        .get(0);
    tx.execute(
        "UPDATE hub_catchup_backfills SET after_publication=$5,complete=$6,deferred=$7,updated_at=$8
         WHERE provider=$1 AND host=$2 AND repository=$3 AND repository_key=$4",
        &[&who.owner.identity.provider,&who.owner.identity.host,&repository,&key,
            &last,&(!has_more),&deferred,&now],
    )?;
    pg_refresh_publication_coverage(tx, &scope, now)
}

fn pg_catchup_page(
    tx: &mut postgres::Transaction<'_>,
    who: &WorkWho,
    window: &CatchupPageWindow<'_>,
) -> Result<CatchupPage> {
    let CatchupPageWindow {
        subscriber,
        delivery_client,
        delivery,
        event_from,
        event_through,
        event_after,
        merge_from,
        merge_through,
        merge_after,
        limit,
    } = *window;
    if event_after < event_from
        || event_after > event_through
        || merge_after < merge_from
        || merge_after > merge_through
    {
        return Err(HubError::Invalid(
            "catch-up cursor is outside its delivery".into(),
        ));
    }
    let rows=tx.query("SELECT seq,at,kind,subject,stream,snapshot,title,status FROM (
        SELECT e.seq,e.at,e.kind,e.subject,'work'::text stream,v.snapshot,NULL::text title,NULL::text status
          FROM hub_events e JOIN hub_work_versions v ON v.event_seq=e.seq JOIN hub_work_items w ON w.id=v.work
         WHERE e.seq>$1 AND e.seq<=$2 AND w.provider=$3 AND w.host=$4
           AND (($6::bigint IS NOT NULL AND w.repository_id=$6)
             OR ($6::bigint IS NULL AND w.repository=$5 AND w.repository_id IS NULL))
        UNION ALL
        SELECT e.seq,e.at,e.kind,e.subject,'coordination'::text,NULL::text,r.proposed_title,v.status
          FROM hub_events e JOIN hub_coordination_versions v ON v.event_seq=e.seq
          JOIN hub_coordination_requests r ON r.id=v.request
         WHERE e.seq>$1 AND e.seq<=$2 AND r.provider=$3 AND r.host=$4
           AND (($6::bigint IS NOT NULL AND r.repository_id=$6)
             OR ($6::bigint IS NULL AND r.repository=$5 AND r.repository_id IS NULL))) events ORDER BY seq LIMIT $7",
        &[&event_after,&event_through,&who.owner.identity.provider,&who.owner.identity.host,
          &who.repository,&who.repository_id,&((limit+1) as i64)])?;
    let mut more_events = rows.len() > limit;
    let mut work_events = Vec::new();
    let mut coordination_events = Vec::new();
    let mut last_event = event_after;
    let mut page_payload_bytes = 0usize;
    for row in &rows {
        if work_events.len() + coordination_events.len() >= limit {
            more_events = true;
            break;
        }
        let seq: i64 = row.get(0);
        let at = row.get(1);
        let kind: String = row.get(2);
        let subject: String = row.get(3);
        let stream: String = row.get(4);
        let snapshot: Option<String> = row.get(5);
        let title: Option<String> = row.get(6);
        let row_status: Option<String> = row.get(7);
        let row_bytes = kind.len()
            + subject.len()
            + snapshot.as_ref().map_or(0, String::len)
            + title.as_ref().map_or(0, String::len)
            + row_status.as_ref().map_or(0, String::len)
            + 128;
        if page_payload_bytes.saturating_add(row_bytes) > MAX_CATCHUP_PAGE_PAYLOAD_BYTES {
            if work_events.is_empty() && coordination_events.is_empty() {
                return Err(HubError::Unavailable(
                    "a retained catch-up work event exceeds the page byte limit".into(),
                ));
            }
            more_events = true;
            break;
        }
        page_payload_bytes += row_bytes;
        last_event = seq;
        if stream == "work" {
            work_events.push(CatchupWorkEvent {
                seq,
                at,
                kind,
                work_id: subject,
                item: json_value(snapshot.as_deref().ok_or_else(|| {
                    HubError::Unavailable("missing retained work snapshot".into())
                })?)?,
            });
        } else {
            coordination_events.push(CatchupCoordinationEvent {
                seq,
                at,
                kind,
                request_id: subject,
                title: title.ok_or_else(|| {
                    HubError::Unavailable("missing retained coordination title".into())
                })?,
                status: row_status.ok_or_else(|| {
                    HubError::Unavailable("missing retained coordination status".into())
                })?,
            });
        }
    }
    let next_event_after = if more_events {
        last_event
    } else {
        event_through
    };
    let key = catchup_repository_key(who.repository_id);
    let repository = repository_namespace(&who.repository, who.repository_id);
    let rows = tx.query(
        &format!(
            "SELECT {CATCHUP_MERGE_COLUMNS} FROM hub_catchup_merges
        WHERE provider=$1 AND host=$2 AND repository=$3 AND repository_key=$4 AND seq>$5 AND seq<=$6
        ORDER BY seq LIMIT $7"
        ),
        &[
            &who.owner.identity.provider,
            &who.owner.identity.host,
            &repository,
            &key,
            &merge_after,
            &merge_through,
            &((limit + 1) as i64),
        ],
    )?;
    let mut more_merges = rows.len() > limit;
    let mut merges = Vec::new();
    for row in &rows {
        if merges.len() >= limit {
            more_merges = true;
            break;
        }
        let row_bytes = row.get::<_, String>(0).len()
            + row.get::<_, String>(2).len()
            + row.get::<_, String>(3).len()
            + row
                .get::<_, Option<String>>(6)
                .as_ref()
                .map_or(0, String::len)
            + row.get::<_, String>(7).len()
            + row.get::<_, String>(8).len()
            + row.get::<_, String>(9).len()
            + row.get::<_, String>(10).len()
            + row.get::<_, String>(12).len()
            + 128;
        if page_payload_bytes.saturating_add(row_bytes) > MAX_CATCHUP_PAGE_PAYLOAD_BYTES {
            if work_events.is_empty() && coordination_events.is_empty() && merges.is_empty() {
                return Err(HubError::Unavailable(
                    "a retained catch-up merge exceeds the page byte limit".into(),
                ));
            }
            more_merges = true;
            break;
        }
        page_payload_bytes += row_bytes;
        merges.push(pg_catchup_merge(row)?);
    }
    let next_merge_after = if more_merges {
        merges.last().map_or(merge_after, |m| m.seq)
    } else {
        merge_through
    };
    let work_floor: i64 = tx
        .query_one(
            "SELECT COALESCE(MAX(event_floor_seq),0)::bigint
        FROM hub_work_retention WHERE provider=$1 AND host=$2
          AND (($4::bigint IS NOT NULL AND repository_id=$4)
            OR ($4::bigint IS NULL AND repository=$3 AND repository_id IS NULL))",
            &[
                &who.owner.identity.provider,
                &who.owner.identity.host,
                &who.repository,
                &who.repository_id,
            ],
        )?
        .get(0);
    let coordination_floor = tx
        .query_one(
            "SELECT COALESCE(MAX(event_floor_seq),0)::bigint FROM hub_coordination_retention
        WHERE provider=$1 AND host=$2 AND (($4::bigint IS NOT NULL AND repository_id=$4)
          OR ($4::bigint IS NULL AND repository=$3 AND repository_id IS NULL))",
            &[
                &who.owner.identity.provider,
                &who.owner.identity.host,
                &who.repository,
                &who.repository_id,
            ],
        )?
        .get(0);
    let merge_floor = tx
        .query_opt(
            "SELECT merge_floor_seq FROM hub_catchup_repositories
        WHERE provider=$1 AND host=$2 AND repository=$3 AND repository_key=$4",
            &[
                &who.owner.identity.provider,
                &who.owner.identity.host,
                &repository,
                &key,
            ],
        )?
        .map_or(0, |r| r.get(0));
    let mut source_coverage = vec![
        CatchupSourceCoverage {
            source: CatchupMergeSource::TrackedPublication,
            state: super::catchup::CatchupCoverageState::Incomplete,
            checked_at: None,
            unread_pages: 0,
            deferred: 0,
            window_from: None,
        },
        CatchupSourceCoverage {
            source: CatchupMergeSource::ForgePull,
            state: super::catchup::CatchupCoverageState::Unavailable,
            checked_at: None,
            unread_pages: 0,
            deferred: 0,
            window_from: None,
        },
    ];
    for row in tx.query(
        "SELECT source,state,checked_at,unread_pages,deferred,window_from FROM hub_catchup_sources
        WHERE provider=$1 AND host=$2 AND repository=$3 AND repository_key=$4 ORDER BY source",
        &[
            &who.owner.identity.provider,
            &who.owner.identity.host,
            &repository,
            &key,
        ],
    )? {
        let source = parse_source(row.get::<_, String>(0).as_str())?;
        source_coverage.retain(|entry| entry.source != source);
        source_coverage.push(CatchupSourceCoverage {
            source,
            state: parse_coverage(row.get::<_, String>(1).as_str())?,
            checked_at: Some(row.get(2)),
            unread_pages: row.get::<_, i64>(3) as u64,
            deferred: row.get::<_, i64>(4) as u64,
            window_from: row.get(5),
        });
    }
    source_coverage.sort_by_key(|entry| source_name(entry.source));
    Ok(CatchupPage {
        subscriber: subscriber.to_owned(),
        delivery_client: delivery_client.to_owned(),
        delivery: delivery.to_owned(),
        event_from,
        event_through,
        event_after,
        next_event_after,
        merge_from,
        merge_through,
        merge_after,
        next_merge_after,
        work_events,
        coordination_events,
        merges,
        retention: CatchupRetention {
            work_gap: event_from < work_floor,
            coordination_gap: event_from < coordination_floor,
            merge_gap: merge_from < merge_floor,
            event_available_from: work_floor.max(coordination_floor),
            merge_available_from: merge_floor,
        },
        source_coverage,
        complete: next_event_after == event_through && next_merge_after == merge_through,
    })
}

fn pg_usage_summary(
    tx: &mut postgres::Transaction<'_>,
    work: &str,
    group_limit: usize,
) -> Result<UsageSummary> {
    let r=tx.query_one("SELECT COUNT(*)::bigint,SUM(input_tokens)::bigint,SUM(output_tokens)::bigint,COUNT(input_tokens)::bigint,COUNT(output_tokens)::bigint,COUNT(*) FILTER (WHERE cost_amount_micros IS NULL)::bigint,COUNT(*) FILTER (WHERE outcome='failed')::bigint FROM hub_work_model_usage WHERE work=$1 AND COALESCE(request::jsonb->>'measurement_scope','request')<>'session_cumulative'",&[&work])?;
    let observations: i64 = r.get(0);
    let mut token_groups = Vec::new();
    for r in tx.query("SELECT agent,model_provider,model,usage_basis,COUNT(*)::bigint,SUM(input_tokens)::bigint,SUM(output_tokens)::bigint,COUNT(input_tokens)::bigint,COUNT(output_tokens)::bigint FROM hub_work_model_usage WHERE work=$1 AND COALESCE(request::jsonb->>'measurement_scope','request')<>'session_cumulative' GROUP BY agent,model_provider,model,usage_basis ORDER BY agent COLLATE \"C\",model_provider COLLATE \"C\",model COLLATE \"C\",usage_basis COLLATE \"C\" LIMIT $2",&[&work,&((group_limit+1)as i64)])? {
        let basis:String=r.get(3); token_groups.push(TokenGroup { agent:r.get(0),provider:r.get(1),model:r.get(2),usage_basis:serde_json::from_str(&format!("\"{basis}\"")).map_err(|e|HubError::Unavailable(e.to_string()))?,observations:r.get::<_,i64>(4) as usize,input_tokens:r.get(5),output_tokens:r.get(6),input_known_observations:r.get::<_,i64>(7) as usize,output_known_observations:r.get::<_,i64>(8) as usize });
    }
    let mut cost_groups = Vec::new();
    for r in tx.query("SELECT cost_kind,cost_currency,cost_basis,usage_basis,agent,model_provider,model,SUM(cost_amount_micros)::bigint,COUNT(*)::bigint FROM hub_work_model_usage WHERE work=$1 AND cost_amount_micros IS NOT NULL AND COALESCE(request::jsonb->>'measurement_scope','request')<>'session_cumulative' GROUP BY cost_kind,cost_currency,cost_basis,usage_basis,agent,model_provider,model ORDER BY agent COLLATE \"C\",model_provider COLLATE \"C\",model COLLATE \"C\",usage_basis COLLATE \"C\",cost_kind COLLATE \"C\",cost_currency COLLATE \"C\",cost_basis COLLATE \"C\" LIMIT $2",&[&work,&((group_limit+1)as i64)])? {
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

fn catchup_event_high_water(
    tx: &mut postgres::Transaction<'_>,
    who: &WorkWho,
    minimum: i64,
) -> Result<i64> {
    Ok(tx
        .query_one(
            "SELECT MAX(value) FROM (
        SELECT COALESCE(MAX(seq),0)::bigint value FROM hub_events UNION ALL SELECT $5::bigint
        UNION ALL SELECT COALESCE(MAX(event_floor_seq),0) FROM hub_work_retention
          WHERE provider=$1 AND host=$2 AND (($4::bigint IS NOT NULL AND repository_id=$4)
            OR ($4::bigint IS NULL AND repository=$3 AND repository_id IS NULL))
        UNION ALL SELECT COALESCE(MAX(snapshot_floor_seq),0) FROM hub_work_retention
          WHERE provider=$1 AND host=$2 AND (($4::bigint IS NOT NULL AND repository_id=$4)
            OR ($4::bigint IS NULL AND repository=$3 AND repository_id IS NULL))
        UNION ALL SELECT COALESCE(MAX(event_floor_seq),0) FROM hub_coordination_retention
          WHERE provider=$1 AND host=$2 AND (($4::bigint IS NOT NULL AND repository_id=$4)
            OR ($4::bigint IS NULL AND repository=$3 AND repository_id IS NULL))) watermarks",
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

fn current_capture_work_for_session(
    tx: &mut postgres::Transaction<'_>,
    who: &WorkWho,
    session: &str,
    now: i64,
) -> Result<Option<WorkItem>> {
    let rows = tx.query(
        "SELECT item.snapshot, item.state='active', participant.role='lead'
         FROM hub_work_items item
         JOIN hub_work_participants participant ON participant.work=item.id
         WHERE item.provider=$1 AND item.host=$2 AND item.repository=$3
           AND item.repository_id IS NOT DISTINCT FROM $4 AND item.state IN ('active','paused','blocked')
           AND participant.token=$5 AND participant.session=$6
           AND participant.generation=item.generation
         ORDER BY item.state='active' DESC, participant.role='lead' DESC, item.id LIMIT 2",
        &[
            &who.owner.identity.provider,
            &who.owner.identity.host,
            &who.repository,
            &who.repository_id,
            &who.token,
            &session,
        ],
    )?;
    // An active Work outranks paused or blocked ones, and the session's lead role breaks ties.
    if let [first, second] = rows.as_slice()
        && (first.get::<_, bool>(1), first.get::<_, bool>(2))
            == (second.get::<_, bool>(1), second.get::<_, bool>(2))
    {
        return Err(HubError::Invalid(
            "session participates in more than one capturable work item".into(),
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
                        preflight: None,
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
                preflight: None,
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
                    preflight: None,
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
                preflight: None,
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
                preflight: None,
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
                preflight: None,
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
            preflight: None,
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
            preflight: None,
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
            preflight: None,
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
        preflight: None,
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

/// Ended merge groups are kept this long (30 days) before an observation of the repository drops them.
const MERGE_GROUP_KEEP_SECONDS: i64 = 2_592_000;

const MERGE_GROUP_COLUMNS: &str = "head_sha, head_ref, base_ref, base_sha, pr_number, pr_head_sha,
    context, ended_reason, observed_at, ended_at, state, binding_digest, description, phase,
    generation, updated_at, ruleset_requirement, status_source";

fn merge_group_row(row: &postgres::Row) -> MergeGroup {
    let head_sha: String = row.get(0);
    let phase: Option<String> = row.get(13);
    MergeGroup {
        check: phase.map(|phase| ForgeCheck {
            head_sha: head_sha.clone(),
            state: row.get(10),
            binding_digest: row.get(11),
            description: row.get(12),
            phase,
            generation: row.get(14),
            updated_at: row.get(15),
        }),
        head_sha,
        head_ref: row.get(1),
        base_ref: row.get(2),
        base_sha: row.get(3),
        pr_number: row.get(4),
        pr_head_sha: row.get(5),
        context: row.get(6),
        ended_reason: row.get(7),
        observed_at: row.get(8),
        ended_at: row.get(9),
        generation: row.get(14),
        ruleset_requirement: row.get(16),
        status_source: row.get(17),
    }
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
            session: row.get(18),
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
    let repository = repository_namespace(&who.repository, who.repository_id);
    let scope = json_string(&(
        &who.owner.id,
        &who.owner.identity.provider,
        &who.owner.identity.host,
        &repository,
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
    let repository = repository_namespace(repository, repository_id);
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
    let repository = repository_namespace(repository, repository_id);
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
            "SELECT intent, task, via, notice_cursor, denied, deny_times, unchecked FROM hub_sessions
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
            unchecked: row.get(6),
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
    // The state and generation columns are rewritten with every snapshot, so SQL can filter them.
    // Two rows are enough: more than one eligible assignment is already ambiguous.
    let rows = tx.query(
        "SELECT w.snapshot FROM hub_work_items w
         JOIN hub_work_participants p ON p.work=w.id
         WHERE w.provider=$1 AND w.host=$2 AND w.repository=$3
           AND w.repository_id IS NOT DISTINCT FROM $4 AND w.state IN ('active','blocked')
           AND p.token=$5 AND p.session=$6 AND p.generation=w.generation
         ORDER BY w.id LIMIT 2",
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
        let joined = tx.query_one(
            "SELECT EXISTS(SELECT 1 FROM hub_work_items w
             JOIN hub_work_participants p ON p.work=w.id
             WHERE w.provider=$1 AND w.host=$2 AND w.repository=$3
               AND w.repository_id IS NOT DISTINCT FROM $4 AND p.token=$5 AND p.session=$6)",
            &[
                &who.owner.identity.provider,
                &who.owner.identity.host,
                &who.repository,
                &who.repository_id,
                &who.token,
                &name,
            ],
        )?;
        if !joined.get::<_, bool>(0) {
            return Ok(());
        }
    }
    let mut eligible = Vec::new();
    for row in rows {
        eligible.push(json_value::<WorkItem>(&row.get::<_, String>(0))?);
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
             created_at, updated_at, denied, deny_times, unchecked)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $7, $8, $9, $10)
         ON CONFLICT (token, session) DO UPDATE SET intent = excluded.intent,
             task = excluded.task, via = excluded.via,
             notice_cursor = excluded.notice_cursor, updated_at = excluded.updated_at,
             denied = excluded.denied, deny_times = excluded.deny_times,
             unchecked = excluded.unchecked",
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
            &guarded.unchecked_json,
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

fn promise_kind(kind: PromiseVersionKindV1) -> &'static str {
    match kind {
        PromiseVersionKindV1::Create => "create",
        PromiseVersionKindV1::Adopt => "adopt",
        PromiseVersionKindV1::Amend => "amend",
        PromiseVersionKindV1::Correct => "correct",
        PromiseVersionKindV1::Retire => "retire",
        PromiseVersionKindV1::Transfer => "transfer",
        PromiseVersionKindV1::RecoveryTransfer => "recovery_transfer",
    }
}

fn promise_state(state: PromiseStateV1) -> &'static str {
    match state {
        PromiseStateV1::Active => "active",
        PromiseStateV1::Retired => "retired",
    }
}

fn promise_actor_active(
    tx: &mut postgres::Transaction<'_>,
    actor_user: &str,
    actor_token: Option<&str>,
    now: i64,
) -> Result<()> {
    let active: bool = match actor_token {
        Some(token) => tx
            .query_one(
                "SELECT EXISTS(SELECT 1 FROM hub_users u JOIN hub_agent_tokens t ON t.owner=u.id
                 WHERE u.id=$1 AND u.disabled=FALSE AND t.id=$2 AND t.revoked=FALSE AND t.expires_at>$3)",
                &[&actor_user, &token, &now],
            )?
            .get(0),
        None => tx
            .query_one(
                "SELECT EXISTS(SELECT 1 FROM hub_users WHERE id=$1 AND disabled=FALSE)",
                &[&actor_user],
            )?
            .get(0),
    };
    if !active {
        return Err(HubError::Unauthorized);
    }
    Ok(())
}

fn promise_operation_lock(
    tx: &mut postgres::Transaction<'_>,
    workspace_id: &str,
    operation_id: &str,
) -> Result<()> {
    let key = json_string(&(workspace_id, operation_id))?;
    tx.query_one(
        "SELECT pg_advisory_xact_lock($1, hashtext($2))",
        &[&PROMISE_OPERATION_LOCK_CLASS, &key],
    )?;
    Ok(())
}

fn promise_replay(
    tx: &mut postgres::Transaction<'_>,
    workspace_id: &str,
    operation_id: &str,
    actor_user: &str,
    actor_token: Option<&str>,
    fingerprint: &str,
) -> Result<Option<String>> {
    let row = tx.query_opt(
        "SELECT actor_user,actor_token,fingerprint,response FROM hub_promise_operations
         WHERE workspace_id=$1 AND operation_id=$2",
        &[&workspace_id, &operation_id],
    )?;
    match row {
        Some(row)
            if row.get::<_, String>(0) == actor_user
                && row.get::<_, Option<String>>(1).as_deref() == actor_token
                && row.get::<_, String>(2) == fingerprint =>
        {
            Ok(Some(row.get(3)))
        }
        Some(_) => Err(HubError::Invalid("promise_idempotency_conflict".into())),
        None => Ok(None),
    }
}

#[allow(
    clippy::too_many_arguments,
    reason = "each value is an independent immutable operation receipt column"
)]
fn promise_record_operation(
    tx: &mut postgres::Transaction<'_>,
    workspace_id: &str,
    operation_id: &str,
    actor_user: &str,
    actor_token: Option<&str>,
    fingerprint: &str,
    response: &str,
    now: i64,
) -> Result<()> {
    tx.execute(
        "INSERT INTO hub_promise_operations
         (workspace_id,operation_id,actor_user,actor_token,fingerprint,response,created_at)
         VALUES($1,$2,$3,$4,$5,$6,$7)",
        &[
            &workspace_id,
            &operation_id,
            &actor_user,
            &actor_token,
            &fingerprint,
            &response,
            &now,
        ],
    )?;
    Ok(())
}

fn promise_insert_version(
    tx: &mut postgres::Transaction<'_>,
    version: &PromiseVersionV1,
) -> Result<()> {
    let payload = encode_promise(version)?;
    let current_scope = encode_promise(&version.repository_admissions)?;
    let number = i64::try_from(version.reference.version)
        .map_err(|_| HubError::Invalid("promise version exceeds storage range".into()))?;
    tx.execute(
        "INSERT INTO hub_promise_versions
         (promise_id,version,digest,prior_digest,kind,actor_user,payload,current_scope,created_at)
         VALUES($1,$2,$3,$4,$5,$6,$7,($8::text)::jsonb,$9)",
        &[
            &version.reference.promise_id,
            &number,
            &version.reference.digest,
            &version.prior_digest,
            &promise_kind(version.kind),
            &version.created_by,
            &payload,
            &current_scope,
            &version.created_at,
        ],
    )?;
    for admission in &version.repository_admissions {
        let encoded = encode_promise(admission)?;
        tx.execute(
            "INSERT INTO hub_promise_version_repositories
             (workspace_id,promise_id,version,repository_id,admission) VALUES($1,$2,$3,$4,$5)",
            &[
                &version.reference.workspace_id,
                &version.reference.promise_id,
                &number,
                &admission.repository_id,
                &encoded,
            ],
        )?;
    }
    Ok(())
}

fn promise_current(
    client: &mut impl postgres::GenericClient,
    workspace_id: &str,
    promise_id: &str,
    lock: bool,
) -> Result<Option<PromiseVersionV1>> {
    let suffix = if lock { " FOR UPDATE OF p" } else { "" };
    let row = client.query_opt(
        &format!(
            "SELECT p.current_version,p.current_digest,v.payload,v.current_scope::text
             FROM hub_promises p JOIN hub_promise_versions v
               ON v.promise_id=p.id AND v.version=p.current_version
             WHERE p.workspace_id=$1 AND p.id=$2{suffix}"
        ),
        &[&workspace_id, &promise_id],
    )?;
    row.map(|row| {
        let decoded = decode_promise_current_row(
            workspace_id,
            promise_id,
            row.get(0),
            &row.get::<_, String>(1),
            &row.get::<_, String>(2),
        )?;
        if decode_current_scope(&row.get::<_, String>(3)).as_deref()
            != Some(decoded.repository_admissions.as_slice())
            || !promise_index_exact(client, &decoded)?
        {
            return Err(HubError::Unavailable(
                "promise current scope mismatch".into(),
            ));
        }
        Ok(decoded)
    })
    .transpose()
}

fn decode_promise_current_row(
    workspace_id: &str,
    promise_id: &str,
    version: i64,
    digest: &str,
    payload: &str,
) -> Result<PromiseVersionV1> {
    let decoded = decode_version(payload)?;
    if decoded.reference.version != u64::try_from(version).unwrap_or_default()
        || decoded.reference.digest != digest
        || decoded.reference.workspace_id != workspace_id
        || decoded.reference.promise_id != promise_id
    {
        return Err(HubError::Unavailable(
            "promise current pointer mismatch".into(),
        ));
    }
    Ok(decoded)
}

fn promise_index_exact(
    client: &mut impl postgres::GenericClient,
    version: &PromiseVersionV1,
) -> Result<bool> {
    let number = i64::try_from(version.reference.version).unwrap();
    let rows = client.query(
        "SELECT workspace_id,repository_id,admission FROM hub_promise_version_repositories
         WHERE promise_id=$1 AND version=$2 ORDER BY repository_id COLLATE \"C\" LIMIT 9",
        &[&version.reference.promise_id, &number],
    )?;
    let mut index = Vec::new();
    for row in rows {
        let workspace_id: String = row.get(0);
        let key: String = row.get(1);
        let admission: PromiseRepositoryAdmissionV1 =
            serde_json::from_str(&row.get::<_, String>(2))
                .map_err(|_| HubError::Unavailable("invalid promise repository index".into()))?;
        if workspace_id != version.reference.workspace_id || admission.repository_id != key {
            return Ok(false);
        }
        index.push(admission);
    }
    Ok(index == version.repository_admissions)
}

fn promise_lifetime_authorized(
    client: &mut impl postgres::GenericClient,
    workspace_id: &str,
    promise_id: &str,
    permitted: Option<&[String]>,
    current: &PromiseCurrentAdmissionsV1,
) -> Result<Option<Vec<PromiseRepositoryAdmissionV1>>> {
    let row = client.query_opt(
        "SELECT lifetime_scope::text FROM hub_promises WHERE workspace_id=$1 AND id=$2",
        &[&workspace_id, &promise_id],
    )?;
    Ok(row
        .as_ref()
        .and_then(|row| decode_lifetime_scope(&row.get::<_, String>(0)))
        .filter(|scope| scope_authorized(scope, permitted, current)))
}

fn promise_report_parent_authorized(
    client: &mut impl postgres::GenericClient,
    workspace_id: &str,
    promise_id: &str,
    permitted: Option<&[String]>,
    current: &PromiseCurrentAdmissionsV1,
) -> Result<bool> {
    let row = client.query_opt(
        "SELECT p.current_version,v.payload,v.current_scope::text FROM hub_promises p
         JOIN hub_promise_versions v ON v.promise_id=p.id AND v.version=p.current_version
         WHERE p.workspace_id=$1 AND p.id=$2",
        &[&workspace_id, &promise_id],
    )?;
    let Some(row) = row else {
        return Ok(false);
    };
    let version: i64 = row.get(0);
    let payload: String = row.get(1);
    let encoded_scope: String = row.get(2);
    Ok(
        encoded_admissions_authorized(&encoded_scope, permitted, current)
            && payload_admissions_authorized(
                &payload,
                workspace_id,
                promise_id,
                version,
                permitted,
                current,
            ),
    )
}

fn promise_version_visible(
    client: &mut impl postgres::GenericClient,
    workspace_id: &str,
    promise_id: &str,
    number: i64,
    permitted: Option<&[String]>,
    current: &PromiseCurrentAdmissionsV1,
) -> Result<Option<PromiseVersionV1>> {
    let row = client.query_opt(
        "SELECT v.digest,v.payload,v.current_scope::text FROM hub_promise_versions v
         JOIN hub_promises p ON p.id=v.promise_id
         WHERE p.workspace_id=$1 AND v.promise_id=$2 AND v.version=$3",
        &[&workspace_id, &promise_id, &number],
    )?;
    let Some(row) = row else {
        return Ok(None);
    };
    let digest: String = row.get(0);
    let payload: String = row.get(1);
    if !payload_admissions_authorized(
        &payload,
        workspace_id,
        promise_id,
        number,
        permitted,
        current,
    ) {
        return Ok(None);
    }
    let encoded_scope: String = row.get(2);
    if !encoded_admissions_authorized(&encoded_scope, permitted, current) {
        return Ok(None);
    }
    let scope = decode_current_scope(&encoded_scope)
        .ok_or_else(|| HubError::Unavailable("invalid promise version scope".into()))?;
    let version = decode_version(&payload)?;
    if version.reference.workspace_id != workspace_id
        || version.reference.promise_id != promise_id
        || i64::try_from(version.reference.version).ok() != Some(number)
        || version.reference.digest != digest
        || scope != version.repository_admissions
        || !promise_index_exact(client, &version)?
    {
        return Err(HubError::Unavailable(
            "promise version scope mismatch".into(),
        ));
    }
    Ok(Some(version))
}

fn promise_input_visible(
    client: &mut impl postgres::GenericClient,
    workspace_id: &str,
    input_id: &str,
    permitted: Option<&[String]>,
    current: &PromiseCurrentAdmissionsV1,
) -> Result<Option<PromiseInputViewV1>> {
    let row = client.query_opt(
        "SELECT kind,repository_id,admission,promise_id,promise_version,promise_digest
         FROM hub_promise_inputs WHERE workspace_id=$1 AND id=$2",
        &[&workspace_id, &input_id],
    )?;
    let Some(row) = row else {
        return Ok(None);
    };
    let kind: String = row.get(0);
    let repository: String = row.get(1);
    let encoded: String = row.get(2);
    let promise_id: Option<String> = row.get(3);
    let version_number: Option<i64> = row.get(4);
    let digest: Option<String> = row.get(5);
    if permitted.is_some_and(|grants| !grants.contains(&repository)) {
        return Ok(None);
    }
    if encoded != encode_promise(&current_admission(current, &repository))? {
        return Ok(None);
    }
    if kind == "report" {
        let Some(promise_id) = promise_id.as_deref() else {
            return Ok(None);
        };
        let Some(lifetime) =
            promise_lifetime_authorized(client, workspace_id, promise_id, permitted, current)?
        else {
            return Ok(None);
        };
        if !lifetime
            .iter()
            .any(|admission| admission.repository_id == repository)
        {
            return Ok(None);
        }
        let Some(number) = version_number else {
            return Ok(None);
        };
        let Some(version) =
            promise_version_visible(client, workspace_id, promise_id, number, permitted, current)?
        else {
            return Ok(None);
        };
        if digest.as_deref() != Some(version.reference.digest.as_str()) {
            return Ok(None);
        }
    } else if kind != "proposal" {
        return Ok(None);
    }
    let row = client.query_one(
        "SELECT id,kind,repository_id,actor_user,actor_token IS NOT NULL,payload,
                adopted_promise,created_at FROM hub_promise_inputs WHERE workspace_id=$1 AND id=$2",
        &[&workspace_id, &input_id],
    )?;
    let view = decode_input_view(
        row.get(0),
        row.get(1),
        row.get(2),
        row.get(3),
        row.get(4),
        row.get(5),
        row.get(6),
        row.get(7),
    )?;
    if let PromiseInputV1::Report {
        promise_id: payload_id,
        promise_version,
        promise_digest,
        ..
    } = &view.payload
        && (promise_id.as_deref() != Some(payload_id.as_str())
            || version_number != i64::try_from(*promise_version).ok()
            || digest.as_deref() != Some(promise_digest.as_str()))
    {
        return Err(HubError::Unavailable(
            "promise input reference mismatch".into(),
        ));
    }
    Ok(Some(view))
}

fn promise_version_at(
    client: &mut impl postgres::GenericClient,
    reference: &super::promise::PromiseRefV1,
) -> Result<Option<PromiseVersionV1>> {
    let number = i64::try_from(reference.version).unwrap_or(-1);
    let row = client.query_opt(
        "SELECT v.payload FROM hub_promise_versions v JOIN hub_promises p ON p.id=v.promise_id
         WHERE p.workspace_id=$1 AND v.promise_id=$2 AND v.version=$3 AND v.digest=$4",
        &[
            &reference.workspace_id,
            &reference.promise_id,
            &number,
            &reference.digest,
        ],
    )?;
    row.map(|row| decode_version(&row.get::<_, String>(0)))
        .transpose()
}

fn promise_receipt_matches(
    client: &mut impl postgres::GenericClient,
    receipt: &PromiseReceiptV1,
    admissions: &[PromiseRepositoryAdmissionV1],
) -> Result<()> {
    let version = promise_version_at(client, &receipt.reference)?
        .ok_or_else(|| HubError::Unavailable("retained promise receipt version missing".into()))?;
    if version.repository_admissions != admissions {
        return Err(HubError::Invalid("promise_not_found".into()));
    }
    Ok(())
}

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

    fn insert_device(
        &self,
        owner: &str,
        name: &str,
        digest: &TokenDigest,
        replaces: Option<&str>,
        now: i64,
    ) -> Result<String> {
        validate_device_name(name)?;
        let id = new_id("dev")?;
        self.run(false, |client| {
            let mut tx = client.transaction()?;
            let inserted = tx.execute(
                "INSERT INTO hub_devices (id, owner, name, digest, created_at, last_used_at)
                 SELECT $1, id, $3, $4, $5, $5 FROM hub_users WHERE id = $2 AND NOT disabled
                 FOR SHARE",
                &[&id, &owner, &name, &digest.as_str(), &now],
            )?;
            if inserted == 0 {
                return Err(HubError::Invalid("unknown or disabled owner".into()));
            }
            // The old computer goes first, so its row lock precedes the event lock, as a
            // concurrent mint through it takes them.
            if let Some(old) = replaces {
                revoke_device_in(&mut tx, old, Some(owner), owner, now)?;
            }
            append(&mut tx, &Event::device_created(&id, owner, name)?)?;
            tx.commit()?;
            Ok(id.clone())
        })
    }

    fn authenticate_device(
        &self,
        digest: &TokenDigest,
        now: i64,
    ) -> Result<Option<DevicePrincipal>> {
        self.run(false, |client| {
            let found = client
                .query_opt(
                    "SELECT d.id, d.name, d.created_at, d.last_used_at,
                            u.id, u.provider, u.host, u.forge_user_id, u.login, u.disabled
                     FROM hub_devices d JOIN hub_users u ON u.id = d.owner
                     WHERE d.digest = $1 AND d.revoked_at IS NULL
                       AND COALESCE(d.last_used_at, d.created_at) > $2",
                    &[&digest.as_str(), &(now - DEVICE_IDLE_SECONDS)],
                )?
                .map(|row| DevicePrincipal {
                    device_id: row.get(0),
                    name: row.get(1),
                    created_at: row.get(2),
                    last_used_at: row.get(3),
                    owner: User {
                        id: row.get(4),
                        identity: ForgeIdentity {
                            provider: row.get(5),
                            host: row.get(6),
                            user_id: row.get(7),
                        },
                        login: row.get(8),
                        disabled: row.get(9),
                    },
                });
            if let Some(device) = found.as_ref()
                && device
                    .last_used_at
                    .is_none_or(|at| at <= now - DEVICE_TOUCH_SECONDS)
            {
                client.execute(
                    "UPDATE hub_devices SET last_used_at = $2 WHERE id = $1",
                    &[&device.device_id, &now],
                )?;
            }
            Ok(found)
        })
    }

    fn devices(&self, owner: Option<&str>, now: i64) -> Result<Vec<Device>> {
        self.run(true, |client| {
            let rows = client.query(
                "SELECT d.id, d.name, d.created_at, d.last_used_at, d.revoked_at,
                        u.id, u.provider, u.host, u.forge_user_id, u.login, u.disabled,
                        COALESCE(ARRAY(
                            SELECT DISTINCT t.repository FROM hub_agent_tokens t
                            WHERE t.device_id = d.id AND NOT t.revoked AND t.expires_at > $3
                              AND NOT t.read_only
                            ORDER BY t.repository), '{}')
                 FROM hub_devices d JOIN hub_users u ON u.id = d.owner
                 WHERE ($1::text IS NULL OR d.owner = $1)
                   AND (d.revoked_at IS NULL OR d.revoked_at > $4)
                 ORDER BY d.revoked_at IS NULL DESC, d.created_at DESC, d.id DESC LIMIT $2",
                &[&owner, &MAX_DEVICES, &now, &(now - REFUSAL_SECONDS)],
            )?;
            Ok(rows
                .iter()
                .map(|row| Device {
                    id: row.get(0),
                    name: row.get(1),
                    created_at: row.get(2),
                    last_used_at: row.get(3),
                    revoked_at: row.get(4),
                    owner: User {
                        id: row.get(5),
                        identity: ForgeIdentity {
                            provider: row.get(6),
                            host: row.get(7),
                            user_id: row.get(8),
                        },
                        login: row.get(9),
                        disabled: row.get(10),
                    },
                    repositories: row.get(11),
                })
                .collect())
        })
    }

    fn revoke_device(&self, device: &str, owner: Option<&str>, by: &str, now: i64) -> Result<bool> {
        self.run(false, |client| {
            let mut tx = client.transaction()?;
            let revoked = revoke_device_in(&mut tx, device, owner, by, now)?;
            tx.commit()?;
            Ok(revoked)
        })
    }

    fn rename_device(&self, device: &str, owner: &str, name: &str) -> Result<bool> {
        validate_device_name(name)?;
        self.run(false, |client| {
            let mut tx = client.transaction()?;
            let changed = tx.execute(
                "UPDATE hub_devices SET name = $3
                 WHERE id = $1 AND owner = $2 AND revoked_at IS NULL AND name <> $3",
                &[&device, &owner, &name],
            )?;
            if changed > 0 {
                append(&mut tx, &Event::device_renamed(device, name)?)?;
            }
            tx.commit()?;
            Ok(changed > 0)
        })
    }

    fn insert_device_token(
        &self,
        token: &NewAgentToken,
        device: &str,
        auto: bool,
    ) -> Result<String> {
        let now = unix_now();
        token.validate(now)?;
        let id = new_id("tok")?;
        self.run(false, |client| {
            let mut tx = client.transaction()?;
            let inserted = tx.execute(
                "INSERT INTO hub_agent_tokens
                     (id, owner, name, repository, digest, expires_at, created_at, repository_id,
                      read_only, device_id)
                 SELECT $1, u.id, $3, $4, $5, $6, $7, $8, $9, d.id
                 FROM hub_users u JOIN hub_devices d ON d.owner = u.id
                 WHERE u.id = $2 AND NOT u.disabled AND d.id = $10 AND d.revoked_at IS NULL
                 FOR SHARE",
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
                    &device,
                ],
            )?;
            if inserted == 0 {
                return Err(HubError::Unauthorized);
            }
            append(&mut tx, &Event::token_created(&id, token)?)?;
            if auto {
                append(
                    &mut tx,
                    &Event::auto_connected(device, &id, &token.repository)?,
                )?;
            }
            tx.commit()?;
            Ok(id.clone())
        })
    }

    fn record_refusal(&self, refusal: &Refusal) -> Result<()> {
        self.run(false, |client| {
            let mut tx = client.transaction()?;
            tx.execute(
                "INSERT INTO hub_sign_in_refusals
                     (provider, host, forge_user_id, login, reason, via, at, count)
                 VALUES ($1, $2, $3, $4, $5, $6, $7, 1)
                 ON CONFLICT (provider, host, forge_user_id, reason) DO UPDATE SET
                     login = excluded.login, via = excluded.via, at = excluded.at,
                     count = hub_sign_in_refusals.count + 1",
                &[
                    &refusal.provider,
                    &refusal.host,
                    &refusal.forge_user_id,
                    &refusal.login,
                    &refusal.reason,
                    &refusal.via,
                    &refusal.at,
                ],
            )?;
            tx.execute(
                "DELETE FROM hub_sign_in_refusals WHERE at <= $1",
                &[&(refusal.at - REFUSAL_SECONDS)],
            )?;
            tx.execute(
                "DELETE FROM hub_sign_in_refusals WHERE (provider, host, forge_user_id, reason) NOT IN
                     (SELECT provider, host, forge_user_id, reason FROM hub_sign_in_refusals
                      ORDER BY at DESC, provider, host, forge_user_id, reason LIMIT $1)",
                &[&MAX_REFUSALS],
            )?;
            tx.commit()?;
            Ok(())
        })
    }

    fn refusals(&self, now: i64) -> Result<Vec<Refusal>> {
        self.run(true, |client| {
            let rows = client.query(
                "SELECT provider, host, forge_user_id, login, reason, via, at, count
                 FROM hub_sign_in_refusals WHERE at > $1
                 ORDER BY at DESC, provider, host, forge_user_id, reason LIMIT $2",
                &[&(now - REFUSAL_SECONDS), &MAX_REFUSALS],
            )?;
            Ok(rows
                .iter()
                .map(|row| Refusal {
                    provider: row.get(0),
                    host: row.get(1),
                    forge_user_id: row.get(2),
                    login: row.get(3),
                    reason: row.get(4),
                    via: row.get(5),
                    at: row.get(6),
                    count: row.get(7),
                })
                .collect())
        })
    }

    fn repository_usage(&self, now: i64) -> Result<Vec<RepositoryUsage>> {
        self.run(true, |client| {
            let rows = client.query(
                "SELECT r.full_name, r.host, r.connection,
                        COUNT(DISTINCT CASE WHEN u.host = r.host THEN t.device_id END),
                        COUNT(CASE WHEN u.host = r.host THEN t.id END)
                 FROM hub_repositories r
                 LEFT JOIN hub_agent_tokens t ON t.repository = r.full_name
                     AND NOT t.revoked AND t.expires_at > $1 AND NOT t.read_only
                 LEFT JOIN hub_users u ON u.id = t.owner
                 GROUP BY r.id, r.full_name, r.host, r.connection
                 ORDER BY r.full_name, r.host LIMIT $2",
                &[&now, &LIST_LIMIT],
            )?;
            Ok(rows
                .iter()
                .map(|row| RepositoryUsage {
                    full_name: row.get(0),
                    host: row.get(1),
                    connection: row.get(2),
                    devices: row.get(3),
                    agent_tokens: row.get(4),
                })
                .collect())
        })
    }

    fn insert_browser_session(
        &self,
        owner: &str,
        digest: &TokenDigest,
        expires_at: i64,
        now: i64,
    ) -> Result<()> {
        self.run(false, |client| {
            let mut tx = client.transaction()?;
            tx.execute(
                "DELETE FROM hub_browser_sessions WHERE expires_at <= $1",
                &[&now],
            )?;
            let inserted = tx.execute(
                "INSERT INTO hub_browser_sessions (digest, owner, created_at, expires_at)
                 SELECT $1, id, $3, $4 FROM hub_users WHERE id = $2 AND NOT disabled FOR SHARE",
                &[&digest.as_str(), &owner, &now, &expires_at],
            )?;
            if inserted == 0 {
                return Err(HubError::Invalid("unknown or disabled owner".into()));
            }
            tx.execute(
                "DELETE FROM hub_browser_sessions WHERE owner = $1 AND digest NOT IN
                     (SELECT digest FROM hub_browser_sessions WHERE owner = $1
                      ORDER BY created_at DESC, digest LIMIT $2)",
                &[&owner, &MAX_BROWSER_SESSIONS],
            )?;
            tx.commit()?;
            Ok(())
        })
    }

    fn browser_session(
        &self,
        digest: &TokenDigest,
        now: i64,
        slide_to: i64,
    ) -> Result<Option<(User, i64)>> {
        self.run(false, |client| {
            let Some(row) = client.query_opt(
                "SELECT u.id, u.provider, u.host, u.forge_user_id, u.login, u.disabled, s.expires_at
                 FROM hub_browser_sessions s JOIN hub_users u ON u.id = s.owner
                 WHERE s.digest = $1 AND s.expires_at > $2 AND s.created_at > $3",
                &[&digest.as_str(), &now, &(now - BROWSER_SESSION_MAX_SECONDS)],
            )?
            else {
                return Ok(None);
            };
            let expires_at: i64 = row.get(6);
            let user = User {
                id: row.get(0),
                identity: ForgeIdentity {
                    provider: row.get(1),
                    host: row.get(2),
                    user_id: row.get(3),
                },
                login: row.get(4),
                disabled: row.get(5),
            };
            if slide_to <= expires_at + SLIDE_STEP_SECONDS {
                return Ok(Some((user, expires_at)));
            }
            client.execute(
                "UPDATE hub_browser_sessions SET expires_at = $2 WHERE digest = $1",
                &[&digest.as_str(), &slide_to],
            )?;
            Ok(Some((user, slide_to)))
        })
    }

    fn delete_browser_session(&self, digest: &TokenDigest) -> Result<bool> {
        self.run(false, |client| {
            Ok(client.execute(
                "DELETE FROM hub_browser_sessions WHERE digest = $1",
                &[&digest.as_str()],
            )? > 0)
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

    fn repository_events(
        &self,
        who: &Principal,
        before: Option<i64>,
        limit: usize,
    ) -> Result<Vec<ActivityEvent>> {
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
                     AND ($6::bigint IS NULL OR e.seq < $6::bigint)
                     ORDER BY e.seq DESC LIMIT $5"
                ),
                &[
                    &who.repository,
                    &who.repository_id,
                    &who.owner.identity.host,
                    &who.owner.identity.provider,
                    &event_limit(limit),
                    &before,
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

    fn observation_floor(&self, repo: &str) -> Result<i64> {
        self.run(true, |client| {
            Ok(client
                .query_one(
                    "SELECT COALESCE(MAX(observed_at), 0) FROM hub_observations
                     WHERE repository = $1",
                    &[&repo],
                )?
                .get(0))
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

    fn observe_merge_group(
        &self,
        repo: &str,
        group: &NewMergeGroup,
        ended: Option<&str>,
        now: i64,
    ) -> Result<MergeGroup> {
        group.validate()?;
        ended.map(validate_group_note).transpose()?;
        let cutoff = now.saturating_sub(MERGE_GROUP_KEEP_SECONDS);
        let ended_at = ended.map(|_| now);
        self.run(false, |client| {
            let mut tx = client.transaction()?;
            // The repository row lock serializes its observations.
            repository_by_rep(&mut tx, repo)?;
            tx.execute(
                "DELETE FROM hub_merge_groups
                 WHERE repository = $1 AND ended_reason IS NOT NULL AND ended_at < $2",
                &[&repo, &cutoff],
            )?;
            tx.execute(
                "INSERT INTO hub_merge_groups (repository, head_sha, head_ref, base_ref,
                     base_sha, pr_number, pr_head_sha, context, ended_reason, observed_at,
                     ended_at, updated_at)
                 VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $10)
                 ON CONFLICT (repository, head_sha) DO NOTHING",
                &[
                    &repo,
                    &group.head_sha,
                    &group.head_ref,
                    &group.base_ref,
                    &group.base_sha,
                    &group.pr_number,
                    &group.pr_head_sha,
                    &group.context,
                    &ended,
                    &now,
                    &ended_at,
                ],
            )?;
            if let Some(reason) = ended {
                tx.execute(
                    "UPDATE hub_merge_groups SET ended_reason = $3, ended_at = $4, updated_at = $4
                     WHERE repository = $1 AND head_sha = $2 AND ended_reason IS NULL",
                    &[&repo, &group.head_sha, &reason, &now],
                )?;
            }
            let stored = tx.query_one(
                &format!(
                    "SELECT {MERGE_GROUP_COLUMNS} FROM hub_merge_groups
                     WHERE repository = $1 AND head_sha = $2"
                ),
                &[&repo, &group.head_sha],
            )?;
            tx.commit()?;
            Ok(merge_group_row(&stored))
        })
    }

    fn merge_group(&self, repo: &str, head_sha: &str) -> Result<Option<MergeGroup>> {
        if !is_lower_hex(head_sha, 40) {
            return Err(HubError::Invalid("merge group is out of bounds".into()));
        }
        self.run(true, |client| {
            Ok(client
                .query_opt(
                    &format!(
                        "SELECT {MERGE_GROUP_COLUMNS} FROM hub_merge_groups
                         WHERE repository = $1 AND head_sha = $2"
                    ),
                    &[&repo, &head_sha],
                )?
                .map(|row| merge_group_row(&row)))
        })
    }

    fn active_merge_groups(&self, repo: &str, limit: usize) -> Result<Vec<MergeGroup>> {
        self.run(true, |client| {
            let limit = limit as i64;
            Ok(client
                .query(
                    &format!(
                        "SELECT {MERGE_GROUP_COLUMNS} FROM hub_merge_groups
                         WHERE repository = $1 AND ended_reason IS NULL
                         ORDER BY observed_at, head_sha COLLATE \"C\" LIMIT $2"
                    ),
                    &[&repo, &limit],
                )?
                .iter()
                .map(merge_group_row)
                .collect())
        })
    }

    fn merge_groups_for_pull(
        &self,
        repo: &str,
        pr_number: i64,
        limit: usize,
    ) -> Result<Vec<MergeGroup>> {
        self.run(true, |client| {
            let limit = limit as i64;
            Ok(client
                .query(
                    &format!(
                        "SELECT {MERGE_GROUP_COLUMNS} FROM hub_merge_groups
                         WHERE repository = $1 AND pr_number = $2
                         ORDER BY observed_at DESC, head_sha COLLATE \"C\" LIMIT $3"
                    ),
                    &[&repo, &pr_number, &limit],
                )?
                .iter()
                .map(merge_group_row)
                .collect())
        })
    }

    fn end_merge_group(&self, repo: &str, head_sha: &str, reason: &str, now: i64) -> Result<bool> {
        if !is_lower_hex(head_sha, 40) {
            return Err(HubError::Invalid("merge group is out of bounds".into()));
        }
        validate_group_note(reason)?;
        self.run(false, |client| {
            Ok(client.execute(
                "UPDATE hub_merge_groups SET ended_reason = $3, ended_at = $4, updated_at = $4
                 WHERE repository = $1 AND head_sha = $2 AND ended_reason IS NULL",
                &[&repo, &head_sha, &reason, &now],
            )? == 1)
        })
    }

    fn reserve_merge_group_check(
        &self,
        repo: &str,
        head_sha: &str,
        expected: i64,
        new: &NewForgeCheck,
        now: i64,
    ) -> Result<ForgeCheckReserve> {
        new.validate()?;
        if new.head_sha != head_sha {
            return Err(HubError::Invalid(
                "forge check is not for this merge group".into(),
            ));
        }
        self.run(false, |client| {
            Ok(
                if client.execute(
                    "UPDATE hub_merge_groups SET state = $4, binding_digest = $5,
                         description = $6, phase = 'attempting', generation = generation + 1,
                         updated_at = $7
                     WHERE repository = $1 AND head_sha = $2 AND generation = $3
                         AND ended_reason IS NULL",
                    &[
                        &repo,
                        &head_sha,
                        &expected,
                        &new.state,
                        &new.binding_digest,
                        &new.description,
                        &now,
                    ],
                )? == 1
                {
                    ForgeCheckReserve::Reserved(expected.saturating_add(1))
                } else {
                    ForgeCheckReserve::Conflict
                },
            )
        })
    }

    fn complete_merge_group_check(
        &self,
        repo: &str,
        head_sha: &str,
        generation: i64,
        phase: &str,
        now: i64,
    ) -> Result<bool> {
        if !is_lower_hex(head_sha, 40) {
            return Err(HubError::Invalid("merge group is out of bounds".into()));
        }
        validate_check_phase(phase)?;
        // No lifecycle guard: a post that happened after the group ended is still recorded.
        self.run(false, |client| {
            Ok(client.execute(
                "UPDATE hub_merge_groups SET phase = $4, generation = generation + 1,
                     updated_at = $5
                 WHERE repository = $1 AND head_sha = $2 AND generation = $3",
                &[&repo, &head_sha, &generation, &phase, &now],
            )? == 1)
        })
    }

    fn note_merge_group_source(
        &self,
        repo: &str,
        head_sha: &str,
        ruleset_requirement: &str,
        status_source: &str,
    ) -> Result<()> {
        if !is_lower_hex(head_sha, 40) {
            return Err(HubError::Invalid("merge group is out of bounds".into()));
        }
        validate_group_note(ruleset_requirement)?;
        validate_group_note(status_source)?;
        self.run(false, |client| {
            client.execute(
                "UPDATE hub_merge_groups SET ruleset_requirement = $3, status_source = $4
                 WHERE repository = $1 AND head_sha = $2",
                &[&repo, &head_sha, &ruleset_requirement, &status_source],
            )?;
            Ok(())
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

    fn append_notices(&self, kind: &'static str, notices: &[(String, String)]) -> Result<()> {
        let events = notices
            .iter()
            .map(|(subject, data)| Event::notice(kind, subject, data))
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

    fn publication_search(
        &self,
        repository: &str,
        before: Option<&str>,
        search: &str,
        limit: usize,
    ) -> Result<PublicationSearch> {
        self.run(true, |client| {
            let scan = SEARCH_SCAN_ROWS as i64;
            let window: Vec<(i64, String)> = client
                .query(
                    "SELECT p.created_at, p.id FROM hub_publications p WHERE p.repository = $1
                     AND ($2::text IS NULL OR (p.created_at, p.id) < (SELECT q.created_at, q.id
                         FROM hub_publications q WHERE q.repository = $1 AND q.id = $2))
                     ORDER BY p.created_at DESC, p.id DESC LIMIT $3",
                    &[&repository, &before, &scan],
                )?
                .iter()
                .map(|row| (row.get(0), row.get(1)))
                .collect();
            let Some((oldest_at, oldest_id)) = window.last().cloned() else {
                return Ok(PublicationSearch {
                    rows: vec![],
                    searched_to: None,
                    window_full: false,
                });
            };
            let limit = limit as i64;
            let pattern = like_pattern(search);
            let number = search_number(search);
            let rows = client
                .query(
                    &format!(
                        "{PUBLICATION_SELECT} WHERE p.repository = $1
                         AND ($2::text IS NULL OR (p.created_at, p.id) < (SELECT q.created_at, q.id
                             FROM hub_publications q WHERE q.repository = $1 AND q.id = $2))
                         AND (p.created_at, p.id) >= ($6::bigint, $7::text)
                         AND (p.pr_number = $5::bigint
                             OR p.branch ILIKE $4 ESCAPE '\\' OR p.base ILIKE $4 ESCAPE '\\'
                             OR p.id ILIKE $4 ESCAPE '\\' OR u.login ILIKE $4 ESCAPE '\\'
                             OR t.name ILIKE $4 ESCAPE '\\'
                             OR (CASE WHEN octet_length(p.manifest) <= 2097152
                                 AND p.manifest IS JSON OBJECT
                                 THEN p.manifest::jsonb ->> 'title' END) ILIKE $4 ESCAPE '\\')
                         ORDER BY p.created_at DESC, p.id DESC LIMIT $3"
                    ),
                    &[
                        &repository,
                        &before,
                        &limit,
                        &pattern,
                        &number,
                        &oldest_at,
                        &oldest_id,
                    ],
                )?
                .iter()
                .map(publication_row)
                .collect::<Result<Vec<_>>>()?;
            Ok(PublicationSearch {
                rows,
                searched_to: Some(oldest_id),
                window_full: window.len() == SEARCH_SCAN_ROWS,
            })
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
            let retry_catchup = if let Update::Merged { merge_commit } = update {
                p.merge_commit.as_deref() == Some(merge_commit.as_str())
                    && pg_publication_catchup_merge(&p, now).is_ok()
                    && tx
                        .query_one(
                            "SELECT EXISTS(SELECT 1 FROM hub_catchup_publication_failures WHERE publication=$1)",
                            &[&id],
                        )?
                        .get::<_, bool>(0)
            } else {
                false
            };
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
            if (changed && matches!(update, Update::Merged { .. })) || retry_catchup {
                let row = tx.query_one(
                    "SELECT u.provider,u.host,t.repository,t.repository_id
                     FROM hub_agent_tokens t JOIN hub_users u ON u.id=t.owner WHERE t.id=$1",
                    &[&p.token],
                )?;
                let scope = CatchupRepositoryScope {
                    provider: row.get(0),
                    host: row.get(1),
                    repository: row.get(2),
                    repository_id: row.get(3),
                };
                pg_retain_publication_projection(&mut tx, &scope, &p, now)?;
                pg_refresh_publication_coverage(&mut tx, &scope, now)?;
            }
            tx.commit()?;
            Ok(p)
        })
    }

    fn repair_catchup_publication_merge(
        &self,
        repair: &CatchupPublicationRepair,
        now: i64,
    ) -> Result<CatchupPublicationRepairResult> {
        repair.validate()?;
        self.run(false, |client| {
            let mut tx = client.transaction()?;
            let mut publication = publication_by_id(&mut tx, &repair.publication, true)?
                .filter(|publication| publication.repository == repair.repository)
                .ok_or_else(|| HubError::Invalid("unknown publication in repository".into()))?;
            let current_merge_commit = publication.merge_commit.clone();
            let row = tx.query_one(
                "SELECT u.provider,u.host,t.repository,t.repository_id
                 FROM hub_agent_tokens t JOIN hub_users u ON u.id=t.owner WHERE t.id=$1",
                &[&publication.token],
            )?;
            let scope = CatchupRepositoryScope {
                provider: row.get(0),
                host: row.get(1),
                repository: row.get(2),
                repository_id: row.get(3),
            };
            lock_work_repository(
                &mut tx,
                &scope.provider,
                &scope.host,
                &scope.repository,
                scope.repository_id,
            )?;
            let retained_failure = tx.query_opt(
                "SELECT 1 FROM hub_catchup_publication_failures
                 WHERE provider=$1 AND host=$2 AND repository=$3 AND repository_key=$4
                   AND publication=$5 FOR UPDATE",
                &[
                    &scope.provider,
                    &scope.host,
                    &repository_namespace(&scope.repository, scope.repository_id),
                    &catchup_repository_key(scope.repository_id),
                    &repair.publication,
                ],
            )?;
            if retained_failure.is_none() {
                return Err(HubError::Invalid(
                    "no retained catch-up projection failure for publication".into(),
                ));
            }

            let (merge, replace_invalid) = match pg_publication_catchup_merge(&publication, now) {
                Ok(merge) => {
                    if publication.merge_commit.as_deref() != Some(repair.merge_commit.as_str()) {
                        return Err(HubError::Invalid(
                            "publication merge fact is immutable and cannot be repaired".into(),
                        ));
                    }
                    (merge, false)
                }
                Err(_) => {
                    publication.merge_commit = Some(repair.merge_commit.clone());
                    publication.updated_at = now;
                    let merge = pg_publication_catchup_merge(&publication, now).map_err(|_| {
                        HubError::Invalid(
                            "proposed merge commit does not repair the retained projection failure"
                                .into(),
                        )
                    })?;
                    (merge, true)
                }
            };
            let result = CatchupPublicationRepairResult {
                publication: publication.id.clone(),
                repository: publication.repository.clone(),
                previous_merge_commit_sha256: current_merge_commit
                    .as_deref()
                    .map(|value| lower_hex_of(value.as_bytes())),
                proposed_merge_commit: repair.merge_commit.clone(),
                applied: repair.apply,
            };
            if !repair.apply {
                tx.commit()?;
                return Ok(result);
            }
            if replace_invalid {
                tx.execute(
                    "UPDATE hub_publications SET merge_commit=$2,updated_at=$3 WHERE id=$1",
                    &[
                        &publication.id,
                        &publication.merge_commit,
                        &publication.updated_at,
                    ],
                )?;
            }
            let observation = pg_record_catchup_merge(&mut tx, &scope, &merge, now)?;
            if observation.pull_number != merge.pull_number
                || observation.merge_commit != repair.merge_commit
            {
                return Err(HubError::Unavailable(
                    "catch-up publication repair did not retain the exact projection".into(),
                ));
            }
            let deleted = tx.execute(
                "DELETE FROM hub_catchup_publication_failures WHERE provider=$1 AND host=$2
                 AND repository=$3 AND repository_key=$4 AND publication=$5",
                &[
                    &scope.provider,
                    &scope.host,
                    &repository_namespace(&scope.repository, scope.repository_id),
                    &catchup_repository_key(scope.repository_id),
                    &publication.id,
                ],
            )?;
            if deleted != 1 {
                return Err(HubError::Unavailable(
                    "retained catch-up projection failure changed during repair".into(),
                ));
            }
            pg_refresh_publication_coverage(&mut tx, &scope, now)?;
            if replace_invalid {
                append(
                    &mut tx,
                    &Event::publication_merge_corrected(
                        &publication.repository,
                        &publication.id,
                        current_merge_commit.as_deref(),
                        &repair.merge_commit,
                    )?,
                )?;
            }
            tx.commit()?;
            Ok(result)
        })
    }

    fn record_catchup_merge(
        &self,
        scope: &CatchupRepositoryScope,
        merge: &NewCatchupMerge,
        observed_at: i64,
    ) -> Result<CatchupMergeObservation> {
        self.run(false, |client| {
            let mut tx = client.transaction()?;
            let value = pg_record_catchup_merge(&mut tx, scope, merge, observed_at)?;
            tx.commit()?;
            Ok(value)
        })
    }

    fn catchup_merge_known(
        &self,
        scope: &CatchupRepositoryScope,
        pull_number: i64,
        merge_commit: Option<&str>,
    ) -> Result<bool> {
        scope.validate()?;
        if pull_number < 1 {
            return Err(HubError::Invalid("pull number must be positive".into()));
        }
        self.run(true, |client| {
            let repository = repository_namespace(&scope.repository, scope.repository_id);
            Ok(client
                .query_one(
                    "SELECT EXISTS(SELECT 1 FROM hub_catchup_merges
            WHERE provider=$1 AND host=$2 AND repository=$3 AND repository_key=$4 AND pull_number=$5
              AND ($6::text IS NULL OR merge_commit=$6))",
                    &[
                        &scope.provider,
                        &scope.host,
                        &repository,
                        &catchup_repository_key(scope.repository_id),
                        &pull_number,
                        &merge_commit,
                    ],
                )?
                .get(0))
        })
    }

    fn catchup_merge_highwater(&self, scope: &CatchupRepositoryScope) -> Result<i64> {
        scope.validate()?;
        self.run(true, |client| {
            let repository = repository_namespace(&scope.repository, scope.repository_id);
            Ok(client
                .query_opt(
                    "SELECT next_merge_seq-1 FROM hub_catchup_repositories WHERE provider=$1
            AND host=$2 AND repository=$3 AND repository_key=$4",
                    &[
                        &scope.provider,
                        &scope.host,
                        &repository,
                        &catchup_repository_key(scope.repository_id),
                    ],
                )?
                .map_or(0, |r| r.get(0)))
        })
    }

    fn catchup_merges_through(
        &self,
        scope: &CatchupRepositoryScope,
        after: i64,
        through: i64,
        limit: usize,
    ) -> Result<Vec<CatchupMergeObservation>> {
        scope.validate()?;
        if after < 0 || through < after || !(1..=MAX_CATCHUP_PAGE).contains(&limit) {
            return Err(HubError::Invalid("invalid catch-up merge page".into()));
        }
        let repository = repository_namespace(&scope.repository, scope.repository_id);
        self.run(true,|client|client.query(&format!("SELECT {CATCHUP_MERGE_COLUMNS} FROM hub_catchup_merges
            WHERE provider=$1 AND host=$2 AND repository=$3 AND repository_key=$4 AND seq>$5 AND seq<=$6
            ORDER BY seq LIMIT $7"),&[&scope.provider,&scope.host,&repository,
            &catchup_repository_key(scope.repository_id),&after,&through,&(limit as i64)])?.iter().map(pg_catchup_merge).collect())
    }

    fn record_catchup_source_status(
        &self,
        scope: &CatchupRepositoryScope,
        status: &CatchupSourceStatus,
    ) -> Result<()> {
        scope.validate()?;
        status.validate()?;
        let repository = repository_namespace(&scope.repository, scope.repository_id);
        self.run(false,|client|{client.execute("INSERT INTO hub_catchup_sources
            (provider,host,repository,repository_key,repository_id,source,state,checked_at,unread_pages,deferred)
            VALUES ($1,$2,$3,$4,$5,'tracked_publication','incomplete',$6,1,0)
            ON CONFLICT DO NOTHING",&[&scope.provider,&scope.host,&repository,
            &catchup_repository_key(scope.repository_id),&scope.repository_id,&status.checked_at])?;
            client.execute("INSERT INTO hub_catchup_sources
            (provider,host,repository,repository_key,repository_id,source,state,checked_at,unread_pages,deferred,window_from)
            VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11)
            ON CONFLICT(provider,host,repository,repository_key,source) DO UPDATE SET state=EXCLUDED.state,
              checked_at=EXCLUDED.checked_at,unread_pages=EXCLUDED.unread_pages,deferred=EXCLUDED.deferred,
              window_from=EXCLUDED.window_from
            WHERE EXCLUDED.checked_at>=hub_catchup_sources.checked_at",&[&scope.provider,&scope.host,&repository,
            &catchup_repository_key(scope.repository_id),&scope.repository_id,&source_name(status.source),
            &coverage_name(status.state),&status.checked_at,&(status.unread_pages as i64),&(status.deferred as i64),&status.window_from])?;Ok(())})
    }

    fn work_start(&self, token: &str, start: &WorkStart, now: i64) -> Result<WorkStartResult> {
        self.run(false, |client| {
            let mut tx = client.transaction()?;
            let who = work_who(&mut tx, token, now)?;
            lock_work_capacity(&mut tx, &who)?;
            if start.preflight.is_some() {
                pg_try_backfill_catchup_publications(&mut tx, &who, now)?;
            }
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
            let scope = start.validate(&who)?;
            let preflight = preflight::evaluate(&mut tx, &who, start, &scope, now)?;
            if super::work_preflight::coordinates_active_scope_overlap(start, &preflight) {
                let mut result = start_in(&mut tx, &who, start, now)?;
                super::work_preflight::require_overlap_coordination(&result, &preflight)?;
                result.preflight = Some(preflight);
                tx.commit()?;
                return Ok(result);
            }
            require_clear_preflight(&who, start, &preflight)?;
            let mut result = start_in(&mut tx, &who, start, now)?;
            result.preflight = Some(preflight);
            tx.commit()?;
            Ok(result)
        })
    }

    fn work_preflight(
        &self,
        token: &str,
        start: &WorkStart,
        now: i64,
    ) -> Result<super::work_preflight::WorkPreflightResult> {
        self.run(false, |client| {
            let mut tx = client.transaction()?;
            let who = work_who(&mut tx, token, now)?;
            lock_work_capacity(&mut tx, &who)?;
            pg_try_backfill_catchup_publications(&mut tx, &who, now)?;
            let scope = start.validate(&who)?;
            let result = preflight::evaluate(&mut tx, &who, start, &scope, now)?;
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
            if start.preflight.is_some() {
                pg_try_backfill_catchup_publications(&mut tx, &who, now)?;
            }
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
                let mut inspected = planned.clone();
                inspected.work_id = Some(item.id.clone());
                inspected.request_key = None;
                let preflight =
                    preflight::evaluate(&mut tx, &who, &inspected, &planned_scope, now)?;
                require_clear_preflight(&who, start, &preflight)?;
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
                let result = WorkStartResult {
                    item,
                    created: false,
                    joined: false,
                    took_over: false,
                    conflict,
                    context: None,
                    coordination_request: None,
                    preflight: Some(preflight),
                };
                tx.commit()?;
                return Ok(result);
            }
            let mut created = start.clone();
            created.request_key = Some(new_id("req")?);
            let preflight = preflight::evaluate(&mut tx, &who, &created, &planned_scope, now)?;
            require_clear_preflight(&who, start, &preflight)?;
            let mut result = start_in(&mut tx, &who, &created, now)?;
            result.preflight = Some(preflight);
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
            for source in &checkpoint.source_refs {
                if let SourceRef::PriorWorkVersion { work_id, event_seq } = source {
                    if work_id != &checkpoint.work_id {
                        return Err(HubError::Invalid(
                            "prior work version must reference the checkpoint work".into(),
                        ));
                    }
                    let retained = tx
                        .query_opt(
                            "SELECT 1 FROM hub_work_versions
                             WHERE work = $1 AND event_seq = $2 AND event_seq <= $3",
                            &[work_id, event_seq, &item.updated_seq],
                        )?
                        .is_some();
                    if !retained {
                        return Err(HubError::Invalid(
                            "prior work version is unavailable".into(),
                        ));
                    }
                }
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

    fn work_search(
        &self,
        token: &str,
        search: Option<&str>,
        cursor: Option<&str>,
        limit: usize,
        now: i64,
    ) -> Result<WorkSearchPage> {
        self.run(true, |client| {
            let mut tx = client.transaction()?;
            let who = work_who(&mut tx, token, now)?;
            let after = cursor.map(super::work::parse_search_cursor).transpose()?;
            // A cursor must name a work item of this scope at exactly that creation time.
            if let Some((created_at, id)) = &after {
                let known = tx.query_opt(
                    "SELECT 1 FROM hub_work_items WHERE id = $5 AND created_at = $6
                       AND provider = $1 AND host = $2 AND repository = $3
                       AND repository_id IS NOT DISTINCT FROM $4",
                    &[
                        &who.owner.identity.provider,
                        &who.owner.identity.host,
                        &who.repository,
                        &who.repository_id,
                        id,
                        created_at,
                    ],
                )?;
                if known.is_none() {
                    return Err(HubError::Invalid("unknown cursor".into()));
                }
            }
            let pattern = search.map(like_pattern);
            let number = search.and_then(search_number).map(|n| n.to_string());
            let after_at = after.as_ref().map(|a| a.0);
            let after_id = after.as_ref().map(|a| a.1.clone());
            let rows = tx.query(
                "SELECT w.id, w.title, w.state, w.lead_agent, u.login, w.branch, w.source_issue, w.updated_at, w.created_at
                 FROM hub_work_items w JOIN hub_users u ON u.id = w.owner
                 WHERE w.provider = $1 AND w.host = $2 AND w.repository = $3
                   AND w.repository_id IS NOT DISTINCT FROM $4
                   AND ($5::bigint IS NULL OR (w.created_at, w.id) < ($5::bigint, $6::text))
                   AND ($7::text IS NULL OR w.source_issue = $8::text
                     OR w.title ILIKE $7 ESCAPE '\\' OR w.branch ILIKE $7 ESCAPE '\\'
                     OR w.lead_agent ILIKE $7 ESCAPE '\\' OR u.login ILIKE $7 ESCAPE '\\'
                     OR w.id ILIKE $7 ESCAPE '\\' OR w.scope ILIKE $7 ESCAPE '\\')
                 ORDER BY w.created_at DESC, w.id DESC LIMIT $9",
                &[
                    &who.owner.identity.provider,
                    &who.owner.identity.host,
                    &who.repository,
                    &who.repository_id,
                    &after_at,
                    &after_id,
                    &pattern,
                    &number,
                    &(limit as i64 + 1),
                ],
            )?;
            let items = rows
                .iter()
                .map(|row| WorkSearchItem {
                    id: row.get(0),
                    title: row.get(1),
                    state: row.get(2),
                    lead_agent: row.get(3),
                    owner: row.get(4),
                    branch: row.get(5),
                    source_issue: row.get(6),
                    updated_at: row.get(7),
                    created_at: row.get(8),
                })
                .collect();
            tx.commit()?;
            Ok(WorkSearchPage::of(items, limit))
        })
    }

    fn change_ingest(
        &self,
        token: &str,
        batch: &CaptureBatchV1,
        now: i64,
    ) -> Result<IngestReceiptV1> {
        batch.validate()?;
        let request = json_string(batch)?;
        let identity = json_string(&batch.session)?;
        self.run(false, |client| {
            let mut tx = client.transaction()?;
            let who = work_who(&mut tx, token, now)?;
            if who.read_only {
                return Err(HubError::Unauthorized);
            }
            let item = scoped_work(&mut tx, &who, &batch.work_id, now, true)?
                .ok_or_else(|| HubError::Invalid("unknown work".into()))?;
            if let Some(row)=tx.query_opt("SELECT author_token,kind,request,response FROM hub_change_operations WHERE work=$1 AND operation_id=$2",&[&batch.work_id,&batch.operation_id])? {
                let author:String=row.get(0);let kind:String=row.get(1);let retained:String=row.get(2);let response:String=row.get(3);
                if author==token&&kind=="event_batch"&&retained==request {let mut receipt:IngestReceiptV1=json_value(&response)?;receipt.replayed=true;tx.commit()?;return Ok(receipt);}
                return Err(HubError::Invalid("operation_id is already used by a different capture request".into()));
            }
            if !item.state.accepts_capture() || item.generation != batch.generation {
                return Err(HubError::Invalid("capture work is no longer current".into()));
            }
            if tx.query_opt("SELECT 1 FROM hub_work_participants WHERE work=$1 AND token=$2 AND session=$3 AND generation=$4",&[&batch.work_id,&token,&batch.session.session_id,&batch.generation])?.is_none() {
                return Err(HubError::Invalid("capture session is not a current work participant".into()));
            }
            if let Some(row)=tx.query_opt("SELECT identity FROM hub_change_sessions WHERE work=$1 AND generation=$2 AND session_id=$3 FOR UPDATE",&[&batch.work_id,&batch.generation,&batch.session.session_id])? {
                if row.get::<_,String>(0)!=identity {return Err(HubError::Invalid("capture session identity changed".into()));}
            } else {
                let first=batch.events[0].observed_at;
                tx.execute("INSERT INTO hub_change_sessions (work,session_id,generation,identity,watermark,event_count,gap_count,started,ended,start_sequence,end_sequence,lifecycle_valid,first_observed_at,last_observed_at,updated_at) VALUES ($1,$2,$3,$4,0,0,0,FALSE,FALSE,NULL,NULL,FALSE,$5,$5,$6)",&[&batch.work_id,&batch.session.session_id,&batch.generation,&identity,&first,&now])?;
            }
            let mut accepted=0_usize;let mut duplicate=0_usize;let mut new_event_bytes=0_i64;let mut watermark=0_u64;let mut gap_count=0_usize;let mut last_observed=0_i64;
            for event in &batch.events {
                let encoded=json_string(event)?;
                if let Some(row)=tx.query_opt("SELECT sequence,event FROM hub_change_events WHERE work=$1 AND generation=$2 AND session_id=$3 AND event_id=$4",&[&batch.work_id,&batch.generation,&batch.session.session_id,&event.event_id])? {
                    if row.get::<_,i64>(0)==event.sequence as i64&&row.get::<_,String>(1)==encoded {duplicate+=1;watermark=watermark.max(event.sequence);continue;}
                    return Err(HubError::Invalid("event_id is already used by different capture data".into()));
                }
                let sequence=event.sequence as i64;
                if tx.query_opt("SELECT 1 FROM hub_change_events WHERE work=$1 AND generation=$2 AND session_id=$3 AND sequence=$4",&[&batch.work_id,&batch.generation,&batch.session.session_id,&sequence])?.is_some(){return Err(HubError::Invalid("capture sequence is already used by another event".into()));}
                tx.execute("INSERT INTO hub_change_events (work,generation,session_id,event_id,sequence,event,observed_at) VALUES ($1,$2,$3,$4,$5,$6,$7)",&[&batch.work_id,&batch.generation,&batch.session.session_id,&event.event_id,&sequence,&encoded,&event.observed_at])?;
                if let Some(artifact)=event.artifact.as_ref().filter(|artifact|artifact.retention==RetentionClassV1::SharedApproved){declare_shared_body(&mut tx,batch,token,event,artifact,now)?;}
                accepted+=1;new_event_bytes=new_event_bytes.checked_add(work_payload_row_bytes(encoded.len())?).ok_or_else(||HubError::Invalid("change capture size is too large".into()))?;gap_count+=usize::from(event.gap.is_some());watermark=watermark.max(event.sequence);last_observed=last_observed.max(event.observed_at);
            }
            // Aggregate in SQL so a large retained session is not loaded and re-parsed per batch.
            // A compaction restart continues the same session, not a second origin.
            let life=tx.query_one("SELECT COUNT(*)::bigint,COALESCE(MAX(sequence),0)::bigint,COUNT(*) FILTER (WHERE kind='session_started' AND boundary IS DISTINCT FROM 'compact')::bigint,MIN(sequence) FILTER (WHERE kind='session_started' AND boundary IS DISTINCT FROM 'compact'),COUNT(*) FILTER (WHERE kind='session_ended')::bigint,MIN(sequence) FILTER (WHERE kind='session_ended') FROM (SELECT sequence,event::jsonb->>'kind' AS kind,event::jsonb->>'boundary' AS boundary FROM hub_change_events WHERE work=$1 AND generation=$2 AND session_id=$3) e",&[&batch.work_id,&batch.generation,&batch.session.session_id])?;
            let retained_events:i64=life.get(0);let retained_watermark:i64=life.get(1);
            let start_sequence:Option<i64>=if life.get::<_,i64>(2)==1{life.get(3)}else{None};let end_sequence:Option<i64>=if life.get::<_,i64>(4)==1{life.get(5)}else{None};
            let lifecycle_valid=start_sequence==Some(1)&&end_sequence==Some(retained_watermark)&&retained_watermark>1;let gaps_i64=gap_count as i64;
            tx.execute("UPDATE hub_change_sessions SET watermark=$4,event_count=$5,gap_count=gap_count+$6,started=$7,ended=$8,start_sequence=$9,end_sequence=$10,lifecycle_valid=$11,last_observed_at=GREATEST(last_observed_at,$12),updated_at=$13 WHERE work=$1 AND generation=$2 AND session_id=$3",&[&batch.work_id,&batch.generation,&batch.session.session_id,&retained_watermark,&retained_events,&gaps_i64,&start_sequence.is_some(),&end_sequence.is_some(),&start_sequence,&end_sequence,&lifecycle_valid,&last_observed,&now])?;
            let retained_operations:i64=tx.query_one("SELECT COUNT(*)::bigint FROM hub_change_operations WHERE work=$1",&[&batch.work_id])?.get(0);
            if retained_events>MAX_CHANGE_EVENTS_PER_SESSION||retained_operations>=MAX_CHANGE_OPERATIONS_PER_WORK{return Err(HubError::WorkCapacity);}
            let capture_bytes=work_payload_row_bytes(request.len())?.checked_add(new_event_bytes).ok_or_else(||HubError::Invalid("change capture size is too large".into()))?;
            adjust_work_payload_capacity(&mut tx,&who,capture_bytes,now)?;
            let receipt=IngestReceiptV1{work_id:batch.work_id.clone(),operation_id:batch.operation_id.clone(),accepted,duplicate,watermark:retained_watermark as u64,replayed:false};
            tx.execute("INSERT INTO hub_change_operations (work,operation_id,author_token,kind,request,response,received_at) VALUES ($1,$2,$3,'event_batch',$4,$5,$6)",&[&batch.work_id,&batch.operation_id,&token,&request,&json_string(&receipt)?,&now])?;
            tx.commit()?;Ok(receipt)
        })
    }

    fn shared_body_lookup(
        &self,
        token: &str,
        work_id: &str,
        digest: &str,
        now: i64,
    ) -> Result<Option<SharedBodyAccess>> {
        self.run(true, |client| {
            let mut tx = client
                .build_transaction()
                .isolation_level(IsolationLevel::RepeatableRead)
                .start()?;
            let who = work_who(&mut tx, token, now)?;
            if scoped_work(&mut tx, &who, work_id, now, false)?.is_none() {
                return Ok(None);
            }
            let rows = tx
                .query(
                    &format!(
                        "SELECT {SHARED_BODY_COLUMNS} FROM hub_change_shared_bodies
                         WHERE work=$1 AND digest=$2 ORDER BY declared_at,author"
                    ),
                    &[&work_id, &digest],
                )?
                .iter()
                .map(shared_body_row)
                .collect::<Result<Vec<_>>>()?;
            Ok(Some(SharedBodyAccess {
                rows,
                caller: who.owner.id,
            }))
        })
    }

    fn shared_bodies_own(
        &self,
        token: &str,
        work_id: &str,
        digests: &[String],
        now: i64,
    ) -> Result<Option<Vec<SharedBodyRow>>> {
        self.run(true, |client| {
            let mut tx = client
                .build_transaction()
                .isolation_level(IsolationLevel::RepeatableRead)
                .start()?;
            let who = work_who(&mut tx, token, now)?;
            if scoped_work(&mut tx, &who, work_id, now, false)?.is_none() {
                return Ok(None);
            }
            let rows = tx
                .query(
                    &format!(
                        "SELECT {SHARED_BODY_COLUMNS} FROM hub_change_shared_bodies
                         WHERE work=$1 AND author=$2 AND digest = ANY($3)"
                    ),
                    &[&work_id, &who.owner.id, &digests],
                )?
                .iter()
                .map(shared_body_row)
                .collect::<Result<Vec<_>>>()?;
            Ok(Some(rows))
        })
    }

    fn shared_conversation_page(
        &self,
        token: &str,
        work_id: &str,
        cursor: Option<&str>,
        all_authors: bool,
        now: i64,
    ) -> Result<Option<Vec<ConversationEntryV1>>> {
        let after = cursor.map(parse_conversation_cursor).transpose()?;
        self.run(true, |client| {
            let mut tx = client
                .build_transaction()
                .isolation_level(IsolationLevel::RepeatableRead)
                .start()?;
            let who = work_who(&mut tx, token, now)?;
            if scoped_work(&mut tx, &who, work_id, now, false)?.is_none() {
                return Ok(None);
            }
            let rows = tx.query(
                &format!(
                    "WITH page AS (
                         SELECT {SHARED_BODY_COLUMNS} FROM hub_change_shared_bodies
                         WHERE work=$1 AND ($2::text IS NULL
                           OR (session_id COLLATE \"C\",sequence,digest COLLATE \"C\")
                              > ($2::text COLLATE \"C\",$3::bigint,$4::text COLLATE \"C\"))
                           AND ($6::boolean OR author=$7)
                         ORDER BY session_id COLLATE \"C\",sequence,digest COLLATE \"C\" LIMIT $5)
                     SELECT page.*, e.event, e.observed_at FROM page
                     JOIN hub_change_events e ON e.work=page.work AND e.generation=page.generation
                       AND e.session_id=page.session_id AND e.event_id=page.event_id
                     ORDER BY page.session_id COLLATE \"C\",page.sequence,page.digest COLLATE \"C\""
                ),
                &[
                    &work_id,
                    &after.as_ref().map(|after| after.0.as_str()),
                    &after.as_ref().map(|after| after.1 as i64),
                    &after.as_ref().map(|after| after.2.as_str()),
                    &(MAX_CONVERSATION_PAGE_ENTRIES as i64 + 1),
                    &all_authors,
                    &who.owner.id,
                ],
            )?;
            let entries = rows
                .iter()
                .map(|row| {
                    shared_body_row(row)?.conversation_entry(
                        &row.get::<_, String>(SHARED_BODY_COLUMN_COUNT),
                        row.get::<_, i64>(SHARED_BODY_COLUMN_COUNT + 1),
                        now,
                    )
                })
                .collect::<Result<Vec<_>>>()?;
            Ok(Some(entries))
        })
    }

    fn shared_body_mark_available(
        &self,
        work_id: &str,
        digest: &str,
        author: &str,
        now: i64,
        retention_seconds: i64,
    ) -> Result<Option<SharedBodyState>> {
        self.run(false, |client| {
            let mut tx = client.transaction()?;
            let expires = now.saturating_add(retention_seconds);
            tx.execute(
                "UPDATE hub_change_shared_bodies
                 SET state='available', uploaded_at=$4, expires_at=$5
                 WHERE work=$1 AND digest=$2 AND author=$3 AND state='declared'",
                &[&work_id, &digest, &author, &now, &expires],
            )?;
            let state = tx
                .query_opt(
                    "SELECT state FROM hub_change_shared_bodies
                     WHERE work=$1 AND digest=$2 AND author=$3",
                    &[&work_id, &digest, &author],
                )?
                .map(|row| SharedBodyState::parse(&row.get::<_, String>(0)))
                .transpose()?;
            tx.commit()?;
            Ok(state)
        })
    }

    fn shared_body_tombstone(
        &self,
        work_id: &str,
        digest: &str,
        author: Option<&str>,
        actor: &str,
        now: i64,
    ) -> Result<Vec<SharedBodyRow>> {
        self.run(false, |client| {
            let mut tx = client.transaction()?;
            tx.execute(
                "UPDATE hub_change_shared_bodies
                 SET state='deleted', deleted_at=$4, deleted_by=$5
                 WHERE work=$1 AND digest=$2 AND ($3::text IS NULL OR author=$3)
                   AND state<>'deleted'",
                &[&work_id, &digest, &author, &now, &actor],
            )?;
            let rows = tx
                .query(
                    &format!(
                        "SELECT {SHARED_BODY_COLUMNS} FROM hub_change_shared_bodies
                         WHERE work=$1 AND digest=$2 AND ($3::text IS NULL OR author=$3)
                         ORDER BY author"
                    ),
                    &[&work_id, &digest, &author],
                )?
                .iter()
                .map(shared_body_row)
                .collect::<Result<Vec<_>>>()?;
            tx.commit()?;
            Ok(rows)
        })
    }

    fn shared_bodies_expire(
        &self,
        now: i64,
        limit: usize,
        retention_seconds: i64,
    ) -> Result<Vec<SharedBodyRow>> {
        self.run(false, |client| {
            let limit = limit as i64;
            client
                .query(
                    &format!(
                        // MATERIALIZED: a semi-join over the subquery could ignore its LIMIT.
                        "WITH due AS MATERIALIZED (
                             SELECT work AS w,digest AS d,author AS a FROM hub_change_shared_bodies
                             WHERE (state='available' AND expires_at<=$1)
                                OR (state='declared' AND COALESCE(expires_at,declared_at+$3)<=$1)
                             ORDER BY COALESCE(expires_at,declared_at+$3),work,digest,author LIMIT $2
                             FOR UPDATE SKIP LOCKED)
                         UPDATE hub_change_shared_bodies b SET state='expired',
                             expires_at=COALESCE(b.expires_at,b.declared_at+$3)
                         FROM due WHERE b.work=due.w AND b.digest=due.d AND b.author=due.a
                         RETURNING {SHARED_BODY_COLUMNS}"
                    ),
                    &[&now, &limit, &retention_seconds],
                )?
                .iter()
                .map(shared_body_row)
                .collect()
        })
    }

    fn shared_body_demote_available(
        &self,
        work_id: &str,
        digest: &str,
        author: Option<&str>,
    ) -> Result<usize> {
        self.run(false, |client| {
            Ok(client.execute(
                "UPDATE hub_change_shared_bodies SET state='declared'
                 WHERE work=$1 AND digest=$2 AND ($3::text IS NULL OR author=$3)
                   AND state='available'",
                &[&work_id, &digest, &author],
            )? as usize)
        })
    }

    fn shared_bodies_pending_physical_delete(
        &self,
        now: i64,
        limit: usize,
    ) -> Result<Vec<SharedBodyRow>> {
        self.run(true, |client| {
            let limit = limit as i64;
            client
                .query(
                    &format!(
                        "SELECT {SHARED_BODY_COLUMNS} FROM hub_change_shared_bodies
                         WHERE state IN ('expired','deleted') AND physical_deleted_at IS NULL
                           AND (next_delete_at IS NULL OR next_delete_at<=$1)
                         ORDER BY COALESCE(deleted_at,expires_at),work,digest,author LIMIT $2"
                    ),
                    &[&now, &limit],
                )?
                .iter()
                .map(shared_body_row)
                .collect()
        })
    }

    fn shared_body_object_in_use(&self, work_id: &str, digest: &str) -> Result<bool> {
        self.run(true, |client| {
            Ok(client
                .query_one(
                    "SELECT EXISTS (SELECT 1 FROM hub_change_shared_bodies
                     WHERE work=$1 AND digest=$2 AND state='available')",
                    &[&work_id, &digest],
                )?
                .get(0))
        })
    }

    fn shared_body_mark_physically_deleted(
        &self,
        work_id: &str,
        digest: &str,
        author: &str,
        now: i64,
    ) -> Result<()> {
        self.run(false, |client| {
            client.execute(
                "UPDATE hub_change_shared_bodies SET physical_deleted_at=$4, next_delete_at=NULL
                 WHERE work=$1 AND digest=$2 AND author=$3 AND state IN ('expired','deleted')
                   AND physical_deleted_at IS NULL",
                &[&work_id, &digest, &author, &now],
            )?;
            Ok(())
        })
    }

    fn shared_body_delete_failed(
        &self,
        work_id: &str,
        digest: &str,
        author: &str,
        retry_at: i64,
    ) -> Result<()> {
        self.run(false, |client| {
            client.execute(
                "UPDATE hub_change_shared_bodies
                 SET physical_deleted_at=NULL, delete_attempts=delete_attempts+1, next_delete_at=$4
                 WHERE work=$1 AND digest=$2 AND author=$3 AND state IN ('expired','deleted')",
                &[&work_id, &digest, &author, &retry_at],
            )?;
            Ok(())
        })
    }

    fn change_action_start(
        &self,
        token: &str,
        start: &ActionStartV1,
        now: i64,
    ) -> Result<ActionMutationReceiptV1> {
        start.validate()?;
        let request = json_string(start)?;
        if start.started_at > now.saturating_add(300) {
            return Err(HubError::Invalid(
                "action start time is too far in the future".into(),
            ));
        }
        self.run(false,|client|{let mut tx=client.transaction()?;let who=work_who(&mut tx,token,now)?;if who.read_only{return Err(HubError::Unauthorized);}let item=scoped_work(&mut tx,&who,&start.work_id,now,true)?.ok_or_else(||HubError::Invalid("unknown work".into()))?;
            if let Some(row)=tx.query_opt("SELECT author_token,kind,request,response FROM hub_change_operations WHERE work=$1 AND operation_id=$2",&[&start.work_id,&start.operation_id])?{let author:String=row.get(0);let kind:String=row.get(1);let retained:String=row.get(2);let response:String=row.get(3);if author==token&&kind=="action_start"&&retained==request{let mut receipt:ActionMutationReceiptV1=json_value(&response)?;receipt.replayed=true;tx.commit()?;return Ok(receipt);}return Err(HubError::Invalid("operation_id is already used by a different action request".into()));}
            if !item.state.accepts_capture()||item.generation!=start.generation{return Err(HubError::Invalid("action work is no longer current".into()));}
            if start.source.repository()!=who.repository{return Err(HubError::Invalid("action source belongs to another repository".into()));}
            if tx.query_opt("SELECT 1 FROM hub_work_participants WHERE work=$1 AND token=$2 AND session=$3 AND generation=$4",&[&start.work_id,&token,&start.session_id,&start.generation])?.is_none(){return Err(HubError::Invalid("action session is not a current work participant".into()));}
            if tx.query_opt("SELECT 1 FROM hub_change_actions WHERE work=$1 AND action_id=$2",&[&start.work_id,&start.action_id])?.is_some(){return Err(HubError::Invalid("action_id is already used".into()));}
            let action_count:i64=tx.query_one("SELECT COUNT(*)::bigint FROM hub_change_actions WHERE work=$1",&[&start.work_id])?.get(0);let operation_count:i64=tx.query_one("SELECT COUNT(*)::bigint FROM hub_change_operations WHERE work=$1",&[&start.work_id])?.get(0);if action_count>=MAX_CHANGE_ACTIONS_PER_WORK||operation_count>=MAX_CHANGE_OPERATIONS_PER_WORK{return Err(HubError::WorkCapacity);}
            let capture_bytes=work_payload_row_bytes(request.len())?.checked_mul(2).ok_or_else(||HubError::Invalid("change capture size is too large".into()))?;adjust_work_payload_capacity(&mut tx,&who,capture_bytes,now)?;
            let received_order:i64=tx.query_one("SELECT COALESCE(MAX(received_order),0)+1 FROM hub_change_actions WHERE work=$1",&[&start.work_id])?.get(0);
            let source_id=start.source.stable_id()?;let source_kind=enum_name(&start.source.kind())?;
            tx.execute("INSERT INTO hub_change_actions (work,action_id,generation,session_id,author_token,source_id,source_kind,start_request,started_at,received_at,received_order,updated_at) VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$10)",&[&start.work_id,&start.action_id,&start.generation,&start.session_id,&token,&source_id,&source_kind,&request,&start.started_at,&now,&received_order])?;
            let receipt=ActionMutationReceiptV1{work_id:start.work_id.clone(),operation_id:start.operation_id.clone(),action_id:start.action_id.clone(),replayed:false};tx.execute("INSERT INTO hub_change_operations (work,operation_id,author_token,kind,request,response,received_at) VALUES ($1,$2,$3,'action_start',$4,$5,$6)",&[&start.work_id,&start.operation_id,&token,&request,&json_string(&receipt)?,&now])?;tx.commit()?;Ok(receipt)})
    }

    fn change_action_finish(
        &self,
        token: &str,
        finish: &ActionFinishV1,
        now: i64,
    ) -> Result<ActionMutationReceiptV1> {
        finish.validate()?;
        if finish.assurance != crate::change_record::AssuranceBasisV1::CallerAsserted {
            return Err(HubError::Invalid(
                "agent capture can only record caller_asserted assurance".into(),
            ));
        }
        let request = json_string(finish)?;
        if finish.finished_at > now.saturating_add(300) {
            return Err(HubError::Invalid(
                "action finish time is too far in the future".into(),
            ));
        }
        self.run(false,|client|{let mut tx=client.transaction()?;let who=work_who(&mut tx,token,now)?;if who.read_only{return Err(HubError::Unauthorized);}let item=scoped_work(&mut tx,&who,&finish.work_id,now,true)?.ok_or_else(||HubError::Invalid("unknown work".into()))?;
            if let Some(row)=tx.query_opt("SELECT author_token,kind,request,response FROM hub_change_operations WHERE work=$1 AND operation_id=$2",&[&finish.work_id,&finish.operation_id])?{let author:String=row.get(0);let kind:String=row.get(1);let retained:String=row.get(2);let response:String=row.get(3);if author==token&&kind=="action_finish"&&retained==request{let mut receipt:ActionMutationReceiptV1=json_value(&response)?;receipt.replayed=true;tx.commit()?;return Ok(receipt);}return Err(HubError::Invalid("operation_id is already used by a different action request".into()));}
            if !item.state.accepts_capture()||item.generation!=finish.generation{return Err(HubError::Invalid("action work is no longer current".into()));}
            let row=tx.query_opt("SELECT author_token,generation,started_at,finish_request,start_request FROM hub_change_actions WHERE work=$1 AND action_id=$2 FOR UPDATE",&[&finish.work_id,&finish.action_id])?.ok_or_else(||HubError::Invalid("unknown action".into()))?;let author:String=row.get(0);let generation:i64=row.get(1);let started_at:i64=row.get(2);let already:Option<String>=row.get(3);let start_request:String=row.get(4);if author!=token||generation!=finish.generation{return Err(HubError::Invalid("action finish author or generation changed".into()));}let retained_start:ActionStartV1=json_value(&start_request)?;if retained_start.kind==crate::change_record::ActionKindV1::Execution&&finish.result!=crate::change_record::CheckResultV1::Unknown{return Err(HubError::Invalid("execution actions cannot assert verification results".into()));}
            if retained_start.kind==crate::change_record::ActionKindV1::Verification&&retained_start.recipe_provenance!=crate::change_record::RecipeProvenanceV1::ConfiguredExactMatch&&matches!(finish.result,crate::change_record::CheckResultV1::Passed|crate::change_record::CheckResultV1::Failed){return Err(HubError::Invalid("verification result requires an exact configured recipe".into()));}
            if already.is_some(){return Err(HubError::Invalid("action is already finished".into()));}
            if finish.finished_at<started_at{return Err(HubError::Invalid("action finish precedes its start".into()));}
            let operation_count:i64=tx.query_one("SELECT COUNT(*)::bigint FROM hub_change_operations WHERE work=$1",&[&finish.work_id])?.get(0);if operation_count>=MAX_CHANGE_OPERATIONS_PER_WORK{return Err(HubError::WorkCapacity);}let capture_bytes=work_payload_row_bytes(request.len())?.checked_mul(2).ok_or_else(||HubError::Invalid("change capture size is too large".into()))?;adjust_work_payload_capacity(&mut tx,&who,capture_bytes,now)?;
            tx.execute("UPDATE hub_change_actions SET finish_request=$3,finished_at=$4,updated_at=$5 WHERE work=$1 AND action_id=$2",&[&finish.work_id,&finish.action_id,&request,&finish.finished_at,&now])?;let receipt=ActionMutationReceiptV1{work_id:finish.work_id.clone(),operation_id:finish.operation_id.clone(),action_id:finish.action_id.clone(),replayed:false};tx.execute("INSERT INTO hub_change_operations (work,operation_id,author_token,kind,request,response,received_at) VALUES ($1,$2,$3,'action_finish',$4,$5,$6)",&[&finish.work_id,&finish.operation_id,&token,&request,&json_string(&receipt)?,&now])?;tx.commit()?;Ok(receipt)})
    }

    fn skill_adoption(&self, repo: &str) -> Result<Option<SkillAdoption>> {
        self.run(true, |client| {
            Ok(client
                .query_opt(
                    "SELECT repository, source, lock_json, default_branch, observed_at, reported_by
                     FROM hub_skill_adoptions WHERE repository = $1",
                    &[&repo],
                )?
                .map(|row| SkillAdoption {
                    repository: row.get(0),
                    source: row.get(1),
                    lock_json: row.get(2),
                    default_branch: row.get(3),
                    observed_at: row.get(4),
                    reported_by: row.get(5),
                }))
        })
    }

    fn skill_adoption_put(&self, a: &SkillAdoption) -> Result<SkillAdoption> {
        self.run(false, |client| {
            client.execute(
                "INSERT INTO hub_skill_adoptions
                     (repository, source, lock_json, default_branch, observed_at, reported_by)
                 VALUES ($1, $2, $3, $4, $5, $6)
                 ON CONFLICT (repository) DO UPDATE SET source = excluded.source,
                     lock_json = excluded.lock_json, default_branch = excluded.default_branch,
                     observed_at = excluded.observed_at, reported_by = excluded.reported_by
                 WHERE excluded.source = 'default_branch'
                    OR hub_skill_adoptions.source <> 'default_branch'",
                &[
                    &a.repository,
                    &a.source,
                    &a.lock_json,
                    &a.default_branch,
                    &a.observed_at,
                    &a.reported_by,
                ],
            )?;
            Ok(())
        })?;
        self.skill_adoption(&a.repository)?
            .ok_or_else(|| HubError::Unavailable("skill adoption vanished".into()))
    }

    fn skill_adoption_clear(&self, repo: &str, source: Option<&str>) -> Result<()> {
        self.run(false, |client| {
            client.execute(
                "DELETE FROM hub_skill_adoptions
                 WHERE repository = $1 AND ($2::TEXT IS NULL OR source = $2)",
                &[&repo, &source],
            )?;
            Ok(())
        })
    }

    fn skill_evidence_add(&self, r: &EvidenceRow) -> Result<()> {
        self.run(false, |client| {
            client.execute(
                "INSERT INTO hub_skill_evidence (id, repository, branch, run_id, skill,
                     skill_sha256, gate, kind, head_sha, assurance, summary, url, pull_request,
                     detail, token_id, session, agent, recorded_at)
                 VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, $16, $17, $18)",
                &[
                    &r.id,
                    &r.repository,
                    &r.branch,
                    &r.run_id,
                    &r.skill,
                    &r.skill_sha256,
                    &r.gate,
                    &r.kind,
                    &r.head_sha,
                    &r.assurance,
                    &r.summary,
                    &r.url,
                    &r.pull_request,
                    &r.detail,
                    &r.token_id,
                    &r.session,
                    &r.agent,
                    &r.recorded_at,
                ],
            )?;
            Ok(())
        })
    }

    fn skill_evidence(&self, repo: &str, run_id: &str) -> Result<Vec<EvidenceRow>> {
        self.run(true, |client| {
            Ok(client
                .query(
                    "SELECT id, repository, branch, run_id, skill, skill_sha256, gate, kind,
                            head_sha, assurance, summary, url, pull_request, detail, token_id,
                            session, agent, recorded_at
                     FROM hub_skill_evidence WHERE repository = $1 AND run_id = $2 ORDER BY seq",
                    &[&repo, &run_id],
                )?
                .iter()
                .map(|row| EvidenceRow {
                    id: row.get(0),
                    repository: row.get(1),
                    branch: row.get(2),
                    run_id: row.get(3),
                    skill: row.get(4),
                    skill_sha256: row.get(5),
                    gate: row.get(6),
                    kind: row.get(7),
                    head_sha: row.get(8),
                    assurance: row.get(9),
                    summary: row.get(10),
                    url: row.get(11),
                    pull_request: row.get(12),
                    detail: row.get(13),
                    token_id: row.get(14),
                    session: row.get(15),
                    agent: row.get(16),
                    recorded_at: row.get(17),
                })
                .collect())
        })
    }

    fn skill_run_heads(
        &self,
        repo: &str,
        branch: Option<&str>,
        run_id: Option<&str>,
        limit: usize,
    ) -> Result<Vec<RunHead>> {
        self.run(true, |client| {
            let limit = limit as i64;
            Ok(client
                .query(
                    "SELECT run_id, branch, skill, skill_sha256 FROM hub_skill_evidence
                     WHERE seq IN (SELECT MAX(seq) FROM hub_skill_evidence WHERE repository = $1
                                   GROUP BY run_id)
                       AND ($2::text IS NULL OR branch = $2) AND ($3::text IS NULL OR run_id = $3)
                     ORDER BY seq DESC LIMIT $4",
                    &[&repo, &branch, &run_id, &limit],
                )?
                .iter()
                .map(|row| RunHead {
                    run_id: row.get(0),
                    branch: row.get(1),
                    skill: row.get(2),
                    skill_sha256: row.get(3),
                })
                .collect())
        })
    }

    fn change_record(
        &self,
        token: &str,
        work_id: &str,
        now: i64,
    ) -> Result<Option<ChangeRecordSummaryV1>> {
        self.run(true,|client|{let mut tx=client.build_transaction().isolation_level(IsolationLevel::RepeatableRead).start()?;let who=work_who(&mut tx,token,now)?;let Some(item)=scoped_work(&mut tx,&who,work_id,now,false)? else {tx.commit()?;return Ok(None)};
            let row=tx.query_one("SELECT COUNT(*)::bigint,COALESCE(SUM(gap_count + CASE WHEN watermark > event_count THEN watermark-event_count ELSE 0 END),0)::bigint,COALESCE(COUNT(*) FILTER (WHERE lifecycle_valid AND start_sequence=1 AND end_sequence=watermark),0)::bigint FROM hub_change_sessions WHERE work=$1 AND generation=$2",&[&work_id,&item.generation])?;let count:i64=row.get(0);let gaps:i64=row.get(1);let complete:i64=row.get(2);
            let setup_observations:i64=tx.query_one("SELECT COUNT(*)::bigint FROM hub_change_events WHERE work=$1 AND generation=$2 AND event::jsonb ? 'setup'",&[&work_id,&item.generation])?.get(0);
            let setup_rows=tx.query("WITH latest AS (
                    SELECT session_id,
                           event::jsonb->'setup'->>'model' AS model,
                           event::jsonb->'setup'->>'permission_mode' AS permission_mode,
                           event::jsonb->'setup'->>'effort' AS effort,
                           ROW_NUMBER() OVER (PARTITION BY session_id ORDER BY sequence DESC) AS rank
                    FROM hub_change_events
                    WHERE work=$1 AND generation=$2 AND event::jsonb ? 'setup'
                ), picked AS (
                    SELECT latest.*, COUNT(*) OVER () AS total FROM latest WHERE rank=1
                )
                SELECT picked.session_id, s.identity::jsonb->>'parent_session_id',
                       picked.model, picked.permission_mode, picked.effort, picked.total
                FROM picked JOIN hub_change_sessions s
                  ON s.work=$1 AND s.generation=$2 AND s.session_id=picked.session_id
                ORDER BY picked.session_id COLLATE \"C\" LIMIT $3",&[&work_id,&item.generation,&(MAX_PROJECTED_SESSION_SETUPS as i64)])?;
            let setups_omitted=setup_rows.first().map_or(0,|r|r.get::<_,i64>(5) as u64).saturating_sub(setup_rows.len() as u64);
            let setups=setup_rows.iter().map(|r|SessionSetupV1{session_id:r.get(0),parent_session_id:r.get(1),model:r.get(2),permission_mode:r.get(3),effort:r.get(4)}).collect();
            let usage_observations:i64=tx.query_one("SELECT COUNT(*)::bigint FROM hub_work_model_usage WHERE work=$1 AND generation=$2",&[&work_id,&item.generation])?.get(0);
            let action=tx.query_opt("WITH scoped AS (
                    SELECT a.*,
                        COALESCE((a.start_request::jsonb->>'observed_order')::bigint,0) AS observed_order,
                        a.start_request::jsonb->>'observation_scope_id' AS observation_scope_id,
                        COALESCE(a.start_request::jsonb->>'observation_scope_id',a.action_id) AS scope_key
                    FROM hub_change_actions a WHERE a.work=$1 AND a.generation=$2
                ), scope_state AS (
                    SELECT scoped.*,
                        MIN(CASE WHEN observation_scope_id IS NOT NULL AND observed_order>0 THEN 1 ELSE 0 END)
                            OVER (PARTITION BY scope_key,session_id) AS all_ordered
                    FROM scoped
                ), ranked AS (
                    SELECT scope_state.*,
                        ROW_NUMBER() OVER (PARTITION BY scope_key,session_id ORDER BY
                            CASE WHEN all_ordered=1 THEN observed_order ELSE received_order END DESC,
                            received_order DESC) AS scope_rank
                    FROM scope_state
                )
                SELECT start_request,finish_request,source_id FROM ranked
                WHERE scope_rank=1 ORDER BY received_order DESC LIMIT 1",&[&work_id,&item.generation])?.map(|r| -> Result<StoredActionV1> {let start:String=r.get(0);let finish:Option<String>=r.get(1);Ok(StoredActionV1{start:json_value(&start)?,finish:finish.map(|value|json_value(&value)).transpose()?,source_id:r.get(2)})}).transpose()?;
            let decision=tx.query_opt("SELECT event_seq,snapshot::text FROM hub_work_versions WHERE work=$1 AND snapshot::jsonb #>> '{current,kind}'='decision' ORDER BY event_seq DESC LIMIT 1",&[&work_id])?.map(|r| -> Result<StoredDecisionV1> {let retained:WorkItem=json_value(&r.get::<_,String>(1))?;Ok(StoredDecisionV1{event_seq:r.get(0),checkpoint:retained.current.ok_or_else(||HubError::Unavailable("retained decision checkpoint is missing".into()))?})}).transpose()?;
            let summary=project_change_record(&item,SessionStatsV1{count:count as usize,gap_count:gaps as usize,complete_count:complete as usize,setup_observation_count:setup_observations as usize,usage_observation_count:usage_observations as usize,setups,setups_omitted},action,decision,now);tx.commit()?;Ok(Some(summary))})
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

    fn capture_work_for_session(
        &self,
        token: &str,
        session: &str,
        now: i64,
    ) -> Result<Option<WorkItem>> {
        super::intent::text("session", session, 255)?;
        self.run(true, |client| {
            let mut tx = client.transaction()?;
            let who = work_who(&mut tx, token, now)?;
            let item = current_capture_work_for_session(&mut tx, &who, session, now)?;
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

    fn work_catchup_preview(
        &self,
        token: &str,
        query: &CatchupPreviewQuery,
        now: i64,
    ) -> Result<CatchupPreview> {
        query.validate()?;
        self.run(false, |client| {
            let mut tx = client.transaction()?;
            let who = work_who(&mut tx, token, now)?;
            lock_work_delivery(&mut tx, &who)?;
            pg_try_backfill_catchup_publications(&mut tx, &who, now)?;
            let key = catchup_repository_key(who.repository_id);
            let repository = repository_namespace(&who.repository, who.repository_id);
            let subscriber_token =
                (query.subscriber != HUMAN_SUBSCRIBER).then(|| who.token.clone());
            let (event_ack, merge_ack) = tx
                .query_opt(
                    "SELECT event_cursor,merge_cursor FROM hub_catchup_acks WHERE owner=$1
                     AND subscriber_token IS NOT DISTINCT FROM $2 AND subscriber=$3
                     AND provider=$4 AND host=$5 AND repository=$6 AND repository_key=$7",
                    &[
                        &who.owner.id,
                        &subscriber_token,
                        &query.subscriber,
                        &who.owner.identity.provider,
                        &who.owner.identity.host,
                        &repository,
                        &key,
                    ],
                )?
                .map_or((0, 0), |row| (row.get(0), row.get(1)));
            let event_through = catchup_event_high_water(&mut tx, &who, event_ack)?;
            let merge_through = tx
                .query_opt(
                    "SELECT next_merge_seq-1 FROM hub_catchup_repositories WHERE provider=$1
                     AND host=$2 AND repository=$3 AND repository_key=$4",
                    &[
                        &who.owner.identity.provider,
                        &who.owner.identity.host,
                        &repository,
                        &key,
                    ],
                )?
                .map_or(merge_ack, |row| row.get::<_, i64>(0))
                .max(merge_ack);
            let page = pg_catchup_page(
                &mut tx,
                &who,
                &CatchupPageWindow {
                    subscriber: &query.subscriber,
                    delivery_client: "",
                    delivery: "",
                    event_from: event_ack,
                    event_through,
                    event_after: event_ack,
                    merge_from: merge_ack,
                    merge_through,
                    merge_after: merge_ack,
                    limit: CATCHUP_PREVIEW_LIMIT,
                },
            )?;
            tx.commit()?;
            Ok(CatchupPreview::from_page(page))
        })
    }

    fn work_catchup(&self, token: &str, query: &CatchupQuery, now: i64) -> Result<CatchupPage> {
        query.validate()?;
        self.run(false,|client|{let mut tx=client.transaction()?;let who=work_who(&mut tx,token,now)?;
        lock_work_delivery(&mut tx,&who)?;let key=catchup_repository_key(who.repository_id);
        let repository=repository_namespace(&who.repository,who.repository_id);
        let subscriber_token=(query.subscriber!=HUMAN_SUBSCRIBER).then(||who.token.clone());
        if let Some(delivery)=&query.delivery{
            let row=tx.query_opt("SELECT event_from,event_through,merge_from,merge_through,page_event_after,
                page_merge_after,next_event_after,next_merge_after,page FROM hub_catchup_deliveries
                WHERE id=$1 AND owner=$2 AND subscriber_token IS NOT DISTINCT FROM $3 AND subscriber=$4
                  AND provider=$5 AND host=$6 AND repository=$7 AND repository_key=$8
                  AND delivery_client=$9 AND updated_at>=$10 FOR UPDATE",
                &[delivery,&who.owner.id,&subscriber_token,&query.subscriber,&who.owner.identity.provider,
                  &who.owner.identity.host,&repository,&key,&query.delivery_client,&now.saturating_sub(86_400)])?.ok_or_else(||HubError::Invalid("unknown catch-up delivery".into()))?;
            let values=(row.get::<_,i64>(0),row.get::<_,i64>(1),row.get::<_,i64>(2),row.get::<_,i64>(3),
                row.get::<_,i64>(4),row.get::<_,i64>(5),row.get::<_,i64>(6),row.get::<_,i64>(7),row.get::<_,String>(8));
            let ea=query.event_after.unwrap();let et=query.event_through.unwrap();let ma=query.merge_after.unwrap();let mt=query.merge_through.unwrap();
            if et!=values.1||mt!=values.3{return Err(HubError::Invalid("catch-up watermark changed".into()));}
            if ea==values.4&&ma==values.5{return json_value(&values.8);}
            if ea!=values.6||ma!=values.7{return Err(HubError::Invalid("catch-up continuation skipped a page".into()));}
            let page=pg_catchup_page(&mut tx,&who,&CatchupPageWindow{
                subscriber:&query.subscriber,delivery_client:&query.delivery_client,delivery,
                event_from:values.0,event_through:values.1,event_after:ea,merge_from:values.2,
                merge_through:values.3,merge_after:ma,limit:query.limit,
            })?;
            let serialized=json_string(&page)?;check_work_delivery_page(&serialized)?;
            pg_check_catchup_delivery_capacity(&mut tx,&who,&repository,key,subscriber_token.as_deref(),
                values.8.len() as i64,serialized.len() as i64)?;
            tx.execute("UPDATE hub_catchup_deliveries SET page_event_after=$2,page_merge_after=$3,
                next_event_after=$4,next_merge_after=$5,page=$6,complete=$7,updated_at=$8 WHERE id=$1",
                &[delivery,&ea,&ma,&page.next_event_after,&page.next_merge_after,&serialized,&page.complete,&now])?;
            tx.commit()?;return Ok(page);
        }
        pg_try_backfill_catchup_publications(&mut tx,&who,now)?;
        tx.execute("DELETE FROM hub_catchup_deliveries WHERE owner=$1 AND provider=$2 AND host=$3
            AND repository=$4 AND repository_key=$5 AND updated_at<$6",
            &[&who.owner.id,&who.owner.identity.provider,&who.owner.identity.host,
              &repository,&key,&now.saturating_sub(86_400)])?;
        let cursors=tx.query_opt("SELECT event_cursor,merge_cursor FROM hub_catchup_acks WHERE owner=$1
            AND subscriber_token IS NOT DISTINCT FROM $2 AND subscriber=$3 AND provider=$4 AND host=$5
            AND repository=$6 AND repository_key=$7 FOR UPDATE",&[&who.owner.id,&subscriber_token,
            &query.subscriber,&who.owner.identity.provider,&who.owner.identity.host,&repository,&key])?
            .map_or((0,0),|r|(r.get::<_,i64>(0),r.get::<_,i64>(1)));
        tx.execute("UPDATE hub_catchup_deliveries SET superseded=TRUE,updated_at=$8
            WHERE owner=$1 AND subscriber_token IS NOT DISTINCT FROM $2 AND subscriber=$3
              AND provider=$4 AND host=$5 AND repository=$6 AND repository_key=$7
              AND NOT acknowledged AND NOT superseded AND (event_from<>$9 OR merge_from<>$10)",
            &[&who.owner.id,&subscriber_token,&query.subscriber,&who.owner.identity.provider,
              &who.owner.identity.host,&repository,&key,&now,&cursors.0,&cursors.1])?;
        let existing=tx.query_opt("SELECT id,event_from,event_through,merge_from,merge_through
            FROM hub_catchup_deliveries WHERE owner=$1 AND subscriber_token IS NOT DISTINCT FROM $2
              AND subscriber=$3 AND provider=$4 AND host=$5 AND repository=$6 AND repository_key=$7
              AND delivery_client=$8 AND NOT acknowledged AND NOT superseded
              ORDER BY created_at DESC,id DESC LIMIT 1 FOR UPDATE",
            &[&who.owner.id,&subscriber_token,&query.subscriber,&who.owner.identity.provider,
              &who.owner.identity.host,&repository,&key,&query.delivery_client])?;
        let(delivery,event_from,event_through,merge_from,merge_through,is_new)=if let Some(row)=existing{
            (row.get(0),row.get(1),row.get(2),row.get(3),row.get(4),false)
        }else{
            let active_total:i64=tx.query_one("SELECT COUNT(*) FROM hub_catchup_deliveries WHERE owner=$1 AND provider=$2
                AND host=$3 AND repository=$4 AND repository_key=$5 AND NOT acknowledged AND NOT superseded",
                &[&who.owner.id,&who.owner.identity.provider,&who.owner.identity.host,&repository,&key])?.get(0);
            let(active_class,class_limit,active_group,group_limit)=if subscriber_token.is_none(){
                let count:i64=tx.query_one("SELECT COUNT(*) FROM hub_catchup_deliveries WHERE owner=$1
                    AND provider=$2 AND host=$3 AND repository=$4 AND repository_key=$5
                    AND subscriber_token IS NULL AND NOT acknowledged AND NOT superseded",
                    &[&who.owner.id,&who.owner.identity.provider,&who.owner.identity.host,&repository,&key])?.get(0);
                (count,MAX_ACTIVE_HUMAN_DELIVERIES,active_total,MAX_ACTIVE_CATCHUP_DELIVERIES)
            }else{
                let token_count:i64=tx.query_one("SELECT COUNT(*) FROM hub_catchup_deliveries WHERE owner=$1
                    AND provider=$2 AND host=$3 AND repository=$4 AND repository_key=$5
                    AND subscriber_token=$6 AND NOT acknowledged AND NOT superseded",
                    &[&who.owner.id,&who.owner.identity.provider,&who.owner.identity.host,&repository,&key,&who.token])?.get(0);
                let agent_count:i64=tx.query_one("SELECT COUNT(*) FROM hub_catchup_deliveries WHERE owner=$1
                    AND provider=$2 AND host=$3 AND repository=$4 AND repository_key=$5
                    AND subscriber_token IS NOT NULL AND NOT acknowledged AND NOT superseded",
                    &[&who.owner.id,&who.owner.identity.provider,&who.owner.identity.host,&repository,&key])?.get(0);
                (token_count,MAX_ACTIVE_AGENT_DELIVERIES_PER_TOKEN,agent_count,MAX_ACTIVE_AGENT_DELIVERIES)
            };
            if active_class>=class_limit||active_group>=group_limit||active_total>=MAX_ACTIVE_CATCHUP_DELIVERIES{return Err(HubError::WorkCapacity);}
            let et=catchup_event_high_water(&mut tx,&who,cursors.0)?;
            let mt=tx.query_opt("SELECT next_merge_seq-1 FROM hub_catchup_repositories WHERE provider=$1
                AND host=$2 AND repository=$3 AND repository_key=$4",
                &[&who.owner.identity.provider,&who.owner.identity.host,&repository,&key])?.map_or(cursors.1,|r|r.get::<_,i64>(0)).max(cursors.1);
            (new_id("cup")?,cursors.0,et,cursors.1,mt,true)
        };
        let page=pg_catchup_page(&mut tx,&who,&CatchupPageWindow{
            subscriber:&query.subscriber,delivery_client:&query.delivery_client,delivery:&delivery,
            event_from,event_through,event_after:event_from,merge_from,merge_through,
            merge_after:merge_from,limit:query.limit,
        })?;let serialized=json_string(&page)?;check_work_delivery_page(&serialized)?;
        let previous_bytes=if is_new{0}else{tx.query_one("SELECT octet_length(page)::bigint FROM hub_catchup_deliveries WHERE id=$1",
            &[&delivery])?.get(0)};
        pg_check_catchup_delivery_capacity(&mut tx,&who,&repository,key,subscriber_token.as_deref(),previous_bytes,serialized.len() as i64)?;
        if is_new{
            tx.execute("INSERT INTO hub_catchup_deliveries
            (id,owner,subscriber_token,subscriber,provider,host,repository,repository_key,repository_id,
             event_from,event_through,merge_from,merge_through,page_event_after,page_merge_after,
             next_event_after,next_merge_after,page,complete,created_at,updated_at,delivery_client)
            VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13,$10,$12,$14,$15,$16,$17,$18,$18,$19)",
            &[&delivery,&who.owner.id,&subscriber_token,&query.subscriber,&who.owner.identity.provider,
              &who.owner.identity.host,&repository,&key,&who.repository_id,&event_from,&event_through,
              &merge_from,&merge_through,&page.next_event_after,&page.next_merge_after,&serialized,&page.complete,&now,
              &query.delivery_client])?;
        }else{tx.execute("UPDATE hub_catchup_deliveries SET page_event_after=$2,page_merge_after=$3,
            next_event_after=$4,next_merge_after=$5,page=$6,complete=$7,updated_at=$8 WHERE id=$1",
            &[&delivery,&event_from,&merge_from,&page.next_event_after,&page.next_merge_after,&serialized,&page.complete,&now])?;}
        tx.execute("DELETE FROM hub_catchup_deliveries WHERE owner=$1 AND subscriber_token IS NOT DISTINCT FROM $2
            AND subscriber=$3 AND delivery_client=$4 AND provider=$5 AND host=$6 AND repository=$7
            AND repository_key=$8 AND (acknowledged OR superseded) AND id NOT IN
              (SELECT id FROM hub_catchup_deliveries WHERE owner=$1 AND subscriber_token IS NOT DISTINCT FROM $2
               AND subscriber=$3 AND delivery_client=$4 AND provider=$5 AND host=$6 AND repository=$7
               AND repository_key=$8 AND (acknowledged OR superseded)
               ORDER BY updated_at DESC,id DESC LIMIT 8)",
            &[&who.owner.id,&subscriber_token,&query.subscriber,&query.delivery_client,
              &who.owner.identity.provider,&who.owner.identity.host,&repository,&key])?;
        tx.commit()?;Ok(page)})
    }

    fn work_catchup_ack(
        &self,
        token: &str,
        ack: &CatchupAck,
        now: i64,
    ) -> Result<CatchupAckResult> {
        ack.validate()?;
        self.run(false,|client|{let mut tx=client.transaction()?;let who=work_who(&mut tx,token,now)?;
        lock_work_delivery(&mut tx,&who)?;let key=catchup_repository_key(who.repository_id);
        let repository=repository_namespace(&who.repository,who.repository_id);
        let row=tx.query_opt("SELECT subscriber_token,subscriber,event_from,event_through,merge_from,merge_through,acknowledged
            FROM hub_catchup_deliveries WHERE id=$1 AND owner=$2 AND provider=$3 AND host=$4 AND repository=$5
              AND repository_key=$6 AND complete AND (subscriber_token IS NULL OR subscriber_token=$7)
              AND updated_at>=$8 FOR UPDATE",
            &[&ack.delivery,&who.owner.id,&who.owner.identity.provider,&who.owner.identity.host,&repository,&key,&who.token,&now.saturating_sub(86_400)])?
            .ok_or_else(||HubError::Invalid("catch-up delivery is unknown or incomplete".into()))?;
        let subscriber_token:Option<String>=row.get(0);let subscriber:String=row.get(1);let event_from:i64=row.get(2);
        let event_through:i64=row.get(3);let merge_from:i64=row.get(4);let merge_through:i64=row.get(5);let acknowledged:bool=row.get(6);
        if ack.expected_event_from!=event_from||ack.expected_merge_from!=merge_from{return Err(HubError::Invalid("catch-up acknowledgement does not match delivery".into()));}
        let current=tx.query_opt("SELECT event_cursor,merge_cursor FROM hub_catchup_acks WHERE owner=$1
            AND subscriber_token IS NOT DISTINCT FROM $2 AND subscriber=$3 AND provider=$4 AND host=$5
            AND repository=$6 AND repository_key=$7 FOR UPDATE",&[&who.owner.id,&subscriber_token,&subscriber,
            &who.owner.identity.provider,&who.owner.identity.host,&repository,&key])?.map_or((0,0),|r|(r.get(0),r.get(1)));
        let status=if acknowledged{CatchupAckStatus::AlreadyAcknowledged}else if current!=(event_from,merge_from){CatchupAckStatus::Conflict}else{
            tx.execute("INSERT INTO hub_catchup_acks(owner,subscriber_token,subscriber,provider,host,repository,
                repository_key,repository_id,event_cursor,merge_cursor,updated_at) VALUES($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11)
                ON CONFLICT(owner,subscriber_token,subscriber,provider,host,repository,repository_key)
                DO UPDATE SET event_cursor=EXCLUDED.event_cursor,merge_cursor=EXCLUDED.merge_cursor,updated_at=EXCLUDED.updated_at",
                &[&who.owner.id,&subscriber_token,&subscriber,&who.owner.identity.provider,&who.owner.identity.host,
                  &repository,&key,&who.repository_id,&event_through,&merge_through,&now])?;
            tx.execute("UPDATE hub_catchup_deliveries SET acknowledged=TRUE,ack_actor=$2,updated_at=$3 WHERE id=$1",&[&ack.delivery,&who.token,&now])?;
            CatchupAckStatus::Advanced};
        let current=if status==CatchupAckStatus::Advanced{(event_through,merge_through)}else{current};tx.commit()?;
        Ok(CatchupAckResult{status,event_through,merge_through,current_event_ack:current.0,current_merge_ack:current.1})})
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
            let catchup_repository=repository_namespace(&scope.repository,scope.repository_id);
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
            let active_catchup_deliveries: i64 = tx
                .query_one(
                    "SELECT COUNT(*) FROM hub_catchup_deliveries
                     WHERE provider=$1 AND host=$2 AND repository=$3 AND repository_key=COALESCE($4::bigint,-1)
                       AND NOT acknowledged AND NOT superseded AND updated_at>=$5",
                    &[&scope.provider,&scope.host,&catchup_repository,&scope.repository_id,
                        &now.saturating_sub(86_400)],
                )?
                .get(0);
            if active_catchup_deliveries > 0 {
                return Err(HubError::Invalid(
                    "work compaction is blocked by an unacknowledged catch-up delivery".into(),
                ));
            }
            let(current_merge_floor,merge_highwater)=tx.query_opt(
                "SELECT merge_floor_seq,next_merge_seq-1 FROM hub_catchup_repositories
                 WHERE provider=$1 AND host=$2 AND repository=$3 AND repository_key=COALESCE($4::bigint,-1)",
                &[&scope.provider,&scope.host,&catchup_repository,&scope.repository_id])?
                .map_or((0,0),|r|(r.get::<_,i64>(0),r.get::<_,i64>(1)));
            let first_retained:i64=tx.query_one(
                "SELECT COALESCE(MIN(seq),$5::bigint+1)::bigint FROM hub_catchup_merges
                 WHERE provider=$1 AND host=$2 AND repository=$3 AND repository_key=COALESCE($4::bigint,-1)
                   AND observed_at>=$6",
                &[&scope.provider,&scope.host,&catchup_repository,&scope.repository_id,&merge_highwater,&before])?.get(0);
            let merge_retention_floor=(first_retained-1).max(current_merge_floor).min(merge_highwater);
            let merge_observations:i64=tx.query_one(
                "SELECT COUNT(*)::bigint FROM hub_catchup_merges WHERE provider=$1 AND host=$2
                 AND repository=$3 AND repository_key=COALESCE($4::bigint,-1) AND seq>$5 AND seq<=$6",
                &[&scope.provider,&scope.host,&catchup_repository,&scope.repository_id,
                  &current_merge_floor,&merge_retention_floor])?.get(0);
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
                if merge_retention_floor>current_merge_floor {
                    tx.execute("UPDATE hub_catchup_repositories SET merge_floor_seq=$5,updated_at=$6
                        WHERE provider=$1 AND host=$2 AND repository=$3 AND repository_key=COALESCE($4::bigint,-1)",
                        &[&scope.provider,&scope.host,&catchup_repository,&scope.repository_id,
                          &merge_retention_floor,&now])?;
                    tx.execute("DELETE FROM hub_catchup_merges WHERE provider=$1 AND host=$2
                        AND repository=$3 AND repository_key=COALESCE($4::bigint,-1) AND seq<=$5",
                        &[&scope.provider,&scope.host,&catchup_repository,&scope.repository_id,&merge_retention_floor])?;
                }
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
                deleted_merge_observations: merge_observations as u64,
                merge_retention_floor,
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

    fn promise_create(
        &self,
        workspace_id: &str,
        actor_user: &str,
        request: &PromiseCreateV1,
        admissions: &[PromiseRepositoryAdmissionV1],
        now: i64,
    ) -> Result<PromiseReceiptV1> {
        request.validate()?;
        let fingerprint = operation_fingerprint("create", workspace_id, actor_user, request)?;
        self.run(false, |client| {
            let mut tx = client.transaction()?;
            promise_operation_lock(&mut tx, workspace_id, &request.operation_id)?;
            promise_actor_active(&mut tx, actor_user, None, now)?;
            if let Some(response) = promise_replay(
                &mut tx,
                workspace_id,
                &request.operation_id,
                actor_user,
                None,
                &fingerprint,
            )? {
                let mut receipt: PromiseReceiptV1 = serde_json::from_str(&response)
                    .map_err(|_| HubError::Unavailable("invalid promise receipt".into()))?;
                promise_receipt_matches(&mut tx,&receipt,admissions)?;
                receipt.replayed = true;
                tx.commit()?;
                return Ok(receipt);
            }
            ensure_admissions_digest(workspace_id, admissions, &request.expected_admissions_digest)?;
            let promise_id = new_id("prm")?;
            let version = PromiseVersionV1::initial(
                workspace_id,
                &promise_id,
                actor_user,
                None,
                request.definition.clone(),
                admissions.to_vec(),
                now,
            )?;
            let lifetime = encode_promise(&extend_lifetime(&[], &version.repository_admissions)?)?;
            tx.execute(
                "INSERT INTO hub_promises
                 (id,workspace_id,steward_user,current_version,current_digest,lifetime_scope,state,created_at)
                 VALUES($1,$2,$3,1,$4,($5::text)::jsonb,'active',$6)",
                &[
                    &promise_id,
                    &workspace_id,
                    &actor_user,
                    &version.reference.digest,
                    &lifetime,
                    &now,
                ],
            )?;
            promise_insert_version(&mut tx, &version)?;
            let receipt = PromiseReceiptV1 {
                reference: version.reference.clone(),
                replayed: false,
            };
            let response = encode_promise(&receipt)?;
            promise_record_operation(
                &mut tx,
                workspace_id,
                &request.operation_id,
                actor_user,
                None,
                &fingerprint,
                &response,
                now,
            )?;
            append(
                &mut tx,
                &Event::new(
                    "promise_created",
                    &promise_id,
                    json!({"workspace_id":workspace_id,"version":1,"digest":version.reference.digest,"actor":actor_user}),
                )?,
            )?;
            tx.commit()?;
            Ok(receipt)
        })
    }

    fn promise_adopt(
        &self,
        workspace_id: &str,
        actor_user: &str,
        request: &PromiseAdoptV1,
        admissions: &[PromiseRepositoryAdmissionV1],
        now: i64,
    ) -> Result<PromiseReceiptV1> {
        request.validate()?;
        let fingerprint = operation_fingerprint("adopt", workspace_id, actor_user, request)?;
        self.run(false, |client| {
            let mut tx = client.transaction()?;
            promise_operation_lock(&mut tx, workspace_id, &request.operation_id)?;
            promise_actor_active(&mut tx, actor_user, None, now)?;
            if let Some(response) = promise_replay(
                &mut tx,
                workspace_id,
                &request.operation_id,
                actor_user,
                None,
                &fingerprint,
            )? {
                let mut receipt: PromiseReceiptV1 = serde_json::from_str(&response)
                    .map_err(|_| HubError::Unavailable("invalid promise receipt".into()))?;
                promise_receipt_matches(&mut tx,&receipt,admissions)?;
                receipt.replayed = true;
                tx.commit()?;
                return Ok(receipt);
            }
            ensure_admissions_digest(workspace_id, admissions, &request.expected_admissions_digest)?;
            let proposal = tx.query_opt(
                "SELECT repository_id,admission FROM hub_promise_inputs
                 WHERE id=$1 AND workspace_id=$2 AND kind='proposal' AND adopted_promise IS NULL
                 FOR UPDATE",
                &[&request.proposal_id, &workspace_id],
            )?;
            let proposal = proposal.ok_or_else(|| HubError::Invalid("promise_not_found".into()))?;
            let proposal_repository: String = proposal.get(0);
            if !request
                .definition
                .repositories
                .iter()
                .any(|repository| repository.repository_id == proposal_repository)
            {
                return Err(HubError::Invalid("promise_not_found".into()));
            }
            let proposal_admission: String = proposal.get(1);
            if admissions.iter().find(|item| item.repository_id == proposal_repository)
                .map(encode_promise).transpose()?.as_deref() != Some(proposal_admission.as_str()) {
                return Err(HubError::Invalid("promise_not_found".into()));
            }
            let promise_id = new_id("prm")?;
            let version = PromiseVersionV1::initial(
                workspace_id,
                &promise_id,
                actor_user,
                Some(request.proposal_id.clone()),
                request.definition.clone(),
                admissions.to_vec(),
                now,
            )?;
            let lifetime = encode_promise(&extend_lifetime(&[], &version.repository_admissions)?)?;
            tx.execute(
                "INSERT INTO hub_promises
                 (id,workspace_id,steward_user,current_version,current_digest,lifetime_scope,state,created_at)
                 VALUES($1,$2,$3,1,$4,($5::text)::jsonb,'active',$6)",
                &[
                    &promise_id,
                    &workspace_id,
                    &actor_user,
                    &version.reference.digest,
                    &lifetime,
                    &now,
                ],
            )?;
            promise_insert_version(&mut tx, &version)?;
            if tx.execute(
                "UPDATE hub_promise_inputs SET adopted_promise=$2
                 WHERE id=$1 AND adopted_promise IS NULL",
                &[&request.proposal_id, &promise_id],
            )? != 1
            {
                return Err(HubError::Invalid("promise_conflict".into()));
            }
            let receipt = PromiseReceiptV1 {
                reference: version.reference.clone(),
                replayed: false,
            };
            let response = encode_promise(&receipt)?;
            promise_record_operation(
                &mut tx,
                workspace_id,
                &request.operation_id,
                actor_user,
                None,
                &fingerprint,
                &response,
                now,
            )?;
            append(
                &mut tx,
                &Event::new(
                    "promise_adopted",
                    &promise_id,
                    json!({"workspace_id":workspace_id,"proposal_id":request.proposal_id,"digest":version.reference.digest,"actor":actor_user}),
                )?,
            )?;
            tx.commit()?;
            Ok(receipt)
        })
    }

    fn promise_change(
        &self,
        workspace_id: &str,
        actor_user: &str,
        promise_id: &str,
        authority: PromiseChangeAuthority,
        request: &PromiseChangeV1,
        admissions: &[PromiseRepositoryAdmissionV1],
        current_map: &PromiseCurrentAdmissionsV1,
        now: i64,
    ) -> Result<PromiseReceiptV1> {
        request.validate()?;
        let fingerprint =
            operation_fingerprint("change", workspace_id, actor_user, &(promise_id, request))?;
        self.run(false, |client| {
            let mut tx = client.transaction()?;
            promise_operation_lock(&mut tx, workspace_id, &request.operation_id)?;
            promise_actor_active(&mut tx, actor_user, None, now)?;
            if let Some(response) = promise_replay(
                &mut tx,
                workspace_id,
                &request.operation_id,
                actor_user,
                None,
                &fingerprint,
            )? {
                let mut receipt: PromiseReceiptV1 = serde_json::from_str(&response)
                    .map_err(|_| HubError::Unavailable("invalid promise receipt".into()))?;
                promise_receipt_matches(&mut tx,&receipt,admissions)?;
                receipt.replayed = true;
                tx.commit()?;
                return Ok(receipt);
            }
            let current = promise_current(&mut tx, workspace_id, promise_id, true)?
                .ok_or_else(|| HubError::Invalid("promise_not_found".into()))?;
            if !admissions_current(&current.repository_admissions,current_map) {
                return Err(HubError::Invalid("promise_not_found".into()));
            }
            if authority == PromiseChangeAuthority::Steward && current.steward_user != actor_user {
                return Err(HubError::Invalid("promise_forbidden".into()));
            }
            if current.reference.version != request.expected_version
                || current.reference.digest != request.expected_digest
            {
                return Err(HubError::Invalid("promise_conflict".into()));
            }
            let version = current.next(actor_user, authority, request, admissions.to_vec(), now)?;
            let retained: String = tx.query_one(
                "SELECT lifetime_scope::text FROM hub_promises WHERE workspace_id=$1 AND id=$2",
                &[&workspace_id, &promise_id],
            )?.get(0);
            let retained = decode_lifetime_scope(&retained)
                .ok_or_else(|| HubError::Unavailable("invalid promise lifetime scope".into()))?;
            let lifetime = encode_promise(&extend_lifetime(&retained, &version.repository_admissions)?)?;
            promise_insert_version(&mut tx, &version)?;
            let old_version = i64::try_from(current.reference.version).unwrap();
            let new_version = i64::try_from(version.reference.version).unwrap();
            if tx.execute(
                "UPDATE hub_promises SET steward_user=$4,current_version=$5,current_digest=$6,state=$7,lifetime_scope=($9::text)::jsonb
                 WHERE workspace_id=$1 AND id=$2 AND current_version=$3 AND current_digest=$8",
                &[
                    &workspace_id,
                    &promise_id,
                    &old_version,
                    &version.steward_user,
                    &new_version,
                    &version.reference.digest,
                    &promise_state(version.state),
                    &current.reference.digest,
                    &lifetime,
                ],
            )? != 1
            {
                return Err(HubError::Invalid("promise_conflict".into()));
            }
            let receipt = PromiseReceiptV1 {
                reference: version.reference.clone(),
                replayed: false,
            };
            let response = encode_promise(&receipt)?;
            promise_record_operation(
                &mut tx,
                workspace_id,
                &request.operation_id,
                actor_user,
                None,
                &fingerprint,
                &response,
                now,
            )?;
            append(
                &mut tx,
                &Event::new(
                    "promise_changed",
                    promise_id,
                    json!({"workspace_id":workspace_id,"version":version.reference.version,"digest":version.reference.digest,"kind":promise_kind(version.kind),"actor":actor_user}),
                )?,
            )?;
            tx.commit()?;
            Ok(receipt)
        })
    }

    fn promise_change_replay(
        &self,
        workspace_id: &str,
        actor_user: &str,
        promise_id: &str,
        request: &PromiseChangeV1,
        current: &PromiseCurrentAdmissionsV1,
        now: i64,
    ) -> Result<Option<PromiseReceiptV1>> {
        request.validate()?;
        let fingerprint =
            operation_fingerprint("change", workspace_id, actor_user, &(promise_id, request))?;
        self.run(false, |client| {
            let mut tx = client.transaction()?;
            promise_operation_lock(&mut tx, workspace_id, &request.operation_id)?;
            promise_actor_active(&mut tx, actor_user, None, now)?;
            let response = promise_replay(
                &mut tx,
                workspace_id,
                &request.operation_id,
                actor_user,
                None,
                &fingerprint,
            )?;
            let receipt = response
                .map(|value| {
                    let mut receipt: PromiseReceiptV1 = serde_json::from_str(&value)
                        .map_err(|_| HubError::Unavailable("invalid promise receipt".into()))?;
                    if receipt.reference.promise_id != promise_id
                        || receipt.reference.workspace_id != workspace_id
                    {
                        return Err(HubError::Unavailable(
                            "invalid promise receipt target".into(),
                        ));
                    }
                    let version =
                        promise_version_at(&mut tx, &receipt.reference)?.ok_or_else(|| {
                            HubError::Unavailable("retained promise replay version missing".into())
                        })?;
                    if !admissions_current(&version.repository_admissions, current) {
                        return Err(HubError::Invalid("promise_not_found".into()));
                    }
                    receipt.replayed = true;
                    Ok(receipt)
                })
                .transpose()?;
            tx.commit()?;
            Ok(receipt)
        })
    }

    fn promise_input(
        &self,
        workspace_id: &str,
        actor_user: &str,
        actor_token: Option<&str>,
        request: &PromiseInputV1,
        admission: &PromiseRepositoryAdmissionV1,
        now: i64,
    ) -> Result<PromiseInputReceiptV1> {
        request.validate()?;
        admission.validate()?;
        if admission.repository_id != request.repository_id() {
            return Err(HubError::Invalid(
                "promise input admission differs from scope".into(),
            ));
        }
        let fingerprint_actor = actor_token.unwrap_or(actor_user);
        let fingerprint =
            operation_fingerprint(request.kind(), workspace_id, fingerprint_actor, request)?;
        self.run(false, |client| {
            let mut tx = client.transaction()?;
            promise_operation_lock(&mut tx, workspace_id, request.operation_id())?;
            promise_actor_active(&mut tx, actor_user, actor_token, now)?;
            if let Some(response) = promise_replay(
                &mut tx,
                workspace_id,
                request.operation_id(),
                actor_user,
                actor_token,
                &fingerprint,
            )? {
                let mut receipt: PromiseInputReceiptV1 = serde_json::from_str(&response)
                    .map_err(|_| HubError::Unavailable("invalid promise input receipt".into()))?;
                let retained = tx.query_opt("SELECT admission FROM hub_promise_inputs WHERE id=$1 AND workspace_id=$2",&[&receipt.input_id,&workspace_id])?
                    .ok_or_else(|| HubError::Unavailable("retained promise input missing".into()))?;
                let retained: PromiseRepositoryAdmissionV1 = serde_json::from_str(&retained.get::<_,String>(0))
                    .map_err(|_| HubError::Unavailable("invalid retained input admission".into()))?;
                if retained != *admission { return Err(HubError::Invalid("promise_not_found".into())); }
                receipt.replayed = true;
                tx.commit()?;
                return Ok(receipt);
            }
            let (promise_id, promise_version, promise_digest) = match request {
                PromiseInputV1::Proposal { .. } => (None, None, None),
                PromiseInputV1::Report {
                    promise_id,
                    promise_version,
                    promise_digest,
                    repository_id,
                    ..
                } => {
                    let current = promise_current(&mut tx, workspace_id, promise_id, true)?
                        .ok_or_else(|| HubError::Invalid("promise_not_found".into()))?;
                    if current.reference.version != *promise_version
                        || current.reference.digest != *promise_digest
                        || !current
                            .definition
                            .repositories
                            .iter()
                            .any(|repository| repository.repository_id == *repository_id)
                    {
                        return Err(HubError::Invalid("promise_conflict".into()));
                    }
                    if !current.repository_admissions.contains(admission) {
                        return Err(HubError::Invalid("promise_not_found".into()));
                    }
                    (
                        Some(promise_id.as_str()),
                        Some(i64::try_from(*promise_version).unwrap()),
                        Some(promise_digest.as_str()),
                    )
                }
            };
            let input_id = new_id(if matches!(request, PromiseInputV1::Proposal { .. }) {
                "prp"
            } else {
                "prr"
            })?;
            let payload = encode_promise(request)?;
            tx.execute(
                "INSERT INTO hub_promise_inputs
                 (id,workspace_id,kind,repository_id,admission,actor_user,actor_token,promise_id,promise_version,promise_digest,payload,adopted_promise,created_at)
                 VALUES($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,NULL,$12)",
                &[
                    &input_id,
                    &workspace_id,
                    &request.kind(),
                    &request.repository_id(),
                    &encode_promise(admission)?,
                    &actor_user,
                    &actor_token,
                    &promise_id,
                    &promise_version,
                    &promise_digest,
                    &payload,
                    &now,
                ],
            )?;
            let receipt = PromiseInputReceiptV1 {
                input_id: input_id.clone(),
                replayed: false,
            };
            let response = encode_promise(&receipt)?;
            promise_record_operation(
                &mut tx,
                workspace_id,
                request.operation_id(),
                actor_user,
                actor_token,
                &fingerprint,
                &response,
                now,
            )?;
            append(
                &mut tx,
                &Event::new(
                    "promise_input_recorded",
                    &input_id,
                    json!({"workspace_id":workspace_id,"kind":request.kind(),"repository_id":request.repository_id(),"actor":actor_user}),
                )?,
            )?;
            tx.commit()?;
            Ok(receipt)
        })
    }

    fn promise_get(
        &self,
        workspace_id: &str,
        promise_id: &str,
        permitted_repositories: Option<&[String]>,
        current: &PromiseCurrentAdmissionsV1,
    ) -> Result<Option<PromiseViewV1>> {
        if permitted_repositories.is_some_and(<[String]>::is_empty) {
            return Ok(None);
        }
        let repositories = permitted_repositories.map(<[String]>::to_vec);
        let current_json = encode_promise(current)?;
        self.run(true, |client| {
            let row = client.query_opt(
                "SELECT p.current_version,p.current_digest,v.payload,v.current_scope::text
                 FROM hub_promises p JOIN hub_promise_versions v
                   ON v.promise_id=p.id AND v.version=p.current_version
                 WHERE p.workspace_id=$1 AND p.id=$2
                   AND jsonb_typeof(v.current_scope)='array'
                   AND jsonb_array_length(v.current_scope) BETWEEN 1 AND 8
                   AND ($3::text[] IS NULL OR NOT EXISTS(
                     SELECT 1 FROM jsonb_array_elements(v.current_scope) AS s(value)
                     WHERE jsonb_typeof(s.value)<>'object'
                        OR jsonb_typeof(s.value->'repository_id') IS DISTINCT FROM 'string'
                        OR NOT (s.value->>'repository_id'=ANY($3))))
                   AND NOT EXISTS (SELECT 1 FROM jsonb_array_elements(v.current_scope) AS s(value)
                     WHERE jsonb_typeof(s.value)<>'object'
                        OR jsonb_typeof(s.value->'repository_id') IS DISTINCT FROM 'string'
                        OR s.value<>COALESCE(($4::text)::jsonb->(s.value->>'repository_id'),
                         jsonb_build_object('repository_id',s.value->>'repository_id','forge',NULL,
                                            'native_repository_ids','[]'::jsonb)))",
                &[&workspace_id, &promise_id, &repositories, &current_json],
            )?;
            let Some(row) = row else {
                return Ok(None);
            };
            let encoded_scope: String = row.get(3);
            if !encoded_admissions_authorized(&encoded_scope, permitted_repositories, current) {
                return Ok(None);
            }
            let payload: String = row.get(2);
            if !payload_admissions_authorized(
                &payload,
                workspace_id,
                promise_id,
                row.get(0),
                permitted_repositories,
                current,
            ) {
                return Ok(None);
            }
            let scope = decode_current_scope(&encoded_scope)
                .ok_or_else(|| HubError::Unavailable("invalid promise scope".into()))?;
            let decoded = decode_promise_current_row(
                workspace_id,
                promise_id,
                row.get(0),
                &row.get::<_, String>(1),
                &payload,
            )?;
            if !admissions_current(&decoded.repository_admissions, current)
                || permitted_repositories.is_some_and(|grants| {
                    decoded
                        .definition
                        .repositories
                        .iter()
                        .any(|repository| !grants.contains(&repository.repository_id))
                })
            {
                return Ok(None);
            }
            if scope != decoded.repository_admissions || !promise_index_exact(client, &decoded)? {
                return Err(HubError::Unavailable(
                    "promise current index mismatch".into(),
                ));
            }
            Ok(Some(PromiseViewV1 { current: decoded }))
        })
    }

    fn promise_input_get(
        &self,
        workspace_id: &str,
        input_id: &str,
        permitted_repositories: Option<&[String]>,
        current: &PromiseCurrentAdmissionsV1,
    ) -> Result<Option<PromiseInputViewV1>> {
        if !valid_promise_input_id(input_id) {
            return Err(HubError::Invalid("invalid promise input id".into()));
        }
        self.run(true, |client| {
            let mut tx = client
                .build_transaction()
                .isolation_level(postgres::IsolationLevel::RepeatableRead)
                .read_only(true)
                .start()?;
            promise_input_visible(
                &mut tx,
                workspace_id,
                input_id,
                permitted_repositories,
                current,
            )
        })
    }

    fn promise_inputs(
        &self,
        workspace_id: &str,
        scope: PromiseInputScope<'_>,
        permitted_repositories: Option<&[String]>,
        after: Option<&str>,
        limit: usize,
        current: &PromiseCurrentAdmissionsV1,
    ) -> Result<PromiseInputPageV1> {
        if !(1..=super::promise::MAX_PROMISE_PAGE).contains(&limit)
            || after.is_some_and(|id| !valid_promise_input_id(id))
        {
            return Err(HubError::Invalid("invalid promise input page".into()));
        }
        let (kind, scope_id) = match scope {
            PromiseInputScope::Proposal(repository_id) => ("proposal", repository_id),
            PromiseInputScope::Report(promise_id) => ("report", promise_id),
        };
        self.run(true, |client| {
            let mut tx = client.build_transaction()
                .isolation_level(postgres::IsolationLevel::RepeatableRead)
                .read_only(true)
                .start()?;
            let scope = if kind == "report" {
                let Some(scope) = promise_lifetime_authorized(
                    &mut tx, workspace_id, scope_id, permitted_repositories, current,
                )? else {
                    return Err(HubError::Invalid("promise_not_found".into()));
                };
                if !promise_report_parent_authorized(
                    &mut tx, workspace_id, scope_id, permitted_repositories, current,
                )? {
                    return Err(HubError::Invalid("promise_not_found".into()));
                }
                scope
            } else if permitted_repositories.is_some_and(|grants| !grants.iter().any(|key| key == scope_id)) {
                return Ok(PromiseInputPageV1 { inputs: vec![], next: None });
            } else { vec![PromiseRepositoryAdmissionV1::unbound(scope_id)] };
            let scope_column = if kind == "proposal" { "repository_id" } else { "promise_id" };
            if let Some(cursor) = after {
                let cursor_exists: bool = tx.query_one(
                    &format!("SELECT EXISTS(SELECT 1 FROM hub_promise_inputs WHERE workspace_id=$1 AND kind=$2 AND {scope_column}=$3 AND id=$4)"),
                    &[&workspace_id, &kind, &scope_id, &cursor],
                )?.get(0);
                if !cursor_exists || promise_input_visible(
                    &mut tx, workspace_id, cursor, permitted_repositories, current,
                )?.is_none() {
                    return Err(HubError::Invalid("invalid promise input cursor".into()));
                }
            }
            if kind == "report" && promise_current(&mut tx, workspace_id, scope_id, false)?.is_none() {
                return Err(HubError::Invalid("promise_not_found".into()));
            }
            let encoded_by_key = scope.iter().map(|admission| {
                let key = admission.repository_id.clone();
                encode_promise(&current_admission(current, &key)).map(|encoded| (key, encoded))
            }).collect::<Result<BTreeMap<_, _>>>()?;
            let current_json = encode_promise(&encoded_by_key)?;
            let row_limit = i64::try_from(limit + 1).unwrap();
            let rows = tx.query(
                &format!("SELECT i.id FROM hub_promise_inputs i
                    WHERE i.workspace_id=$1 AND i.kind=$2 AND i.{scope_column}=$3
                      AND ($4='' OR (i.created_at,i.id)>(SELECT created_at,id FROM hub_promise_inputs WHERE id=$4))
                      AND i.admission=(($5::text)::jsonb->>i.repository_id)
                    ORDER BY i.created_at,i.id LIMIT $6"),
                &[&workspace_id, &kind, &scope_id, &after.unwrap_or(""), &current_json, &row_limit],
            )?;
            let mut inputs = Vec::with_capacity(rows.len());
            for row in rows {
                let id: String = row.get(0);
                let Some(input) = promise_input_visible(
                    &mut tx, workspace_id, &id, permitted_repositories, current,
                )? else {
                    return Err(HubError::Invalid("promise_not_found".into()));
                };
                inputs.push(input);
            }
            let next = if inputs.len() > limit {
                inputs.pop();
                inputs.last().map(|input| input.input_id.clone())
            } else { None };
            Ok(PromiseInputPageV1 { inputs, next })
        })
    }

    fn promise_history(
        &self,
        workspace_id: &str,
        promise_id: &str,
        permitted_repositories: Option<&[String]>,
        before_version: Option<u64>,
        limit: usize,
        current: &PromiseCurrentAdmissionsV1,
    ) -> Result<Option<PromiseHistoryPageV1>> {
        if !(1..=super::promise::MAX_PROMISE_PAGE).contains(&limit) || before_version == Some(0) {
            return Err(HubError::Invalid("invalid promise page limit".into()));
        }
        self.run(true, |client| {
            let mut tx = client
                .build_transaction()
                .isolation_level(postgres::IsolationLevel::RepeatableRead)
                .read_only(true)
                .start()?;
            if promise_lifetime_authorized(
                &mut tx,
                workspace_id,
                promise_id,
                permitted_repositories,
                current,
            )?
            .is_none()
            {
                return if before_version.is_some() {
                    Err(HubError::Invalid("invalid promise history cursor".into()))
                } else {
                    Ok(None)
                };
            }
            if let Some(cursor) = before_version {
                promise_version_visible(
                    &mut tx,
                    workspace_id,
                    promise_id,
                    i64::try_from(cursor).unwrap_or(-1),
                    permitted_repositories,
                    current,
                )?
                .ok_or_else(|| HubError::Invalid("invalid promise history cursor".into()))?;
            }
            let before = before_version
                .map(|value| i64::try_from(value).unwrap_or(i64::MAX))
                .unwrap_or(i64::MAX);
            let row_limit = i64::try_from(limit + 1).unwrap();
            let rows = tx.query(
                "SELECT version FROM hub_promise_versions
                 WHERE promise_id=$1 AND version<$2 ORDER BY version DESC LIMIT $3",
                &[&promise_id, &before, &row_limit],
            )?;
            let mut versions = Vec::with_capacity(rows.len());
            for row in rows {
                let Some(version) = promise_version_visible(
                    &mut tx,
                    workspace_id,
                    promise_id,
                    row.get(0),
                    permitted_repositories,
                    current,
                )?
                else {
                    return Ok(None);
                };
                versions.push(version);
            }
            let next = if versions.len() > limit {
                versions.pop();
                versions.last().map(|version| version.reference.version)
            } else {
                None
            };
            Ok(Some(PromiseHistoryPageV1 { versions, next }))
        })
    }

    fn promise_list(
        &self,
        workspace_id: &str,
        permitted_repositories: Option<&[String]>,
        after: Option<&str>,
        limit: usize,
        current: &PromiseCurrentAdmissionsV1,
    ) -> Result<PromisePageV1> {
        if !(1..=super::promise::MAX_PROMISE_PAGE).contains(&limit)
            || after.is_some_and(|value| !valid_promise_id(value))
        {
            return Err(HubError::Invalid("invalid promise page".into()));
        }
        if permitted_repositories.is_some_and(|repositories| repositories.is_empty()) {
            return Ok(PromisePageV1 {
                promises: vec![],
                next: None,
            });
        }
        let repositories = permitted_repositories.map(<[String]>::to_vec);
        let current_json = encode_promise(current)?;
        self.run(true, |client| {
            let mut tx = client
                .build_transaction()
                .isolation_level(postgres::IsolationLevel::RepeatableRead)
                .read_only(true)
                .start()?;
            if let Some(cursor) = after {
                let row = tx.query_opt(
                    "SELECT p.current_version,p.current_digest,v.payload,v.current_scope::text
                     FROM hub_promises p JOIN hub_promise_versions v
                       ON v.promise_id=p.id AND v.version=p.current_version
                     WHERE p.workspace_id=$1 AND p.id=$2
                       AND jsonb_typeof(v.current_scope)='array'
                       AND jsonb_array_length(v.current_scope) BETWEEN 1 AND 8
                       AND ($3::text[] IS NULL OR NOT EXISTS(
                         SELECT 1 FROM jsonb_array_elements(v.current_scope) AS s(value)
                         WHERE jsonb_typeof(s.value)<>'object'
                            OR jsonb_typeof(s.value->'repository_id') IS DISTINCT FROM 'string'
                            OR NOT (s.value->>'repository_id'=ANY($3))))
                       AND NOT EXISTS (SELECT 1 FROM jsonb_array_elements(v.current_scope) AS s(value)
                         WHERE jsonb_typeof(s.value)<>'object'
                            OR jsonb_typeof(s.value->'repository_id') IS DISTINCT FROM 'string'
                            OR s.value<>COALESCE(($4::text)::jsonb->(s.value->>'repository_id'),
                             jsonb_build_object('repository_id',s.value->>'repository_id','forge',NULL,
                                                'native_repository_ids','[]'::jsonb)))",
                    &[&workspace_id, &cursor, &repositories, &current_json],
                )?;
                let row = row.ok_or_else(|| HubError::Invalid("invalid promise cursor".into()))?;
                let encoded_scope: String = row.get(3);
                if !encoded_admissions_authorized(&encoded_scope, permitted_repositories, current) {
                    return Err(HubError::Invalid("invalid promise cursor".into()));
                }
                let payload: String = row.get(2);
                if !payload_admissions_authorized(
                    &payload,
                    workspace_id,
                    cursor,
                    row.get(0),
                    permitted_repositories,
                    current,
                ) {
                    return Err(HubError::Invalid("invalid promise cursor".into()));
                }
                let scope = decode_current_scope(&encoded_scope)
                    .ok_or_else(|| HubError::Unavailable("invalid promise cursor scope".into()))?;
                let decoded = decode_promise_current_row(
                    workspace_id,
                    cursor,
                    row.get(0),
                    &row.get::<_, String>(1),
                    &payload,
                )?;
                if !admissions_current(&decoded.repository_admissions, current)
                    || permitted_repositories.is_some_and(|grants| {
                        decoded.definition.repositories.iter().any(|repository| !grants.contains(&repository.repository_id))
                    })
                {
                    return Err(HubError::Invalid("invalid promise cursor".into()));
                }
                if scope != decoded.repository_admissions || !promise_index_exact(&mut tx, &decoded)? {
                    return Err(HubError::Unavailable("promise cursor index mismatch".into()));
                }
            }
            let mut scan_after = after.unwrap_or("").to_owned();
            let source = if repositories.is_some() {
                "WITH candidate AS MATERIALIZED (
                    SELECT DISTINCT r.promise_id,r.version
                    FROM hub_promise_version_repositories r
                    WHERE r.workspace_id=$1 AND r.repository_id=ANY($3::text[])
                      AND r.promise_id>$2
                 )
                 SELECT p.id,p.current_version,p.current_digest,v.payload,v.current_scope::text FROM candidate c
                 JOIN hub_promises p ON p.id=c.promise_id AND p.current_version=c.version
                 JOIN hub_promise_versions v ON v.promise_id=p.id AND v.version=p.current_version"
            } else {
                "SELECT p.id,p.current_version,p.current_digest,v.payload,v.current_scope::text FROM hub_promises p JOIN hub_promise_versions v
                 ON v.promise_id=p.id AND v.version=p.current_version"
            };
            let sql = format!("{source}
                 WHERE p.workspace_id=$1 AND p.id>$2
                   AND jsonb_typeof(v.current_scope)='array'
                   AND jsonb_array_length(v.current_scope) BETWEEN 1 AND 8
                   AND ($3::text[] IS NULL OR NOT EXISTS(
                     SELECT 1 FROM jsonb_array_elements(v.current_scope) AS s(value)
                     WHERE jsonb_typeof(s.value)<>'object'
                        OR jsonb_typeof(s.value->'repository_id') IS DISTINCT FROM 'string'
                        OR NOT (s.value->>'repository_id'=ANY($3))))
                   AND NOT EXISTS (SELECT 1 FROM jsonb_array_elements(v.current_scope) AS s(value)
                     WHERE jsonb_typeof(s.value)<>'object'
                        OR jsonb_typeof(s.value->'repository_id') IS DISTINCT FROM 'string'
                        OR s.value<>COALESCE(($4::text)::jsonb->(s.value->>'repository_id'),jsonb_build_object('repository_id',s.value->>'repository_id','forge',NULL,'native_repository_ids','[]'::jsonb)))
                 ORDER BY p.id LIMIT $5");
            let mut promises = Vec::new();
            while promises.len() <= limit {
                let row_limit = i64::try_from(limit + 1 - promises.len()).unwrap();
                let rows = tx.query(
                    &sql,
                    &[&workspace_id, &scan_after, &repositories, &current_json, &row_limit],
                )?;
                if rows.is_empty() {
                    break;
                }
                let exhausted = rows.len() < row_limit as usize;
                for row in rows {
                    let id: String = row.get(0);
                    scan_after = id.clone();
                    let current_version: i64 = row.get(1);
                    let current_digest: String = row.get(2);
                    let encoded_scope: String = row.get(4);
                    if !encoded_admissions_authorized(&encoded_scope, permitted_repositories, current) {
                        continue;
                    }
                    let payload: String = row.get(3);
                    if !payload_admissions_authorized(
                        &payload,
                        workspace_id,
                        &id,
                        current_version,
                        permitted_repositories,
                        current,
                    ) {
                        continue;
                    }
                    let scope = decode_current_scope(&encoded_scope)
                        .ok_or_else(|| HubError::Unavailable("invalid promise scope".into()))?;
                    let version = decode_version(&payload)?;
                    if !admissions_current(&version.repository_admissions, current)
                        || permitted_repositories.is_some_and(|grants| version.definition.repositories.iter().any(|repository| !grants.contains(&repository.repository_id)))
                    {
                        continue;
                    }
                    if version.reference.workspace_id != workspace_id
                        || version.reference.promise_id != id
                        || version.reference.version != u64::try_from(current_version).unwrap_or_default()
                        || version.reference.digest != current_digest
                    {
                        return Err(HubError::Unavailable("promise current pointer mismatch".into()));
                    }
                    if scope != version.repository_admissions || !promise_index_exact(&mut tx, &version)? {
                        return Err(HubError::Unavailable("promise version repository index mismatch".into()));
                    }
                    promises.push(PromiseViewV1 { current: version });
                }
                if exhausted {
                    break;
                }
            }
            let next = if promises.len() > limit {
                promises.pop();
                promises
                    .last()
                    .map(|view| view.current.reference.promise_id.clone())
            } else {
                None
            };
            Ok(PromisePageV1 { promises, next })
        })
    }

    fn latest_event_seq(&self) -> Result<i64> {
        self.run(true, |client| {
            Ok(client
                .query_one("SELECT COALESCE(MAX(seq), 0) FROM hub_events", &[])?
                .get(0))
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
