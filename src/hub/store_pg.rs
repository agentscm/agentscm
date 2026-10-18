use std::collections::BTreeSet;
use std::net::IpAddr;
use std::str::FromStr;
use std::sync::{Mutex, PoisonError};
use std::time::Duration;

use postgres::config::{Host, SslMode};
use postgres::{Client, Config, IsolationLevel, NoTls};

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

/// Held for the whole migration transaction so hubs starting together serialize.
const MIGRATION_LOCK: i64 = 0x6167_656e_7473_636d;
/// Serializes event appends so `seq` order is commit order across hub processes.
const EVENT_LOCK: i64 = 0x6167_656e_7473_6576;
/// First key of the advisory lock that serializes intent announces per repository.
const INTENT_LOCK_CLASS: i32 = 0x6167_656e;
/// First key of the advisory lock that serializes publication creates per token, so the
/// open-publication cap holds across hub processes.
const PUBLICATION_LOCK_CLASS: i32 = 0x6167_6570;
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
    Ok(tx.commit()?)
}

fn append(tx: &mut postgres::Transaction<'_>, event: &Event) -> Result<()> {
    tx.execute("SELECT pg_advisory_xact_lock($1)", &[&EVENT_LOCK])?;
    tx.execute(
        "INSERT INTO hub_events (id, at, kind, subject, data) VALUES ($1, $2, $3, $4, $5)",
        &[
            &event.id,
            &event.at,
            &event.kind,
            &event.subject,
            &event.data,
        ],
    )?;
    Ok(())
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

fn publication_row(row: &postgres::Row) -> Publication {
    Publication {
        id: row.get(0),
        repository: row.get(1),
        token: row.get(2),
        owner: row.get(20),
        agent: row.get(3),
        owner_login: row.get(4),
        revision: row.get(5),
        event_seq: row.get(6),
        commit: row.get(7),
        branch: row.get(8),
        base: row.get(9),
        manifest: row.get(10),
        branch_state: row.get(11),
        branch_message: row.get(12),
        pr_number: row.get(13),
        pr_node_id: row.get(14),
        pr_url: row.get(15),
        merge_commit: row.get(16),
        error: row.get(17),
        created_at: row.get(18),
        updated_at: row.get(19),
    }
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
    Ok(row.as_ref().map(publication_row))
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

/// The named session, else the token's only recent hook-connected one, else the default.
fn session_name(
    tx: &mut postgres::Transaction<'_>,
    token: &str,
    requested: Option<&str>,
    now: i64,
) -> Result<String> {
    if let Some(name) = requested {
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
fn touch_in(
    tx: &mut postgres::Transaction<'_>,
    who: &Who,
    call: &Checked,
    now: i64,
) -> Result<Touched> {
    let token = who.token.as_str();
    // Two calls of one session must not each start an intent.
    lock_repository(tx, &who.repository)?;
    tx.execute(
        "DELETE FROM hub_sessions WHERE token = $1 AND updated_at < $2",
        &[&token, &(now - STALE_SESSION_SECONDS)],
    )?;
    let name = session_name(tx, token, call.session.as_deref(), now)?;
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
            lock_repository(&mut tx, &who.repository)?;
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

    fn create_publication(&self, token: &str, new: &NewPublication, now: i64) -> Result<Create> {
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
                return Ok(Create::of_existing(found, &who.repository, &manifest));
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
                     commit_sha, branch, base, manifest, created_at, updated_at)
                 VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $10) ON CONFLICT (id) DO NOTHING",
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
            Ok(Create::of_existing(found, &who.repository, &manifest))
        })
    }

    fn publication(&self, repository: &str, id: &str) -> Result<Option<Publication>> {
        self.run(true, |client| {
            let mut tx = client.transaction()?;
            Ok(publication_by_id(&mut tx, id, false)?.filter(|p| p.repository == repository))
        })
    }

    fn update_publication(&self, id: &str, update: &Update, now: i64) -> Result<Publication> {
        self.run(false, |client| {
            let mut tx = client.transaction()?;
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
