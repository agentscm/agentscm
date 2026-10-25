use std::collections::BTreeSet;
use std::path::Path;
use std::sync::{Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use rusqlite::{Connection, OptionalExtension, ToSql, Transaction, TransactionBehavior, params};
use serde_json::json;

use crate::change_record::{
    ActionFinishV1, ActionMutationReceiptV1, ActionStartV1, ArtifactRefV1, CaptureBatchV1,
    CapturedEventKindV1, CapturedEventV1, ChangeRecordSummaryV1, ConversationEntryV1,
    IngestReceiptV1, MAX_CONVERSATION_PAGE_ENTRIES, MAX_PROJECTED_SESSION_SETUPS, RetentionClassV1,
    SessionSetupV1, parse_conversation_cursor,
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
    Create, MAX_UNFINISHED, NewPublication, PUBLICATION_SELECT, Publication, PublicationSearch,
    SEARCH_SCAN_ROWS, Update,
};
use super::session::{
    Checked, DEFAULT_SESSION, NOTICES_PER_TOUCH, Row, SESSION_TTL_SECONDS, STALE_SESSION_SECONDS,
    SessionCall, Touched, check, check_name, expiry_cause, guard, held, plan as plan_touch,
};
use super::skills::{EvidenceRow, RunHead, SkillAdoption};
use super::store::{
    ACTIVITY_KINDS, ActivityEvent, AgentToken, Event, ForgeIdentity, HubEvent, LIST_LIMIT,
    NAMED_MIGRATIONS, NewAgentToken, Principal, SessionView, Store, TokenDigest, User, event_limit,
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
use super::{HubError, Result};

const MAX_CHANGE_EVENTS_PER_SESSION: i64 = 100_000;
const MAX_CHANGE_ACTIONS_PER_WORK: i64 = 10_000;
const MAX_CHANGE_OPERATIONS_PER_WORK: i64 = 120_000;

// shortcut: one serialized connection, no pool; upgrade when hub load needs concurrent DB work
pub(super) struct SqliteStore {
    conn: Mutex<Connection>,
}

impl SqliteStore {
    /// Creates the file (mode 0600 on unix) if absent; the parent directory must exist.
    pub(super) fn open(path: &Path) -> Result<Self> {
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            // SQLite gives the -wal and -shm files the database file's mode.
            std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .mode(0o600)
                .open(path)?;
        }
        let mut conn = Connection::open(path)?;
        conn.busy_timeout(Duration::from_secs(5))?;
        enable_wal(&conn)?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        migrate(&mut conn)?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    fn conn(&self) -> MutexGuard<'_, Connection> {
        self.conn.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn set_disabled(&self, actor: Option<&str>, user: &str, disabled: bool) -> Result<()> {
        let mut conn = self.conn();
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let changed = tx.execute(
            "UPDATE hub_users SET disabled = ?2 WHERE id = ?1 AND disabled <> ?2",
            params![user, disabled],
        )?;
        if changed > 0 {
            append(&tx, &Event::user_toggled(user, disabled, actor)?)?;
        } else if tx
            .query_row("SELECT 1 FROM hub_users WHERE id = ?1", [user], |_| Ok(()))
            .optional()?
            .is_none()
        {
            return Err(HubError::Invalid("unknown user".into()));
        }
        Ok(tx.commit()?)
    }

    fn repository_query(
        &self,
        tail: &str,
        args: &[&dyn ToSql],
    ) -> Result<Vec<ConnectedRepository>> {
        let conn = self.conn();
        let mut statement = conn.prepare(&format!("{REPOSITORY_SELECT} {tail}"))?;
        let rows = statement
            .query_map(args, repository_row)?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    fn revoke(&self, owner: Option<&str>, token: &str) -> Result<bool> {
        let mut conn = self.conn();
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let changed = tx.execute(
            "UPDATE hub_agent_tokens SET revoked = 1
             WHERE id = ?1 AND revoked = 0 AND (?2 IS NULL OR owner = ?2)",
            params![token, owner],
        )?;
        if changed > 0 {
            append(&tx, &Event::token_revoked(token, owner)?)?;
        }
        tx.commit()?;
        Ok(changed > 0)
    }
}

/// The busy timeout does not cover the exclusive lock that switching to WAL
/// needs, so hubs starting together on a new file retry briefly.
fn enable_wal(conn: &Connection) -> Result<()> {
    for _ in 0..50 {
        match conn.pragma_update(None, "journal_mode", "WAL") {
            Err(rusqlite::Error::SqliteFailure(error, _))
                if error.code == rusqlite::ErrorCode::DatabaseBusy =>
            {
                std::thread::sleep(Duration::from_millis(100));
            }
            result => return Ok(result?),
        }
    }
    Err(HubError::Unavailable("database is locked".into()))
}

fn migrate(conn: &mut Connection) -> Result<()> {
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    tx.execute_batch(
        "CREATE TABLE IF NOT EXISTS hub_schema_migrations (
            version INTEGER PRIMARY KEY, applied_at INTEGER NOT NULL)",
    )?;
    let applied = tx
        .prepare("SELECT version FROM hub_schema_migrations")?
        .query_map([], |row| row.get(0))?
        .collect::<std::result::Result<Vec<i64>, _>>()?;
    for migration in pending(&applied)? {
        tx.execute_batch(migration.sqlite)?;
        tx.execute(
            "INSERT INTO hub_schema_migrations (version, applied_at) VALUES (?1, ?2)",
            params![migration.version, unix_now()],
        )?;
    }
    tx.execute_batch(
        "CREATE TABLE IF NOT EXISTS hub_named_migrations (
            name TEXT PRIMARY KEY, applied_at INTEGER NOT NULL)",
    )?;
    let applied_names: Vec<String> = tx
        .prepare("SELECT name FROM hub_named_migrations")?
        .query_map([], |row| row.get(0))?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    let applied_refs: Vec<&str> = applied_names.iter().map(String::as_str).collect();
    for migration in plan_named(NAMED_MIGRATIONS, &applied_refs)? {
        tx.execute_batch(migration.sqlite)?;
        tx.execute(
            "INSERT INTO hub_named_migrations (name, applied_at) VALUES (?1, ?2)",
            params![migration.name, unix_now()],
        )?;
    }
    Ok(tx.commit()?)
}

fn append(tx: &Transaction<'_>, event: &Event) -> Result<i64> {
    tx.prepare_cached(
        "INSERT INTO hub_events (id, at, kind, subject, data) VALUES (?1, ?2, ?3, ?4, ?5)",
    )?
    .execute(params![
        event.id,
        event.at,
        event.kind,
        event.subject,
        event.data
    ])?;
    Ok(tx.last_insert_rowid())
}

fn work_who(tx: &Transaction<'_>, token: &str, now: i64) -> Result<WorkWho> {
    tx.query_row(
        "SELECT t.repository, t.repository_id, t.name, t.read_only,
                u.id, u.provider, u.host, u.forge_user_id, u.login
         FROM hub_agent_tokens t JOIN hub_users u ON u.id = t.owner
         WHERE t.id = ?1 AND t.revoked = 0 AND t.expires_at > ?2 AND u.disabled = 0",
        params![token, now],
        |row| {
            Ok(WorkWho {
                token: token.to_owned(),
                repository: row.get(0)?,
                repository_id: row.get(1)?,
                agent: row.get(2)?,
                read_only: row.get(3)?,
                owner: User {
                    id: row.get(4)?,
                    identity: ForgeIdentity {
                        provider: row.get(5)?,
                        host: row.get(6)?,
                        user_id: row.get(7)?,
                    },
                    login: row.get(8)?,
                    disabled: false,
                },
            })
        },
    )
    .map_err(|error| match error {
        rusqlite::Error::QueryReturnedNoRows => HubError::Unauthorized,
        error => error.into(),
    })
}

const SHARED_BODY_COLUMNS: &str = "work,digest,size_bytes,session_id,generation,event_id,sequence,
    declared_token,declared_at,state,uploaded_at,expires_at,deleted_at,deleted_by,physical_deleted_at,
    author,delete_attempts";
/// Where a query that appends columns after `SHARED_BODY_COLUMNS` finds them.
const SHARED_BODY_COLUMN_COUNT: usize = 17;

fn shared_body_row(row: &rusqlite::Row<'_>) -> Result<SharedBodyRow> {
    Ok(SharedBodyRow {
        work: row.get(0)?,
        digest: row.get(1)?,
        size_bytes: row.get::<_, i64>(2)? as u64,
        session_id: row.get(3)?,
        generation: row.get(4)?,
        event_id: row.get(5)?,
        sequence: row.get::<_, i64>(6)? as u64,
        declared_token: row.get(7)?,
        declared_at: row.get(8)?,
        state: SharedBodyState::parse(&row.get::<_, String>(9)?)?,
        uploaded_at: row.get(10)?,
        expires_at: row.get(11)?,
        deleted_at: row.get(12)?,
        deleted_by: row.get(13)?,
        physical_deleted_at: row.get(14)?,
        author: row.get(15)?,
        delete_attempts: row.get(16)?,
    })
}

/// Records that an accepted event named a shared body for the human owning `token`. Each human's
/// declaration of a digest is its own row, and a later one must agree on size with the same
/// human's earlier one (never another's, which would leak that they declared it).
fn declare_shared_body(
    tx: &Transaction<'_>,
    batch: &CaptureBatchV1,
    token: &str,
    event: &CapturedEventV1,
    artifact: &ArtifactRefV1,
    now: i64,
) -> Result<()> {
    let size = artifact.size_bytes as i64;
    let declared: Option<i64> = tx
        .query_row(
            "SELECT size_bytes FROM hub_change_shared_bodies WHERE work=?1 AND digest=?2
               AND author=(SELECT owner FROM hub_agent_tokens WHERE id=?3)",
            params![batch.work_id, artifact.digest, token],
            |row| row.get(0),
        )
        .optional()?;
    if declared.is_some_and(|declared| declared != size) {
        return Err(HubError::Invalid(
            "shared body digest was already declared with a different size".into(),
        ));
    }
    tx.execute(
        "INSERT INTO hub_change_shared_bodies
         (work,digest,author,size_bytes,media,session_id,generation,event_id,sequence,
          declared_token,declared_at,state)
         VALUES (?1,?2,(SELECT owner FROM hub_agent_tokens WHERE id=?8),?3,'json',?4,?5,?6,?7,
                 ?8,?9,'declared')
         ON CONFLICT (work,digest,author) DO NOTHING",
        params![
            batch.work_id,
            artifact.digest,
            size,
            batch.session.session_id,
            batch.generation,
            event.event_id,
            event.sequence as i64,
            token,
            now
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
    let value = json_string(value)?;
    Ok(value.trim_matches('"').to_owned())
}

fn sqlite_usage_summary(
    tx: &Transaction<'_>,
    work: &str,
    group_limit: usize,
) -> Result<UsageSummary> {
    let (
        observations,
        input_tokens,
        output_tokens,
        input_known,
        output_known,
        missing_cost,
        failed,
    ): (i64, Option<i64>, Option<i64>, i64, i64, i64, i64) = tx.query_row(
        "SELECT COUNT(*), SUM(input_tokens), SUM(output_tokens),
                COUNT(input_tokens), COUNT(output_tokens),
                COALESCE(SUM(CASE WHEN cost_amount_micros IS NULL THEN 1 ELSE 0 END),0),
                COALESCE(SUM(CASE WHEN outcome='failed' THEN 1 ELSE 0 END),0)
           FROM hub_work_model_usage WHERE work=?1
             AND COALESCE(json_extract(request,'$.measurement_scope'),'request')<>'session_cumulative'",
        [work],
        |r| {
            Ok((
                r.get(0)?,
                r.get(1)?,
                r.get(2)?,
                r.get(3)?,
                r.get(4)?,
                r.get(5)?,
                r.get(6)?,
            ))
        },
    )?;
    let mut statement = tx.prepare(
        "SELECT agent,model_provider,model,usage_basis,COUNT(*),SUM(input_tokens),SUM(output_tokens),COUNT(input_tokens),COUNT(output_tokens)
           FROM hub_work_model_usage WHERE work=?1
             AND COALESCE(json_extract(request,'$.measurement_scope'),'request')<>'session_cumulative'
          GROUP BY agent,model_provider,model,usage_basis ORDER BY agent,model_provider,model,usage_basis LIMIT ?2")?;
    let mut token_groups = statement
        .query_map(params![work, (group_limit + 1) as i64], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, String>(3)?,
                r.get::<_, i64>(4)?,
                r.get::<_, Option<i64>>(5)?,
                r.get::<_, Option<i64>>(6)?,
                r.get::<_, i64>(7)?,
                r.get::<_, i64>(8)?,
            ))
        })?
        .map(|row| {
            let (agent, provider, model, basis, n, input, output, ik, ok) = row?;
            Ok(TokenGroup {
                agent,
                provider,
                model,
                usage_basis: serde_json::from_str(&format!("\"{basis}\""))
                    .map_err(|e| rusqlite::Error::ToSqlConversionFailure(Box::new(e)))?,
                observations: n as usize,
                input_tokens: input,
                output_tokens: output,
                input_known_observations: ik as usize,
                output_known_observations: ok as usize,
            })
        })
        .collect::<std::result::Result<Vec<_>, rusqlite::Error>>()?;
    let mut statement = tx.prepare(
        "SELECT cost_kind,cost_currency,cost_basis,usage_basis,agent,model_provider,model,SUM(cost_amount_micros),COUNT(*)
           FROM hub_work_model_usage WHERE work=?1 AND cost_amount_micros IS NOT NULL
             AND COALESCE(json_extract(request,'$.measurement_scope'),'request')<>'session_cumulative'
          GROUP BY cost_kind,cost_currency,cost_basis,usage_basis,agent,model_provider,model
          ORDER BY agent,model_provider,model,usage_basis,cost_kind,cost_currency,cost_basis LIMIT ?2")?;
    let mut cost_groups = statement
        .query_map(params![work, (group_limit + 1) as i64], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, String>(3)?,
                r.get::<_, String>(4)?,
                r.get::<_, String>(5)?,
                r.get::<_, String>(6)?,
                r.get::<_, i64>(7)?,
                r.get::<_, i64>(8)?,
            ))
        })?
        .map(|row| {
            let (kind, currency, basis, usage_basis, agent, provider, model, amount, n) = row?;
            Ok(CostGroup {
                kind: serde_json::from_str(&format!("\"{kind}\""))
                    .map_err(|e| rusqlite::Error::ToSqlConversionFailure(Box::new(e)))?,
                currency,
                basis,
                usage_basis: serde_json::from_str(&format!("\"{usage_basis}\""))
                    .map_err(|e| rusqlite::Error::ToSqlConversionFailure(Box::new(e)))?,
                agent,
                provider,
                model,
                amount_micros: amount,
                observations: n as usize,
            })
        })
        .collect::<std::result::Result<Vec<_>, rusqlite::Error>>()?;
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
        input_tokens,
        output_tokens,
        input_known_observations: input_known as usize,
        output_known_observations: output_known as usize,
        missing_cost_observations: missing_cost as usize,
        reported_failed_attempts: failed as usize,
        groups_complete,
        cost_groups,
        token_groups,
    })
}

