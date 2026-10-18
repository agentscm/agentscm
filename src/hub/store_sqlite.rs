use std::collections::BTreeSet;
use std::path::Path;
use std::sync::{Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use rusqlite::{Connection, OptionalExtension, ToSql, Transaction, TransactionBehavior, params};

use super::connection::{
    CONNECTION_SELECT, ConnectedRepository, Connection as ForgeConnection, DELIVERY_KEEP_SECONDS,
    NewConnection, Observation, Observed, Outbound, REPOSITORY_SELECT, gone_event,
    observation_kinds, validate_observation, validate_repository_row, validate_status,
};
use super::handoff::{
    Answered, Asked, HandoffNote, MAX_OPEN, PER_TOUCH, Request, Verdict, answer_kind, clean_note,
    cutoff, file_key, grant_call, judge, remaining, still_holds,
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
    NewAgentToken, Principal, SessionView, Store, TokenDigest, User, event_limit, new_id, pending,
    session_key, unix_now, validate_login,
};
use super::{HubError, Result};

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
    Ok(tx.commit()?)
}

fn append(tx: &Transaction<'_>, event: &Event) -> Result<()> {
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
    Ok(())
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

/// The named session, else the token's only recent hook-connected one, else the default.
fn session_name(
    tx: &Transaction<'_>,
    token: &str,
    requested: Option<&str>,
    now: i64,
) -> Result<String> {
    if let Some(name) = requested {
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

fn session_row(tx: &Transaction<'_>, token: &str, session: &str) -> Result<Option<Row>> {
    Ok(tx
        .query_row(
            "SELECT intent, task, via, notice_cursor, denied, deny_times FROM hub_sessions
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
                })
            },
        )
        .optional()?)
}

/// One touch of the session inside the caller's transaction. A call without `report` (made on
/// the session's behalf, or answered without text) tells and consumes nothing.
fn touch_in(tx: &Transaction<'_>, who: &Who, call: &Checked, now: i64) -> Result<Touched> {
    let token = who.token.as_str();
    tx.execute(
        "DELETE FROM hub_sessions WHERE token = ?1 AND updated_at < ?2",
        params![token, now - STALE_SESSION_SECONDS],
    )?;
    let name = session_name(tx, token, call.session.as_deref(), now)?;
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
             created_at, updated_at, denied, deny_times)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?7, ?8, ?9)
         ON CONFLICT (token, session) DO UPDATE SET intent = excluded.intent,
             task = excluded.task, via = excluded.via,
             notice_cursor = excluded.notice_cursor, updated_at = excluded.updated_at,
             denied = excluded.denied, deny_times = excluded.deny_times",
        params![
            token,
            name,
            step.new.as_ref().map(|new| &new.id),
            step.task,
            serde_json::json!(step.via).to_string(),
            cursor,
            now,
            guarded.denied_json,
            guarded.times_json
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

    fn repository_events(&self, who: &Principal, limit: usize) -> Result<Vec<ActivityEvent>> {
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
             ORDER BY e.seq DESC LIMIT ?5"
        ))?;
        let events = statement
            .query_map(
                params![
                    who.repository,
                    who.repository_id,
                    who.owner.identity.host,
                    who.owner.identity.provider,
                    event_limit(limit)
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

    fn append_pr_notices(&self, notices: &[(String, String)]) -> Result<()> {
        let events = notices
            .iter()
            .map(|(intent, data)| Event::pr_notice(intent, data))
            .collect::<Result<Vec<_>>>()?;
        let mut conn = self.conn();
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        for event in &events {
            append(&tx, event)?;
        }
        Ok(tx.commit()?)
    }

    fn create_publication(&self, token: &str, new: &NewPublication, now: i64) -> Result<Create> {
        let mut conn = self.conn();
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let who = active_token(&tx, token, now)?;
        new.validate(&who.repository)?;
        let m = &new.manifest;
        let manifest = m.canonical();
        if let Some(found) = publication_by_id(&tx, &new.id)? {
            return Ok(Create::of_existing(found, &who.repository, &manifest));
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
                 branch, base, manifest, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?10)",
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
                now
            ],
        )?;
        append(&tx, &Event::publication_requested(&who.repository, new)?)?;
        let found = publication_by_id(&tx, &new.id)?
            .ok_or_else(|| HubError::Unavailable("publication not found after insert".into()))?;
        tx.commit()?;
        Ok(Create::of_existing(found, &who.repository, &manifest))
    }

    fn publication(&self, repository: &str, id: &str) -> Result<Option<Publication>> {
        Ok(publication_by_id(&self.conn(), id)?.filter(|p| p.repository == repository))
    }

    fn update_publication(&self, id: &str, update: &Update, now: i64) -> Result<Publication> {
        let mut conn = self.conn();
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let mut p = publication_by_id(&tx, id)?
            .ok_or_else(|| HubError::Invalid("unknown publication".into()))?;
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