fn sqlite_insight_input(
    tx: &Transaction<'_>,
    mut item: WorkItem,
    now: i64,
    group_limit: usize,
) -> Result<InsightInput> {
    let mut statement=tx.prepare("SELECT event_seq,json_extract(snapshot,'$.updated_at','$.state','$.generation','$.last_checkin_at','$.current.source_refs') FROM hub_work_versions WHERE work=?1 ORDER BY event_seq LIMIT 257")?;
    let rows = statement
        .query_map([&item.id], |r| {
            Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?))
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    let mut history = Vec::with_capacity(rows.len().min(MAX_HISTORY));
    let mut projections = Vec::with_capacity(rows.len());
    for (_, projection) in &rows {
        projections.push(json_value::<(
            i64,
            WorkState,
            i64,
            i64,
            Option<Vec<SourceRef>>,
        )>(projection)?);
    }
    for ((seq, _), (at, state, generation, last_checkin_at, _)) in
        rows.iter().zip(&projections).take(MAX_HISTORY)
    {
        history.push(HistoryPoint {
            seq: *seq,
            at: *at,
            state: *state,
            generation: *generation,
            last_checkin_at: *last_checkin_at,
        });
    }
    let floors:(i64,i64)=tx.query_row("SELECT COALESCE(event_floor_seq,0),COALESCE(snapshot_floor_seq,0) FROM hub_work_retention WHERE provider=(SELECT provider FROM hub_work_items WHERE id=?1) AND host=(SELECT host FROM hub_work_items WHERE id=?1) AND repository=(SELECT repository FROM hub_work_items WHERE id=?1) AND repository_id IS (SELECT repository_id FROM hub_work_items WHERE id=?1)",[&item.id],|r|Ok((r.get(0)?,r.get(1)?))).optional()?.unwrap_or((0,0));
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
    'history_commits: for (_, _, _, _, refs) in projections.iter().take(MAX_HISTORY) {
        for source in refs.iter().flatten() {
            if let SourceRef::Commit { oid, .. } = source
                && super::insights::valid_oid(oid)
                && seen_commits.insert(oid.clone())
            {
                checkpoint_commits.push(oid.clone());
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
    let mut usage_commits = tx.prepare("SELECT source_commit FROM hub_work_model_usage WHERE work=?1 AND source_commit IS NOT NULL GROUP BY source_commit ORDER BY MIN(received_at),source_commit LIMIT 17")?;
    for oid in usage_commits.query_map([&item.id], |r| r.get::<_, String>(0))? {
        let oid = oid?;
        if !commits.contains(&oid) {
            commits.push(oid);
        }
    }
    let commits_truncated = checkpoint_overflow || commits.len() > MAX_PUBLICATIONS;
    commits.truncate(MAX_PUBLICATIONS);
    let (provider, host, repository_id): (String, String, Option<i64>) = tx.query_row(
        "SELECT provider,host,repository_id FROM hub_work_items WHERE id=?1",
        [&item.id],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
    )?;
    let mut publications = Vec::new();
    let mut publications_complete = !commits_truncated;
    if let Some(repository_id) = repository_id {
        let connected: Option<String> = tx.query_row("SELECT r.id FROM hub_repositories r JOIN hub_connections c ON c.id=r.connection WHERE r.host=?1 AND r.repository_id=?2 AND c.provider=?3", params![host,repository_id,provider], |r| r.get(0)).optional()?;
        let connected = connected.unwrap_or_default();
        {
            for commit in &commits {
                let remaining = MAX_PUBLICATIONS.saturating_sub(publications.len());
                if remaining == 0 {
                    publications_complete = false;
                    break;
                }
                let mut s=tx.prepare("SELECT p.id,p.branch_state,p.pr_number,p.merge_commit,p.updated_at FROM hub_publications p JOIN hub_agent_tokens t ON t.id=p.token JOIN hub_users u ON u.id=t.owner WHERE p.commit_sha=?1 AND p.repository=?2 AND t.repository=?2 AND t.repository_id=?3 AND u.provider=?4 AND u.host=?5 ORDER BY p.updated_at,p.id LIMIT ?6")?;
                let found = s
                    .query_map(
                        params![
                            commit,
                            item.repository,
                            repository_id,
                            provider,
                            host,
                            (remaining + 1) as i64
                        ],
                        |r| {
                            Ok((
                                r.get::<_, String>(0)?,
                                r.get::<_, String>(1)?,
                                r.get::<_, Option<i64>>(2)?,
                                r.get::<_, Option<String>>(3)?,
                                r.get::<_, i64>(4)?,
                            ))
                        },
                    )?
                    .collect::<std::result::Result<Vec<_>, _>>()?;
                if found.len() > remaining {
                    publications_complete = false;
                }
                for (id, branch_state, pr_number, merge_commit, observed_at) in
                    found.into_iter().take(remaining)
                {
                    if pr_number.is_some() && connected.is_empty() {
                        publications_complete = false;
                    }
                    let mut checks = if let Some(number) =
                        pr_number.filter(|_| !connected.is_empty())
                    {
                        let mut cs=tx.prepare("SELECT head_sha,state,phase,binding_digest,updated_at FROM hub_forge_checks WHERE repository=?1 AND number=?2 AND head_sha=?3 ORDER BY context LIMIT 17")?;
                        cs.query_map(params![connected, number, commit], |r| {
                            Ok(RecordedCheck {
                                head_sha: r.get(0)?,
                                state: r.get(1)?,
                                phase: r.get(2)?,
                                binding_digest: r.get(3)?,
                                observed_at: r.get(4)?,
                            })
                        })?
                        .collect::<std::result::Result<Vec<_>, _>>()?
                    } else {
                        Vec::new()
                    };
                    if checks.len() > 16 {
                        publications_complete = false;
                        checks.truncate(16);
                    }
                    publications.push(PublicationEvidence {
                        publication_id: id,
                        commit: commit.clone(),
                        branch_state,
                        pr_number,
                        merge_commit,
                        observed_at,
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
        }
    } else {
        publications_complete = false;
    }
    project_claim(tx, &mut item, now)?;
    Ok(InsightInput {
        usage: sqlite_usage_summary(tx, &item.id, group_limit)?,
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

fn project_claim(tx: &Transaction<'_>, item: &mut WorkItem, now: i64) -> Result<()> {
    if now < item.last_checkin_at {
        item.claim_status = ClaimStatus::Unknown;
        return Ok(());
    }
    let active = tx
        .query_row(
            "SELECT 1 FROM hub_agent_tokens t JOIN hub_users u ON u.id = t.owner
             WHERE t.id = ?1 AND t.revoked = 0 AND t.expires_at > ?2 AND u.disabled = 0",
            params![item.lead_token, now],
            |_| Ok(()),
        )
        .optional()?
        .is_some();
    item.claim_status = if active && item.last_checkin_at >= now - CLAIM_STALE_SECONDS {
        ClaimStatus::Active
    } else {
        ClaimStatus::Stale
    };
    Ok(())
}

fn scoped_work(
    tx: &Transaction<'_>,
    who: &WorkWho,
    id: &str,
    now: i64,
) -> Result<Option<WorkItem>> {
    let snapshot: Option<String> = tx
        .query_row(
            "SELECT snapshot FROM hub_work_items
             WHERE id = ?1 AND provider = ?2 AND host = ?3 AND repository = ?4
               AND repository_id IS ?5",
            params![
                id,
                who.owner.identity.provider,
                who.owner.identity.host,
                who.repository,
                who.repository_id
            ],
            |row| row.get(0),
        )
        .optional()?;
    snapshot
        .map(|snapshot| {
            let mut item: WorkItem = json_value(&snapshot)?;
            project_claim(tx, &mut item, now)?;
            Ok(item)
        })
        .transpose()
}

fn relation_targets(tx: &Transaction<'_>, who: &WorkWho, ids: &[String]) -> Result<()> {
    for id in ids {
        if scoped_work(tx, who, id, 0)?.is_none() {
            return Err(HubError::Invalid(format!(
                "related work {id} is outside the repository or unknown"
            )));
        }
    }
    Ok(())
}

fn work_scope_overlaps(
    tx: &Transaction<'_>,
    who: &WorkWho,
    scope: &[String],
    exclude: Option<&str>,
    now: i64,
) -> Result<Vec<WorkScopeOverlap>> {
    if scope.is_empty() {
        return Ok(Vec::new());
    }
    let mut statement = tx.prepare(
        "SELECT snapshot FROM hub_work_items
         WHERE provider = ?1 AND host = ?2 AND repository = ?3 AND repository_id IS ?4
           AND state <> 'finished' AND (?5 IS NULL OR id <> ?5)
         ORDER BY id LIMIT 101",
    )?;
    let snapshots = statement
        .query_map(
            params![
                who.owner.identity.provider,
                who.owner.identity.host,
                who.repository,
                who.repository_id,
                exclude
            ],
            |row| row.get::<_, String>(0),
        )?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    if snapshots.len() > 100 {
        return Err(HubError::WorkOverlapIncomplete);
    }
    let mut found = Vec::new();
    for snapshot in snapshots {
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

fn save_work_version(tx: &Transaction<'_>, item: &WorkItem, seq: i64) -> Result<()> {
    let snapshot = json_string(item)?;
    tx.execute(
        "UPDATE hub_work_items SET lead_token = ?2, lead_agent = ?3, lead_session = ?4,
             generation = ?5, state = ?6, next_action = ?7, current_checkpoint = ?8,
             snapshot = ?9, updated_at = ?10, updated_seq = ?11, scope = ?12,
             branch = ?13, worktree = ?14 WHERE id = ?1",
        params![
            item.id,
            item.lead_token,
            item.lead_agent,
            item.lead_session,
            item.generation,
            state_name(item.state),
            item.next_action,
            item.current.as_ref().map(json_string).transpose()?,
            snapshot,
            item.updated_at,
            seq,
            json_string(&item.scope)?,
            item.branch,
            item.worktree
        ],
    )?;
    tx.execute(
        "INSERT INTO hub_work_versions (work, event_seq, state, snapshot)
         VALUES (?1, ?2, ?3, ?4)",
        params![item.id, seq, state_name(item.state), json_string(item)?],
    )?;
    Ok(())
}

fn work_payload_bytes(
    tx: &Transaction<'_>,
    provider: &str,
    host: &str,
    repository: &str,
    repository_id: Option<i64>,
) -> Result<i64> {
    Ok(tx.query_row(
        "SELECT COALESCE(SUM(bytes), 0) FROM (
             SELECT length(CAST(snapshot AS BLOB)) + 256 AS bytes FROM hub_work_items
              WHERE provider = ?1 AND host = ?2 AND repository = ?3 AND repository_id IS ?4
             UNION ALL
             SELECT length(CAST(v.snapshot AS BLOB)) + 256 FROM hub_work_versions v
              JOIN hub_work_items w ON w.id = v.work
              WHERE w.provider = ?1 AND w.host = ?2 AND w.repository = ?3
                AND w.repository_id IS ?4
             UNION ALL
             SELECT length(CAST(o.request AS BLOB)) + 256 FROM hub_work_checkpoint_ops o
              JOIN hub_work_items w ON w.id = o.work
              WHERE w.provider = ?1 AND w.host = ?2 AND w.repository = ?3
                AND w.repository_id IS ?4
             UNION ALL
             SELECT length(CAST(u.request AS BLOB)) + 256 FROM hub_work_model_usage u
              JOIN hub_work_items w ON w.id = u.work
              WHERE w.provider = ?1 AND w.host = ?2 AND w.repository = ?3
                AND w.repository_id IS ?4
             UNION ALL
             SELECT CASE WHEN acknowledged = 0 THEN 4194304
                         ELSE length(CAST(page AS BLOB)) + 256 END
               FROM hub_work_deliveries
              WHERE provider = ?1 AND host = ?2 AND repository = ?3 AND repository_id IS ?4
             UNION ALL
             SELECT length(CAST(e.data AS BLOB)) + length(CAST(e.kind AS BLOB))
                    + length(CAST(e.subject AS BLOB)) + 256
               FROM hub_events e JOIN hub_work_items w ON w.id = e.subject
              WHERE w.provider = ?1 AND w.host = ?2 AND w.repository = ?3
                AND w.repository_id IS ?4
                AND e.kind IN ('work_started', 'work_joined', 'work_takeover', 'work_checkpoint')
             UNION ALL
             SELECT length(CAST(r.proposed_scope AS BLOB))
                    + length(CAST(r.proposed_title AS BLOB)) + 256
               FROM hub_coordination_requests r
              WHERE r.provider = ?1 AND r.host = ?2 AND r.repository = ?3
                AND r.repository_id IS ?4
             UNION ALL
             SELECT length(CAST(t.reasons AS BLOB)) + 256
               FROM hub_coordination_targets t
               JOIN hub_coordination_requests r ON r.id = t.request
              WHERE r.provider = ?1 AND r.host = ?2 AND r.repository = ?3
                AND r.repository_id IS ?4
             UNION ALL
             SELECT length(CAST(COALESCE(s.note, '') AS BLOB)) + 256
               FROM hub_coordination_responses s
               JOIN hub_coordination_requests r ON r.id = s.request
              WHERE r.provider = ?1 AND r.host = ?2 AND r.repository = ?3
                AND r.repository_id IS ?4
             UNION ALL
             SELECT length(CAST(o.payload AS BLOB)) + 256
               FROM hub_coordination_operations o
               JOIN hub_coordination_requests r ON r.id = o.request
              WHERE r.provider = ?1 AND r.host = ?2 AND r.repository = ?3
                AND r.repository_id IS ?4
             UNION ALL
             SELECT 256 FROM hub_coordination_acknowledgements a
               JOIN hub_coordination_requests r ON r.id = a.request
              WHERE r.provider = ?1 AND r.host = ?2 AND r.repository = ?3
                AND r.repository_id IS ?4
             UNION ALL
             SELECT 256 FROM hub_coordination_versions v
               JOIN hub_coordination_requests r ON r.id = v.request
              WHERE r.provider = ?1 AND r.host = ?2 AND r.repository = ?3
                AND r.repository_id IS ?4
             UNION ALL
             SELECT length(CAST(e.data AS BLOB)) + length(CAST(e.kind AS BLOB))
                    + length(CAST(e.subject AS BLOB)) + 256
               FROM hub_events e JOIN hub_coordination_requests r ON r.id = e.subject
              WHERE r.provider = ?1 AND r.host = ?2 AND r.repository = ?3
                AND r.repository_id IS ?4
         )",
        params![provider, host, repository, repository_id],
        |row| row.get(0),
    )?)
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

fn work_event_high_water(tx: &Transaction<'_>, who: &WorkWho, minimum: i64) -> Result<i64> {
    Ok(tx.query_row(
        "SELECT MAX(value) FROM (
             SELECT COALESCE(MAX(seq),0) AS value FROM hub_events
             UNION ALL SELECT ?5
             UNION ALL SELECT event_floor_seq FROM hub_work_retention
               WHERE provider=?1 AND host=?2 AND repository=?3 AND repository_id IS ?4
             UNION ALL SELECT snapshot_floor_seq FROM hub_work_retention
               WHERE provider=?1 AND host=?2 AND repository=?3 AND repository_id IS ?4
             UNION ALL SELECT event_floor_seq FROM hub_coordination_retention
               WHERE provider=?1 AND host=?2 AND repository=?3 AND repository_id IS ?4)",
        params![
            who.owner.identity.provider,
            who.owner.identity.host,
            who.repository,
            who.repository_id,
            minimum
        ],
        |row| row.get(0),
    )?)
}

fn work_transition_payload_bytes(snapshot: &str, event: &Event) -> Result<i64> {
    let snapshot = work_payload_row_bytes(snapshot.len())?;
    let event = work_event_payload_bytes(event)?;
    snapshot
        .checked_mul(2)
        .and_then(|bytes| bytes.checked_add(event))
        .ok_or_else(|| HubError::Invalid("work payload size is too large".into()))
}

fn stored_work_item_bytes(tx: &Transaction<'_>, id: &str) -> Result<i64> {
    Ok(tx.query_row(
        "SELECT length(CAST(snapshot AS BLOB)) + 256 FROM hub_work_items WHERE id = ?1",
        [id],
        |row| row.get(0),
    )?)
}

fn adjust_work_payload_capacity(
    tx: &Transaction<'_>,
    who: &WorkWho,
    delta: i64,
    now: i64,
) -> Result<()> {
    let used: Option<i64> = tx
        .query_row(
            "SELECT charged_bytes FROM hub_work_usage
             WHERE provider = ?1 AND host = ?2 AND repository = ?3 AND repository_id IS ?4",
            params![
                who.owner.identity.provider,
                who.owner.identity.host,
                who.repository,
                who.repository_id
            ],
            |row| row.get(0),
        )
        .optional()?;
    let used = used.unwrap_or(0);
    check_work_payload_capacity(used, delta, WORK_PAYLOAD_CAPACITY_BYTES)?;
    let charged = used
        .checked_add(delta)
        .filter(|charged| *charged >= 0)
        .ok_or_else(|| HubError::Unavailable("work payload accounting is inconsistent".into()))?;
    let changed = tx.execute(
        "UPDATE hub_work_usage SET charged_bytes = ?5, updated_at = ?6
         WHERE provider = ?1 AND host = ?2 AND repository = ?3 AND repository_id IS ?4",
        params![
            who.owner.identity.provider,
            who.owner.identity.host,
            who.repository,
            who.repository_id,
            charged,
            now
        ],
    )?;
    if changed == 0 {
        tx.execute(
            "INSERT INTO hub_work_usage
                (provider, host, repository, repository_id, charged_bytes, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                who.owner.identity.provider,
                who.owner.identity.host,
                who.repository,
                who.repository_id,
                charged,
                now
            ],
        )?;
    }
    Ok(())
}

fn work_retention_floors(tx: &Transaction<'_>, who: &WorkWho) -> Result<(i64, i64)> {
    Ok(tx
        .query_row(
            "SELECT event_floor_seq, snapshot_floor_seq FROM hub_work_retention
             WHERE provider = ?1 AND host = ?2 AND repository = ?3 AND repository_id IS ?4",
            params![
                who.owner.identity.provider,
                who.owner.identity.host,
                who.repository,
                who.repository_id
            ],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?
        .unwrap_or((0, 0)))
}

fn replace_relations(
    tx: &Transaction<'_>,
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
    tx.execute("DELETE FROM hub_work_relations WHERE work = ?1", [work])?;
    for (kind, ids) in [("depends_on", depends_on), ("related", related)] {
        for id in ids {
            tx.execute(
                "INSERT INTO hub_work_relations (work, related, kind) VALUES (?1, ?2, ?3)",
                params![work, id, kind],
            )?;
        }
    }
    Ok(())
}

fn unfinished_for_session(
    tx: &Transaction<'_>,
    who: &WorkWho,
    session: &str,
    now: i64,
) -> Result<Option<WorkItem>> {
    let mut statement = tx.prepare(
        "SELECT snapshot FROM hub_work_items
         WHERE provider = ?1 AND host = ?2 AND repository = ?3 AND repository_id IS ?4
           AND lead_token = ?5 AND lead_session = ?6 AND state <> 'finished'
         ORDER BY id LIMIT 2",
    )?;
    let snapshots = statement
        .query_map(
            params![
                who.owner.identity.provider,
                who.owner.identity.host,
                who.repository,
                who.repository_id,
                who.token,
                session
            ],
            |row| row.get::<_, String>(0),
        )?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    if snapshots.len() > 1 {
        return Err(HubError::Invalid(
            "session is lead for more than one unfinished work item".into(),
        ));
    }
    snapshots
        .into_iter()
        .next()
        .map(|snapshot| {
            let mut item: WorkItem = json_value(&snapshot)?;
            project_claim(tx, &mut item, now)?;
            Ok(item)
        })
        .transpose()
}

fn active_capture_work_for_session(
    tx: &Transaction<'_>,
    who: &WorkWho,
    session: &str,
    now: i64,
) -> Result<Option<WorkItem>> {
    let mut statement = tx.prepare(
        "SELECT item.snapshot FROM hub_work_items item
         JOIN hub_work_participants participant ON participant.work=item.id
         WHERE item.provider=?1 AND item.host=?2 AND item.repository=?3
           AND item.repository_id IS ?4 AND item.state='active'
           AND participant.token=?5 AND participant.session=?6
           AND participant.generation=item.generation
         ORDER BY item.id LIMIT 2",
    )?;
    let snapshots = statement
        .query_map(
            params![
                who.owner.identity.provider,
                who.owner.identity.host,
                who.repository,
                who.repository_id,
                who.token,
                session
            ],
            |row| row.get::<_, String>(0),
        )?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    if snapshots.len() > 1 {
        return Err(HubError::Invalid(
            "session participates in more than one active work item".into(),
        ));
    }
    snapshots
        .into_iter()
        .next()
        .map(|snapshot| {
            let mut item: WorkItem = json_value(&snapshot)?;
            project_claim(tx, &mut item, now)?;
            Ok(item)
        })
        .transpose()
}

fn resolve_work(
    tx: &Transaction<'_>,
    who: &WorkWho,
    start: &WorkStart,
    now: i64,
) -> Result<Option<WorkItem>> {
    if let Some(id) = &start.work_id {
        return scoped_work(tx, who, id, now);
    }
    let sql = if start.source.is_some() {
        "SELECT snapshot FROM hub_work_items
         WHERE provider = ?1 AND host = ?2 AND repository = ?3 AND repository_id IS ?4
           AND source_provider = ?5 AND source_host = ?6 AND source_repository = ?7
           AND source_issue = ?8"
    } else {
        "SELECT snapshot FROM hub_work_items
         WHERE provider = ?1 AND host = ?2 AND repository = ?3 AND repository_id IS ?4
           AND owner = ?5 AND request_key = ?6"
    };
    let snapshot: Option<String> = match &start.source {
        Some(source) => tx
            .query_row(
                sql,
                params![
                    who.owner.identity.provider,
                    who.owner.identity.host,
                    who.repository,
                    who.repository_id,
                    source.provider,
                    source.host,
                    source.repository,
                    source.issue
                ],
                |row| row.get(0),
            )
            .optional()?,
        None => tx
            .query_row(
                sql,
                params![
                    who.owner.identity.provider,
                    who.owner.identity.host,
                    who.repository,
                    who.repository_id,
                    who.owner.id,
                    start.request_key
                ],
                |row| row.get(0),
            )
            .optional()?,
    };
    snapshot
        .map(|snapshot| {
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
    tx: &Transaction<'_>,
    who: &WorkWho,
    request: &str,
    through: i64,
    now: i64,
) -> Result<Option<CoordinationRequest>> {
    let row = tx
        .query_row(
            "SELECT r.requester_owner, r.requester_token, r.requester_agent,
                    r.requester_session, u.login, v.proposed_work, r.source_provider,
                    r.source_host, r.source_repository, r.source_issue, r.request_key,
                    r.proposed_title, r.proposed_scope, v.status, r.created_at, v.updated_at,
                    r.created_seq, v.event_seq
             FROM hub_coordination_requests r
             JOIN hub_users u ON u.id = r.requester_owner
             JOIN hub_coordination_versions v ON v.request = r.id
             WHERE r.id = ?1 AND r.provider = ?2 AND r.host = ?3
               AND r.repository = ?4 AND r.repository_id IS ?5
               AND v.event_seq = (SELECT MAX(v2.event_seq) FROM hub_coordination_versions v2
                                  WHERE v2.request = r.id AND v2.event_seq <= ?6)",
            params![
                request,
                who.owner.identity.provider,
                who.owner.identity.host,
                who.repository,
                who.repository_id,
                through
            ],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, String>(4)?,
                    row.get::<_, Option<String>>(5)?,
                    row.get::<_, Option<String>>(6)?,
                    row.get::<_, Option<String>>(7)?,
                    row.get::<_, Option<String>>(8)?,
                    row.get::<_, Option<String>>(9)?,
                    row.get::<_, Option<String>>(10)?,
                    row.get::<_, String>(11)?,
                    row.get::<_, String>(12)?,
                    row.get::<_, String>(13)?,
                    row.get::<_, i64>(14)?,
                    row.get::<_, i64>(15)?,
                    row.get::<_, i64>(16)?,
                    row.get::<_, i64>(17)?,
                ))
            },
        )
        .optional()?;
    let Some((
        requester_owner,
        requester_token,
        requester_agent,
        requester_session,
        requester_login,
        proposed_work,
        source_provider,
        source_host,
        source_repository,
        source_issue,
        request_key,
        proposed_title,
        proposed_scope,
        stored_status,
        created_at,
        updated_at,
        created_seq,
        updated_seq,
    )) = row
    else {
        return Ok(None);
    };
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
    let mut statement = tx.prepare(
        "SELECT work, reasons, reasons_complete FROM hub_coordination_targets
         WHERE request = ?1 ORDER BY work",
    )?;
    let rows = statement
        .query_map([request], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, bool>(2)?,
            ))
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    for (work, reasons, reasons_complete) in rows {
        let item = scoped_work(tx, who, &work, now)?
            .ok_or_else(|| HubError::Unavailable("coordination target work is missing".into()))?;
        targets.push(CoordinationTarget {
            work_id: work,
            lead: coordination_lead(&item),
            generation: item.generation,
            reasons: json_value(&reasons)?,
            reasons_complete,
        });
    }
    let mut acknowledgements = Vec::new();
    let mut statement = tx.prepare(
        "SELECT target_work, token, agent, session, owner, login, generation, through_seq,
                acknowledged_at, event_seq
         FROM hub_coordination_acknowledgements a
         WHERE request = ?1 AND event_seq <= ?2
           AND event_seq = (SELECT MAX(a2.event_seq) FROM hub_coordination_acknowledgements a2
                            WHERE a2.request = a.request AND a2.target_work = a.target_work
                              AND a2.token = a.token AND a2.session = a.session
                              AND a2.generation = a.generation AND a2.event_seq <= ?2)
         ORDER BY CASE WHEN
             (a.target_work = '' AND a.token = ?4 AND a.session = ?5 AND a.generation = 0)
             OR EXISTS (SELECT 1 FROM hub_coordination_targets t
                        JOIN hub_work_items w ON w.id = t.work
                        WHERE t.request = a.request AND t.work = a.target_work
                          AND w.lead_token = a.token AND w.lead_session = a.session
                          AND w.generation = a.generation)
             THEN 0 ELSE 1 END, event_seq DESC LIMIT ?3",
    )?;
    let rows = statement
        .query_map(
            params![
                request,
                through,
                MAX_PROJECTED_COORDINATION_UPDATES + 1,
                requester_token,
                requester_session
            ],
            |row| {
                Ok(CoordinationAcknowledgement {
                    target_work_id: match row.get::<_, String>(0)?.as_str() {
                        "" => None,
                        value => Some(value.to_owned()),
                    },
                    agent: CoordinationAgent {
                        token: row.get(1)?,
                        agent: row.get(2)?,
                        session: row.get(3)?,
                        owner: row.get(4)?,
                        login: row.get(5)?,
                    },
                    generation: row.get(6)?,
                    through_seq: row.get(7)?,
                    acknowledged_at: row.get(8)?,
                    seq: row.get(9)?,
                })
            },
        )?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    let acknowledgements_complete = rows.len() <= MAX_PROJECTED_COORDINATION_UPDATES as usize;
    acknowledgements.extend(
        rows.into_iter()
            .take(MAX_PROJECTED_COORDINATION_UPDATES as usize),
    );
    let mut responses = Vec::new();
    let mut statement = tx.prepare(
        "SELECT id, operation_id, target_work, token, agent, session, owner, login,
                target_generation, kind, note, created_at, event_seq
         FROM hub_coordination_responses WHERE request = ?1 AND event_seq <= ?2
         ORDER BY CASE WHEN EXISTS (
             SELECT 1 FROM hub_coordination_targets t JOIN hub_work_items w ON w.id = t.work
             WHERE t.request = hub_coordination_responses.request
               AND t.work = hub_coordination_responses.target_work
               AND w.lead_token = hub_coordination_responses.token
               AND w.lead_session = hub_coordination_responses.session
               AND w.generation = hub_coordination_responses.target_generation)
             THEN 0 ELSE 1 END, event_seq DESC LIMIT ?3",
    )?;
    let rows = statement
        .query_map(
            params![request, through, MAX_PROJECTED_COORDINATION_UPDATES + 1],
            |row| {
                let kind: String = row.get(9)?;
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, String>(4)?,
                    row.get::<_, String>(5)?,
                    row.get::<_, String>(6)?,
                    row.get::<_, String>(7)?,
                    row.get::<_, i64>(8)?,
                    kind,
                    row.get::<_, Option<String>>(10)?,
                    row.get::<_, i64>(11)?,
                    row.get::<_, i64>(12)?,
                ))
            },
        )?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    let responses_complete = rows.len() <= MAX_PROJECTED_COORDINATION_UPDATES as usize;
    for (
        id,
        operation_id,
        target_work_id,
        token,
        agent,
        session,
        owner,
        login,
        target_generation,
        kind,
        note,
        at,
        seq,
    ) in rows
        .into_iter()
        .take(MAX_PROJECTED_COORDINATION_UPDATES as usize)
    {
        responses.push(CoordinationResponse {
            id,
            operation_id,
            target_work_id,
            responder: CoordinationAgent {
                token,
                agent,
                session,
                owner,
                login,
            },
            target_generation,
            kind: parse_response_kind(&kind)?,
            note,
            created_at: at,
            seq,
        });
    }
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
            request_key,
            title: proposed_title,
            scope: json_value(&proposed_scope)?,
        },
        targets,
        status,
        acknowledgements,
        acknowledgements_complete,
        responses,
        responses_complete,
        created_at,
        updated_at,
        created_seq,
        updated_seq,
    }))
}

fn coordination_all_targets_responded(tx: &Transaction<'_>, request: &str) -> Result<bool> {
    Ok(tx.query_row(
        "SELECT NOT EXISTS (
             SELECT 1 FROM hub_coordination_targets t
             JOIN hub_work_items w ON w.id = t.work
             WHERE t.request = ?1 AND NOT EXISTS (
                 SELECT 1 FROM hub_coordination_responses r
                 WHERE r.request = t.request AND r.target_work = t.work
                   AND r.token = w.lead_token AND r.session = w.lead_session
                   AND r.target_generation = w.generation
             )
         )",
        [request],
        |row| row.get(0),
    )?)
}

fn coordination_was_initially_bound(tx: &Transaction<'_>, request: &str) -> Result<bool> {
    Ok(tx.query_row(
        "SELECT proposed_work IS NOT NULL FROM hub_coordination_versions
         WHERE request = ?1 ORDER BY event_seq LIMIT 1",
        [request],
        |row| row.get(0),
    )?)
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
    tx: &Transaction<'_>,
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
            overlaps
                .first()
                .map(|overlap| overlap.item.repository.as_str()),
            &identity,
            &proposal.title,
            &proposal.scope,
            &target_ids,
        ))?
        .as_bytes(),
    );
    let existing: Option<String> = tx
        .query_row(
            "SELECT id FROM hub_coordination_requests
             WHERE provider = ?1 AND host = ?2 AND repository = ?3 AND repository_id IS ?4
               AND fingerprint = ?5 AND status IN ('pending', 'responded')",
            params![
                who.owner.identity.provider,
                who.owner.identity.host,
                who.repository,
                who.repository_id,
                fingerprint
            ],
            |row| row.get(0),
        )
        .optional()?;
    if let Some(existing) = existing {
        return coordination_request_at(tx, who, &existing, i64::MAX, now)?
            .ok_or_else(|| HubError::Unavailable("coordination request disappeared".into()));
    }
    let (active, owner_active, token_active): (i64, i64, i64) = tx.query_row(
        "SELECT COUNT(*),
                COALESCE(SUM(CASE WHEN requester_owner = ?5 THEN 1 ELSE 0 END), 0),
                COALESCE(SUM(CASE WHEN requester_token = ?6 THEN 1 ELSE 0 END), 0)
         FROM hub_coordination_requests
         WHERE provider = ?1 AND host = ?2 AND repository = ?3 AND repository_id IS ?4
           AND status IN ('pending', 'responded')",
        params![
            who.owner.identity.provider,
            who.owner.identity.host,
            who.repository,
            who.repository_id,
            who.owner.id,
            who.token
        ],
        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
    )?;
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
    let targets = json_string(&target_ids)?;
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
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14,
                 ?15, ?16, ?17, ?18, 'pending', ?19, ?19, ?20, ?20)",
        params![
            id,
            who.owner.identity.provider,
            who.owner.identity.host,
            who.repository,
            who.repository_id,
            who.owner.id,
            who.token,
            who.agent,
            requester_session,
            proposal.work_id,
            source_provider,
            source_host,
            source_repository,
            source_issue,
            proposal.request_key,
            proposal.title,
            scope,
            fingerprint,
            now,
            seq
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
                (request, work, original_generation, reasons, reasons_complete)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                id,
                overlap.item.id,
                overlap.item.generation,
                reasons,
                reasons_complete
            ],
        )?;
        target_payloads.push(reasons);
    }
    tx.execute(
        "INSERT INTO hub_coordination_versions
            (request, event_seq, status, proposed_work, updated_at)
         VALUES (?1, ?2, 'pending', ?3, ?4)",
        params![id, seq, proposal.work_id, now],
    )?;
    let mut parts = vec![scope.as_str(), targets.as_str(), proposal.title.as_str()];
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
    tx: &Transaction<'_>,
    request: &str,
    status: CoordinationStatus,
    proposed_work: Option<&str>,
    now: i64,
    seq: i64,
) -> Result<()> {
    tx.execute(
        "UPDATE hub_coordination_requests
         SET status = ?2, proposed_work = ?3,
             source_provider = CASE WHEN ?3 IS NULL THEN source_provider ELSE NULL END,
             source_host = CASE WHEN ?3 IS NULL THEN source_host ELSE NULL END,
             source_repository = CASE WHEN ?3 IS NULL THEN source_repository ELSE NULL END,
             source_issue = CASE WHEN ?3 IS NULL THEN source_issue ELSE NULL END,
             request_key = CASE WHEN ?3 IS NULL THEN request_key ELSE NULL END,
             updated_at = ?4, updated_seq = ?5 WHERE id = ?1",
        params![request, status_name(status), proposed_work, now, seq],
    )?;
    tx.execute(
        "INSERT INTO hub_coordination_versions
            (request, event_seq, status, proposed_work, updated_at)
         VALUES (?1, ?2, ?3, ?4, ?5)",
        params![request, seq, status_name(status), proposed_work, now],
    )?;
    Ok(())
}

fn bind_coordination_request(
    tx: &Transaction<'_>,
    who: &WorkWho,
    session: &str,
    item: &WorkItem,
    now: i64,
) -> Result<()> {
    let source_match = match &item.source {
        WorkSource::Native { request_key } => (
            "r.request_key = ?6",
            Some(request_key.as_str()),
            None,
            None,
            None,
            None,
        ),
        WorkSource::Forge {
            provider,
            host,
            repository,
            issue,
        } => (
            "r.source_provider = ?7 AND r.source_host = ?8 AND r.source_repository = ?9 AND r.source_issue = ?10",
            None,
            Some(provider.as_str()),
            Some(host.as_str()),
            Some(repository.as_str()),
            Some(issue.as_str()),
        ),
    };
    let sql = format!(
        "SELECT r.id FROM hub_coordination_requests r
         WHERE r.provider = ?1 AND r.host = ?2 AND r.repository = ?3 AND r.repository_id IS ?4
           AND r.requester_token = ?5 AND r.requester_session = ?11
           AND r.proposed_work IS NULL AND r.status IN ('pending', 'responded')
           AND ({}) ORDER BY r.created_seq DESC LIMIT 1",
        source_match.0
    );
    let mut request: Option<String> = tx
        .query_row(
            &sql,
            params![
                who.owner.identity.provider,
                who.owner.identity.host,
                who.repository,
                who.repository_id,
                who.token,
                source_match.1,
                source_match.2,
                source_match.3,
                source_match.4,
                source_match.5,
                session
            ],
            |row| row.get(0),
        )
        .optional()?;
    if request.is_none()
        && matches!(&item.source, WorkSource::Native { request_key }
            if super::coordination::is_automatic_request_key(request_key))
    {
        let mut statement = tx.prepare(
            "SELECT id, proposed_scope FROM hub_coordination_requests
             WHERE provider = ?1 AND host = ?2 AND repository = ?3 AND repository_id IS ?4
               AND requester_token = ?5 AND requester_session = ?6 AND proposed_work IS NULL
               AND status IN ('pending', 'responded') AND proposed_title = ?7
             ORDER BY created_seq DESC",
        )?;
        let candidates = statement
            .query_map(
                params![
                    who.owner.identity.provider,
                    who.owner.identity.host,
                    who.repository,
                    who.repository_id,
                    who.token,
                    session,
                    item.title
                ],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
            )?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        request = candidates.into_iter().find_map(|(id, scope)| {
            json_value::<Vec<String>>(&scope)
                .ok()
                .filter(|proposal_scope| uncovered_scope(proposal_scope, &item.scope).is_empty())
                .map(|_| id)
        });
    }
    let Some(request) = request else {
        return Ok(());
    };
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
    tx: &Transaction<'_>,
    who: &WorkWho,
    work: &str,
    generation: i64,
    lead_session: &str,
    now: i64,
) -> Result<()> {
    let mut statement = tx.prepare(
        "SELECT r.id, r.proposed_work FROM hub_coordination_requests r
         JOIN hub_coordination_targets t ON t.request = r.id
         WHERE t.work = ?1 AND r.status IN ('pending', 'responded')
         ORDER BY r.id LIMIT 101",
    )?;
    let rows = statement
        .query_map([work], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, Option<String>>(1)?))
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    if rows.len() > MAX_ACTIVE_COORDINATION_REQUESTS as usize {
        return Err(HubError::WorkCapacity);
    }
    for (request, proposed_work) in rows {
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
    tx: &Transaction<'_>,
    request: &str,
    operation: &str,
    kind: &str,
    payload: &str,
) -> Result<bool> {
    let previous = tx
        .query_row(
            "SELECT kind, payload FROM hub_coordination_operations
             WHERE request = ?1 AND operation_id = ?2",
            params![request, operation],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
        )
        .optional()?;
    match previous {
        None => Ok(false),
        Some((previous_kind, previous_payload))
            if previous_kind == kind && previous_payload == payload =>
        {
            Ok(true)
        }
        Some(_) => Err(HubError::Invalid(
            "coordination operation_id was already used for another request".into(),
        )),
    }
}

fn coordination_page(
    tx: &Transaction<'_>,
    who: &WorkWho,
    session: Option<&str>,
    cursor: (i64, Option<i64>, bool),
    limit: usize,
    now: i64,
) -> Result<CoordinationInbox> {
    let (after, requested_through, pending_only) = cursor;
    let retention_floor: i64 = tx
        .query_row(
            "SELECT event_floor_seq FROM hub_coordination_retention
             WHERE provider = ?1 AND host = ?2 AND repository = ?3 AND repository_id IS ?4",
            params![
                who.owner.identity.provider,
                who.owner.identity.host,
                who.repository,
                who.repository_id
            ],
            |row| row.get(0),
        )
        .optional()?
        .unwrap_or(0);
    let through = requested_through.unwrap_or(
        tx.query_row("SELECT COALESCE(MAX(seq), 0) FROM hub_events", [], |row| {
            row.get::<_, i64>(0)
        })?
        .max(retention_floor)
        .max(after),
    );
    let addressed = if session.is_some() {
        "AND (r.requester_token = ?7 AND r.requester_session = ?8
              OR EXISTS (SELECT 1 FROM hub_coordination_targets t
                         JOIN hub_work_items w ON w.id = t.work
                         WHERE t.request = r.id AND w.lead_token = ?7 AND w.lead_session = ?8))"
    } else {
        ""
    };
    let status = if pending_only {
        "AND v.status IN ('pending', 'responded')"
    } else {
        ""
    };
    let sql = format!(
        "SELECT r.id, v.event_seq FROM hub_coordination_requests r
         JOIN hub_coordination_versions v ON v.request = r.id
         WHERE r.provider = ?1 AND r.host = ?2 AND r.repository = ?3 AND r.repository_id IS ?4
           AND v.event_seq > ?5 AND v.event_seq <= ?6
           AND v.event_seq = (SELECT MAX(v2.event_seq) FROM hub_coordination_versions v2
                              WHERE v2.request = r.id AND v2.event_seq <= ?6)
           {status}
           {addressed}
         ORDER BY v.event_seq, r.id LIMIT ?9"
    );
    let session = session.unwrap_or("");
    let mut statement = tx.prepare(&sql)?;
    let rows = statement
        .query_map(
            params![
                who.owner.identity.provider,
                who.owner.identity.host,
                who.repository,
                who.repository_id,
                after,
                through,
                who.token,
                session,
                i64::try_from(limit + 1).unwrap_or(i64::MAX)
            ],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?)),
        )?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    let mut complete = rows.len() <= limit;
    let rows = rows.into_iter().take(limit).collect::<Vec<_>>();
    let mut next_after = after;
    let mut requests = Vec::new();
    let mut bytes = 0_usize;
    for (id, seq) in rows {
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
    tx: &Transaction<'_>,
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
            let participant_generation: Option<i64> = tx
                .query_row(
                    "SELECT generation FROM hub_work_participants
                     WHERE work = ?1 AND token = ?2 AND session = ?3",
                    params![item.id, who.token, start.session],
                    |row| row.get(0),
                )
                .optional()?;
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
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?8)
                 ON CONFLICT (work, token, session) DO UPDATE SET role = excluded.role,
                    agent = excluded.agent, generation = excluded.generation,
                    updated_at = excluded.updated_at",
                params![
                    item.id,
                    who.token,
                    who.owner.id,
                    who.agent,
                    start.session,
                    role_name(start.role),
                    item.generation,
                    now
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
            "UPDATE hub_work_participants SET role = 'helper', updated_at = ?2
             WHERE work = ?1 AND role = 'lead'",
            params![item.id, now],
        )?;
        tx.execute(
            "INSERT INTO hub_work_participants
                (work, token, owner, agent, session, role, generation, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, 'lead', ?6, ?7, ?7)
             ON CONFLICT (work, token, session) DO UPDATE SET role = 'lead',
                owner = excluded.owner, agent = excluded.agent, generation = excluded.generation,
                updated_at = excluded.updated_at",
            params![
                item.id,
                who.token,
                who.owner.id,
                who.agent,
                start.session,
                item.generation,
                now
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
    let participant = WorkParticipant {
        owner: who.owner.id.clone(),
        login: who.owner.login.clone(),
        token: who.token.clone(),
        agent: who.agent.clone(),
        session: start.session.clone(),
        role: WorkRole::Lead,
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
        participants: vec![participant],
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
    tx.execute(
        "INSERT INTO hub_work_items
            (id, provider, host, repository, repository_id, owner, lead_token, lead_agent,
             lead_session, generation, source_provider, source_host, source_repository,
             source_issue, request_key, title, state, scope, branch, worktree, next_action,
             current_checkpoint, snapshot, created_at, updated_at, created_seq, updated_seq)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, 1, ?10, ?11, ?12, ?13, ?14,
                 ?15, 'active', ?16, ?17, ?18, ?19, NULL, ?20, ?21, ?21, ?22, ?22)",
        params![
            item.id,
            who.owner.identity.provider,
            who.owner.identity.host,
            who.repository,
            who.repository_id,
            who.owner.id,
            who.token,
            who.agent,
            start.session,
            source_provider,
            source_host,
            source_repository,
            source_issue,
            request_key,
            item.title,
            json_string(&item.scope)?,
            item.branch,
            item.worktree,
            item.next_action,
            json_string(&item)?,
            now,
            seq
        ],
    )?;
    tx.execute(
        "INSERT INTO hub_work_participants
            (work, token, owner, agent, session, role, generation, created_at, updated_at)
         VALUES (?1, ?2, ?3, ?4, ?5, 'lead', 1, ?6, ?6)",
        params![id, who.token, who.owner.id, who.agent, start.session, now],
    )?;
    replace_relations(tx, who, &id, &item.depends_on, &item.related_work)?;
    tx.execute(
        "INSERT INTO hub_work_versions (work, event_seq, state, snapshot)
         VALUES (?1, ?2, 'active', ?3)",
        params![id, seq, json_string(&item)?],
    )?;
    let snapshot = json_string(&item)?;
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
    tx: &Transaction<'_>,
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
        let mut statement = tx.prepare(
            "SELECT v.snapshot FROM hub_work_items w JOIN hub_work_versions v ON v.work = w.id
             WHERE w.provider = ?1 AND w.host = ?2 AND w.repository = ?3
               AND w.repository_id IS ?4 AND w.id > ?5 AND v.event_seq = (
                   SELECT MAX(v2.event_seq) FROM hub_work_versions v2
                   WHERE v2.work = w.id AND v2.event_seq <= ?6)
               AND v.state <> 'finished'
             ORDER BY w.id LIMIT ?7",
        )?;
        let snapshots = statement
            .query_map(
                params![
                    who.owner.identity.provider,
                    who.owner.identity.host,
                    who.repository,
                    who.repository_id,
                    work_after.as_deref().unwrap_or(""),
                    through,
                    (limit + 1) as i64
                ],
                |row| row.get::<_, String>(0),
            )?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        more_work = snapshots.len() > limit;
        for snapshot in snapshots.into_iter().take(limit) {
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

    let mut statement = tx.prepare(
        "SELECT e.seq, e.at, e.kind, e.subject, v.snapshot
         FROM hub_events e JOIN hub_work_versions v ON v.event_seq = e.seq
         JOIN hub_work_items w ON w.id = v.work
         WHERE e.seq > ?1 AND e.seq <= ?2 AND w.provider = ?3 AND w.host = ?4
           AND w.repository = ?5 AND w.repository_id IS ?6
         ORDER BY e.seq LIMIT ?7",
    )?;
    let rows = statement
        .query_map(
            params![
                after,
                through,
                who.owner.identity.provider,
                who.owner.identity.host,
                who.repository,
                who.repository_id,
                (limit + 1) as i64
            ],
            |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, String>(4)?,
                ))
            },
        )?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    let mut more_events = rows.len() > limit;
    let mut events = Vec::new();
    for (seq, at, kind, work_id, snapshot) in rows.into_iter().take(limit) {
        if !events.is_empty() && bytes.saturating_add(snapshot.len()) > MAX_BRIEFING_BYTES {
            more_events = true;
            break;
        }
        bytes = bytes.saturating_add(snapshot.len());
        let mut item: WorkItem = json_value(&snapshot)?;
        project_claim(tx, &mut item, now)?;
        events.push(WorkEvent {
            seq,
            at,
            kind,
            work_id,
            item,
        });
    }
    let next_after = if more_events {
        events.last().map_or(after, |event| event.seq)
    } else {
        through
    };
    let minimum: Option<i64> =
        tx.query_row("SELECT MIN(seq) FROM hub_events", [], |row| row.get(0))?;
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

/// Rechecks inside the write transaction that the token is still usable.
fn active_token(tx: &Transaction<'_>, token: &str, now: i64) -> Result<Who> {
    tx.query_row(
        "SELECT t.repository, t.name, u.id, u.provider, u.host, u.forge_user_id, u.login
         FROM hub_agent_tokens t JOIN hub_users u ON u.id = t.owner
         WHERE t.id = ?1 AND t.revoked = 0 AND t.expires_at > ?2 AND u.disabled = 0",
        params![token, now],
        |row| {
            Ok(Who {
                token: token.to_owned(),
                repository: row.get(0)?,
                agent: row.get(1)?,
                owner: User {
                    id: row.get(2)?,
                    identity: ForgeIdentity {
                        provider: row.get(3)?,
                        host: row.get(4)?,
                        user_id: row.get(5)?,
                    },
                    login: row.get(6)?,
                    disabled: false,
                },
            })
        },
    )
    .optional()?
    .ok_or(HubError::Unauthorized)
}

fn publication_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Publication> {
    Ok(Publication {
        id: row.get(0)?,
        repository: row.get(1)?,
        token: row.get(2)?,
        owner: row.get(20)?,
        authorization_binding: row.get(21)?,
        agent: row.get(3)?,
        owner_login: row.get(4)?,
        revision: row.get(5)?,
        event_seq: row.get(6)?,
        commit: row.get(7)?,
        branch: row.get(8)?,
        base: row.get(9)?,
        manifest: row.get(10)?,
        branch_state: row.get(11)?,
        branch_message: row.get(12)?,
        pr_number: row.get(13)?,
        pr_node_id: row.get(14)?,
        pr_url: row.get(15)?,
        merge_commit: row.get(16)?,
        error: row.get(17)?,
        created_at: row.get(18)?,
        updated_at: row.get(19)?,
    })
}

fn publication_by_id(tx: &Connection, id: &str) -> Result<Option<Publication>> {
    Ok(tx
        .query_row(
            &format!("{PUBLICATION_SELECT} WHERE p.id = ?1"),
            [id],
            publication_row,
        )
        .optional()?)
}

fn connection_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<ForgeConnection> {
    Ok(ForgeConnection {
        id: row.get(0)?,
        provider: row.get(1)?,
        host: row.get(2)?,
        kind: row.get(3)?,
        app_id: row.get(4)?,
        app_slug: row.get(5)?,
        acts_as: row.get(6)?,
        outbound: row.get(7)?,
        status: row.get(8)?,
        status_detail: row.get(9)?,
        next_poll_at: row.get(10)?,
    })
}

fn connection_by_id(tx: &Connection, id: &str) -> Result<Option<ForgeConnection>> {
    Ok(tx
        .query_row(
            &format!("{CONNECTION_SELECT} WHERE id = ?1"),
            [id],
            connection_row,
        )
        .optional()?)
}

fn repository_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<ConnectedRepository> {
    Ok(ConnectedRepository {
        id: row.get(0)?,
        connection: row.get(1)?,
        host: row.get(2)?,
        repository_id: row.get(3)?,
        full_name: row.get(4)?,
        node_id: row.get(5)?,
        installation_id: row.get(6)?,
        pulls_etag: row.get(7)?,
    })
}

fn repository_by_rep(tx: &Connection, id: &str) -> Result<ConnectedRepository> {
    tx.query_row(
        &format!("{REPOSITORY_SELECT} WHERE id = ?1"),
        [id],
        repository_row,
    )
    .optional()?
    .ok_or_else(|| HubError::Invalid("unknown repository".into()))
}

/// Ended merge groups are kept this long (30 days) before an observation of the repository drops them.
const MERGE_GROUP_KEEP_SECONDS: i64 = 2_592_000;

const MERGE_GROUP_COLUMNS: &str = "head_sha, head_ref, base_ref, base_sha, pr_number, pr_head_sha,
    context, ended_reason, observed_at, ended_at, state, binding_digest, description, phase,
    generation, updated_at, ruleset_requirement, status_source";

fn merge_group_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<MergeGroup> {
    let head_sha: String = row.get(0)?;
    let phase: Option<String> = row.get(13)?;
    Ok(MergeGroup {
        check: match phase {
            Some(phase) => Some(ForgeCheck {
                head_sha: head_sha.clone(),
                state: row.get(10)?,
                binding_digest: row.get(11)?,
                description: row.get(12)?,
                phase,
                generation: row.get(14)?,
                updated_at: row.get(15)?,
            }),
            None => None,
        },
        head_sha,
        head_ref: row.get(1)?,
        base_ref: row.get(2)?,
        base_sha: row.get(3)?,
        pr_number: row.get(4)?,
        pr_head_sha: row.get(5)?,
        context: row.get(6)?,
        ended_reason: row.get(7)?,
        observed_at: row.get(8)?,
        ended_at: row.get(9)?,
        generation: row.get(14)?,
        ruleset_requirement: row.get(16)?,
        status_source: row.get(17)?,
    })
}

fn external_run_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Result<ExternalRun>> {
    let kind: String = row.get(1)?;
    let outcome: String = row.get(8)?;
    let (Some(kind), Some(outcome)) =
        (ExternalKind::parse(&kind), ExternalOutcome::parse(&outcome))
    else {
        return Ok(Err(HubError::Unavailable(
            "stored external run is invalid".into(),
        )));
    };
    Ok(Ok(ExternalRun {
        provider: row.get(0)?,
        kind,
        run_id: row.get(2)?,
        attempt: row.get(3)?,
        commit_sha: row.get(4)?,
        branch: row.get(5)?,
        environment: row.get(6)?,
        name: row.get(7)?,
        outcome,
        provider_updated_at: row.get(9)?,
        html_url: row.get(10)?,
    }))
}

fn intent_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<(Intent, String, Option<String>)> {
    Ok((
        Intent {
            id: row.get(0)?,
            repository: row.get(1)?,
            task: row.get(2)?,
            branch: row.get(3)?,
            base: row.get(4)?,
            machine: row.get(5)?,
            paths: Vec::new(),
            expires_at: row.get(6)?,
            created_at: row.get(14)?,
            updated_at: row.get(15)?,
            token: row.get(7)?,
            agent: row.get(8)?,
            owner: User {
                id: row.get(9)?,
                identity: ForgeIdentity {
                    provider: row.get(10)?,
                    host: row.get(11)?,
                    user_id: row.get(12)?,
                },
                login: row.get(13)?,
                disabled: false,
            },
            via: Vec::new(),
            session: row.get(18)?,
        },
        row.get(16)?,
        row.get(17)?,
    ))
}

/// Intents by id, whether or not they are live.
fn load_intents(tx: &Transaction<'_>, ids: &[String]) -> Result<Vec<Intent>> {
    let ids = serde_json::json!(ids).to_string();
    let mut statement = tx.prepare(&format!(
        "{INTENT_SELECT} WHERE i.id IN (SELECT value FROM json_each(?1))"
    ))?;
    let rows = statement
        .query_map([ids], intent_row)?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    finish(rows)
}

/// Live intents of other holders in the repository that overlap any of `keys`.
fn probe(
    tx: &Transaction<'_>,
    who: &Who,
    own: &str,
    keys: &[String],
    now: i64,
) -> Result<BTreeSet<String>> {
    let (points, ranges) = probe_set(keys);
    let mut ids = BTreeSet::new();
    let live = "repository = ?1 AND intent <> ?2 AND expires_at > ?3";
    if !points.is_empty() {
        let points = serde_json::json!(points).to_string();
        let sql = format!(
            "SELECT DISTINCT intent FROM hub_intent_paths WHERE {live}
             AND key IN (SELECT value FROM json_each(?4))"
        );
        collect(
            tx,
            &sql,
            params![who.repository, own, now, points],
            &mut ids,
        )?;
    }
    for (start, end) in ranges {
        let sql = format!(
            "SELECT DISTINCT intent FROM hub_intent_paths WHERE {live} AND key >= ?4 AND key < ?5"
        );
        let args = params![who.repository, own, now, start, end];
        collect(tx, &sql, args, &mut ids)?;
    }
    Ok(ids)
}

fn collect(
    tx: &Transaction<'_>,
    sql: &str,
    args: &[&dyn ToSql],
    ids: &mut BTreeSet<String>,
) -> Result<()> {
    let mut statement = tx.prepare_cached(sql)?;
    let mut rows = statement.query(args)?;
    while let Some(row) = rows.next()? {
        ids.insert(row.get(0)?);
    }
    Ok(())
}

/// The announce of `intent` (validated into `keys`) inside the caller's transaction.
fn announce_in(
    tx: &Transaction<'_>,
    who: &Who,
    intent: &NewIntent,
    keys: &[String],
    via: &[String],
    now: i64,
) -> Result<Announced> {
    let paths = display_all(keys);
    let paths_json = serde_json::json!(paths).to_string();
    let expires_at = now + intent.ttl_seconds;
    tx.execute(
        "DELETE FROM hub_intent_paths WHERE repository = ?1 AND expires_at <= ?2",
        params![who.repository, now],
    )?;
    let existing = tx
        .query_row(
            "SELECT token, released_at IS NOT NULL, expires_at, task, branch, base, machine, paths
             FROM hub_intents WHERE id = ?1",
            [&intent.id],
            |row| {
                Ok(Existing {
                    token: row.get(0)?,
                    released: row.get(1)?,
                    expires_at: row.get(2)?,
                    task: row.get(3)?,
                    branch: row.get(4)?,
                    base: row.get(5)?,
                    machine: row.get(6)?,
                    paths: row.get(7)?,
                })
            },
        )
        .optional()?;
    let plan = plan(
        existing.as_ref(),
        &who.token,
        intent,
        &paths_json,
        expires_at,
        now,
    )?;
    match &plan {
        Plan::Unchanged => {}
        Plan::Renew => {
            tx.execute(
                "UPDATE hub_intents SET expires_at = ?2, updated_at = ?3 WHERE id = ?1",
                params![intent.id, expires_at, now],
            )?;
            tx.execute(
                "UPDATE hub_intent_paths SET expires_at = ?2 WHERE intent = ?1",
                params![intent.id, expires_at],
            )?;
            append(tx, &Event::intent_renewed(&intent.id, expires_at)?)?;
        }
        Plan::Write { .. } => {
            tx.execute(
                "INSERT INTO hub_intents (id, repository, token, task, branch, base, machine,
                     paths, expires_at, created_at, updated_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?10)
                 ON CONFLICT (id) DO UPDATE SET task = excluded.task,
                     branch = excluded.branch, base = excluded.base,
                     machine = excluded.machine, paths = excluded.paths,
                     expires_at = excluded.expires_at, updated_at = excluded.updated_at",
                params![
                    intent.id,
                    who.repository,
                    who.token,
                    intent.task,
                    intent.branch,
                    intent.base,
                    intent.machine,
                    paths_json,
                    expires_at,
                    now
                ],
            )?;
            tx.execute(
                "DELETE FROM hub_intent_paths WHERE intent = ?1",
                [&intent.id],
            )?;
            tx.execute(
                "INSERT INTO hub_intent_paths (intent, repository, key, expires_at)
                 SELECT ?1, ?2, value, ?3 FROM json_each(?4)",
                params![
                    intent.id,
                    who.repository,
                    expires_at,
                    serde_json::json!(keys).to_string()
                ],
            )?;
            append(
                tx,
                &Event::intent_announced(who, intent, &paths, expires_at)?,
            )?;
        }
    }
    let mut ids = probe(tx, who, &intent.id, keys, now)?;
    ids.insert(intent.id.clone());
    let ids: Vec<String> = ids.into_iter().collect();
    let announced = announced(&intent.id, keys, load_intents(tx, &ids)?)?;
    if let Plan::Write { previous } = &plan {
        for event in notice_events(who, intent, via, previous, &announced)? {
            append(tx, &event)?;
        }
    }
    Ok(announced)
}

/// Releases the token's own intent; true if it was not already released.
fn release_in(tx: &Transaction<'_>, token: &str, intent: &str, now: i64) -> Result<bool> {
    let changed = tx.execute(
        "UPDATE hub_intents SET released_at = ?3, updated_at = ?3
         WHERE id = ?1 AND token = ?2 AND released_at IS NULL",
        params![intent, token, now],
    )?;
    if changed > 0 {
        tx.execute("DELETE FROM hub_intent_paths WHERE intent = ?1", [intent])?;
        append(tx, &Event::intent_released(intent)?)?;
    }
    Ok(changed > 0)
}

fn release_session_intent(
    tx: &Transaction<'_>,
    token: &str,
    session: &str,
    now: i64,
) -> Result<bool> {
    let intent = tx
        .query_row(
            "SELECT intent FROM hub_sessions WHERE token = ?1 AND session = ?2",
            params![token, session],
            |row| row.get::<_, Option<String>>(0),
        )
        .optional()?
        .flatten();
    intent
        .as_deref()
        .map(|intent| release_in(tx, token, intent, now))
        .transpose()
        .map(Option::unwrap_or_default)
}

/// The named session, else the token's only recent hook-connected one, else the default.
fn session_name(
    tx: &Transaction<'_>,
    token: &str,
    requested: Option<&str>,
    now: i64,
) -> Result<String> {
    if let Some(name) = requested {
        if name == DEFAULT_SESSION {
            return Ok(name.to_owned());
        }
        if let Some(canonical) = tx
            .query_row(
                "SELECT canonical_session FROM hub_session_aliases
                 WHERE token=?1 AND observed_session=?2",
                params![token, name],
                |row| row.get(0),
            )
            .optional()?
        {
            return Ok(canonical);
        }
        return Ok(name.to_owned());
    }
    let mut statement = tx.prepare(
        "SELECT session FROM hub_sessions
         WHERE token = ?1 AND updated_at > ?2 AND via LIKE '%\"hook:%' LIMIT 2",
    )?;
    let mut recent = statement
        .query_map(params![token, now - SESSION_TTL_SECONDS], |row| row.get(0))?
        .collect::<std::result::Result<Vec<String>, _>>()?;
    Ok(match recent.len() {
        1 => recent.remove(0),
        _ => DEFAULT_SESSION.to_owned(),
    })
}

fn alias_hook_session(
    tx: &Transaction<'_>,
    token: &str,
    observed: &str,
    now: i64,
) -> Result<String> {
    if observed == DEFAULT_SESSION {
        return Ok(observed.to_owned());
    }
    if let Some(canonical) = tx
        .query_row(
            "SELECT canonical_session FROM hub_session_aliases
             WHERE token=?1 AND observed_session=?2",
            params![token, observed],
            |row| row.get(0),
        )
        .optional()?
    {
        return Ok(canonical);
    }
    if session_row(tx, token, observed)?.is_some() {
        return Ok(observed.to_owned());
    }
    if tx
        .query_row(
            "SELECT 1 FROM hub_session_aliases
             WHERE token=?1 AND canonical_session=?2",
            params![token, DEFAULT_SESSION],
            |_| Ok(()),
        )
        .optional()?
        .is_some()
    {
        return Err(HubError::Invalid(
            "durable default work is already bound to another hook session".into(),
        ));
    }
    let eligible = tx
        .query_row(
            "SELECT 1 WHERE EXISTS (
                 SELECT 1 FROM hub_work_participants p
                 WHERE p.token=?1 AND p.session=?2)",
            params![token, DEFAULT_SESSION],
            |_| Ok(()),
        )
        .optional()?
        .is_some();
    if !eligible {
        return Ok(observed.to_owned());
    }
    let changed = tx.execute(
        "INSERT OR IGNORE INTO hub_session_aliases
            (token,observed_session,canonical_session,created_at) VALUES (?1,?2,?3,?4)",
        params![token, observed, DEFAULT_SESSION, now],
    )?;
    if changed == 0 {
        return Err(HubError::Invalid(
            "durable default work is already bound to another hook session".into(),
        ));
    }
    Ok(DEFAULT_SESSION.to_owned())
}

fn session_row(tx: &Transaction<'_>, token: &str, session: &str) -> Result<Option<Row>> {
    Ok(tx
        .query_row(
            "SELECT intent, task, via, notice_cursor, denied, deny_times, unchecked FROM hub_sessions
             WHERE token = ?1 AND session = ?2",
            params![token, session],
            |row| {
                Ok(Row {
                    intent: row.get(0)?,
                    task: row.get(1)?,
                    via: row.get(2)?,
                    cursor: row.get(3)?,
                    denied: row.get(4)?,
                    deny_times: row.get(5)?,
                    unchecked: row.get(6)?,
                })
            },
        )
        .optional()?)
}

/// One touch of the session inside the caller's transaction. A call without `report` (made on
/// Recheck managed assignments in the same write transaction as the file intent.
/// Sessions never linked to durable work keep the legacy advisory workflow.
fn guard_managed_touch(
    tx: &Transaction<'_>,
    who: &Who,
    name: &str,
    keys: &[String],
    now: i64,
) -> Result<()> {
    if keys.is_empty() {
        return Ok(());
    }
    let work = work_who(tx, &who.token, now)?;
    let rows = {
        let mut statement = tx.prepare(
            "SELECT w.snapshot, p.generation FROM hub_work_items w
             JOIN hub_work_participants p ON p.work=w.id
             WHERE w.provider=?1 AND w.host=?2 AND w.repository=?3
               AND w.repository_id IS ?4 AND p.token=?5 AND p.session=?6",
        )?;
        statement
            .query_map(
                params![
                    work.owner.identity.provider,
                    work.owner.identity.host,
                    work.repository,
                    work.repository_id,
                    work.token,
                    name
                ],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?)),
            )?
            .collect::<std::result::Result<Vec<_>, _>>()?
    };
    if rows.is_empty() {
        return Ok(());
    }
    let mut eligible = Vec::new();
    for (snapshot, generation) in rows {
        let item: WorkItem = json_value(&snapshot)?;
        if matches!(item.state, WorkState::Active | WorkState::Blocked)
            && item.generation == generation
        {
            eligible.push(item);
        }
    }
    if eligible.len() != 1 || !uncovered_scope(&eligible[0].scope, keys).is_empty() {
        return Err(HubError::Invalid("These files are not covered by a current unfinished assignment. Start or explicitly rejoin the work, or checkpoint the scope before editing.".into()));
    }
    Ok(())
}

/// the session's behalf, or answered without text) tells and consumes nothing.
fn touch_in(tx: &Transaction<'_>, who: &Who, call: &Checked, now: i64) -> Result<Touched> {
    let token = who.token.as_str();
    tx.execute(
        "DELETE FROM hub_sessions WHERE token = ?1 AND updated_at < ?2",
        params![token, now - STALE_SESSION_SECONDS],
    )?;
    let mut name = session_name(tx, token, call.session.as_deref(), now)?;
    if call.via.starts_with("hook:")
        && let Some(observed) = call.session.as_deref()
        && name == observed
    {
        name = alias_hook_session(tx, token, observed, now)?;
    }
    guard_managed_touch(tx, who, &name, &call.keys, now)?;
    let mut row = session_row(tx, token, &name)?;
    if row.is_none() && name != DEFAULT_SESSION && call.via.starts_with("hook:") {
        // An agent that called agentscm_start before its first hook keeps that session.
        let adopted = tx.execute(
            "UPDATE hub_sessions SET session = ?3
             WHERE token = ?1 AND session = ?4 AND via = '[\"mcp\"]' AND updated_at > ?2",
            params![token, now - SESSION_TTL_SECONDS, name, DEFAULT_SESSION],
        )?;
        if adopted > 0 {
            // Requests to and from the session follow it.
            for column in ["holder", "requester"] {
                tx.execute(
                    &format!(
                        "UPDATE hub_handoffs SET {column}_session = ?2
                         WHERE {column}_token = ?1 AND {column}_session = ?3"
                    ),
                    params![token, name, DEFAULT_SESSION],
                )?;
            }
            row = session_row(tx, token, &name)?;
        }
    }
    let (current, lapsed) = match row.as_ref().and_then(|row| row.intent.as_deref()) {
        Some(id) => tx
            .query_row(
                "SELECT released_at IS NOT NULL, expires_at, created_at, paths FROM hub_intents
                 WHERE id = ?1",
                [id],
                |found| {
                    Ok((
                        found.get::<_, bool>(0)?,
                        found.get::<_, i64>(1)?,
                        found.get::<_, i64>(2)?,
                        found.get::<_, String>(3)?,
                    ))
                },
            )
            .optional()?
            .map(|(released, expires, created, paths)| {
                let cause = expiry_cause(released, expires, created, now);
                held(id, released, expires, created, now, &paths)
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
    let notices = match &current {
        Some(held) if call.report && !guarded.refused() => {
            let mut statement = tx.prepare(
                "SELECT seq, id, at, kind, subject, data FROM hub_events
                 WHERE seq > ?1 AND subject = ?2 AND kind IN ('intent_notice', 'pr_notice')
                 ORDER BY seq LIMIT ?3",
            )?;
            statement
                .query_map(params![cursor, held.id, NOTICES_PER_TOUCH as i64], |row| {
                    Ok(HubEvent {
                        seq: row.get(0)?,
                        id: row.get(1)?,
                        at: row.get(2)?,
                        kind: row.get(3)?,
                        subject: row.get(4)?,
                        data: row.get(5)?,
                    })
                })?
                .collect::<std::result::Result<Vec<_>, _>>()?
        }
        _ => Vec::new(),
    };
    let announced = match &step.new {
        Some(new) => Some(announce_in(tx, who, new, &step.keys, &step.via, now)?),
        None => None,
    };
    if let Some(new) = &step.new {
        // Every touch is a sign of life, also when the hold cap leaves the expiry as it was.
        tx.execute(
            "UPDATE hub_intents SET updated_at = ?2 WHERE id = ?1",
            params![new.id, now],
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
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?7, ?8, ?9, ?10)
         ON CONFLICT (token, session) DO UPDATE SET intent = excluded.intent,
             task = excluded.task, via = excluded.via,
             notice_cursor = excluded.notice_cursor, updated_at = excluded.updated_at,
             denied = excluded.denied, deny_times = excluded.deny_times,
             unchecked = excluded.unchecked",
        params![
            token,
            name,
            step.new.as_ref().map(|new| &new.id),
            step.task,
            serde_json::json!(step.via).to_string(),
            cursor,
            now,
            guarded.denied_json,
            guarded.times_json,
            guarded.unchecked_json
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
    tx: &Transaction<'_>,
    who: &Who,
    name: &str,
    row: &Row,
    key: &str,
    note: Option<&str>,
    auto: bool,
    now: i64,
) -> Result<Asked> {
    let token = who.token.as_str();
    let mut ids = BTreeSet::new();
    collect(
        tx,
        "SELECT DISTINCT intent FROM hub_intent_paths
         WHERE repository = ?1 AND key = ?2 AND expires_at > ?3",
        params![who.repository, key, now],
        &mut ids,
    )?;
    let holders = load_intents(tx, &ids.into_iter().collect::<Vec<_>>())?;
    let open = tx
        .prepare_cached(
            "SELECT path, holder_intent FROM hub_handoffs
             WHERE requester_token = ?1 AND requester_session = ?2 AND state = 'pending'
               AND created_at > ?3",
        )?
        .query_map(params![token, name, cutoff(now)], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    let asked: Vec<String> = open
        .iter()
        .filter(|(asked, _)| asked == key)
        .map(|(_, intent)| intent.clone())
        .collect();
    let queued = tx
        .prepare_cached(
            "SELECT holder_intent FROM hub_handoffs
             WHERE repository = ?1 AND path = ?2 AND state = 'pending' AND created_at > ?3",
        )?
        .query_map(params![who.repository, key, cutoff(now)], |row| row.get(0))?
        .collect::<std::result::Result<Vec<String>, _>>()?;
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
    let holder_session: Option<String> = tx
        .query_row(
            "SELECT session FROM hub_sessions WHERE intent = ?1",
            [&holder.id],
            |row| row.get(0),
        )
        .optional()?;
    let Some(holder_session) = holder_session else {
        return Ok(Asked::NotHeld);
    };
    tx.execute(
        "DELETE FROM hub_handoffs WHERE repository = ?1 AND created_at < ?2",
        params![who.repository, now - STALE_SESSION_SECONDS],
    )?;
    let id = new_id("hof")?;
    tx.execute(
        "INSERT INTO hub_handoffs (id, repository, path, requester_token, requester_session,
             holder_token, holder_session, holder_intent, note, state, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, 'pending', ?10)",
        params![
            id,
            who.repository,
            key,
            token,
            name,
            holder.token,
            holder_session,
            holder.id,
            note,
            now
        ],
    )?;
    let event = Event::handoff_request(token, &id, name, key, note, &holder, auto)?;
    append(tx, &event)?;
    Ok(Asked::Requested { holder })
}

/// A refused call asks the holder of each contested file to hand it over, as `agentscm_ask`
/// would. Best effort: a failure is logged and rolled back to its savepoint, and the touch and
/// its refusal stand. Returns the files a request was filed for.
fn auto_ask(tx: &Transaction<'_>, who: &Who, name: &str, keys: &[String], now: i64) -> Vec<String> {
    let mut requested = Vec::new();
    for key in keys.iter().take(MAX_OPEN) {
        let ask = || -> Result<Asked> {
            tx.execute_batch("SAVEPOINT auto_ask")?;
            let row = session_row(tx, &who.token, name)?
                .ok_or_else(|| HubError::Invalid("unknown session".into()))?;
            let note = clean_note(row.task.as_deref());
            let asked = ask_in(tx, who, name, &row, key, note.as_deref(), true, now)?;
            tx.execute_batch("RELEASE auto_ask")?;
            Ok(asked)
        };
        match ask() {
            Ok(Asked::Requested { .. }) => requested.push(key.clone()),
            Ok(_) => {}
            Err(error) => {
                eprintln!("hub: handoff request for a refused edit: {error}");
                let _ = tx.execute_batch("ROLLBACK TO auto_ask; RELEASE auto_ask");
            }
        }
    }
    requested
}

/// The session's open requests and unseen answers, marked told.
fn handoffs_in(
    tx: &Transaction<'_>,
    token: &str,
    session: &str,
    now: i64,
) -> Result<Vec<HandoffNote>> {
    let requests = tx
        .prepare_cached(
            "SELECT h.id, h.path, h.note, t.name, u.login
             FROM hub_handoffs h JOIN hub_agent_tokens t ON t.id = h.requester_token
             JOIN hub_users u ON u.id = t.owner
             WHERE h.holder_token = ?1 AND h.holder_session = ?2 AND h.state = 'pending'
               AND h.holder_told = 0 AND h.created_at > ?3
             ORDER BY h.created_at, h.id LIMIT ?4",
        )?
        .query_map(params![token, session, cutoff(now), PER_TOUCH], |row| {
            Ok((
                row.get::<_, String>(0)?,
                HandoffNote {
                    kind: "request",
                    path: row.get(1)?,
                    note: row.get(2)?,
                    reason: None,
                    agent: row.get(3)?,
                    login: row.get(4)?,
                    session: session.to_owned(),
                },
            ))
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    let answers = tx
        .prepare_cached(
            "SELECT h.id, h.state, h.path, t.name, u.login, h.reply
             FROM hub_handoffs h JOIN hub_agent_tokens t ON t.id = h.holder_token
             JOIN hub_users u ON u.id = t.owner
             WHERE h.requester_token = ?1 AND h.requester_session = ?2
               AND h.state IN ('granted', 'declined') AND h.requester_told = 0
             ORDER BY h.decided_at, h.id LIMIT ?3",
        )?
        .query_map(params![token, session, PER_TOUCH], |row| {
            Ok((
                row.get::<_, String>(0)?,
                HandoffNote {
                    kind: answer_kind(&row.get::<_, String>(1)?),
                    path: row.get(2)?,
                    note: None,
                    reason: row.get(5)?,
                    agent: row.get(3)?,
                    login: row.get(4)?,
                    session: session.to_owned(),
                },
            ))
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    for (id, _) in &requests {
        tx.execute(
            "UPDATE hub_handoffs SET holder_told = 1 WHERE id = ?1",
            [id],
        )?;
    }
    for (id, _) in &answers {
        tx.execute(
            "UPDATE hub_handoffs SET requester_told = 1 WHERE id = ?1",
            [id],
        )?;
    }
    Ok(requests
        .into_iter()
        .chain(answers)
        .map(|(_, note)| note)
        .collect())
}

/// Ends a request without an answer.
fn lapse(tx: &Transaction<'_>, request: &str, now: i64) -> Result<()> {
    tx.execute(
        "UPDATE hub_handoffs SET state = 'lapsed', decided_at = ?2 WHERE id = ?1",
        params![request, now],
    )?;
    Ok(())
}

/// The tokens of one repository as the forge knows it, owned by users of one host and provider.
const TOKEN_SCOPE: &str =
    "t.repository = ?1 AND t.repository_id IS ?2 AND u.host = ?3 AND u.provider = ?4";

impl Store for SqliteStore {
    fn backend(&self) -> &'static str {
        "sqlite"
    }

    fn ping(&self) -> Result<()> {
        self.conn().query_row("SELECT 1", [], |_| Ok(()))?;
        Ok(())
    }

    fn upsert_user(&self, identity: &ForgeIdentity, login: &str) -> Result<User> {
        identity.validate()?;
        validate_login(login)?;
        let mut conn = self.conn();
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let id = new_id("usr")?;
        let inserted = tx.execute(
            "INSERT INTO hub_users (id, provider, host, forge_user_id, login, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6) ON CONFLICT DO NOTHING",
            params![
                id,
                identity.provider,
                identity.host,
                identity.user_id,
                login,
                unix_now()
            ],
        )?;
        let user = if inserted > 0 {
            append(&tx, &Event::user_created(&id, identity, login)?)?;
            User {
                id,
                identity: identity.clone(),
                login: login.to_owned(),
                disabled: false,
            }
        } else {
            let (id, previous, disabled): (String, String, bool) = tx.query_row(
                "SELECT id, login, disabled FROM hub_users
                 WHERE provider = ?1 AND host = ?2 AND forge_user_id = ?3",
                params![identity.provider, identity.host, identity.user_id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )?;
            if previous != login {
                tx.execute(
                    "UPDATE hub_users SET login = ?2 WHERE id = ?1",
                    params![id, login],
                )?;
                append(&tx, &Event::user_login_changed(&id, &previous, login)?)?;
            }
            User {
                id,
                identity: identity.clone(),
                login: login.to_owned(),
                disabled,
            }
        };
        tx.commit()?;
        Ok(user)
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
        let mut conn = self.conn();
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let inserted = tx.execute(
            "INSERT INTO hub_agent_tokens
                 (id, owner, name, repository, digest, expires_at, created_at, repository_id, read_only)
             SELECT ?1, id, ?3, ?4, ?5, ?6, ?7, ?8, ?9 FROM hub_users WHERE id = ?2 AND disabled = 0",
            params![
                id,
                token.owner,
                token.name,
                token.repository,
                token.digest.as_str(),
                token.expires_at,
                now,
                token.repository_id,
                token.read_only
            ],
        )?;
        if inserted == 0 {
            return Err(HubError::Invalid("unknown or disabled owner".into()));
        }
        append(&tx, &Event::token_created(&id, token)?)?;
        tx.commit()?;
        Ok(id)
    }

    fn revoke_agent_token(&self, token: &str) -> Result<bool> {
        self.revoke(None, token)
    }

    fn revoke_owned_agent_token(&self, owner: &str, token: &str) -> Result<bool> {
        self.revoke(Some(owner), token)
    }

    fn list_agent_tokens(&self, owner: &str) -> Result<Vec<AgentToken>> {
        let conn = self.conn();
        let mut statement = conn.prepare(
            "SELECT id, name, repository, repository_id, expires_at, revoked, created_at, read_only
             FROM hub_agent_tokens WHERE owner = ?1
             ORDER BY created_at DESC, id DESC LIMIT ?2",
        )?;
        let tokens = statement
            .query_map(params![owner, LIST_LIMIT], |row| {
                Ok(AgentToken {
                    id: row.get(0)?,
                    name: row.get(1)?,
                    repository: row.get(2)?,
                    repository_id: row.get(3)?,
                    expires_at: row.get(4)?,
                    revoked: row.get(5)?,
                    created_at: row.get(6)?,
                    read_only: row.get(7)?,
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(tokens)
    }

    fn list_users(&self) -> Result<Vec<User>> {
        let conn = self.conn();
        let mut statement = conn.prepare(
            "SELECT id, provider, host, forge_user_id, login, disabled FROM hub_users
             ORDER BY created_at, id LIMIT ?1",
        )?;
        let users = statement
            .query_map([LIST_LIMIT], |row| {
                Ok(User {
                    id: row.get(0)?,
                    identity: ForgeIdentity {
                        provider: row.get(1)?,
                        host: row.get(2)?,
                        user_id: row.get(3)?,
                    },
                    login: row.get(4)?,
                    disabled: row.get(5)?,
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(users)
    }

    fn user_by_id(&self, id: &str) -> Result<Option<User>> {
        Ok(self.conn().query_row(
            "SELECT id, provider, host, forge_user_id, login, disabled FROM hub_users WHERE id = ?1",
            [id],
            |row| Ok(User {
                id: row.get(0)?,
                identity: ForgeIdentity { provider: row.get(1)?, host: row.get(2)?, user_id: row.get(3)? },
                login: row.get(4)?, disabled: row.get(5)?,
            }),
        ).optional()?)
    }

    fn user_for_identity(&self, identity: &ForgeIdentity) -> Result<Option<User>> {
        identity.validate()?;
        Ok(self.conn().query_row(
            "SELECT id, provider, host, forge_user_id, login, disabled FROM hub_users WHERE provider=?1 AND host=?2 AND forge_user_id=?3",
            params![identity.provider,identity.host,identity.user_id],
            |row| Ok(User { id:row.get(0)?, identity:ForgeIdentity {provider:row.get(1)?,host:row.get(2)?,user_id:row.get(3)?}, login:row.get(4)?,disabled:row.get(5)? }),
        ).optional()?)
    }

    fn record_sign_in(&self, user: &str, method: &str, admin: bool) -> Result<()> {
        let mut conn = self.conn();
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        if tx
            .query_row("SELECT 1 FROM hub_users WHERE id = ?1", [user], |_| Ok(()))
            .optional()?
            .is_none()
        {
            return Err(HubError::Invalid("unknown user".into()));
        }
        append(&tx, &Event::user_signed_in(user, method, admin)?)?;
        Ok(tx.commit()?)
    }

    fn authenticate(&self, digest: &TokenDigest, now: i64) -> Result<Option<Principal>> {
        Ok(self
            .conn()
            .query_row(
                "SELECT t.id, t.name, t.repository, t.expires_at,
                        u.id, u.provider, u.host, u.forge_user_id, u.login, t.repository_id,
                        t.read_only
                 FROM hub_agent_tokens t JOIN hub_users u ON u.id = t.owner
                 WHERE t.digest = ?1 AND t.revoked = 0 AND t.expires_at > ?2 AND u.disabled = 0",
                params![digest.as_str(), now],
                |row| {
                    Ok(Principal {
                        token_id: row.get(0)?,
                        name: row.get(1)?,
                        repository: row.get(2)?,
                        repository_id: row.get(9)?,
                        expires_at: row.get(3)?,
                        read_only: row.get(10)?,
                        owner: User {
                            id: row.get(4)?,
                            identity: ForgeIdentity {
                                provider: row.get(5)?,
                                host: row.get(6)?,
                                user_id: row.get(7)?,
                            },
                            login: row.get(8)?,
                            disabled: false,
                        },
                    })
                },
            )
            .optional()?)
    }

    fn announce_intent(&self, token: &str, intent: &NewIntent, now: i64) -> Result<Announced> {
        let keys = intent.validate()?;
        let mut conn = self.conn();
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let who = active_token(&tx, token, now)?;
        let announced = announce_in(&tx, &who, intent, &keys, &[], now)?;
        tx.commit()?;
        Ok(announced)
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
        let mut conn = self.conn();
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        active_token(&tx, token, now)?;
        let changed = tx.execute(
            "UPDATE hub_intents SET expires_at = ?3, updated_at = ?4
             WHERE id = ?1 AND token = ?2 AND released_at IS NULL AND expires_at > ?4",
            params![intent, token, expires_at, now],
        )?;
        if changed == 0 {
            return Ok(None);
        }
        tx.execute(
            "UPDATE hub_intent_paths SET expires_at = ?2 WHERE intent = ?1",
            params![intent, expires_at],
        )?;
        append(&tx, &Event::intent_renewed(intent, expires_at)?)?;
        let loaded = load_intents(&tx, &[intent.to_owned()])?;
        tx.commit()?;
        Ok(loaded.into_iter().next())
    }

    fn release_intent(&self, token: &str, intent: &str, now: i64) -> Result<bool> {
        let mut conn = self.conn();
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        active_token(&tx, token, now)?;
        let released = release_in(&tx, token, intent, now)?;
        tx.commit()?;
        Ok(released)
    }

    fn touch_session(&self, token: &str, call: &SessionCall, now: i64) -> Result<Touched> {
        let call = check(call)?;
        let mut conn = self.conn();
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let who = active_token(&tx, token, now)?;
        let touched = touch_in(&tx, &who, &call, now)?;
        tx.commit()?;
        Ok(touched)
    }

    fn end_session(&self, token: &str, session: Option<&str>, now: i64) -> Result<bool> {
        if let Some(session) = session {
            check_name(session)?;
        }
        let mut conn = self.conn();
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        active_token(&tx, token, now)?;
        let name = session_name(&tx, token, session, now)?;
        let Some(row) = session_row(&tx, token, &name)? else {
            return Ok(false);
        };
        if let Some(intent) = &row.intent {
            release_in(&tx, token, intent, now)?;
        }
        tx.execute(
            "DELETE FROM hub_sessions WHERE token = ?1 AND session = ?2",
            params![token, name],
        )?;
        append(&tx, &Event::session_ended(token, &name)?)?;
        tx.commit()?;
        Ok(true)
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
        let mut conn = self.conn();
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let who = active_token(&tx, token, now)?;
        let name = session_name(&tx, token, session, now)?;
        let Some(row) = session_row(&tx, token, &name)? else {
            return Err(HubError::Invalid("unknown session".into()));
        };
        let asked = ask_in(&tx, &who, &name, &row, &key, note.as_deref(), false, now)?;
        tx.commit()?;
        Ok(asked)
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
        let mut conn = self.conn();
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let who = active_token(&tx, token, now)?;
        let name = session_name(&tx, token, session, now)?;
        let found = tx
            .query_row(
                "SELECT h.id, h.requester_token, h.requester_session, h.holder_session,
                        t.name, u.login
                 FROM hub_handoffs h JOIN hub_agent_tokens t ON t.id = h.requester_token
                 JOIN hub_users u ON u.id = t.owner
                 WHERE h.repository = ?1 AND h.path = ?2 AND h.holder_token = ?3
                   AND h.state = 'pending' AND h.created_at > ?4
                   AND h.holder_session = ?5
                 ORDER BY h.created_at, h.id LIMIT 1",
                params![who.repository, key, token, cutoff(now), name],
                |row| {
                    Ok(Request {
                        id: row.get(0)?,
                        requester_token: row.get(1)?,
                        requester_session: row.get(2)?,
                        holder_session: row.get(3)?,
                        agent: row.get(4)?,
                        login: row.get(5)?,
                    })
                },
            )
            .optional()?;
        let Some(request) = found else {
            return Ok(Answered::NoRequest);
        };
        let requester = (request.agent.clone(), request.login.clone());
        // The holder session's intent now: a hold cap may have renewed it since the request.
        let holder_intent = session_row(&tx, token, &request.holder_session)?
            .and_then(|row| row.intent)
            .unwrap_or_default();
        let current = tx
            .query_row(
                "SELECT released_at IS NOT NULL, expires_at, created_at, paths FROM hub_intents
                 WHERE id = ?1",
                [&holder_intent],
                |row| {
                    Ok((
                        row.get::<_, bool>(0)?,
                        row.get::<_, i64>(1)?,
                        row.get::<_, i64>(2)?,
                        row.get::<_, String>(3)?,
                    ))
                },
            )
            .optional()?
            .map(|(released, expires, created, paths)| {
                held(&holder_intent, released, expires, created, now, &paths)
            })
            .transpose()?
            .flatten();
        // A request for a file its holder let go of is moot.
        let Some(current) = current.filter(|held| still_holds(held, &key)) else {
            lapse(&tx, &request.id, now)?;
            tx.commit()?;
            return Ok(Answered::NotHeld);
        };
        if accept {
            let requester_who = match active_token(&tx, &request.requester_token, now) {
                Ok(requester) => requester,
                // The requester's token is gone: nobody to hand the file to.
                Err(HubError::Unauthorized) => {
                    lapse(&tx, &request.id, now)?;
                    tx.commit()?;
                    return Ok(Answered::NoRequest);
                }
                Err(error) => return Err(error),
            };
            // The requester's session ended: nobody to hand the file to.
            let Some(requester_row) =
                session_row(&tx, &request.requester_token, &request.requester_session)?
            else {
                lapse(&tx, &request.id, now)?;
                tx.commit()?;
                return Ok(Answered::NoRequest);
            };
            let left = remaining(&current, &key);
            if left.is_empty() {
                release_in(&tx, token, &current.id, now)?;
            } else {
                tx.execute(
                    "UPDATE hub_intents SET paths = ?2, updated_at = ?3 WHERE id = ?1",
                    params![current.id, serde_json::json!(left).to_string(), now],
                )?;
                tx.execute(
                    "DELETE FROM hub_intent_paths WHERE intent = ?1 AND key = ?2",
                    params![current.id, key],
                )?;
            }
            let call = grant_call(&request.requester_session, &key, Some(&requester_row.via));
            touch_in(&tx, &requester_who, &check(&call)?, now)?;
        }
        tx.execute(
            "UPDATE hub_handoffs SET state = ?2, decided_at = ?3, reply = ?4 WHERE id = ?1",
            params![
                request.id,
                if accept { "granted" } else { "declined" },
                now,
                reason
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
        append(&tx, &event)?;
        tx.commit()?;
        Ok(if accept {
            Answered::Granted { requester }
        } else {
            Answered::Declined { requester }
        })
    }

    fn live_intents(&self, repository: &str, now: i64, limit: usize) -> Result<Vec<Intent>> {
        let conn = self.conn();
        let mut statement = conn.prepare(&format!(
            "{INTENT_SELECT} WHERE i.repository = ?1 AND i.released_at IS NULL AND i.expires_at > ?2
             ORDER BY i.id LIMIT ?3"
        ))?;
        let rows = statement
            .query_map(params![repository, now, limit as i64], intent_row)?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        finish(rows)
    }

    fn notices_after(&self, token: &str, after: i64, limit: usize) -> Result<Vec<HubEvent>> {
        let conn = self.conn();
        let mut statement = conn.prepare(
            "SELECT seq, id, at, kind, subject, data FROM hub_events
             WHERE seq > ?2 AND kind IN ('intent_notice', 'pr_notice')
               AND subject IN (SELECT id FROM hub_intents WHERE token = ?1)
             ORDER BY seq LIMIT ?3",
        )?;
        let events = statement
            .query_map(params![token, after, event_limit(limit)], |row| {
                Ok(HubEvent {
                    seq: row.get(0)?,
                    id: row.get(1)?,
                    at: row.get(2)?,
                    kind: row.get(3)?,
                    subject: row.get(4)?,
                    data: row.get(5)?,
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(events)
    }

    fn live_sessions(&self, who: &Principal, now: i64, limit: usize) -> Result<Vec<SessionView>> {
        let conn = self.conn();
        let mut statement = conn.prepare(&format!(
            "SELECT s.token, t.name, u.login, s.intent, s.task, s.via, s.created_at, s.updated_at,
                    s.session
             FROM hub_sessions s JOIN hub_agent_tokens t ON t.id = s.token
             JOIN hub_users u ON u.id = t.owner
             WHERE {TOKEN_SCOPE} AND t.revoked = 0 AND t.expires_at > ?5 AND u.disabled = 0
               AND s.updated_at >= ?6
             ORDER BY s.updated_at DESC, s.token, s.session LIMIT ?7"
        ))?;
        let sessions = statement
            .query_map(
                params![
                    who.repository,
                    who.repository_id,
                    who.owner.identity.host,
                    who.owner.identity.provider,
                    now,
                    now - SESSION_TTL_SECONDS,
                    limit as i64
                ],
                |row| {
                    let (token, session): (String, String) = (row.get(0)?, row.get(8)?);
                    Ok(SessionView {
                        key: session_key(&token, &session),
                        token,
                        agent: row.get(1)?,
                        owner: row.get(2)?,
                        intent: row.get(3)?,
                        task: row.get(4)?,
                        via: row.get(5)?,
                        created_at: row.get(6)?,
                        updated_at: row.get(7)?,
                    })
                },
            )?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(sessions)
    }

    fn repository_events(
        &self,
        who: &Principal,
        before: Option<i64>,
        limit: usize,
    ) -> Result<Vec<ActivityEvent>> {
        let conn = self.conn();
        // Driven from the subject index: the scope's token and intent ids, not every event.
        let mut statement = conn.prepare(&format!(
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
             AND (?6 IS NULL OR e.seq < ?6)
             ORDER BY e.seq DESC LIMIT ?5"
        ))?;
        let events = statement
            .query_map(
                params![
                    who.repository,
                    who.repository_id,
                    who.owner.identity.host,
                    who.owner.identity.provider,
                    event_limit(limit),
                    before
                ],
                |row| {
                    Ok(ActivityEvent {
                        event: HubEvent {
                            seq: row.get(0)?,
                            id: row.get(1)?,
                            at: row.get(2)?,
                            kind: row.get(3)?,
                            subject: row.get(4)?,
                            data: row.get(5)?,
                        },
                        token: row.get(6)?,
                        agent: row.get(7)?,
                        owner: row.get(8)?,
                    })
                },
            )?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(events)
    }

    fn insert_connection(&self, new: &NewConnection, actor: &str) -> Result<ForgeConnection> {
        new.validate()?;
        let mut conn = self.conn();
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let inserted = tx.execute(
            "INSERT INTO hub_connections
                 (id, provider, host, kind, app_id, app_slug, acts_as, status, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, 'enabled', ?8) ON CONFLICT DO NOTHING",
            params![
                new.id,
                new.provider,
                new.host,
                new.kind,
                new.app_id,
                new.app_slug,
                new.acts_as,
                unix_now()
            ],
        )?;
        if inserted == 0 {
            return Err(HubError::Invalid("connection id is in use".into()));
        }
        append(&tx, &Event::connection_created(new, actor)?)?;
        let created = connection_by_id(&tx, &new.id)?;
        tx.commit()?;
        created.ok_or_else(|| HubError::Unavailable("connection not found".into()))
    }

    fn connections(&self) -> Result<Vec<ForgeConnection>> {
        let conn = self.conn();
        let mut statement = conn.prepare(&format!(
            "{CONNECTION_SELECT} ORDER BY created_at, id LIMIT ?1"
        ))?;
        let rows = statement
            .query_map([LIST_LIMIT], connection_row)?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    fn connection(&self, id: &str) -> Result<Option<ForgeConnection>> {
        connection_by_id(&self.conn(), id)
    }

    fn pat_connection(&self, host: &str) -> Result<Option<ForgeConnection>> {
        Ok(self
            .conn()
            .query_row(
                &format!("{CONNECTION_SELECT} WHERE kind = 'pat' AND host = ?1 ORDER BY created_at, id LIMIT 1"),
                [host],
                connection_row,
            )
            .optional()?)
    }

    fn set_connection_status(&self, id: &str, status: &str, detail: Option<&str>) -> Result<()> {
        validate_status(status, detail)?;
        let mut conn = self.conn();
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let changed = tx.execute(
            "UPDATE hub_connections SET status = ?2, status_detail = ?3
             WHERE id = ?1 AND (status <> ?2 OR status_detail IS NOT ?3)",
            params![id, status, detail],
        )?;
        if changed > 0 {
            append(&tx, &Event::connection_status(id, status, detail)?)?;
        } else if connection_by_id(&tx, id)?.is_none() {
            return Err(HubError::Invalid("unknown connection".into()));
        }
        Ok(tx.commit()?)
    }

    fn set_connection_outbound(&self, id: &str, enabled: bool, actor: &str) -> Result<()> {
        let mut conn = self.conn();
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let changed = tx.execute(
            "UPDATE hub_connections SET outbound = ?2 WHERE id = ?1 AND outbound <> ?2",
            params![id, enabled],
        )?;
        if changed > 0 {
            append(&tx, &Event::connection_outbound(id, enabled, actor)?)?;
        } else if connection_by_id(&tx, id)?.is_none() {
            return Err(HubError::Invalid("unknown connection".into()));
        }
        Ok(tx.commit()?)
    }

    fn get_setting(&self, key: &str) -> Result<Option<String>> {
        Ok(self
            .conn()
            .query_row(
                "SELECT value FROM hub_settings WHERE key = ?1",
                params![key],
                |row| row.get(0),
            )
            .optional()?)
    }

    fn set_setting(&self, key: &str, value: &str, actor: &str) -> Result<()> {
        self.conn().execute(
            "INSERT INTO hub_settings (key, value, updated_at, updated_by) VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT (key) DO UPDATE SET value = ?2, updated_at = ?3, updated_by = ?4",
            params![key, value, unix_now(), actor],
        )?;
        Ok(())
    }

    fn set_next_poll(&self, id: &str, at: i64) -> Result<()> {
        self.conn().execute(
            "UPDATE hub_connections SET next_poll_at = ?2 WHERE id = ?1",
            params![id, at],
        )?;
        Ok(())
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
        let mut conn = self.conn();
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let connected_host = tx
            .query_row(
                "SELECT host FROM hub_connections WHERE id = ?1",
                [connection],
                |row| row.get::<_, String>(0),
            )
            .optional()?;
        if connected_host.as_deref() != Some(host) {
            return Err(HubError::Invalid("unknown connection for that host".into()));
        }
        let existing = tx
            .query_row(
                &format!("{REPOSITORY_SELECT} WHERE host = ?1 AND repository_id = ?2"),
                params![host, repository_id],
                repository_row,
            )
            .optional()?;
        let repo = match existing {
            None => {
                let repo = ConnectedRepository {
                    id: new_id("rep")?,
                    connection: connection.to_owned(),
                    host: host.to_owned(),
                    repository_id,
                    full_name: full_name.to_owned(),
                    node_id: node_id.map(str::to_owned),
                    installation_id,
                    pulls_etag: None,
                };
                tx.execute(
                    "INSERT INTO hub_repositories
                         (id, connection, host, repository_id, full_name, node_id, installation_id, created_at)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                    params![repo.id, connection, host, repository_id, full_name, node_id, installation_id, unix_now()],
                )?;
                append(&tx, &Event::repository_connected(&repo)?)?;
                repo
            }
            Some(mut repo) if repo.connection == connection => {
                if repo.full_name != full_name {
                    let previous = std::mem::replace(&mut repo.full_name, full_name.to_owned());
                    append(&tx, &Event::repository_renamed(&repo, &previous)?)?;
                }
                // Not events: they say how to reach the repository, not what it is.
                repo.node_id = node_id.map(str::to_owned).or(repo.node_id);
                repo.installation_id = installation_id.or(repo.installation_id);
                tx.execute(
                    "UPDATE hub_repositories SET full_name = ?2, node_id = ?3, installation_id = ?4 WHERE id = ?1",
                    params![repo.id, repo.full_name, repo.node_id, repo.installation_id],
                )?;
                repo
            }
            Some(repo) => repo,
        };
        tx.commit()?;
        Ok(repo)
    }

    fn repositories(&self, connection: &str) -> Result<Vec<ConnectedRepository>> {
        self.repository_query(
            "WHERE connection = ?1 ORDER BY created_at, id LIMIT ?2",
            params![connection, LIST_LIMIT],
        )
    }

    fn repository_named(&self, host: &str, full_name: &str) -> Result<Option<ConnectedRepository>> {
        Ok(self
            .repository_query(
                "WHERE host = ?1 AND lower(full_name) = lower(?2) ORDER BY created_at, id LIMIT 1",
                params![host, full_name],
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
                "WHERE host = ?1 AND repository_id = ?2",
                params![host, repository_id],
            )?
            .into_iter()
            .next())
    }

    fn repositories_on(&self, host: &str, limit: usize) -> Result<Vec<ConnectedRepository>> {
        self.repository_query(
            "WHERE host = ?1 ORDER BY full_name, id LIMIT ?2",
            params![host, event_limit(limit)],
        )
    }

    fn set_pulls_etag(&self, repo: &str, etag: Option<&str>) -> Result<()> {
        self.conn().execute(
            "UPDATE hub_repositories SET pulls_etag = ?2 WHERE id = ?1",
            params![repo, etag],
        )?;
        Ok(())
    }

    fn record_delivery(&self, connection: &str, delivery: &str, now: i64) -> Result<bool> {
        let conn = self.conn();
        conn.execute(
            "DELETE FROM hub_webhook_deliveries WHERE received_at < ?1",
            [now - DELIVERY_KEEP_SECONDS],
        )?;
        let inserted = conn.execute(
            "INSERT INTO hub_webhook_deliveries (connection, delivery, received_at)
             VALUES (?1, ?2, ?3) ON CONFLICT DO NOTHING",
            params![connection, delivery, now],
        )?;
        Ok(inserted > 0)
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
        let mut conn = self.conn();
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let repository = repository_by_rep(&tx, repo)?;
        let existing = tx
            .query_row(
                "SELECT data, observed_at, deleted FROM hub_observations
                 WHERE repository = ?1 AND kind = ?2 AND key = ?3",
                params![repo, kind, key],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, i64>(1)?,
                        row.get::<_, bool>(2)?,
                    ))
                },
            )
            .optional()?;
        let previous = match existing {
            Some((_, at, _)) if at > observed_at => return Ok(Observed::Stale),
            Some((old, _, false)) if old == data => {
                tx.execute(
                    "UPDATE hub_observations SET observed_at = ?4
                     WHERE repository = ?1 AND kind = ?2 AND key = ?3",
                    params![repo, kind, key, observed_at],
                )?;
                tx.commit()?;
                return Ok(Observed::Unchanged);
            }
            Some((old, _, deleted)) => (!deleted).then_some(old),
            None => None,
        };
        tx.execute(
            "INSERT INTO hub_observations (repository, kind, key, data, observed_at, deleted)
             VALUES (?1, ?2, ?3, ?4, ?5, 0)
             ON CONFLICT (repository, kind, key) DO UPDATE SET data = excluded.data,
                 observed_at = excluded.observed_at, deleted = 0",
            params![repo, kind, key, data, observed_at],
        )?;
        append(&tx, &Event::observation(observed_kind, &repository, data)?)?;
        tx.commit()?;
        Ok(Observed::Changed { previous })
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
        let mut conn = self.conn();
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let repository = repository_by_rep(&tx, repo)?;
        let existing = tx
            .query_row(
                "SELECT observed_at, deleted FROM hub_observations
                 WHERE repository = ?1 AND kind = ?2 AND key = ?3",
                params![repo, kind, key],
                |row| Ok((row.get::<_, i64>(0)?, row.get::<_, bool>(1)?)),
            )
            .optional()?;
        let gone = match existing {
            Some((at, _)) if at > observed_at => return Ok(false),
            Some((_, deleted)) => deleted,
            // Never seen: leave a marker so an older read cannot add it later.
            None => true,
        };
        tx.execute(
            "INSERT INTO hub_observations (repository, kind, key, data, observed_at, deleted)
             VALUES (?1, ?2, ?3, ?4, ?5, 1)
             ON CONFLICT (repository, kind, key) DO UPDATE SET data = excluded.data,
                 observed_at = excluded.observed_at, deleted = 1",
            params![repo, kind, key, data, observed_at],
        )?;
        // An already-forgotten row only moves its read time forward.
        if !gone {
            append(&tx, &Event::observation(gone_kind, &repository, data)?)?;
        }
        tx.commit()?;
        Ok(!gone)
    }

    fn observations(&self, repo: &str, kind: &str) -> Result<Vec<Observation>> {
        observation_kinds(kind)?;
        let conn = self.conn();
        let mut statement = conn.prepare(
            "SELECT kind, key, data, observed_at FROM hub_observations
             WHERE repository = ?1 AND kind = ?2 AND deleted = 0 ORDER BY key LIMIT ?3",
        )?;
        let rows = statement
            .query_map(params![repo, kind, LIST_LIMIT], |row| {
                Ok(Observation {
                    kind: row.get(0)?,
                    key: row.get(1)?,
                    data: row.get(2)?,
                    observed_at: row.get(3)?,
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    fn prune_tombstones(&self, before: i64) -> Result<usize> {
        Ok(self.conn().execute(
            "DELETE FROM hub_observations WHERE deleted = 1 AND observed_at < ?1",
            [before],
        )?)
    }

    fn outbound(&self, repo: &str, number: i64) -> Result<Option<Outbound>> {
        Ok(self
            .conn()
            .query_row(
                "SELECT comment_id, body_sha256, status_sha, status_description
                 FROM hub_outbound WHERE repository = ?1 AND number = ?2",
                params![repo, number],
                |row| {
                    Ok(Outbound {
                        comment_id: row.get(0)?,
                        body_sha256: row.get(1)?,
                        status_sha: row.get(2)?,
                        status_description: row.get(3)?,
                    })
                },
            )
            .optional()?)
    }

    fn set_outbound_comment(
        &self,
        repo: &str,
        number: i64,
        comment_id: i64,
        body_sha256: &str,
    ) -> Result<()> {
        let mut conn = self.conn();
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let repository = repository_by_rep(&tx, repo)?;
        tx.execute(
            "INSERT INTO hub_outbound (repository, number, comment_id, body_sha256)
             VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT (repository, number) DO UPDATE SET
                 comment_id = excluded.comment_id, body_sha256 = excluded.body_sha256",
            params![repo, number, comment_id, body_sha256],
        )?;
        append(
            &tx,
            &Event::pr_comment_written(&repository, number, comment_id)?,
        )?;
        Ok(tx.commit()?)
    }

    fn set_outbound_status(
        &self,
        repo: &str,
        number: i64,
        sha: &str,
        description: &str,
    ) -> Result<()> {
        let mut conn = self.conn();
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let repository = repository_by_rep(&tx, repo)?;
        tx.execute(
            "INSERT INTO hub_outbound (repository, number, status_sha, status_description)
             VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT (repository, number) DO UPDATE SET
                 status_sha = excluded.status_sha, status_description = excluded.status_description",
            params![repo, number, sha, description],
        )?;
        append(
            &tx,
            &Event::pr_status_written(&repository, number, sha, description)?,
        )?;
        Ok(tx.commit()?)
    }

    fn forge_check(&self, repo: &str, number: i64, context: &str) -> Result<Option<ForgeCheck>> {
        Ok(self
            .conn()
            .query_row(
                "SELECT head_sha, state, binding_digest, description, phase, generation, updated_at
                 FROM hub_forge_checks WHERE repository = ?1 AND number = ?2 AND context = ?3",
                params![repo, number, context],
                |row| {
                    Ok(ForgeCheck {
                        head_sha: row.get(0)?,
                        state: row.get(1)?,
                        binding_digest: row.get(2)?,
                        description: row.get(3)?,
                        phase: row.get(4)?,
                        generation: row.get(5)?,
                        updated_at: row.get(6)?,
                    })
                },
            )
            .optional()?)
    }

    fn forge_checks_showing_success(
        &self,
        repo: &str,
        full_name: &str,
        enabled: &[String],
        limit: usize,
    ) -> Result<Vec<(i64, String, ForgeCheck)>> {
        let conn = self.conn();
        let mut statement = conn.prepare(
            "SELECT number, context, head_sha, state, binding_digest, description, phase, generation,
                    updated_at
             FROM hub_forge_checks
             WHERE repository = ?1 AND (state = 'success' OR phase = 'attempting')
               AND context NOT IN (SELECT value FROM json_each(?3))
               AND (SELECT q.merge_commit FROM hub_publications q
                    WHERE lower(q.repository) = lower(?2) AND q.pr_number = hub_forge_checks.number
                    ORDER BY q.created_at DESC, q.id DESC LIMIT 1) IS NULL
             ORDER BY number, context LIMIT ?4",
        )?;
        let enabled =
            serde_json::to_string(enabled).map_err(|e| HubError::Invalid(e.to_string()))?;
        let rows = statement
            .query_map(params![repo, full_name, enabled, limit as i64], |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    ForgeCheck {
                        head_sha: row.get(2)?,
                        state: row.get(3)?,
                        binding_digest: row.get(4)?,
                        description: row.get(5)?,
                        phase: row.get(6)?,
                        generation: row.get(7)?,
                        updated_at: row.get(8)?,
                    },
                ))
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
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
        let conn = self.conn();
        let (changed, generation) = match expected {
            None => (
                conn.execute(
                    "INSERT INTO hub_forge_checks (repository, number, context, head_sha, state,
                         binding_digest, description, phase, generation, updated_at)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, 'attempting', 1, ?8)
                     ON CONFLICT (repository, number, context) DO NOTHING",
                    params![
                        repo,
                        number,
                        context,
                        new.head_sha,
                        new.state,
                        new.binding_digest,
                        new.description,
                        now
                    ],
                )?,
                1,
            ),
            Some(expected) => (
                conn.execute(
                    "UPDATE hub_forge_checks SET head_sha = ?5, state = ?6, binding_digest = ?7,
                         description = ?8, phase = 'attempting', generation = generation + 1,
                         updated_at = ?9
                     WHERE repository = ?1 AND number = ?2 AND context = ?3 AND generation = ?4",
                    params![
                        repo,
                        number,
                        context,
                        expected,
                        new.head_sha,
                        new.state,
                        new.binding_digest,
                        new.description,
                        now
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
        Ok(self.conn().execute(
            "UPDATE hub_forge_checks SET phase = ?6, generation = generation + 1,
                 updated_at = ?5
             WHERE repository = ?1 AND number = ?2 AND context = ?3 AND generation = ?4",
            params![repo, number, context, generation, now, phase],
        )? == 1)
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
        let mut conn = self.conn();
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        repository_by_rep(&tx, repo)?;
        tx.execute(
            "DELETE FROM hub_merge_groups
             WHERE repository = ?1 AND ended_reason IS NOT NULL AND ended_at < ?2",
            params![repo, now.saturating_sub(MERGE_GROUP_KEEP_SECONDS)],
        )?;
        tx.execute(
            "INSERT INTO hub_merge_groups (repository, head_sha, head_ref, base_ref, base_sha,
                 pr_number, pr_head_sha, context, ended_reason, observed_at, ended_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, CASE WHEN ?9 IS NULL THEN NULL ELSE ?10 END, ?10)
             ON CONFLICT (repository, head_sha) DO NOTHING",
            params![
                repo,
                group.head_sha,
                group.head_ref,
                group.base_ref,
                group.base_sha,
                group.pr_number,
                group.pr_head_sha,
                group.context,
                ended,
                now
            ],
        )?;
        if let Some(reason) = ended {
            tx.execute(
                "UPDATE hub_merge_groups SET ended_reason = ?3, ended_at = ?4, updated_at = ?4
                 WHERE repository = ?1 AND head_sha = ?2 AND ended_reason IS NULL",
                params![repo, group.head_sha, reason, now],
            )?;
        }
        let stored = tx.query_row(
            &format!(
                "SELECT {MERGE_GROUP_COLUMNS} FROM hub_merge_groups
                 WHERE repository = ?1 AND head_sha = ?2"
            ),
            params![repo, group.head_sha],
            merge_group_row,
        )?;
        tx.commit()?;
        Ok(stored)
    }

    fn merge_group(&self, repo: &str, head_sha: &str) -> Result<Option<MergeGroup>> {
        if !is_lower_hex(head_sha, 40) {
            return Err(HubError::Invalid("merge group is out of bounds".into()));
        }
        Ok(self
            .conn()
            .query_row(
                &format!(
                    "SELECT {MERGE_GROUP_COLUMNS} FROM hub_merge_groups
                     WHERE repository = ?1 AND head_sha = ?2"
                ),
                params![repo, head_sha],
                merge_group_row,
            )
            .optional()?)
    }

    fn active_merge_groups(&self, repo: &str, limit: usize) -> Result<Vec<MergeGroup>> {
        let conn = self.conn();
        let mut statement = conn.prepare(&format!(
            "SELECT {MERGE_GROUP_COLUMNS} FROM hub_merge_groups
             WHERE repository = ?1 AND ended_reason IS NULL
             ORDER BY observed_at, head_sha LIMIT ?2"
        ))?;
        let rows = statement
            .query_map(params![repo, limit as i64], merge_group_row)?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    fn merge_groups_for_pull(
        &self,
        repo: &str,
        pr_number: i64,
        limit: usize,
    ) -> Result<Vec<MergeGroup>> {
        let conn = self.conn();
        let mut statement = conn.prepare(&format!(
            "SELECT {MERGE_GROUP_COLUMNS} FROM hub_merge_groups
             WHERE repository = ?1 AND pr_number = ?2
             ORDER BY observed_at DESC, head_sha LIMIT ?3"
        ))?;
        let rows = statement
            .query_map(params![repo, pr_number, limit as i64], merge_group_row)?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    fn end_merge_group(&self, repo: &str, head_sha: &str, reason: &str, now: i64) -> Result<bool> {
        if !is_lower_hex(head_sha, 40) {
            return Err(HubError::Invalid("merge group is out of bounds".into()));
        }
        validate_group_note(reason)?;
        Ok(self.conn().execute(
            "UPDATE hub_merge_groups SET ended_reason = ?3, ended_at = ?4, updated_at = ?4
             WHERE repository = ?1 AND head_sha = ?2 AND ended_reason IS NULL",
            params![repo, head_sha, reason, now],
        )? == 1)
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
        Ok(
            if self.conn().execute(
                "UPDATE hub_merge_groups SET state = ?4, binding_digest = ?5, description = ?6,
                     phase = 'attempting', generation = generation + 1, updated_at = ?7
                 WHERE repository = ?1 AND head_sha = ?2 AND generation = ?3
                     AND ended_reason IS NULL",
                params![
                    repo,
                    head_sha,
                    expected,
                    new.state,
                    new.binding_digest,
                    new.description,
                    now
                ],
            )? == 1
            {
                ForgeCheckReserve::Reserved(expected.saturating_add(1))
            } else {
                ForgeCheckReserve::Conflict
            },
        )
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
        Ok(self.conn().execute(
            "UPDATE hub_merge_groups SET phase = ?4, generation = generation + 1, updated_at = ?5
             WHERE repository = ?1 AND head_sha = ?2 AND generation = ?3",
            params![repo, head_sha, generation, phase, now],
        )? == 1)
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
        self.conn().execute(
            "UPDATE hub_merge_groups SET ruleset_requirement = ?3, status_source = ?4
             WHERE repository = ?1 AND head_sha = ?2",
            params![repo, head_sha, ruleset_requirement, status_source],
        )?;
        Ok(())
    }

    fn observe_external_run(&self, repo: &str, run: &ExternalRun) -> Result<ExternalObserved> {
        run.validate()?;
        let mut conn = self.conn();
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        repository_by_rep(&tx, repo)?;
        let stored = tx
            .query_row(
                &format!(
                    "SELECT {EXTERNAL_RUN_COLUMNS} FROM hub_external_runs
                     WHERE repository = ?1 AND provider = ?2 AND kind = ?3 AND run_id = ?4
                         AND attempt = ?5"
                ),
                params![
                    repo,
                    run.provider,
                    run.kind.as_str(),
                    run.run_id,
                    run.attempt
                ],
                external_run_row,
            )
            .optional()?
            .transpose()?;
        match &stored {
            Some(stored) if stored == run => return Ok(ExternalObserved::Unchanged),
            Some(stored) if !supersedes(stored, run) => return Ok(ExternalObserved::Stale),
            _ => {}
        }
        tx.execute(
            "INSERT INTO hub_external_runs (repository, provider, kind, run_id, attempt,
                 commit_sha, branch, environment, name, outcome, provider_updated_at, html_url)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)
             ON CONFLICT (repository, provider, kind, run_id, attempt) DO UPDATE SET
                 commit_sha = excluded.commit_sha, branch = excluded.branch,
                 environment = excluded.environment, name = excluded.name,
                 outcome = excluded.outcome,
                 provider_updated_at = excluded.provider_updated_at,
                 html_url = excluded.html_url",
            params![
                repo,
                run.provider,
                run.kind.as_str(),
                run.run_id,
                run.attempt,
                run.commit_sha,
                run.branch,
                run.environment,
                run.name,
                run.outcome.as_str(),
                run.provider_updated_at,
                run.html_url
            ],
        )?;
        append(&tx, &Event::external_run_observed(repo, run)?)?;
        tx.commit()?;
        Ok(ExternalObserved::Changed)
    }

    fn external_runs(
        &self,
        repo: &str,
        commit_sha: &str,
        kind: ExternalKind,
        limit: usize,
    ) -> Result<Vec<ExternalRun>> {
        let conn = self.conn();
        let mut statement = conn.prepare(&format!(
            "SELECT {EXTERNAL_RUN_COLUMNS} FROM hub_external_runs
             WHERE repository = ?1 AND commit_sha = ?2 AND kind = ?3
             ORDER BY COALESCE(environment, ''), run_id, attempt LIMIT ?4"
        ))?;
        statement
            .query_map(
                params![repo, commit_sha, kind.as_str(), limit as i64],
                external_run_row,
            )?
            .collect::<std::result::Result<Vec<_>, _>>()?
            .into_iter()
            .collect()
    }

    fn mark_external_reconciled(
        &self,
        repo: &str,
        commit_sha: &str,
        at: i64,
        complete: bool,
    ) -> Result<()> {
        self.conn().execute(
            "INSERT INTO hub_external_reconciles (repository, commit_sha, reconciled_at, complete)
             VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT (repository, commit_sha) DO UPDATE SET
                 reconciled_at = excluded.reconciled_at, complete = excluded.complete",
            params![repo, commit_sha, at, complete],
        )?;
        Ok(())
    }

    fn external_reconciled(&self, repo: &str, commit_sha: &str) -> Result<Option<(i64, bool)>> {
        Ok(self
            .conn()
            .query_row(
                "SELECT reconciled_at, complete FROM hub_external_reconciles
                 WHERE repository = ?1 AND commit_sha = ?2",
                params![repo, commit_sha],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?)
    }

    fn merged_publications(
        &self,
        repository: &str,
        after: &str,
        oldest: i64,
        limit: usize,
    ) -> Result<Vec<Publication>> {
        let conn = self.conn();
        let mut statement = conn.prepare(&format!(
            "{PUBLICATION_SELECT} WHERE lower(p.repository) = lower(?1) AND p.id > ?2
             AND p.merge_commit IS NOT NULL AND p.updated_at >= ?3 ORDER BY p.id LIMIT ?4"
        ))?;
        let rows = statement
            .query_map(
                params![repository, after, oldest, limit as i64],
                publication_row,
            )?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    fn append_notices(&self, kind: &'static str, notices: &[(String, String)]) -> Result<()> {
        let events = notices
            .iter()
            .map(|(subject, data)| Event::notice(kind, subject, data))
            .collect::<Result<Vec<_>>>()?;
        let mut conn = self.conn();
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        for event in &events {
            append(&tx, event)?;
        }
        Ok(tx.commit()?)
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
        let mut conn = self.conn();
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let who = active_token(&tx, token, now)?;
        new.validate(&who.repository)?;
        let m = &new.manifest;
        let manifest = m.canonical();
        if let Some(found) = publication_by_id(&tx, &new.id)? {
            return Ok(if found.authorization_binding.as_deref() == binding {
                Create::of_existing(found, &who.repository, &manifest)
            } else {
                Create::Conflict
            });
        }
        let head: Option<String> = tx
            .query_row(
                "SELECT publication FROM hub_candidate_heads WHERE repository=?1 AND branch=?2",
                params![who.repository, m.branch],
                |r| r.get(0),
            )
            .optional()?;
        let previous = head
            .as_ref()
            .map(|id| publication_by_id(&tx, id))
            .transpose()?
            .flatten();
        if head.is_some() && previous.is_none() {
            return Err(HubError::Unavailable(
                "candidate lineage record missing".into(),
            ));
        }
        if binding.is_none()
            && previous
                .as_ref()
                .is_some_and(|p| p.authorization_binding.is_some())
        {
            return Ok(Create::Conflict);
        }
        if !super::publish::candidate_continuation(new, previous.as_ref(), &who.owner.id)? {
            return Ok(Create::Conflict);
        }
        if m.candidate.is_some() && head.is_none() {
            let occupied: bool = tx.query_row(
                "SELECT EXISTS(SELECT 1 FROM hub_publications WHERE repository=?1 AND branch=?2)",
                params![who.repository, m.branch],
                |r| r.get(0),
            )?;
            if occupied {
                return Ok(Create::Conflict);
            }
        }
        if m.candidate.is_some() {
            tx.execute("INSERT INTO hub_candidate_heads VALUES(?1,?2,?3) ON CONFLICT(repository,branch) DO UPDATE SET publication=excluded.publication", params![who.repository, m.branch, new.id])?;
        }
        let unfinished: i64 = tx.query_row(
            "SELECT count(*) FROM hub_publications
             WHERE token = ?1 AND pr_number IS NULL AND branch_state <> 'rejected'",
            [token],
            |row| row.get(0),
        )?;
        if unfinished >= MAX_UNFINISHED {
            return Ok(Create::TooMany);
        }
        tx.execute(
            "INSERT INTO hub_publications (id, repository, token, revision, event_seq, commit_sha,
                 branch, base, manifest, created_at, updated_at, authorization_binding)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?10, ?11)",
            params![
                new.id,
                who.repository,
                token,
                m.revision,
                m.event_seq,
                m.commit,
                m.branch,
                m.base,
                manifest,
                now,
                binding
            ],
        )?;
        append(&tx, &Event::publication_requested(&who.repository, new)?)?;
        let found = publication_by_id(&tx, &new.id)?
            .ok_or_else(|| HubError::Unavailable("publication not found after insert".into()))?;
        tx.commit()?;
        Ok(if found.authorization_binding.as_deref() == binding {
            Create::of_existing(found, &who.repository, &manifest)
        } else {
            Create::Conflict
        })
    }

    fn publication(&self, repository: &str, id: &str) -> Result<Option<Publication>> {
        Ok(publication_by_id(&self.conn(), id)?.filter(|p| p.repository == repository))
    }

    fn publications(
        &self,
        repository: &str,
        before: Option<&str>,
        limit: usize,
    ) -> Result<Vec<Publication>> {
        let conn = self.conn();
        let mut statement = conn.prepare(&format!(
            "{PUBLICATION_SELECT} WHERE p.repository = ?1
             AND (?2 IS NULL OR (p.created_at, p.id) < (SELECT q.created_at, q.id
                 FROM hub_publications q WHERE q.repository = ?1 AND q.id = ?2))
             ORDER BY p.created_at DESC, p.id DESC LIMIT ?3"
        ))?;
        let rows = statement
            .query_map(params![repository, before, limit as i64], publication_row)?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    fn publication_search(
        &self,
        repository: &str,
        before: Option<&str>,
        search: &str,
        limit: usize,
    ) -> Result<PublicationSearch> {
        let conn = self.conn();
        let window = conn
            .prepare(
                "SELECT p.created_at, p.id FROM hub_publications p WHERE p.repository = ?1
                 AND (?2 IS NULL OR (p.created_at, p.id) < (SELECT q.created_at, q.id
                     FROM hub_publications q WHERE q.repository = ?1 AND q.id = ?2))
                 ORDER BY p.created_at DESC, p.id DESC LIMIT ?3",
            )?
            .query_map(
                params![repository, before, SEARCH_SCAN_ROWS as i64],
                |row| Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?)),
            )?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        let Some((oldest_at, oldest_id)) = window.last().cloned() else {
            return Ok(PublicationSearch {
                rows: vec![],
                searched_to: None,
                window_full: false,
            });
        };
        let pattern = like_pattern(search);
        let number = search_number(search);
        let mut statement = conn.prepare(&format!(
            "{PUBLICATION_SELECT} WHERE p.repository = ?1
             AND (?2 IS NULL OR (p.created_at, p.id) < (SELECT q.created_at, q.id
                 FROM hub_publications q WHERE q.repository = ?1 AND q.id = ?2))
             AND (p.created_at, p.id) >= (?6, ?7)
             AND (p.pr_number = ?5
                 OR lower(p.branch) LIKE ?4 ESCAPE '\\' OR lower(p.base) LIKE ?4 ESCAPE '\\'
                 OR lower(p.id) LIKE ?4 ESCAPE '\\' OR lower(u.login) LIKE ?4 ESCAPE '\\'
                 OR lower(t.name) LIKE ?4 ESCAPE '\\'
                 OR lower(json_extract(CASE WHEN length(p.manifest) <= 2097152 AND json_valid(p.manifest)
                     THEN p.manifest END,
                     '$.title')) LIKE ?4 ESCAPE '\\')
             ORDER BY p.created_at DESC, p.id DESC LIMIT ?3"
        ))?;
        let rows = statement
            .query_map(
                params![
                    repository,
                    before,
                    limit as i64,
                    pattern,
                    number,
                    oldest_at,
                    oldest_id
                ],
                publication_row,
            )?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(PublicationSearch {
            rows,
            searched_to: Some(oldest_id),
            window_full: window.len() == SEARCH_SCAN_ROWS,
        })
    }

    fn pull_publications(
        &self,
        repository: &str,
        after: i64,
        limit: usize,
    ) -> Result<Vec<Publication>> {
        let conn = self.conn();
        let mut statement = conn.prepare(&format!(
            "{PUBLICATION_SELECT} WHERE lower(p.repository) = lower(?1) AND p.pr_number > ?2
             AND p.merge_commit IS NULL
             AND p.id = (SELECT q.id FROM hub_publications q
                 WHERE lower(q.repository) = lower(?1) AND q.pr_number = p.pr_number
                 ORDER BY q.created_at DESC, q.id DESC LIMIT 1)
             ORDER BY p.pr_number LIMIT ?3"
        ))?;
        let rows = statement
            .query_map(params![repository, after, limit as i64], publication_row)?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    fn update_publication(&self, id: &str, update: &Update, now: i64) -> Result<Publication> {
        let mut conn = self.conn();
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let mut p = publication_by_id(&tx, id)?
            .ok_or_else(|| HubError::Invalid("unknown publication".into()))?;
        if matches!(update, Update::PullRequestAttempt) && p.event_seq.is_none() {
            let head: Option<String> = tx
                .query_row(
                    "SELECT publication FROM hub_candidate_heads WHERE repository=?1 AND branch=?2",
                    params![p.repository, p.branch],
                    |r| r.get(0),
                )
                .optional()?;
            if head.as_deref() != Some(id) {
                return Err(HubError::Invalid(
                    "candidate was superseded before PR attempt".into(),
                ));
            }
        }
        let (changed, event) = p.apply(update, now)?;
        if changed {
            tx.execute(
                "UPDATE hub_publications SET branch_state = ?2, branch_message = ?3, pr_number = ?4,
                     pr_node_id = ?5, pr_url = ?6, merge_commit = ?7, error = ?8, updated_at = ?9
                 WHERE id = ?1",
                params![
                    id, p.branch_state, p.branch_message, p.pr_number, p.pr_node_id, p.pr_url,
                    p.merge_commit, p.error, p.updated_at
                ],
            )?;
        }
        if let Some(event) = event {
            append(&tx, &event)?;
        }
        tx.commit()?;
        Ok(p)
    }

    fn work_start(&self, token: &str, start: &WorkStart, now: i64) -> Result<WorkStartResult> {
        let mut conn = self.conn();
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let who = work_who(&tx, token, now)?;
        let result = start_in(&tx, &who, start, now)?;
        tx.commit()?;
        Ok(result)
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
        let mut conn = self.conn();
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let who = work_who(&tx, token, now)?;
        if who.read_only {
            return Err(HubError::Unauthorized);
        }
        let mut planned = start.clone();
        planned.request_key = Some("automatic-session-start".into());
        let planned_scope = planned.validate(&who)?;
        if let Some(item) = unfinished_for_session(&tx, &who, &start.session, now)? {
            tx.commit()?;
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
        let result = start_in(&tx, &who, &created, now)?;
        tx.commit()?;
        Ok(result)
    }

    fn work_checkpoint(
        &self,
        token: &str,
        checkpoint: &WorkCheckpoint,
        now: i64,
    ) -> Result<WorkCheckpointResult> {
        checkpoint.validate()?;
        let request = json_string(checkpoint)?;
        let mut conn = self.conn();
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let who = work_who(&tx, token, now)?;
        if who.read_only {
            return Err(HubError::Unauthorized);
        }
        let mut item = scoped_work(&tx, &who, &checkpoint.work_id, now)?
            .ok_or_else(|| HubError::Invalid("unknown work item".into()))?;
        if let Some((previous, snapshot, event_seq)) = tx
            .query_row(
                "SELECT o.request, v.snapshot, o.event_seq FROM hub_work_checkpoint_ops o
                 JOIN hub_work_versions v ON v.work = o.work AND v.event_seq = o.event_seq
                 WHERE o.work = ?1 AND o.operation_id = ?2",
                params![checkpoint.work_id, checkpoint.operation_id],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, i64>(2)?,
                    ))
                },
            )
            .optional()?
        {
            if previous != request {
                return Err(HubError::Invalid(
                    "checkpoint operation_id was already used for another request".into(),
                ));
            }
            let committed: WorkItem = json_value(&snapshot)?;
            if committed.lead_token != token
                || committed.lead_session != checkpoint.session
                || committed.generation != checkpoint.generation
            {
                return Err(HubError::Invalid(
                    "checkpoint operation belongs to another lead session".into(),
                ));
            }
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
                    .query_row(
                        "SELECT 1 FROM hub_work_versions
                         WHERE work = ?1 AND event_seq = ?2 AND event_seq <= ?3",
                        params![work_id, event_seq, item.updated_seq],
                        |_| Ok(()),
                    )
                    .optional()?
                    .is_some();
                if !retained {
                    return Err(HubError::Invalid(
                        "prior work version is unavailable".into(),
                    ));
                }
            }
        }
        if let Some(scope) = checkpoint.normalized_scope()? {
            let overlaps = work_scope_overlaps(&tx, &who, &scope, Some(&item.id), now)?;
            if !overlaps.is_empty() {
                let request = create_coordination_request(
                    &tx,
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
        let previous_item_bytes = stored_work_item_bytes(&tx, &item.id)?;
        let referenced = checkpoint
            .source_refs
            .iter()
            .filter_map(|source| match source {
                SourceRef::Work { work_id } => Some(work_id.clone()),
                _ => None,
            })
            .collect::<Vec<_>>();
        relation_targets(&tx, &who, &referenced)?;
        replace_relations(
            &tx,
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
        let seq = append(&tx, &event)?;
        item.updated_seq = seq;
        save_work_version(&tx, &item, seq)?;
        tx.execute(
            "INSERT INTO hub_work_checkpoint_ops
                (work, operation_id, request, event_seq)
             VALUES (?1, ?2, ?3, ?4)",
            params![item.id, checkpoint.operation_id, request, seq],
        )?;
        let snapshot = json_string(&item)?;
        let request_bytes = work_payload_row_bytes(request.len())?;
        let delta = work_transition_payload_bytes(&snapshot, &event)?
            .checked_sub(previous_item_bytes)
            .and_then(|bytes| bytes.checked_add(request_bytes))
            .ok_or_else(|| HubError::Invalid("work payload size is too large".into()))?;
        adjust_work_payload_capacity(&tx, &who, delta, now)?;
        if checkpoint.kind == super::work::CheckpointKind::Finish {
            release_session_intent(&tx, token, &checkpoint.session, now)?;
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
    }

    fn work_get(&self, token: &str, id: &str, now: i64) -> Result<Option<WorkItem>> {
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        let who = work_who(&tx, token, now)?;
        let item = scoped_work(&tx, &who, id, now)?;
        tx.commit()?;
        Ok(item)
    }

    fn work_search(
        &self,
        token: &str,
        search: Option<&str>,
        cursor: Option<&str>,
        limit: usize,
        now: i64,
    ) -> Result<WorkSearchPage> {
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        let who = work_who(&tx, token, now)?;
        let after = cursor.map(super::work::parse_search_cursor).transpose()?;
        // A cursor must name a work item of this scope at exactly that creation time.
        if let Some((created_at, id)) = &after {
            let known: Option<i64> = tx
                .query_row(
                    "SELECT 1 FROM hub_work_items WHERE id = ?5 AND created_at = ?6
                       AND provider = ?1 AND host = ?2 AND repository = ?3 AND repository_id IS ?4",
                    params![
                        who.owner.identity.provider,
                        who.owner.identity.host,
                        who.repository,
                        who.repository_id,
                        id,
                        created_at
                    ],
                    |row| row.get(0),
                )
                .optional()?;
            if known.is_none() {
                return Err(HubError::Invalid("unknown cursor".into()));
            }
        }
        let pattern = search.map(like_pattern);
        let number = search.and_then(search_number).map(|n| n.to_string());
        let mut statement = tx.prepare(
            "SELECT w.id, w.title, w.state, w.lead_agent, u.login, w.branch, w.source_issue, w.updated_at, w.created_at
             FROM hub_work_items w JOIN hub_users u ON u.id = w.owner
             WHERE w.provider = ?1 AND w.host = ?2 AND w.repository = ?3 AND w.repository_id IS ?4
               AND (?5 IS NULL OR (w.created_at, w.id) < (?5, ?6))
               AND (?7 IS NULL OR w.source_issue = ?8
                 OR lower(w.title) LIKE ?7 ESCAPE '\\' OR lower(w.branch) LIKE ?7 ESCAPE '\\'
                 OR lower(w.lead_agent) LIKE ?7 ESCAPE '\\' OR lower(u.login) LIKE ?7 ESCAPE '\\'
                 OR lower(w.id) LIKE ?7 ESCAPE '\\' OR lower(w.scope) LIKE ?7 ESCAPE '\\')
             ORDER BY w.created_at DESC, w.id DESC LIMIT ?9",
        )?;
        let items = statement
            .query_map(
                params![
                    who.owner.identity.provider,
                    who.owner.identity.host,
                    who.repository,
                    who.repository_id,
                    after.as_ref().map(|a| a.0),
                    after.as_ref().map(|a| a.1.as_str()),
                    pattern,
                    number,
                    limit as i64 + 1
                ],
                |row| {
                    Ok(WorkSearchItem {
                        id: row.get(0)?,
                        title: row.get(1)?,
                        state: row.get(2)?,
                        lead_agent: row.get(3)?,
                        owner: row.get(4)?,
                        branch: row.get(5)?,
                        source_issue: row.get(6)?,
                        updated_at: row.get(7)?,
                        created_at: row.get(8)?,
                    })
                },
            )?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        drop(statement);
        tx.commit()?;
        Ok(WorkSearchPage::of(items, limit))
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
        let mut conn = self.conn();
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let who = work_who(&tx, token, now)?;
        if who.read_only {
            return Err(HubError::Unauthorized);
        }
        let item = scoped_work(&tx, &who, &batch.work_id, now)?
            .ok_or_else(|| HubError::Invalid("unknown work".into()))?;
        if let Some((author, kind, retained, response)) = tx
            .query_row(
                "SELECT author_token,kind,request,response FROM hub_change_operations
                 WHERE work=?1 AND operation_id=?2",
                params![batch.work_id, batch.operation_id],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, String>(3)?,
                    ))
                },
            )
            .optional()?
        {
            if author == token && kind == "event_batch" && retained == request {
                let mut receipt: IngestReceiptV1 = json_value(&response)?;
                receipt.replayed = true;
                tx.commit()?;
                return Ok(receipt);
            }
            return Err(HubError::Invalid(
                "operation_id is already used by a different capture request".into(),
            ));
        }
        if item.state != WorkState::Active || item.generation != batch.generation {
            return Err(HubError::Invalid(
                "capture work is no longer current".into(),
            ));
        }
        let participant = tx
            .query_row(
                "SELECT 1 FROM hub_work_participants
                 WHERE work=?1 AND token=?2 AND session=?3 AND generation=?4",
                params![
                    batch.work_id,
                    token,
                    batch.session.session_id,
                    batch.generation
                ],
                |_| Ok(()),
            )
            .optional()?
            .is_some();
        if !participant {
            return Err(HubError::Invalid(
                "capture session is not a current work participant".into(),
            ));
        }
        let existing = tx
            .query_row(
                "SELECT identity FROM hub_change_sessions
                 WHERE work=?1 AND generation=?2 AND session_id=?3",
                params![batch.work_id, batch.generation, batch.session.session_id],
                |row| row.get::<_, String>(0),
            )
            .optional()?;
        if let Some(retained_identity) = existing {
            if retained_identity != identity {
                return Err(HubError::Invalid(
                    "capture session identity or generation changed".into(),
                ));
            }
        } else {
            let first = batch.events[0].observed_at;
            tx.execute(
                "INSERT INTO hub_change_sessions
                 (work,session_id,generation,identity,watermark,event_count,gap_count,started,ended,
                  start_sequence,end_sequence,lifecycle_valid,first_observed_at,last_observed_at,updated_at)
                 VALUES (?1,?2,?3,?4,0,0,0,0,0,NULL,NULL,0,?5,?5,?6)",
                params![
                    batch.work_id,
                    batch.session.session_id,
                    batch.generation,
                    identity,
                    first,
                    now
                ],
            )?;
        }
        let mut accepted = 0_usize;
        let mut duplicate = 0_usize;
        let mut new_event_bytes = 0_i64;
        let mut watermark = 0_u64;
        let mut gap_count = 0_usize;
        let mut last_observed = 0_i64;
        for event in &batch.events {
            let encoded = json_string(event)?;
            if let Some((sequence, retained)) = tx
                .query_row(
                    "SELECT sequence,event FROM hub_change_events
                     WHERE work=?1 AND generation=?2 AND session_id=?3 AND event_id=?4",
                    params![
                        batch.work_id,
                        batch.generation,
                        batch.session.session_id,
                        event.event_id
                    ],
                    |row| Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?)),
                )
                .optional()?
            {
                if sequence == event.sequence as i64 && retained == encoded {
                    duplicate += 1;
                    watermark = watermark.max(event.sequence);
                    continue;
                }
                return Err(HubError::Invalid(
                    "event_id is already used by different capture data".into(),
                ));
            }
            if tx
                .query_row(
                    "SELECT 1 FROM hub_change_events
                     WHERE work=?1 AND generation=?2 AND session_id=?3 AND sequence=?4",
                    params![
                        batch.work_id,
                        batch.generation,
                        batch.session.session_id,
                        event.sequence as i64
                    ],
                    |_| Ok(()),
                )
                .optional()?
                .is_some()
            {
                return Err(HubError::Invalid(
                    "capture sequence is already used by another event".into(),
                ));
            }
            tx.execute(
                "INSERT INTO hub_change_events
                 (work,generation,session_id,event_id,sequence,event,observed_at)
                 VALUES (?1,?2,?3,?4,?5,?6,?7)",
                params![
                    batch.work_id,
                    batch.generation,
                    batch.session.session_id,
                    event.event_id,
                    event.sequence as i64,
                    encoded,
                    event.observed_at
                ],
            )?;
            if let Some(artifact) = event
                .artifact
                .as_ref()
                .filter(|artifact| artifact.retention == RetentionClassV1::SharedApproved)
            {
                declare_shared_body(&tx, batch, token, event, artifact, now)?;
            }
            accepted += 1;
            new_event_bytes = new_event_bytes
                .checked_add(work_payload_row_bytes(encoded.len())?)
                .ok_or_else(|| HubError::Invalid("change capture size is too large".into()))?;
            gap_count += usize::from(event.gap.is_some());
            watermark = watermark.max(event.sequence);
            last_observed = last_observed.max(event.observed_at);
        }
        let retained_lifecycle = {
            let mut statement = tx.prepare(
                "SELECT sequence,event FROM hub_change_events
                 WHERE work=?1 AND generation=?2 AND session_id=?3 ORDER BY sequence",
            )?;
            statement
                .query_map(
                    params![batch.work_id, batch.generation, batch.session.session_id],
                    |row| Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?)),
                )?
                .collect::<std::result::Result<Vec<_>, _>>()?
        };
        let mut start_sequences = Vec::new();
        let mut end_sequences = Vec::new();
        for (sequence, encoded) in &retained_lifecycle {
            let event: CapturedEventV1 = json_value(encoded)?;
            match event.kind {
                // A compaction restart continues the same session, not a second origin.
                CapturedEventKindV1::SessionStarted
                    if event.boundary.as_deref() != Some("compact") =>
                {
                    start_sequences.push(*sequence)
                }
                CapturedEventKindV1::SessionEnded => end_sequences.push(*sequence),
                _ => {}
            }
        }
        let retained_watermark = retained_lifecycle
            .last()
            .map_or(0, |(sequence, _)| *sequence);
        let start_sequence = (start_sequences.len() == 1).then(|| start_sequences[0]);
        let end_sequence = (end_sequences.len() == 1).then(|| end_sequences[0]);
        let lifecycle_valid = start_sequence == Some(1)
            && end_sequence == Some(retained_watermark)
            && retained_watermark > 1;
        tx.execute(
            "UPDATE hub_change_sessions SET
                 watermark=?4, event_count=?5,
                 gap_count=gap_count+?6, started=?7, ended=?8,
                 start_sequence=?9,end_sequence=?10,lifecycle_valid=?11,
                 last_observed_at=max(last_observed_at,?12), updated_at=?13
             WHERE work=?1 AND generation=?2 AND session_id=?3",
            params![
                batch.work_id,
                batch.generation,
                batch.session.session_id,
                retained_watermark,
                retained_lifecycle.len() as i64,
                gap_count as i64,
                start_sequence.is_some(),
                end_sequence.is_some(),
                start_sequence,
                end_sequence,
                lifecycle_valid,
                last_observed,
                now
            ],
        )?;
        let retained_events = retained_lifecycle.len() as i64;
        let retained_operations: i64 = tx.query_row(
            "SELECT COUNT(*) FROM hub_change_operations WHERE work=?1",
            [batch.work_id.as_str()],
            |row| row.get(0),
        )?;
        if retained_events > MAX_CHANGE_EVENTS_PER_SESSION
            || retained_operations >= MAX_CHANGE_OPERATIONS_PER_WORK
        {
            return Err(HubError::WorkCapacity);
        }
        let capture_bytes = work_payload_row_bytes(request.len())?
            .checked_add(new_event_bytes)
            .ok_or_else(|| HubError::Invalid("change capture size is too large".into()))?;
        adjust_work_payload_capacity(&tx, &who, capture_bytes, now)?;
        let receipt = IngestReceiptV1 {
            work_id: batch.work_id.clone(),
            operation_id: batch.operation_id.clone(),
            accepted,
            duplicate,
            watermark: retained_watermark as u64,
            replayed: false,
        };
        tx.execute(
            "INSERT INTO hub_change_operations
             (work,operation_id,author_token,kind,request,response,received_at)
             VALUES (?1,?2,?3,'event_batch',?4,?5,?6)",
            params![
                batch.work_id,
                batch.operation_id,
                token,
                request,
                json_string(&receipt)?,
                now
            ],
        )?;
        tx.commit()?;
        Ok(receipt)
    }

    fn shared_body_lookup(
        &self,
        token: &str,
        work_id: &str,
        digest: &str,
        now: i64,
    ) -> Result<Option<SharedBodyAccess>> {
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        let who = work_who(&tx, token, now)?;
        if scoped_work(&tx, &who, work_id, now)?.is_none() {
            return Ok(None);
        }
        let rows = tx
            .prepare(&format!(
                "SELECT {SHARED_BODY_COLUMNS} FROM hub_change_shared_bodies
                 WHERE work=?1 AND digest=?2 ORDER BY declared_at,author"
            ))?
            .query_map(params![work_id, digest], |row| Ok(shared_body_row(row)))?
            .collect::<std::result::Result<Vec<_>, _>>()?
            .into_iter()
            .collect::<Result<Vec<_>>>()?;
        Ok(Some(SharedBodyAccess {
            rows,
            caller: who.owner.id,
        }))
    }

    fn shared_bodies_own(
        &self,
        token: &str,
        work_id: &str,
        digests: &[String],
        now: i64,
    ) -> Result<Option<Vec<SharedBodyRow>>> {
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        let who = work_who(&tx, token, now)?;
        if scoped_work(&tx, &who, work_id, now)?.is_none() {
            return Ok(None);
        }
        let mut statement = tx.prepare(&format!(
            "SELECT {SHARED_BODY_COLUMNS} FROM hub_change_shared_bodies
             WHERE work=?1 AND digest=?2 AND author=?3"
        ))?;
        let mut rows = Vec::new();
        for digest in digests {
            if let Some(row) = statement
                .query_row(params![work_id, digest, who.owner.id], |row| {
                    Ok(shared_body_row(row))
                })
                .optional()?
            {
                rows.push(row?);
            }
        }
        drop(statement);
        Ok(Some(rows))
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
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        let who = work_who(&tx, token, now)?;
        if scoped_work(&tx, &who, work_id, now)?.is_none() {
            return Ok(None);
        }
        let mut statement = tx.prepare(&format!(
            "WITH page AS (
                 SELECT {SHARED_BODY_COLUMNS} FROM hub_change_shared_bodies
                 WHERE work=?1 AND (?2 IS NULL OR (session_id,sequence,digest) > (?2,?3,?4))
                   AND (?6 OR author=?7)
                 ORDER BY session_id,sequence,digest LIMIT ?5)
             SELECT page.*, e.event, e.observed_at FROM page
             JOIN hub_change_events e ON e.work=page.work AND e.generation=page.generation
               AND e.session_id=page.session_id AND e.event_id=page.event_id
             ORDER BY page.session_id,page.sequence,page.digest"
        ))?;
        let entries = statement
            .query_map(
                params![
                    work_id,
                    after.as_ref().map(|after| after.0.as_str()),
                    after.as_ref().map(|after| after.1 as i64),
                    after.as_ref().map(|after| after.2.as_str()),
                    MAX_CONVERSATION_PAGE_ENTRIES as i64 + 1,
                    all_authors,
                    who.owner.id
                ],
                |row| {
                    Ok((
                        shared_body_row(row),
                        row.get::<_, String>(SHARED_BODY_COLUMN_COUNT)?,
                        row.get::<_, i64>(SHARED_BODY_COLUMN_COUNT + 1)?,
                    ))
                },
            )?
            .collect::<std::result::Result<Vec<_>, _>>()?
            .into_iter()
            .map(|(body, event, observed_at)| body?.conversation_entry(&event, observed_at, now))
            .collect::<Result<Vec<_>>>()?;
        drop(statement);
        Ok(Some(entries))
    }

    fn shared_body_mark_available(
        &self,
        work_id: &str,
        digest: &str,
        author: &str,
        now: i64,
        retention_seconds: i64,
    ) -> Result<Option<SharedBodyState>> {
        let mut conn = self.conn();
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        tx.execute(
            "UPDATE hub_change_shared_bodies
             SET state='available', uploaded_at=?4, expires_at=?4+?5
             WHERE work=?1 AND digest=?2 AND author=?3 AND state='declared'",
            params![work_id, digest, author, now, retention_seconds],
        )?;
        let state = tx
            .query_row(
                "SELECT state FROM hub_change_shared_bodies
                 WHERE work=?1 AND digest=?2 AND author=?3",
                params![work_id, digest, author],
                |row| row.get::<_, String>(0),
            )
            .optional()?
            .map(|state| SharedBodyState::parse(&state))
            .transpose()?;
        tx.commit()?;
        Ok(state)
    }

    fn shared_body_tombstone(
        &self,
        work_id: &str,
        digest: &str,
        author: Option<&str>,
        actor: &str,
        now: i64,
    ) -> Result<Vec<SharedBodyRow>> {
        let mut conn = self.conn();
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        tx.execute(
            "UPDATE hub_change_shared_bodies
             SET state='deleted', deleted_at=?4, deleted_by=?5
             WHERE work=?1 AND digest=?2 AND (?3 IS NULL OR author=?3) AND state<>'deleted'",
            params![work_id, digest, author, now, actor],
        )?;
        let rows = tx
            .prepare(&format!(
                "SELECT {SHARED_BODY_COLUMNS} FROM hub_change_shared_bodies
                 WHERE work=?1 AND digest=?2 AND (?3 IS NULL OR author=?3) ORDER BY author"
            ))?
            .query_map(params![work_id, digest, author], |row| {
                Ok(shared_body_row(row))
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?
            .into_iter()
            .collect::<Result<Vec<_>>>()?;
        tx.commit()?;
        Ok(rows)
    }

    fn shared_bodies_expire(
        &self,
        now: i64,
        limit: usize,
        retention_seconds: i64,
    ) -> Result<Vec<SharedBodyRow>> {
        let mut conn = self.conn();
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let rows = {
            let mut statement = tx.prepare(&format!(
                "UPDATE hub_change_shared_bodies SET state='expired',
                     expires_at=CASE WHEN state='declared' THEN declared_at+?3 ELSE expires_at END
                 WHERE (work,digest,author) IN (
                     SELECT work,digest,author FROM hub_change_shared_bodies
                     WHERE (state='available' AND expires_at<=?1)
                        OR (state='declared' AND declared_at+?3<=?1)
                     ORDER BY CASE WHEN state='declared' THEN declared_at+?3 ELSE expires_at END,
                              work,digest,author LIMIT ?2)
                 RETURNING {SHARED_BODY_COLUMNS}"
            ))?;
            statement
                .query_map(params![now, limit as i64, retention_seconds], |row| {
                    Ok(shared_body_row(row))
                })?
                .collect::<std::result::Result<Vec<_>, _>>()?
                .into_iter()
                .collect::<Result<Vec<_>>>()?
        };
        tx.commit()?;
        Ok(rows)
    }

    fn shared_body_demote_available(
        &self,
        work_id: &str,
        digest: &str,
        author: Option<&str>,
    ) -> Result<usize> {
        Ok(self.conn().execute(
            "UPDATE hub_change_shared_bodies SET state='declared'
             WHERE work=?1 AND digest=?2 AND (?3 IS NULL OR author=?3) AND state='available'",
            params![work_id, digest, author],
        )?)
    }

    fn shared_bodies_pending_physical_delete(
        &self,
        now: i64,
        limit: usize,
    ) -> Result<Vec<SharedBodyRow>> {
        let conn = self.conn();
        let mut statement = conn.prepare(&format!(
            "SELECT {SHARED_BODY_COLUMNS} FROM hub_change_shared_bodies
             WHERE state IN ('expired','deleted') AND physical_deleted_at IS NULL
               AND (next_delete_at IS NULL OR next_delete_at<=?1)
             ORDER BY COALESCE(deleted_at,expires_at),work,digest,author LIMIT ?2"
        ))?;
        statement
            .query_map(params![now, limit as i64], |row| Ok(shared_body_row(row)))?
            .collect::<std::result::Result<Vec<_>, _>>()?
            .into_iter()
            .collect()
    }

    fn shared_body_object_in_use(&self, work_id: &str, digest: &str) -> Result<bool> {
        Ok(self.conn().query_row(
            "SELECT EXISTS (SELECT 1 FROM hub_change_shared_bodies
                 WHERE work=?1 AND digest=?2 AND state='available')",
            params![work_id, digest],
            |row| row.get(0),
        )?)
    }

    fn shared_body_mark_physically_deleted(
        &self,
        work_id: &str,
        digest: &str,
        author: &str,
        now: i64,
    ) -> Result<()> {
        self.conn().execute(
            "UPDATE hub_change_shared_bodies SET physical_deleted_at=?4, next_delete_at=NULL
             WHERE work=?1 AND digest=?2 AND author=?3 AND state IN ('expired','deleted')
               AND physical_deleted_at IS NULL",
            params![work_id, digest, author, now],
        )?;
        Ok(())
    }

    fn shared_body_delete_failed(
        &self,
        work_id: &str,
        digest: &str,
        author: &str,
        retry_at: i64,
    ) -> Result<()> {
        self.conn().execute(
            "UPDATE hub_change_shared_bodies
             SET physical_deleted_at=NULL, delete_attempts=delete_attempts+1, next_delete_at=?4
             WHERE work=?1 AND digest=?2 AND author=?3 AND state IN ('expired','deleted')",
            params![work_id, digest, author, retry_at],
        )?;
        Ok(())
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
        let mut conn = self.conn();
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let who = work_who(&tx, token, now)?;
        if who.read_only {
            return Err(HubError::Unauthorized);
        }
        let item = scoped_work(&tx, &who, &start.work_id, now)?
            .ok_or_else(|| HubError::Invalid("unknown work".into()))?;
        if let Some((author, kind, retained, response)) = tx
            .query_row(
                "SELECT author_token,kind,request,response FROM hub_change_operations
                 WHERE work=?1 AND operation_id=?2",
                params![start.work_id, start.operation_id],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, String>(3)?,
                    ))
                },
            )
            .optional()?
        {
            if author == token && kind == "action_start" && retained == request {
                let mut receipt: ActionMutationReceiptV1 = json_value(&response)?;
                receipt.replayed = true;
                tx.commit()?;
                return Ok(receipt);
            }
            return Err(HubError::Invalid(
                "operation_id is already used by a different action request".into(),
            ));
        }
        if item.state != WorkState::Active || item.generation != start.generation {
            return Err(HubError::Invalid("action work is no longer current".into()));
        }
        if start.source.repository() != who.repository {
            return Err(HubError::Invalid(
                "action source belongs to another repository".into(),
            ));
        }
        if tx
            .query_row(
                "SELECT 1 FROM hub_work_participants
                 WHERE work=?1 AND token=?2 AND session=?3 AND generation=?4",
                params![start.work_id, token, start.session_id, start.generation],
                |_| Ok(()),
            )
            .optional()?
            .is_none()
        {
            return Err(HubError::Invalid(
                "action session is not a current work participant".into(),
            ));
        }
        let action_count: i64 = tx.query_row(
            "SELECT COUNT(*) FROM hub_change_actions WHERE work=?1",
            [start.work_id.as_str()],
            |row| row.get(0),
        )?;
        let operation_count: i64 = tx.query_row(
            "SELECT COUNT(*) FROM hub_change_operations WHERE work=?1",
            [start.work_id.as_str()],
            |row| row.get(0),
        )?;
        if action_count >= MAX_CHANGE_ACTIONS_PER_WORK
            || operation_count >= MAX_CHANGE_OPERATIONS_PER_WORK
        {
            return Err(HubError::WorkCapacity);
        }
        adjust_work_payload_capacity(
            &tx,
            &who,
            work_payload_row_bytes(request.len())?
                .checked_mul(2)
                .ok_or_else(|| HubError::Invalid("change capture size is too large".into()))?,
            now,
        )?;
        let received_order: i64 = tx.query_row(
            "SELECT COALESCE(MAX(received_order),0)+1 FROM hub_change_actions WHERE work=?1",
            [start.work_id.as_str()],
            |row| row.get(0),
        )?;
        let source_id = start.source.stable_id()?;
        tx.execute(
            "INSERT INTO hub_change_actions
             (work,action_id,generation,session_id,author_token,source_id,source_kind,start_request,
              started_at,received_at,received_order,updated_at)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?10)",
            params![
                start.work_id,
                start.action_id,
                start.generation,
                start.session_id,
                token,
                source_id,
                enum_name(&start.source.kind())?,
                request,
                start.started_at,
                now,
                received_order
            ],
        )
        .map_err(|error| match error {
            rusqlite::Error::SqliteFailure(ref failure, _)
                if failure.code == rusqlite::ErrorCode::ConstraintViolation =>
            {
                HubError::Invalid("action_id is already used".into())
            }
            error => error.into(),
        })?;
        let receipt = ActionMutationReceiptV1 {
            work_id: start.work_id.clone(),
            operation_id: start.operation_id.clone(),
            action_id: start.action_id.clone(),
            replayed: false,
        };
        tx.execute("INSERT INTO hub_change_operations (work,operation_id,author_token,kind,request,response,received_at) VALUES (?1,?2,?3,'action_start',?4,?5,?6)",params![start.work_id,start.operation_id,token,request,json_string(&receipt)?,now])?;
        tx.commit()?;
        Ok(receipt)
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
        let mut conn = self.conn();
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let who = work_who(&tx, token, now)?;
        if who.read_only {
            return Err(HubError::Unauthorized);
        }
        let item = scoped_work(&tx, &who, &finish.work_id, now)?
            .ok_or_else(|| HubError::Invalid("unknown work".into()))?;
        if let Some((author,kind,retained,response))=tx.query_row("SELECT author_token,kind,request,response FROM hub_change_operations WHERE work=?1 AND operation_id=?2",params![finish.work_id,finish.operation_id],|r|Ok((r.get::<_,String>(0)?,r.get::<_,String>(1)?,r.get::<_,String>(2)?,r.get::<_,String>(3)?))).optional()? {
            if author==token&&kind=="action_finish"&&retained==request { let mut receipt:ActionMutationReceiptV1=json_value(&response)?;receipt.replayed=true;tx.commit()?;return Ok(receipt); }
            return Err(HubError::Invalid("operation_id is already used by a different action request".into()));
        }
        if item.state != WorkState::Active || item.generation != finish.generation {
            return Err(HubError::Invalid("action work is no longer current".into()));
        }
        let (author,generation,started_at,already,start_request):(String,i64,i64,Option<String>,String)=tx.query_row("SELECT author_token,generation,started_at,finish_request,start_request FROM hub_change_actions WHERE work=?1 AND action_id=?2",params![finish.work_id,finish.action_id],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?))).optional()?.ok_or_else(||HubError::Invalid("unknown action".into()))?;
        if author != token || generation != finish.generation {
            return Err(HubError::Invalid(
                "action finish author or generation changed".into(),
            ));
        }
        let retained_start: ActionStartV1 = json_value(&start_request)?;
        if retained_start.kind == crate::change_record::ActionKindV1::Execution
            && finish.result != crate::change_record::CheckResultV1::Unknown
        {
            return Err(HubError::Invalid(
                "execution actions cannot assert verification results".into(),
            ));
        }
        if retained_start.kind == crate::change_record::ActionKindV1::Verification
            && retained_start.recipe_provenance
                != crate::change_record::RecipeProvenanceV1::ConfiguredExactMatch
            && matches!(
                finish.result,
                crate::change_record::CheckResultV1::Passed
                    | crate::change_record::CheckResultV1::Failed
            )
        {
            return Err(HubError::Invalid(
                "verification result requires an exact configured recipe".into(),
            ));
        }
        if already.is_some() {
            return Err(HubError::Invalid("action is already finished".into()));
        }
        if finish.finished_at < started_at {
            return Err(HubError::Invalid("action finish precedes its start".into()));
        }
        let operation_count: i64 = tx.query_row(
            "SELECT COUNT(*) FROM hub_change_operations WHERE work=?1",
            [finish.work_id.as_str()],
            |row| row.get(0),
        )?;
        if operation_count >= MAX_CHANGE_OPERATIONS_PER_WORK {
            return Err(HubError::WorkCapacity);
        }
        adjust_work_payload_capacity(
            &tx,
            &who,
            work_payload_row_bytes(request.len())?
                .checked_mul(2)
                .ok_or_else(|| HubError::Invalid("change capture size is too large".into()))?,
            now,
        )?;
        tx.execute("UPDATE hub_change_actions SET finish_request=?3,finished_at=?4,updated_at=?5 WHERE work=?1 AND action_id=?2",params![finish.work_id,finish.action_id,request,finish.finished_at,now])?;
        let receipt = ActionMutationReceiptV1 {
            work_id: finish.work_id.clone(),
            operation_id: finish.operation_id.clone(),
            action_id: finish.action_id.clone(),
            replayed: false,
        };
        tx.execute("INSERT INTO hub_change_operations (work,operation_id,author_token,kind,request,response,received_at) VALUES (?1,?2,?3,'action_finish',?4,?5,?6)",params![finish.work_id,finish.operation_id,token,request,json_string(&receipt)?,now])?;
        tx.commit()?;
        Ok(receipt)
    }

    fn skill_adoption(&self, repo: &str) -> Result<Option<SkillAdoption>> {
        Ok(self
            .conn()
            .query_row(
                "SELECT repository, source, lock_json, default_branch, observed_at, reported_by
                 FROM hub_skill_adoptions WHERE repository = ?1",
                [repo],
                |row| {
                    Ok(SkillAdoption {
                        repository: row.get(0)?,
                        source: row.get(1)?,
                        lock_json: row.get(2)?,
                        default_branch: row.get(3)?,
                        observed_at: row.get(4)?,
                        reported_by: row.get(5)?,
                    })
                },
            )
            .optional()?)
    }

    fn skill_adoption_put(&self, a: &SkillAdoption) -> Result<SkillAdoption> {
        self.conn().execute(
            "INSERT INTO hub_skill_adoptions
                 (repository, source, lock_json, default_branch, observed_at, reported_by)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)
             ON CONFLICT (repository) DO UPDATE SET source = excluded.source,
                 lock_json = excluded.lock_json, default_branch = excluded.default_branch,
                 observed_at = excluded.observed_at, reported_by = excluded.reported_by
             WHERE excluded.source = 'default_branch'
                OR hub_skill_adoptions.source <> 'default_branch'",
            params![
                a.repository,
                a.source,
                a.lock_json,
                a.default_branch,
                a.observed_at,
                a.reported_by
            ],
        )?;
        self.skill_adoption(&a.repository)?
            .ok_or_else(|| HubError::Unavailable("skill adoption vanished".into()))
    }

    fn skill_adoption_clear(&self, repo: &str, source: Option<&str>) -> Result<()> {
        self.conn().execute(
            "DELETE FROM hub_skill_adoptions WHERE repository = ?1 AND (?2 IS NULL OR source = ?2)",
            params![repo, source],
        )?;
        Ok(())
    }

    fn skill_evidence_add(&self, r: &EvidenceRow) -> Result<()> {
        self.conn().execute(
            "INSERT INTO hub_skill_evidence (id, repository, branch, run_id, skill, skill_sha256,
                 gate, kind, head_sha, assurance, summary, url, pull_request, detail, token_id,
                 session, agent, recorded_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18)",
            params![
                r.id,
                r.repository,
                r.branch,
                r.run_id,
                r.skill,
                r.skill_sha256,
                r.gate,
                r.kind,
                r.head_sha,
                r.assurance,
                r.summary,
                r.url,
                r.pull_request,
                r.detail,
                r.token_id,
                r.session,
                r.agent,
                r.recorded_at
            ],
        )?;
        Ok(())
    }

    fn skill_evidence(&self, repo: &str, run_id: &str) -> Result<Vec<EvidenceRow>> {
        let conn = self.conn();
        let mut statement = conn.prepare(
            "SELECT id, repository, branch, run_id, skill, skill_sha256, gate, kind, head_sha,
                    assurance, summary, url, pull_request, detail, token_id, session, agent,
                    recorded_at
             FROM hub_skill_evidence WHERE repository = ?1 AND run_id = ?2 ORDER BY seq",
        )?;
        let rows = statement
            .query_map(params![repo, run_id], |row| {
                Ok(EvidenceRow {
                    id: row.get(0)?,
                    repository: row.get(1)?,
                    branch: row.get(2)?,
                    run_id: row.get(3)?,
                    skill: row.get(4)?,
                    skill_sha256: row.get(5)?,
                    gate: row.get(6)?,
                    kind: row.get(7)?,
                    head_sha: row.get(8)?,
                    assurance: row.get(9)?,
                    summary: row.get(10)?,
                    url: row.get(11)?,
                    pull_request: row.get(12)?,
                    detail: row.get(13)?,
                    token_id: row.get(14)?,
                    session: row.get(15)?,
                    agent: row.get(16)?,
                    recorded_at: row.get(17)?,
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    fn skill_run_heads(
        &self,
        repo: &str,
        branch: Option<&str>,
        run_id: Option<&str>,
        limit: usize,
    ) -> Result<Vec<RunHead>> {
        let conn = self.conn();
        let mut statement = conn.prepare(
            "SELECT run_id, branch, skill, skill_sha256 FROM hub_skill_evidence
             WHERE seq IN (SELECT MAX(seq) FROM hub_skill_evidence WHERE repository = ?1
                           GROUP BY run_id)
               AND (?2 IS NULL OR branch = ?2) AND (?3 IS NULL OR run_id = ?3)
             ORDER BY seq DESC LIMIT ?4",
        )?;
        let rows = statement
            .query_map(params![repo, branch, run_id, limit as i64], |row| {
                Ok(RunHead {
                    run_id: row.get(0)?,
                    branch: row.get(1)?,
                    skill: row.get(2)?,
                    skill_sha256: row.get(3)?,
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    fn change_record(
        &self,
        token: &str,
        work_id: &str,
        now: i64,
    ) -> Result<Option<ChangeRecordSummaryV1>> {
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        let who = work_who(&tx, token, now)?;
        let Some(item) = scoped_work(&tx, &who, work_id, now)? else {
            tx.commit()?;
            return Ok(None);
        };
        let (count, gaps, complete): (i64, i64, i64) = tx.query_row(
            "SELECT COUNT(*),
                    COALESCE(SUM(gap_count + CASE WHEN watermark > event_count THEN watermark-event_count ELSE 0 END),0),
                    COALESCE(SUM(CASE WHEN lifecycle_valid=1 AND start_sequence=1
                        AND end_sequence=watermark THEN 1 ELSE 0 END),0)
             FROM hub_change_sessions WHERE work=?1 AND generation=?2",
            params![work_id, item.generation],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )?;
        let setup_observations: i64 = tx.query_row(
            "SELECT COUNT(*) FROM hub_change_events
             WHERE work=?1 AND generation=?2 AND json_type(event,'$.setup') IS NOT NULL",
            params![work_id, item.generation],
            |row| row.get(0),
        )?;
        let (setups, setups_omitted) = {
            let mut statement = tx.prepare(
                "WITH latest AS (
                    SELECT session_id,
                           json_extract(event,'$.setup.model') AS model,
                           json_extract(event,'$.setup.permission_mode') AS permission_mode,
                           json_extract(event,'$.setup.effort') AS effort,
                           ROW_NUMBER() OVER (PARTITION BY session_id ORDER BY sequence DESC) AS rank
                    FROM hub_change_events
                    WHERE work=?1 AND generation=?2 AND json_type(event,'$.setup') IS NOT NULL
                ), picked AS (
                    SELECT latest.*, COUNT(*) OVER () AS total FROM latest WHERE rank=1
                )
                SELECT picked.session_id, json_extract(s.identity,'$.parent_session_id'),
                       picked.model, picked.permission_mode, picked.effort, picked.total
                FROM picked JOIN hub_change_sessions s
                  ON s.work=?1 AND s.generation=?2 AND s.session_id=picked.session_id
                ORDER BY picked.session_id LIMIT ?3",
            )?;
            let mut total = 0_i64;
            let setups = statement
                .query_map(
                    params![
                        work_id,
                        item.generation,
                        MAX_PROJECTED_SESSION_SETUPS as i64
                    ],
                    |row| {
                        total = row.get(5)?;
                        Ok(SessionSetupV1 {
                            session_id: row.get(0)?,
                            parent_session_id: row.get(1)?,
                            model: row.get(2)?,
                            permission_mode: row.get(3)?,
                            effort: row.get(4)?,
                        })
                    },
                )?
                .collect::<std::result::Result<Vec<_>, _>>()?;
            let omitted = (total as u64).saturating_sub(setups.len() as u64);
            (setups, omitted)
        };
        let usage_observations: i64 = tx.query_row(
            "SELECT COUNT(*) FROM hub_work_model_usage WHERE work=?1 AND generation=?2",
            params![work_id, item.generation],
            |row| row.get(0),
        )?;
        let action=tx.query_row("WITH scoped AS (
                SELECT a.*,
                    COALESCE(CAST(json_extract(a.start_request,'$.observed_order') AS INTEGER),0) AS observed_order,
                    json_extract(a.start_request,'$.observation_scope_id') AS observation_scope_id,
                    COALESCE(json_extract(a.start_request,'$.observation_scope_id'),a.action_id) AS scope_key
                FROM hub_change_actions a WHERE a.work=?1 AND a.generation=?2
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
            WHERE scope_rank=1 ORDER BY received_order DESC LIMIT 1",params![work_id,item.generation],|r|Ok((r.get::<_,String>(0)?,r.get::<_,Option<String>>(1)?,r.get::<_,String>(2)?))).optional()?.map(|(start,finish,source_id)| -> Result<StoredActionV1> {Ok(StoredActionV1{start:json_value(&start)?,finish:finish.map(|value|json_value(&value)).transpose()?,source_id})}).transpose()?;
        let decision = tx
            .query_row(
                "SELECT event_seq,snapshot FROM hub_work_versions
                 WHERE work=?1 AND json_extract(snapshot,'$.current.kind')='decision'
                 ORDER BY event_seq DESC LIMIT 1",
                [work_id],
                |row| Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?)),
            )
            .optional()?
            .map(|(event_seq, snapshot)| -> Result<StoredDecisionV1> {
                let retained: WorkItem = json_value(&snapshot)?;
                Ok(StoredDecisionV1 {
                    event_seq,
                    checkpoint: retained.current.ok_or_else(|| {
                        HubError::Unavailable("retained decision checkpoint is missing".into())
                    })?,
                })
            })
            .transpose()?;
        let summary = project_change_record(
            &item,
            SessionStatsV1 {
                count: count as usize,
                gap_count: gaps as usize,
                complete_count: complete as usize,
                setup_observation_count: setup_observations as usize,
                usage_observation_count: usage_observations as usize,
                setups,
                setups_omitted,
            },
            action,
            decision,
            now,
        );
        tx.commit()?;
        Ok(Some(summary))
    }

    fn work_usage(&self, token: &str, usage: &UsageRecord, now: i64) -> Result<UsageReceipt> {
        usage.validate()?;
        let request = json_string(usage)?;
        let mut conn = self.conn();
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let who = work_who(&tx, token, now)?;
        if who.read_only {
            return Err(HubError::Unauthorized);
        }
        let item = scoped_work(&tx, &who, &usage.work_id, now)?
            .ok_or_else(|| HubError::Invalid("unknown work".into()))?;
        if let Some((author,existing,received))=tx.query_row("SELECT author_token,request,received_at FROM hub_work_model_usage WHERE work=?1 AND observation_id=?2",params![usage.work_id,usage.observation_id],|r|Ok((r.get::<_,String>(0)?,r.get::<_,String>(1)?,r.get::<_,i64>(2)?))).optional()? {
            if author==token && existing==request { tx.commit()?; return Ok(UsageReceipt { work_id:usage.work_id.clone(),observation_id:usage.observation_id.clone(),received_at:received,replayed:true }); }
            return Err(HubError::Invalid("observation_id is already used by different usage".into()));
        }
        let authorized = if item.state == WorkState::Finished {
            item.lead_token == token
                && item.lead_session == usage.session
                && item.generation == usage.generation
        } else {
            item.generation == usage.generation
                && tx
                    .query_row(
                        "SELECT 1 FROM hub_work_participants
                         WHERE work=?1 AND token=?2 AND session=?3 AND generation=?4",
                        params![usage.work_id, token, usage.session, usage.generation],
                        |_| Ok(()),
                    )
                    .optional()?
                    .is_some()
        };
        if !authorized {
            return Err(HubError::Invalid(
                "usage author is not a current work participant".into(),
            ));
        }
        let count: i64 = tx.query_row(
            "SELECT COUNT(*) FROM hub_work_model_usage WHERE work=?1",
            [&usage.work_id],
            |r| r.get(0),
        )?;
        if count >= MAX_OBSERVATIONS as i64 {
            return Err(HubError::Invalid(
                "work has reached the usage observation limit".into(),
            ));
        }
        let cost = usage.cost.as_ref();
        tx.execute("INSERT INTO hub_work_model_usage (work,observation_id,author_token,request,generation,session,phase,outcome,agent,model_provider,model,usage_basis,source_commit,input_tokens,output_tokens,cost_kind,cost_currency,cost_amount_micros,cost_basis,received_at) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18,?19,?20)",params![usage.work_id,usage.observation_id,token,request,usage.generation,usage.session,enum_name(&usage.phase)?,enum_name(&usage.outcome)?,usage.agent,usage.provider,usage.model,enum_name(&usage.usage_basis)?,usage.source_commit,usage.input_tokens,usage.output_tokens,cost.map(|c|enum_name(&c.kind)).transpose()?,cost.map(|c|c.currency.as_str()),cost.map(|c|c.amount_micros),cost.map(|c|c.basis.as_str()),now])?;
        let delta = work_payload_row_bytes(request.len())?;
        adjust_work_payload_capacity(&tx, &who, delta, now)?;
        tx.commit()?;
        Ok(UsageReceipt {
            work_id: usage.work_id.clone(),
            observation_id: usage.observation_id.clone(),
            received_at: now,
            replayed: false,
        })
    }

    fn work_insight(&self, token: &str, id: &str, now: i64) -> Result<Option<WorkInsight>> {
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        let who = work_who(&tx, token, now)?;
        let Some(item) = scoped_work(&tx, &who, id, now)? else {
            tx.commit()?;
            return Ok(None);
        };
        let insight =
            super::insights::project(sqlite_insight_input(&tx, item, now, MAX_OBSERVATIONS)?);
        tx.commit()?;
        Ok(Some(insight))
    }

    fn work_insights(&self, token: &str, query: &InsightQuery, now: i64) -> Result<InsightPage> {
        let (since, until) = query.window(now)?;
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        let who = work_who(&tx, token, now)?;
        let through = query.through.unwrap_or(tx.query_row(
            "SELECT COALESCE(MAX(seq),0) FROM hub_events",
            [],
            |r| r.get(0),
        )?);
        let cursor = query
            .after
            .as_ref()
            .map(|v| InsightCursor::decode(v, since, until, through))
            .transpose()?;
        let (after_at, after_id) = cursor
            .as_ref()
            .map(|c| (Some(c.created_at), Some(c.id.as_str())))
            .unwrap_or((None, None));
        let snapshots = {
            let mut statement=tx.prepare("SELECT snapshot FROM hub_work_items WHERE provider=?1 AND host=?2 AND repository=?3 AND repository_id IS ?4 AND created_at>=?5 AND created_at<=?6 AND (created_seq IS NULL OR created_seq<=0 OR created_seq<=?7) AND (?8 IS NULL OR created_at>?8 OR (created_at=?8 AND id>?9)) ORDER BY created_at,id LIMIT ?10")?;
            statement
                .query_map(
                    params![
                        who.owner.identity.provider,
                        who.owner.identity.host,
                        who.repository,
                        who.repository_id,
                        since,
                        until,
                        through,
                        after_at,
                        after_id,
                        (query.limit + 1) as i64
                    ],
                    |r| r.get::<_, String>(0),
                )?
                .collect::<std::result::Result<Vec<_>, _>>()?
        };
        let complete = snapshots.len() <= query.limit;
        let mut items = Vec::new();
        for snapshot in snapshots.iter().take(query.limit) {
            let item: WorkItem = json_value(snapshot)?;
            items.push(super::insights::project(sqlite_insight_input(
                &tx, item, now, 20,
            )?));
        }
        let next_after = if complete {
            None
        } else {
            items.last().map(|i| {
                InsightCursor {
                    since,
                    until,
                    through,
                    created_at: i.created_at,
                    id: i.work_id.clone(),
                }
                .encode()
            })
        };
        tx.commit()?;
        Ok(InsightPage {
            version: 1,
            as_of: now,
            since,
            until,
            through,
            next_after,
            complete,
            items,
        })
    }

    fn work_for_session(&self, token: &str, session: &str, now: i64) -> Result<Option<WorkItem>> {
        super::intent::text("session", session, 255)?;
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        let who = work_who(&tx, token, now)?;
        let item = unfinished_for_session(&tx, &who, session, now)?;
        tx.commit()?;
        Ok(item)
    }

    fn capture_work_for_session(
        &self,
        token: &str,
        session: &str,
        now: i64,
    ) -> Result<Option<WorkItem>> {
        super::intent::text("session", session, 255)?;
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        let who = work_who(&tx, token, now)?;
        let item = active_capture_work_for_session(&tx, &who, session, now)?;
        tx.commit()?;
        Ok(item)
    }

    fn session_name_for(&self, token: &str, session: Option<&str>, now: i64) -> Result<String> {
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        work_who(&tx, token, now)?;
        let name = session_name(&tx, token, session, now)?;
        tx.commit()?;
        Ok(name)
    }

    fn work_briefing(&self, token: &str, query: &BriefingQuery, now: i64) -> Result<Briefing> {
        query.validate()?;
        let mut conn = self.conn();
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let who = work_who(&tx, token, now)?;
        let subscriber = query.subscriber.clone();
        let subscriber_key = subscriber.as_deref().unwrap_or("");
        let ack_cursor = tx
            .query_row(
                "SELECT cursor FROM hub_work_acks
                 WHERE owner = ?1 AND provider = ?2 AND host = ?3 AND repository = ?4
                   AND repository_id IS ?5 AND subscriber_token = ?6
                   AND subscriber_session = ?7",
                params![
                    who.owner.id,
                    who.owner.identity.provider,
                    who.owner.identity.host,
                    who.repository,
                    who.repository_id,
                    who.token,
                    subscriber_key
                ],
                |row| row.get::<_, i64>(0),
            )
            .optional()?
            .unwrap_or(0);
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
                None => work_event_high_water(&tx, &who, after)?,
            };
            let page = briefing_page(
                &tx,
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
                .query_row(
                    "SELECT from_seq, through_seq, page_after, page_work_after, page_work_done,
                            next_after, next_work_after, next_work_done, page, complete
                     FROM hub_work_deliveries WHERE id = ?1 AND owner = ?2 AND provider = ?3
                       AND host = ?4 AND repository = ?5 AND repository_id IS ?6
                       AND subscriber_token = ?7 AND subscriber_session = ?8",
                    params![
                        delivery,
                        who.owner.id,
                        who.owner.identity.provider,
                        who.owner.identity.host,
                        who.repository,
                        who.repository_id,
                        who.token,
                        subscriber_key
                    ],
                    |row| {
                        Ok((
                            row.get::<_, i64>(0)?,
                            row.get::<_, i64>(1)?,
                            row.get::<_, i64>(2)?,
                            row.get::<_, Option<String>>(3)?,
                            row.get::<_, bool>(4)?,
                            row.get::<_, i64>(5)?,
                            row.get::<_, Option<String>>(6)?,
                            row.get::<_, bool>(7)?,
                            row.get::<_, String>(8)?,
                            row.get::<_, bool>(9)?,
                        ))
                    },
                )
                .optional()?
                .ok_or_else(|| HubError::Invalid("unknown briefing delivery".into()))?;
            let (
                from,
                through,
                page_after,
                page_work_after,
                page_work_done,
                next_after,
                next_work_after,
                next_work_done,
                cached,
                complete,
            ) = row;
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
                &tx,
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
                "UPDATE hub_work_deliveries SET page_after = ?2, page_work_after = ?3,
                     page_work_done = ?4, next_after = ?5, next_work_after = ?6,
                     next_work_done = ?7, page = ?8, complete = ?9, updated_at = ?10
                 WHERE id = ?1",
                params![
                    delivery,
                    page.after,
                    page.work_after,
                    query.work_done,
                    page.next_after,
                    page.next_work_after,
                    page.next_work_done,
                    page_json,
                    page.complete,
                    now
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
                "a new delivery starts at the acknowledged cursor without paging cursors".into(),
            ));
        }
        let released: i64 = tx.query_row(
            "SELECT COALESCE(SUM(CASE WHEN acknowledged = 0 THEN 4194304
                        ELSE length(CAST(page AS BLOB)) + 256 END), 0)
             FROM hub_work_deliveries WHERE updated_at < ?1
               AND provider = ?2 AND host = ?3 AND repository = ?4 AND repository_id IS ?5",
            params![
                now.saturating_sub(86_400),
                who.owner.identity.provider,
                who.owner.identity.host,
                who.repository,
                who.repository_id
            ],
            |row| row.get(0),
        )?;
        tx.execute(
            "DELETE FROM hub_work_deliveries WHERE updated_at < ?1
               AND provider = ?2 AND host = ?3 AND repository = ?4 AND repository_id IS ?5",
            params![
                now.saturating_sub(86_400),
                who.owner.identity.provider,
                who.owner.identity.host,
                who.repository,
                who.repository_id
            ],
        )?;
        let (owner_open, token_open): (i64, i64) = tx.query_row(
            "SELECT COUNT(*),
                    COALESCE(SUM(CASE WHEN subscriber_token = ?6 THEN 1 ELSE 0 END), 0)
             FROM hub_work_deliveries
             WHERE owner = ?1 AND provider = ?2 AND host = ?3 AND repository = ?4
               AND repository_id IS ?5 AND acknowledged = 0",
            params![
                who.owner.id,
                who.owner.identity.provider,
                who.owner.identity.host,
                who.repository,
                who.repository_id,
                who.token
            ],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
        if owner_open >= 16 || token_open >= 8 {
            return Err(HubError::Invalid(
                "too many unacknowledged briefing deliveries".into(),
            ));
        }
        let through = work_event_high_water(&tx, &who, ack_cursor)?;
        let delivery = new_id("dlv")?;
        let page = briefing_page(
            &tx,
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
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?9, NULL, 0, ?11, ?12, ?13,
                     ?14, ?15, ?16, ?16)",
            params![
                delivery,
                who.owner.id,
                who.owner.identity.provider,
                who.owner.identity.host,
                who.repository,
                who.repository_id,
                who.token,
                subscriber_key,
                ack_cursor,
                through,
                page.next_after,
                page.next_work_after,
                page.next_work_done,
                page_json,
                page.complete,
                now
            ],
        )?;
        adjust_work_payload_capacity(
            &tx,
            &who,
            WORK_DELIVERY_RESERVATION_BYTES
                .checked_sub(released)
                .ok_or_else(|| HubError::Invalid("work payload size is too large".into()))?,
            now,
        )?;
        tx.commit()?;
        Ok(page)
    }

    fn work_ack(&self, token: &str, ack: &BriefingAck, now: i64) -> Result<()> {
        ack.validate()?;
        let mut conn = self.conn();
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let who = work_who(&tx, token, now)?;
        if who.read_only {
            return Err(HubError::Unauthorized);
        }
        let (through, page, acknowledged, subscriber_key) = tx
            .query_row(
                "SELECT through_seq, page, acknowledged, subscriber_session
                 FROM hub_work_deliveries
                 WHERE id = ?1 AND owner = ?2 AND provider = ?3 AND host = ?4
                   AND repository = ?5 AND repository_id IS ?6 AND subscriber_token = ?7
                   AND complete = 1",
                params![
                    ack.delivery,
                    who.owner.id,
                    who.owner.identity.provider,
                    who.owner.identity.host,
                    who.repository,
                    who.repository_id,
                    who.token
                ],
                |row| {
                    Ok((
                        row.get::<_, i64>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, bool>(2)?,
                        row.get::<_, String>(3)?,
                    ))
                },
            )
            .optional()?
            .ok_or_else(|| HubError::Invalid("delivery is unknown or incomplete".into()))?;
        let changed = tx.execute(
            "UPDATE hub_work_acks SET cursor = MAX(cursor, ?6), updated_at = ?7
             WHERE owner = ?1 AND provider = ?2 AND host = ?3 AND repository = ?4
               AND repository_id IS ?5 AND subscriber_token = ?8
               AND subscriber_session = ?9",
            params![
                who.owner.id,
                who.owner.identity.provider,
                who.owner.identity.host,
                who.repository,
                who.repository_id,
                through,
                now,
                who.token,
                subscriber_key
            ],
        )?;
        if changed == 0 {
            tx.execute(
                "INSERT INTO hub_work_acks
                    (owner, provider, host, repository, repository_id, cursor, updated_at,
                     subscriber_token, subscriber_session)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
                params![
                    who.owner.id,
                    who.owner.identity.provider,
                    who.owner.identity.host,
                    who.repository,
                    who.repository_id,
                    through,
                    now,
                    who.token,
                    subscriber_key
                ],
            )?;
        }
        tx.execute(
            "UPDATE hub_work_deliveries SET acknowledged = 1, updated_at = ?2 WHERE id = ?1",
            params![ack.delivery, now],
        )?;
        if !acknowledged {
            adjust_work_payload_capacity(
                &tx,
                &who,
                work_payload_row_bytes(page.len())?
                    .checked_sub(WORK_DELIVERY_RESERVATION_BYTES)
                    .ok_or_else(|| HubError::Invalid("work payload size is too large".into()))?,
                now,
            )?;
        }
        tx.commit()?;
        Ok(())
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
        let mut conn = self.conn();
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let active_deliveries: i64 = tx.query_row(
            "SELECT COUNT(*) FROM hub_work_deliveries
             WHERE provider = ?1 AND host = ?2 AND repository = ?3 AND repository_id IS ?4
               AND acknowledged = 0 AND updated_at >= ?5",
            params![
                scope.provider,
                scope.host,
                scope.repository,
                scope.repository_id,
                now.saturating_sub(86_400)
            ],
            |row| row.get(0),
        )?;
        if active_deliveries > 0 {
            return Err(HubError::Invalid(
                "work compaction is blocked by an unacknowledged briefing delivery".into(),
            ));
        }
        let operations: i64 = tx.query_row(
            "SELECT COUNT(*) FROM hub_work_checkpoint_ops o
             JOIN hub_work_items w ON w.id = o.work
             JOIN hub_events e ON e.seq = o.event_seq
             WHERE w.provider = ?1 AND w.host = ?2 AND w.repository = ?3
               AND w.repository_id IS ?4 AND e.at < ?5",
            params![
                scope.provider,
                scope.host,
                scope.repository,
                scope.repository_id,
                before
            ],
            |row| row.get(0),
        )?;
        let versions: i64 = tx.query_row(
            "SELECT COUNT(*) FROM hub_work_versions v
             JOIN hub_work_items w ON w.id = v.work
             JOIN hub_events e ON e.seq = v.event_seq
             WHERE w.provider = ?1 AND w.host = ?2 AND w.repository = ?3
               AND w.repository_id IS ?4 AND e.at < ?5
               AND v.event_seq <> (SELECT MAX(v2.event_seq) FROM hub_work_versions v2
                   JOIN hub_events e2 ON e2.seq = v2.event_seq
                   WHERE v2.work = v.work AND e2.at < ?5)",
            params![
                scope.provider,
                scope.host,
                scope.repository,
                scope.repository_id,
                before
            ],
            |row| row.get(0),
        )?;
        let candidate_snapshot_floor: Option<i64> = tx.query_row(
            "SELECT MAX(baseline) FROM (
                 SELECT v.work, MAX(v.event_seq) AS baseline, COUNT(*) AS versions
                 FROM hub_work_versions v JOIN hub_work_items w ON w.id = v.work
                 JOIN hub_events e ON e.seq = v.event_seq
                 WHERE w.provider = ?1 AND w.host = ?2 AND w.repository = ?3
                   AND w.repository_id IS ?4 AND e.at < ?5
                 GROUP BY v.work HAVING COUNT(*) > 1
             )",
            params![
                scope.provider,
                scope.host,
                scope.repository,
                scope.repository_id,
                before
            ],
            |row| row.get(0),
        )?;
        let (current_event_floor, current_snapshot_floor): (i64, i64) = tx
            .query_row(
                "SELECT event_floor_seq, snapshot_floor_seq FROM hub_work_retention
                 WHERE provider = ?1 AND host = ?2 AND repository = ?3 AND repository_id IS ?4",
                params![
                    scope.provider,
                    scope.host,
                    scope.repository,
                    scope.repository_id
                ],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?
            .unwrap_or((0, 0));
        let retention_floor = candidate_snapshot_floor.map_or(current_snapshot_floor, |floor| {
            floor.max(current_snapshot_floor)
        });
        let current_coordination_floor: i64 = tx
            .query_row(
                "SELECT event_floor_seq FROM hub_coordination_retention
                 WHERE provider=?1 AND host=?2 AND repository=?3 AND repository_id IS ?4",
                params![
                    scope.provider,
                    scope.host,
                    scope.repository,
                    scope.repository_id
                ],
                |row| row.get(0),
            )
            .optional()?
            .unwrap_or(0);
        let compacted_coordination = {
            let mut statement = tx.prepare(
                "SELECT r.id, (
                     SELECT MAX(e.seq) FROM hub_events e WHERE e.subject=r.id AND e.kind IN (
                       'coordination_requested','coordination_bound','coordination_retargeted',
                       'coordination_acknowledged','coordination_responded',
                       'coordination_resolved','coordination_withdrawn'))
                 FROM hub_coordination_requests r
                 WHERE r.provider=?1 AND r.host=?2 AND r.repository=?3 AND r.repository_id IS ?4
                   AND r.status IN ('resolved','withdrawn') AND r.updated_at < ?5
                   AND EXISTS (SELECT 1 FROM hub_events e WHERE e.subject=r.id AND e.kind IN (
                       'coordination_requested','coordination_bound','coordination_retargeted',
                       'coordination_acknowledged','coordination_responded',
                       'coordination_resolved','coordination_withdrawn'))
                   AND NOT EXISTS (SELECT 1 FROM hub_events e WHERE e.subject=r.id
                       AND e.kind IN ('coordination_requested','coordination_bound',
                         'coordination_retargeted','coordination_acknowledged',
                         'coordination_responded','coordination_resolved',
                         'coordination_withdrawn') AND e.at >= ?5)",
            )?;
            statement
                .query_map(
                    params![
                        scope.provider,
                        scope.host,
                        scope.repository,
                        scope.repository_id,
                        before
                    ],
                    |row| Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?)),
                )?
                .collect::<std::result::Result<Vec<_>, _>>()?
        };
        let coordination_floor = compacted_coordination
            .iter()
            .map(|(_, floor)| *floor)
            .max()
            .unwrap_or(0)
            .max(current_coordination_floor);
        if apply {
            tx.execute(
                "DELETE FROM hub_work_deliveries
                 WHERE provider = ?1 AND host = ?2 AND repository = ?3 AND repository_id IS ?4
                   AND updated_at < ?5",
                params![
                    scope.provider,
                    scope.host,
                    scope.repository,
                    scope.repository_id,
                    now.saturating_sub(86_400)
                ],
            )?;
            tx.execute(
                "DELETE FROM hub_work_checkpoint_ops WHERE rowid IN (
                     SELECT o.rowid FROM hub_work_checkpoint_ops o
                     JOIN hub_work_items w ON w.id = o.work
                     JOIN hub_events e ON e.seq = o.event_seq
                     WHERE w.provider = ?1 AND w.host = ?2 AND w.repository = ?3
                       AND w.repository_id IS ?4 AND e.at < ?5
                 )",
                params![
                    scope.provider,
                    scope.host,
                    scope.repository,
                    scope.repository_id,
                    before
                ],
            )?;
            tx.execute(
                "DELETE FROM hub_work_versions WHERE rowid IN (
                     SELECT v.rowid FROM hub_work_versions v
                     JOIN hub_work_items w ON w.id = v.work
                     JOIN hub_events e ON e.seq = v.event_seq
                     WHERE w.provider = ?1 AND w.host = ?2 AND w.repository = ?3
                       AND w.repository_id IS ?4 AND e.at < ?5
                       AND v.event_seq <> (SELECT MAX(v2.event_seq) FROM hub_work_versions v2
                           JOIN hub_events e2 ON e2.seq = v2.event_seq
                           WHERE v2.work = v.work AND e2.at < ?5)
                 )",
                params![
                    scope.provider,
                    scope.host,
                    scope.repository,
                    scope.repository_id,
                    before
                ],
            )?;
            let removed_event_floor: Option<i64> = tx.query_row(
                "SELECT MAX(e.seq) FROM hub_events e
                 JOIN hub_work_items w ON w.id = e.subject
                 WHERE w.provider = ?1 AND w.host = ?2 AND w.repository = ?3
                   AND w.repository_id IS ?4 AND e.at < ?5
                   AND e.kind IN ('work_started', 'work_joined', 'work_takeover', 'work_checkpoint')
                   AND NOT EXISTS (SELECT 1 FROM hub_work_versions v WHERE v.event_seq = e.seq)
                   AND NOT EXISTS (SELECT 1 FROM hub_work_checkpoint_ops o WHERE o.event_seq = e.seq)",
                params![
                    scope.provider,
                    scope.host,
                    scope.repository,
                    scope.repository_id,
                    before
                ],
                |row| row.get(0),
            )?;
            tx.execute(
                "DELETE FROM hub_events WHERE seq IN (
                     SELECT e.seq FROM hub_events e
                     JOIN hub_work_items w ON w.id = e.subject
                     WHERE w.provider = ?1 AND w.host = ?2 AND w.repository = ?3
                       AND w.repository_id IS ?4 AND e.at < ?5
                       AND e.kind IN ('work_started', 'work_joined', 'work_takeover', 'work_checkpoint')
                       AND NOT EXISTS (SELECT 1 FROM hub_work_versions v WHERE v.event_seq = e.seq)
                       AND NOT EXISTS (SELECT 1 FROM hub_work_checkpoint_ops o WHERE o.event_seq = e.seq)
                 )",
                params![
                    scope.provider,
                    scope.host,
                    scope.repository,
                    scope.repository_id,
                    before
                ],
            )?;
            let event_floor = removed_event_floor
                .map_or(current_event_floor, |floor| floor.max(current_event_floor));
            if candidate_snapshot_floor.is_some() || removed_event_floor.is_some() {
                let changed = tx.execute(
                    "UPDATE hub_work_retention
                     SET event_floor_seq = MAX(event_floor_seq, ?5),
                         snapshot_floor_seq = MAX(snapshot_floor_seq, ?6), updated_at = ?7
                     WHERE provider = ?1 AND host = ?2 AND repository = ?3
                       AND repository_id IS ?4",
                    params![
                        scope.provider,
                        scope.host,
                        scope.repository,
                        scope.repository_id,
                        event_floor,
                        retention_floor,
                        now
                    ],
                )?;
                if changed == 0 {
                    tx.execute(
                        "INSERT INTO hub_work_retention
                            (provider, host, repository, repository_id, event_floor_seq,
                             snapshot_floor_seq, updated_at)
                         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                        params![
                            scope.provider,
                            scope.host,
                            scope.repository,
                            scope.repository_id,
                            event_floor,
                            retention_floor,
                            now
                        ],
                    )?;
                }
            }
            for (request, _) in &compacted_coordination {
                tx.execute(
                    "DELETE FROM hub_coordination_operations WHERE request=?1",
                    [request],
                )?;
                tx.execute(
                    "DELETE FROM hub_coordination_acknowledgements WHERE request=?1",
                    [request],
                )?;
                tx.execute(
                    "DELETE FROM hub_coordination_responses WHERE request=?1",
                    [request],
                )?;
                tx.execute(
                    "DELETE FROM hub_coordination_versions WHERE request=?1",
                    [request],
                )?;
                tx.execute(
                    "DELETE FROM hub_coordination_targets WHERE request=?1",
                    [request],
                )?;
                tx.execute(
                    "DELETE FROM hub_coordination_requests WHERE id=?1",
                    [request],
                )?;
                tx.execute(
                    "DELETE FROM hub_events WHERE subject=?1 AND kind IN (
                       'coordination_requested','coordination_bound','coordination_retargeted',
                       'coordination_acknowledged','coordination_responded',
                       'coordination_resolved','coordination_withdrawn') AND at < ?2",
                    params![request, before],
                )?;
            }
            if coordination_floor > current_coordination_floor {
                let changed = tx.execute(
                    "UPDATE hub_coordination_retention
                     SET event_floor_seq=MAX(event_floor_seq,?5), updated_at=?6
                     WHERE provider=?1 AND host=?2 AND repository=?3 AND repository_id IS ?4",
                    params![
                        scope.provider,
                        scope.host,
                        scope.repository,
                        scope.repository_id,
                        coordination_floor,
                        now
                    ],
                )?;
                if changed == 0 {
                    tx.execute(
                        "INSERT INTO hub_coordination_retention
                            (provider,host,repository,repository_id,event_floor_seq,updated_at)
                         VALUES (?1,?2,?3,?4,?5,?6)",
                        params![
                            scope.provider,
                            scope.host,
                            scope.repository,
                            scope.repository_id,
                            coordination_floor,
                            now
                        ],
                    )?;
                }
            }
            let charged = work_payload_bytes(
                &tx,
                &scope.provider,
                &scope.host,
                &scope.repository,
                scope.repository_id,
            )?;
            let changed = tx.execute(
                "UPDATE hub_work_usage SET charged_bytes = ?5, updated_at = ?6
                 WHERE provider = ?1 AND host = ?2 AND repository = ?3
                   AND repository_id IS ?4",
                params![
                    scope.provider,
                    scope.host,
                    scope.repository,
                    scope.repository_id,
                    charged,
                    now
                ],
            )?;
            if changed == 0 {
                tx.execute(
                    "INSERT INTO hub_work_usage
                        (provider, host, repository, repository_id, charged_bytes, updated_at)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                    params![
                        scope.provider,
                        scope.host,
                        scope.repository,
                        scope.repository_id,
                        charged,
                        now
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
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        let who = work_who(&tx, token, now)?;
        let subscriber_key = subscriber.unwrap_or("");
        let after = tx
            .query_row(
                "SELECT cursor FROM hub_work_acks
                 WHERE owner = ?1 AND provider = ?2 AND host = ?3 AND repository = ?4
                   AND repository_id IS ?5 AND subscriber_token = ?6
                   AND subscriber_session = ?7",
                params![
                    who.owner.id,
                    who.owner.identity.provider,
                    who.owner.identity.host,
                    who.repository,
                    who.repository_id,
                    who.token,
                    subscriber_key
                ],
                |row| row.get::<_, i64>(0),
            )
            .optional()?
            .unwrap_or(0);
        let through = work_event_high_water(&tx, &who, after)?;
        let page = briefing_page(
            &tx,
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
    }

    fn work_overview(
        &self,
        token: &str,
        query: &WorkOverviewQuery,
        now: i64,
    ) -> Result<WorkOverview> {
        let recent_since = query.validate(now)?;
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        let who = work_who(&tx, token, now)?;
        let fetch_limit = i64::try_from(query.limit + 1).unwrap_or(101);
        let current_rows = {
            let mut statement = tx.prepare(
                "SELECT snapshot FROM hub_work_items
                 WHERE provider = ?1 AND host = ?2 AND repository = ?3 AND repository_id IS ?4
                   AND state <> 'finished' ORDER BY id LIMIT ?5",
            )?;
            statement
                .query_map(
                    params![
                        who.owner.identity.provider,
                        who.owner.identity.host,
                        who.repository,
                        who.repository_id,
                        fetch_limit
                    ],
                    |row| row.get::<_, String>(0),
                )?
                .collect::<std::result::Result<Vec<_>, _>>()?
        };
        let mut current = current_rows
            .into_iter()
            .map(|snapshot| json_value::<WorkItem>(&snapshot))
            .collect::<Result<Vec<_>>>()?;
        let mut current_complete = current.len() <= query.limit;
        current.truncate(query.limit);
        for item in &mut current {
            project_claim(&tx, item, now)?;
        }
        let overview_part_budget = (MAX_BRIEFING_BYTES - 4096) / 3;
        while json_string(&current)?.len() > overview_part_budget {
            current.pop();
            current_complete = false;
        }

        let recent_rows = {
            let mut statement = tx.prepare(
                "SELECT v.event_seq, e.at, v.snapshot FROM hub_work_versions v
                 JOIN hub_work_items w ON w.id = v.work
                 JOIN hub_events e ON e.seq = v.event_seq
                 WHERE w.provider = ?1 AND w.host = ?2 AND w.repository = ?3
                   AND w.repository_id IS ?4 AND e.at >= ?5 AND e.at <= ?6
                   AND e.kind = 'work_checkpoint'
                   AND json_extract(v.snapshot, '$.current.kind') IN ('decision', 'finish')
                 ORDER BY v.event_seq DESC LIMIT ?7",
            )?;
            statement
                .query_map(
                    params![
                        who.owner.identity.provider,
                        who.owner.identity.host,
                        who.repository,
                        who.repository_id,
                        recent_since,
                        now,
                        fetch_limit
                    ],
                    |row| {
                        Ok((
                            row.get::<_, i64>(0)?,
                            row.get::<_, i64>(1)?,
                            row.get::<_, String>(2)?,
                        ))
                    },
                )?
                .collect::<std::result::Result<Vec<_>, _>>()?
        };
        let mut recent = recent_rows
            .into_iter()
            .map(|(seq, at, snapshot)| {
                let item: WorkItem = json_value(&snapshot)?;
                let kind = if item.state == WorkState::Finished {
                    WorkOutcomeKind::Completion
                } else {
                    WorkOutcomeKind::Decision
                };
                Ok(WorkOutcome {
                    seq,
                    at,
                    kind,
                    item,
                })
            })
            .collect::<Result<Vec<_>>>()?;
        let (event_floor, snapshot_floor) = work_retention_floors(&tx, &who)?;
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
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        let who = work_who(&tx, token, now)?;
        let snapshots = {
            let mut statement = tx.prepare(
                "SELECT w.snapshot FROM hub_work_items w
                 JOIN hub_work_participants p ON p.work = w.id
                 WHERE w.provider = ?1 AND w.host = ?2 AND w.repository = ?3
                   AND w.repository_id IS ?4 AND w.state IN ('active','blocked')
                   AND p.token = ?5 AND p.session = ?6 AND p.generation = w.generation
                 ORDER BY w.id LIMIT 2",
            )?;
            statement
                .query_map(
                    params![
                        who.owner.identity.provider,
                        who.owner.identity.host,
                        who.repository,
                        who.repository_id,
                        who.token,
                        session
                    ],
                    |row| row.get::<_, String>(0),
                )?
                .collect::<std::result::Result<Vec<_>, _>>()?
        };
        if snapshots.len() > 1 {
            return Err(HubError::Invalid(
                "session participates in more than one unfinished work item".into(),
            ));
        }
        let eligible = !snapshots.is_empty();
        let fallback = if eligible {
            None
        } else {
            tx.query_row(
                "SELECT w.snapshot FROM hub_work_items w
                 JOIN hub_work_participants p ON p.work = w.id
                 WHERE w.provider = ?1 AND w.host = ?2 AND w.repository = ?3
                   AND w.repository_id IS ?4 AND p.token = ?5 AND p.session = ?6
                 ORDER BY w.updated_at DESC, w.id LIMIT 1",
                params![
                    who.owner.identity.provider,
                    who.owner.identity.host,
                    who.repository,
                    who.repository_id,
                    who.token,
                    session
                ],
                |row| row.get::<_, String>(0),
            )
            .optional()?
        };
        let mut item = snapshots
            .into_iter()
            .next()
            .or(fallback)
            .map(|snapshot| json_value::<WorkItem>(&snapshot))
            .transpose()?;
        if let Some(item) = &mut item {
            project_claim(&tx, item, now)?;
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
            requested_scope,
            uncovered_scope,
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
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        let who = work_who(&tx, token, now)?;
        let page = coordination_page(
            &tx,
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
    }

    fn coordination_list(
        &self,
        token: &str,
        query: &CoordinationListQuery,
        now: i64,
    ) -> Result<CoordinationInbox> {
        query.validate()?;
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        let who = work_who(&tx, token, now)?;
        let page = coordination_page(
            &tx,
            &who,
            None,
            (query.after.unwrap_or(0), query.through, query.active_only),
            query.limit,
            now,
        )?;
        tx.commit()?;
        Ok(page)
    }

    fn coordination_ack(
        &self,
        token: &str,
        ack: &CoordinationAck,
        now: i64,
    ) -> Result<CoordinationRequest> {
        ack.validate()?;
        let mut conn = self.conn();
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let who = work_who(&tx, token, now)?;
        if who.read_only {
            return Err(HubError::Unauthorized);
        }
        let request = coordination_request_at(&tx, &who, &ack.request_id, i64::MAX, now)?
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
        let already = request.acknowledgements.iter().any(|receipt| {
            receipt.target_work_id.as_deref().unwrap_or("") == target
                && receipt.agent.token == who.token
                && receipt.agent.session == ack.session
                && receipt.generation == generation
                && receipt.through_seq >= ack.expected_updated_seq
        });
        if already {
            tx.commit()?;
            return Ok(request);
        }
        if request.updated_seq != ack.expected_updated_seq {
            return Err(HubError::Invalid(format!(
                "coordination request changed; current updated_seq is {}",
                request.updated_seq
            )));
        }
        let updates: i64 = tx.query_row(
            "SELECT COUNT(*) FROM hub_coordination_acknowledgements WHERE request = ?1",
            [&request.id],
            |row| row.get(0),
        )?;
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
        let seq = append(&tx, &event)?;
        tx.execute(
            "INSERT INTO hub_coordination_acknowledgements
                (request, target_work, token, agent, session, owner, login, generation,
                 through_seq, acknowledged_at, event_seq)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
            params![
                request.id,
                target,
                who.token,
                who.agent,
                ack.session,
                who.owner.id,
                who.owner.login,
                generation,
                seq,
                now,
                seq
            ],
        )?;
        save_coordination_version(
            &tx,
            &request.id,
            request.status,
            request.proposal.work_id.as_deref(),
            now,
            seq,
        )?;
        let receipt = json_string(&(target, &who.token, &ack.session, generation, seq))?;
        adjust_work_payload_capacity(
            &tx,
            &who,
            coordination_payload_bytes(&[&receipt, ""], &event)?,
            now,
        )?;
        let updated = coordination_request_at(&tx, &who, &ack.request_id, i64::MAX, now)?
            .ok_or_else(|| HubError::Unavailable("coordination request disappeared".into()))?;
        tx.commit()?;
        Ok(updated)
    }

    fn coordination_reply(
        &self,
        token: &str,
        reply: &CoordinationReply,
        now: i64,
    ) -> Result<CoordinationRequest> {
        reply.validate()?;
        let payload = json_string(reply)?;
        let mut conn = self.conn();
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let who = work_who(&tx, token, now)?;
        if who.read_only {
            return Err(HubError::Unauthorized);
        }
        let request = coordination_request_at(&tx, &who, &reply.request_id, i64::MAX, now)?
            .ok_or_else(|| HubError::Invalid("unknown coordination request".into()))?;
        let target = coordination_target_for(&request, &reply.target_work_id, &who, &reply.session)
            .ok_or(HubError::Unauthorized)?;
        if coordination_operation_replay(&tx, &request.id, &reply.operation_id, "reply", &payload)?
        {
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
        let updates: i64 = tx.query_row(
            "SELECT COUNT(*) FROM hub_coordination_responses WHERE request = ?1",
            [&request.id],
            |row| row.get(0),
        )?;
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
        let seq = append(&tx, &event)?;
        tx.execute(
            "INSERT INTO hub_coordination_responses
                (id, request, operation_id, target_work, token, agent, session, owner, login,
                 target_generation, kind, note, created_at, event_seq)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)",
            params![
                response_id,
                request.id,
                reply.operation_id,
                target.work_id,
                who.token,
                who.agent,
                reply.session,
                who.owner.id,
                who.owner.login,
                target.generation,
                response_kind_name(reply.kind),
                reply.note,
                now,
                seq
            ],
        )?;
        tx.execute(
            "INSERT INTO hub_coordination_operations
                (request, operation_id, kind, payload, event_seq)
             VALUES (?1, ?2, 'reply', ?3, ?4)",
            params![request.id, reply.operation_id, payload, seq],
        )?;
        let all_responded = coordination_all_targets_responded(&tx, &request.id)?;
        let status = if all_responded {
            CoordinationStatus::Responded
        } else {
            CoordinationStatus::Pending
        };
        save_coordination_version(
            &tx,
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
            &tx,
            &who,
            coordination_payload_bytes(&[&payload, &response, ""], &event)?,
            now,
        )?;
        let updated = coordination_request_at(&tx, &who, &reply.request_id, i64::MAX, now)?
            .ok_or_else(|| HubError::Unavailable("coordination request disappeared".into()))?;
        tx.commit()?;
        Ok(updated)
    }

    fn coordination_resolve(
        &self,
        token: &str,
        resolve: &CoordinationResolve,
        now: i64,
    ) -> Result<CoordinationRequest> {
        resolve.validate()?;
        let payload = json_string(resolve)?;
        let mut conn = self.conn();
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let who = work_who(&tx, token, now)?;
        if who.read_only {
            return Err(HubError::Unauthorized);
        }
        let request = coordination_request_at(&tx, &who, &resolve.request_id, i64::MAX, now)?
            .ok_or_else(|| HubError::Invalid("unknown coordination request".into()))?;
        let authorized = coordination_is_requester(&request, &who, &resolve.session)
            || request.targets.iter().any(|target| {
                target.lead.token == who.token && target.lead.session == resolve.session
            });
        if !authorized {
            return Err(HubError::Unauthorized);
        }
        if coordination_operation_replay(
            &tx,
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
            let item = scoped_work(&tx, &who, work, now)?
                .ok_or_else(|| HubError::Unavailable("proposed work item is missing".into()))?;
            if item.state == WorkState::Finished {
                Vec::new()
            } else if coordination_was_initially_bound(&tx, &request.id)? {
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
            &tx,
            &who,
            &proposal_scope,
            request.proposal.work_id.as_deref(),
            now,
        )?;
        let mut needs_coordination = Vec::new();
        for overlap in unresolved.drain(..) {
            let participating = tx.query_row(
                "SELECT EXISTS(SELECT 1 FROM hub_work_participants
                 WHERE work = ?1 AND token = ?2 AND session = ?3 AND generation = ?4)",
                params![
                    overlap.item.id,
                    request.requester.token,
                    request.requester.session,
                    overlap.item.generation
                ],
                |row| row.get::<_, bool>(0),
            )?;
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
                &tx,
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
        let seq = append(&tx, &event)?;
        tx.execute(
            "INSERT INTO hub_coordination_operations
                (request, operation_id, kind, payload, event_seq)
             VALUES (?1, ?2, 'resolve', ?3, ?4)",
            params![request.id, resolve.operation_id, payload, seq],
        )?;
        save_coordination_version(
            &tx,
            &request.id,
            CoordinationStatus::Resolved,
            request.proposal.work_id.as_deref(),
            now,
            seq,
        )?;
        adjust_work_payload_capacity(
            &tx,
            &who,
            coordination_payload_bytes(&[&payload, ""], &event)?,
            now,
        )?;
        let updated = coordination_request_at(&tx, &who, &resolve.request_id, i64::MAX, now)?
            .ok_or_else(|| HubError::Unavailable("coordination request disappeared".into()))?;
        tx.commit()?;
        Ok(updated)
    }

    fn coordination_withdraw(
        &self,
        token: &str,
        withdraw: &CoordinationWithdraw,
        now: i64,
    ) -> Result<CoordinationRequest> {
        withdraw.validate()?;
        let mut conn = self.conn();
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let who = work_who(&tx, token, now)?;
        if who.read_only {
            return Err(HubError::Unauthorized);
        }
        let request = coordination_request_at(&tx, &who, &withdraw.request_id, i64::MAX, now)?
            .ok_or_else(|| HubError::Invalid("unknown coordination request".into()))?;
        let requester = coordination_is_requester(&request, &who, &withdraw.session);
        let requester_available = tx
            .query_row(
                "SELECT 1 FROM hub_agent_tokens t JOIN hub_users u ON u.id = t.owner
                 WHERE t.id = ?1 AND t.revoked = 0 AND t.expires_at > ?2 AND u.disabled = 0",
                params![request.requester.token, now],
                |_| Ok(()),
            )
            .optional()?
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
            &tx,
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
        let seq = append(&tx, &event)?;
        tx.execute(
            "INSERT INTO hub_coordination_operations
                (request, operation_id, kind, payload, event_seq)
             VALUES (?1, ?2, 'withdraw', ?3, ?4)",
            params![request.id, withdraw.operation_id, payload, seq],
        )?;
        save_coordination_version(
            &tx,
            &request.id,
            CoordinationStatus::Withdrawn,
            request.proposal.work_id.as_deref(),
            now,
            seq,
        )?;
        adjust_work_payload_capacity(
            &tx,
            &who,
            coordination_payload_bytes(&[&payload, ""], &event)?,
            now,
        )?;
        let updated = coordination_request_at(&tx, &who, &withdraw.request_id, i64::MAX, now)?
            .ok_or_else(|| HubError::Unavailable("coordination request disappeared".into()))?;
        tx.commit()?;
        Ok(updated)
    }

    fn latest_event_seq(&self) -> Result<i64> {
        Ok(self
            .conn()
            .query_row("SELECT COALESCE(MAX(seq), 0) FROM hub_events", [], |row| {
                row.get(0)
            })?)
    }

    fn events_after(&self, seq: i64, limit: usize) -> Result<Vec<HubEvent>> {
        let conn = self.conn();
        let mut statement = conn.prepare(
            "SELECT seq, id, at, kind, subject, data FROM hub_events
             WHERE seq > ?1 ORDER BY seq LIMIT ?2",
        )?;
        let events = statement
            .query_map(params![seq, event_limit(limit)], |row| {
                Ok(HubEvent {
                    seq: row.get(0)?,
                    id: row.get(1)?,
                    at: row.get(2)?,
                    kind: row.get(3)?,
                    subject: row.get(4)?,
                    data: row.get(5)?,
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(events)
    }
}
