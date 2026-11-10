//! Durable work coordination store conformance. Every case runs on SQLite and,
//! when configured, PostgreSQL.
use crate::hub_pg;
use agentscm::hub::capacity::CapacityLimit;

use std::sync::{Arc, Barrier, mpsc};
use std::thread;
use std::time::Duration;

use agentscm::hub::HubError;
use agentscm::hub::User;
use agentscm::hub::catchup::{CatchupPreviewQuery, CatchupQuery};
use agentscm::hub::coordination::{
    CoordinationAck, CoordinationReply, CoordinationResponseKind, CoordinationStatus,
    CoordinationWithdraw,
};
use agentscm::hub::store::{
    self, ForgeIdentity, Store, mint_agent_token, mint_bound_agent_token, unix_now,
};
use agentscm::hub::work::{
    BriefingAck, BriefingQuery, CLAIM_STALE_SECONDS, CheckpointKind, ClaimStatus, ForgeWorkRef,
    SourceRef, WORK_DELIVERY_RESERVATION_BYTES, WORK_PAYLOAD_CAPACITY_BYTES, WorkCheckpoint,
    WorkConflictKind, WorkOverviewQuery, WorkRepositoryScope, WorkRole, WorkStart, WorkState,
};
use agentscm::hub::work_lifecycle::LEASE_SECONDS as LEASE;
use agentscm::hub::work_links::{
    WorkPrHeadStatus, WorkPrLinkQuery, WorkPrLinkTarget, WorkPrLinkUpdate, WorkPrSnapshotStatus,
};
use agentscm::hub::work_transition::{
    OperationalSelect, OutcomeProvenance, ReservationBasis, TerminalOutcome, WorkLifecycleReader,
    WorkTransitionActor, WorkTransitionKind, WorkTransitionRequest,
};
use hub_pg::PgSchema;
use postgres::NoTls;
use tempfile::TempDir;

const REPO: &str = "acme/widgets";
const REPOSITORY_ID: i64 = 101;
const TTL: i64 = 3600;

struct Db {
    url: String,
    _pg: Option<PgSchema>,
    _dir: Option<TempDir>,
}

impl Db {
    fn sqlite() -> Self {
        let dir = TempDir::new().unwrap();
        Self {
            url: format!("sqlite://{}", dir.path().join("hub.sqlite").display()),
            _pg: None,
            _dir: Some(dir),
        }
    }

    fn postgres() -> Option<Self> {
        let pg = PgSchema::create()?;
        Some(Self {
            url: pg.url.clone(),
            _pg: Some(pg),
            _dir: None,
        })
    }

    fn open(&self) -> Arc<dyn Store> {
        store::open(&self.url).unwrap()
    }

    fn assert_event_delete_boundary(&self) {
        const GUARDED: &[&str] = &[
            "reservation_lapsed",
            "session_started",
            "intent_created",
            "workflow_started",
            "work_checkpoint_extra",
        ];
        const COMPACTABLE: &[&str] = &[
            "work_started",
            "work_joined",
            "work_takeover",
            "work_checkpoint",
            "work_pr_link",
            "work_issue_link",
            "work_transition",
            "work_lead_credential",
            "coordination_target_released",
        ];
        if self._pg.is_some() {
            let mut client = postgres::Client::connect(&self.url, NoTls).unwrap();
            for (index, kind) in GUARDED.iter().chain(COMPACTABLE).enumerate() {
                let id = format!("event-guard-{index}");
                client
                    .execute(
                        "INSERT INTO hub_events (id, at, kind, subject, data)
                         VALUES ($1, 1, $2, 'event-guard', '{}')",
                        &[&id, kind],
                    )
                    .unwrap();
            }
            assert!(
                client
                    .execute("UPDATE hub_events SET data = '{}'", &[])
                    .is_err()
            );
            for kind in GUARDED {
                assert!(
                    client
                        .execute("DELETE FROM hub_events WHERE kind = $1", &[kind])
                        .is_err()
                );
            }
            for kind in COMPACTABLE {
                assert_eq!(
                    client
                        .execute("DELETE FROM hub_events WHERE kind = $1", &[kind])
                        .unwrap(),
                    1
                );
            }
        } else {
            let path = self.url.strip_prefix("sqlite://").unwrap();
            let conn = rusqlite::Connection::open(path).unwrap();
            for (index, kind) in GUARDED.iter().chain(COMPACTABLE).enumerate() {
                conn.execute(
                    "INSERT INTO hub_events (id, at, kind, subject, data)
                     VALUES (?1, 1, ?2, 'event-guard', '{}')",
                    rusqlite::params![format!("event-guard-{index}"), kind],
                )
                .unwrap();
            }
            assert!(
                conn.execute("UPDATE hub_events SET data = '{}'", [])
                    .is_err()
            );
            for kind in GUARDED {
                assert!(
                    conn.execute("DELETE FROM hub_events WHERE kind = ?1", [*kind])
                        .is_err()
                );
            }
            for kind in COMPACTABLE {
                assert_eq!(
                    conn.execute("DELETE FROM hub_events WHERE kind = ?1", [*kind])
                        .unwrap(),
                    1
                );
            }
        }
    }

    fn set_work_usage(&self, charged_bytes: i64) {
        if self._pg.is_some() {
            let mut client = postgres::Client::connect(&self.url, NoTls).unwrap();
            client
                .execute(
                    "INSERT INTO hub_work_usage
                        (provider, host, repository, repository_id, charged_bytes, updated_at)
                     VALUES ('github', 'github.com', $1, $2, $3, 1)",
                    &[&REPO, &REPOSITORY_ID, &charged_bytes],
                )
                .unwrap();
        } else {
            let path = self.url.strip_prefix("sqlite://").unwrap();
            let conn = rusqlite::Connection::open(path).unwrap();
            conn.execute(
                "INSERT INTO hub_work_usage
                    (provider, host, repository, repository_id, charged_bytes, updated_at)
                 VALUES ('github', 'github.com', ?1, ?2, ?3, 1)",
                rusqlite::params![REPO, REPOSITORY_ID, charged_bytes],
            )
            .unwrap();
        }
    }

    fn work_usage_and_aggregate(&self) -> (i64, i64) {
        let union = "SELECT octet_length(snapshot)::bigint + 256 AS bytes FROM hub_work_items
                       WHERE provider = 'github' AND host = 'github.com' AND repository = $1
                         AND repository_id IS NOT DISTINCT FROM $2
                     UNION ALL
                     SELECT octet_length(v.snapshot)::bigint + 256 FROM hub_work_versions v
                       JOIN hub_work_items w ON w.id = v.work
                       WHERE w.provider = 'github' AND w.host = 'github.com'
                         AND w.repository = $1 AND w.repository_id IS NOT DISTINCT FROM $2
                     UNION ALL
                     SELECT octet_length(d.snapshot)::bigint + 256 FROM hub_work_declarations d
                       JOIN hub_work_items w ON w.id = d.work
                       WHERE w.provider = 'github' AND w.host = 'github.com'
                         AND w.repository = $1 AND w.repository_id IS NOT DISTINCT FROM $2
                     UNION ALL
                     SELECT octet_length(d.snapshot)::bigint + 256 FROM hub_work_reconciliation_state d
                       JOIN hub_work_items w ON w.id = d.work
                       WHERE w.provider = 'github' AND w.host = 'github.com'
                         AND w.repository = $1 AND w.repository_id IS NOT DISTINCT FROM $2
                     UNION ALL
                     SELECT octet_length(l.snapshot)::bigint + 256 FROM hub_work_pr_links l
                       JOIN hub_work_items w ON w.id = l.work
                       WHERE w.provider = 'github' AND w.host = 'github.com'
                         AND w.repository = $1 AND w.repository_id IS NOT DISTINCT FROM $2
                     UNION ALL
                     SELECT octet_length(o.request)::bigint + 256 FROM hub_work_checkpoint_ops o
                       JOIN hub_work_items w ON w.id = o.work
                       WHERE w.provider = 'github' AND w.host = 'github.com'
                         AND w.repository = $1 AND w.repository_id IS NOT DISTINCT FROM $2
                     UNION ALL
                     SELECT CASE WHEN NOT acknowledged THEN 4194304::bigint
                                 ELSE octet_length(page)::bigint + 256 END
                       FROM hub_work_deliveries
                       WHERE provider = 'github' AND host = 'github.com' AND repository = $1
                         AND repository_id IS NOT DISTINCT FROM $2
                     UNION ALL
                     SELECT octet_length(e.data)::bigint + octet_length(e.kind)::bigint
                            + octet_length(e.subject)::bigint + 256
                       FROM hub_events e JOIN hub_work_items w ON w.id = e.subject
                       WHERE w.provider = 'github' AND w.host = 'github.com'
                         AND w.repository = $1 AND w.repository_id IS NOT DISTINCT FROM $2
                         AND e.kind IN
                            ('work_started', 'work_joined', 'work_takeover', 'work_checkpoint', 'work_pr_link', 'work_issue_link',
                             'work_transition', 'work_lead_credential')";
        if self._pg.is_some() {
            let mut client = postgres::Client::connect(&self.url, NoTls).unwrap();
            let used: i64 = client
                .query_one(
                    "SELECT charged_bytes FROM hub_work_usage
                     WHERE provider = 'github' AND host = 'github.com' AND repository = $1
                       AND repository_id IS NOT DISTINCT FROM $2",
                    &[&REPO, &REPOSITORY_ID],
                )
                .unwrap()
                .get(0);
            let aggregate: i64 = client
                .query_one(
                    &format!("SELECT COALESCE(SUM(bytes), 0)::bigint FROM ({union}) payloads"),
                    &[&REPO, &REPOSITORY_ID],
                )
                .unwrap()
                .get(0);
            (used, aggregate)
        } else {
            let path = self.url.strip_prefix("sqlite://").unwrap();
            let conn = rusqlite::Connection::open(path).unwrap();
            let used: i64 = conn
                .query_row(
                    "SELECT charged_bytes FROM hub_work_usage
                     WHERE provider = 'github' AND host = 'github.com' AND repository = ?1
                       AND repository_id IS ?2",
                    rusqlite::params![REPO, REPOSITORY_ID],
                    |row| row.get(0),
                )
                .unwrap();
            let aggregate: i64 = conn
                .query_row(
                    "SELECT COALESCE(SUM(bytes), 0) FROM (
                         SELECT length(CAST(snapshot AS BLOB)) + 256 AS bytes FROM hub_work_items
                          WHERE provider = 'github' AND host = 'github.com' AND repository = ?1
                            AND repository_id IS ?2
                         UNION ALL SELECT length(CAST(v.snapshot AS BLOB)) + 256
                          FROM hub_work_versions v JOIN hub_work_items w ON w.id = v.work
                          WHERE w.provider = 'github' AND w.host = 'github.com'
                            AND w.repository = ?1 AND w.repository_id IS ?2
                         UNION ALL SELECT length(CAST(d.snapshot AS BLOB)) + 256
                          FROM hub_work_declarations d JOIN hub_work_items w ON w.id = d.work
                          WHERE w.provider = 'github' AND w.host = 'github.com'
                            AND w.repository = ?1 AND w.repository_id IS ?2
                         UNION ALL SELECT length(CAST(d.snapshot AS BLOB)) + 256
                          FROM hub_work_reconciliation_state d JOIN hub_work_items w ON w.id = d.work
                          WHERE w.provider = 'github' AND w.host = 'github.com'
                            AND w.repository = ?1 AND w.repository_id IS ?2
                         UNION ALL SELECT length(CAST(l.snapshot AS BLOB)) + 256
                          FROM hub_work_pr_links l JOIN hub_work_items w ON w.id = l.work
                          WHERE w.provider = 'github' AND w.host = 'github.com'
                            AND w.repository = ?1 AND w.repository_id IS ?2
                         UNION ALL SELECT length(CAST(o.request AS BLOB)) + 256
                          FROM hub_work_checkpoint_ops o JOIN hub_work_items w ON w.id = o.work
                          WHERE w.provider = 'github' AND w.host = 'github.com'
                            AND w.repository = ?1 AND w.repository_id IS ?2
                         UNION ALL SELECT CASE WHEN acknowledged = 0 THEN 4194304
                            ELSE length(CAST(page AS BLOB)) + 256 END FROM hub_work_deliveries
                          WHERE provider = 'github' AND host = 'github.com' AND repository = ?1
                            AND repository_id IS ?2
                         UNION ALL SELECT length(CAST(e.data AS BLOB))
                            + length(CAST(e.kind AS BLOB)) + length(CAST(e.subject AS BLOB)) + 256
                          FROM hub_events e JOIN hub_work_items w ON w.id = e.subject
                          WHERE w.provider = 'github' AND w.host = 'github.com'
                            AND w.repository = ?1 AND w.repository_id IS ?2
                            AND e.kind IN ('work_started', 'work_joined', 'work_takeover',
                                           'work_checkpoint', 'work_pr_link', 'work_issue_link', 'work_transition',
                                           'work_lead_credential')
                     )",
                    rusqlite::params![REPO, REPOSITORY_ID],
                    |row| row.get(0),
                )
                .unwrap();
            (used, aggregate)
        }
    }

    /// Runs raw SQL, to build or inspect a database state the Store cannot reach.
    fn sql(&self, sql: &str) {
        if self._pg.is_some() {
            postgres::Client::connect(&self.url, NoTls)
                .unwrap()
                .batch_execute(sql)
                .unwrap();
        } else {
            let path = self.url.strip_prefix("sqlite://").unwrap();
            rusqlite::Connection::open(path)
                .unwrap()
                .execute_batch(sql)
                .unwrap();
        }
    }

    /// The stored `(terminal_outcome, reservation_basis)` of one Work item.
    fn reservation(&self, id: &str) -> (Option<String>, Option<String>) {
        let sql = "SELECT terminal_outcome, reservation_basis FROM hub_work_items WHERE id = ";
        if self._pg.is_some() {
            let row = postgres::Client::connect(&self.url, NoTls)
                .unwrap()
                .query_one(&format!("{sql}$1"), &[&id])
                .unwrap();
            (row.get(0), row.get(1))
        } else {
            let path = self.url.strip_prefix("sqlite://").unwrap();
            rusqlite::Connection::open(path)
                .unwrap()
                .query_row(&format!("{sql}?1"), [id], |row| {
                    Ok((row.get(0)?, row.get(1)?))
                })
                .unwrap()
        }
    }

    /// Runs a query and returns every column as text, with NULL as the empty string.
    fn rows(&self, sql: &str) -> Vec<Vec<String>> {
        if self._pg.is_some() {
            postgres::Client::connect(&self.url, NoTls)
                .unwrap()
                .simple_query(sql)
                .unwrap()
                .into_iter()
                .filter_map(|message| match message {
                    postgres::SimpleQueryMessage::Row(row) => Some(
                        (0..row.len())
                            .map(|index| row.get(index).unwrap_or("").to_owned())
                            .collect(),
                    ),
                    _ => None,
                })
                .collect()
        } else {
            let path = self.url.strip_prefix("sqlite://").unwrap();
            let conn = rusqlite::Connection::open(path).unwrap();
            let mut statement = conn.prepare(sql).unwrap();
            let columns = statement.column_count();
            statement
                .query_map([], |row| {
                    (0..columns)
                        .map(|index| {
                            Ok(match row.get::<_, rusqlite::types::Value>(index)? {
                                rusqlite::types::Value::Null => String::new(),
                                rusqlite::types::Value::Integer(value) => value.to_string(),
                                rusqlite::types::Value::Text(value) => value,
                                other => format!("{other:?}"),
                            })
                        })
                        .collect()
                })
                .unwrap()
                .collect::<Result<Vec<_>, _>>()
                .unwrap()
        }
    }

    fn delivery_count(&self) -> i64 {
        if self._pg.is_some() {
            postgres::Client::connect(&self.url, NoTls)
                .unwrap()
                .query_one(
                    "SELECT COUNT(*) FROM hub_work_deliveries WHERE repository = $1
                       AND repository_id = $2",
                    &[&REPO, &REPOSITORY_ID],
                )
                .unwrap()
                .get(0)
        } else {
            let path = self.url.strip_prefix("sqlite://").unwrap();
            rusqlite::Connection::open(path)
                .unwrap()
                .query_row(
                    "SELECT COUNT(*) FROM hub_work_deliveries WHERE repository = ?1
                       AND repository_id = ?2",
                    rusqlite::params![REPO, REPOSITORY_ID],
                    |row| row.get(0),
                )
                .unwrap()
        }
    }
}

#[derive(Clone)]
struct Agent {
    token: String,
    owner: String,
    expires_at: i64,
}

struct AgentSpec<'a> {
    provider: &'a str,
    host: &'a str,
    user_id: i64,
    repository: &'a str,
    repository_id: Option<i64>,
    read_only: bool,
    ttl: i64,
}

fn agent_with(store: &dyn Store, spec: AgentSpec<'_>) -> Agent {
    let owner = store
        .upsert_user(
            &ForgeIdentity {
                provider: spec.provider.into(),
                host: spec.host.into(),
                user_id: spec.user_id,
            },
            &format!("user{}", spec.user_id),
        )
        .unwrap();
    let (token, expires_at) = if let Some(repository_id) = spec.repository_id {
        let (token, _, expires_at) = mint_bound_agent_token(
            store,
            &owner.id,
            &format!("agent{}", spec.user_id),
            spec.repository,
            repository_id,
            spec.ttl,
            spec.read_only,
        )
        .unwrap();
        (token, expires_at)
    } else {
        assert!(!spec.read_only);
        let (token, _) = mint_agent_token(
            store,
            &owner.id,
            &format!("agent{}", spec.user_id),
            spec.repository,
            spec.ttl,
        )
        .unwrap();
        (token, unix_now() + spec.ttl)
    };
    Agent {
        token,
        owner: owner.id,
        expires_at,
    }
}

fn agent(store: &dyn Store, user_id: i64) -> Agent {
    agent_with(
        store,
        AgentSpec {
            provider: "github",
            host: "github.com",
            user_id,
            repository: REPO,
            repository_id: Some(REPOSITORY_ID),
            read_only: false,
            ttl: TTL,
        },
    )
}

fn another_token(store: &dyn Store, owner: &Agent, agent: &str) -> Agent {
    let (token, _, expires_at) =
        mint_bound_agent_token(store, &owner.owner, agent, REPO, REPOSITORY_ID, TTL, false)
            .unwrap();
    Agent {
        token,
        owner: owner.owner.clone(),
        expires_at,
    }
}

fn start_with(
    request_key: Option<&str>,
    title: &str,
    session: &str,
    work_id: Option<&str>,
    role: WorkRole,
) -> WorkStart {
    WorkStart {
        issue_choice: None,
        work_id: work_id.map(str::to_owned),
        source: None,
        request_key: request_key.map(str::to_owned),
        title: title.into(),
        description: None,
        session: session.into(),
        role,
        scope: vec!["src/**".into()],
        branch: Some("feature/work".into()),
        worktree: Some("worktree-a".into()),
        next_action: Some("continue".into()),
        depends_on: vec![],
        related_work: vec![],
        takeover_generation: None,
        preflight: None,
    }
}

fn native_start(key: &str, title: &str, session: &str) -> WorkStart {
    start_with(Some(key), title, session, None, WorkRole::Lead)
}

fn automatic_start(title: &str, session: &str) -> WorkStart {
    start_with(None, title, session, None, WorkRole::Lead)
}

fn forge_start(issue: &str, session: &str) -> WorkStart {
    let mut start = start_with(None, "forge work", session, None, WorkRole::Lead);
    start.source = Some(ForgeWorkRef {
        provider: "github".into(),
        host: "github.com".into(),
        repository: REPO.into(),
        issue: issue.into(),
    });
    start
}

fn forge_start_for(provider: &str, host: &str, issue: &str, session: &str) -> WorkStart {
    let mut start = forge_start(issue, session);
    let source = start.source.as_mut().unwrap();
    source.provider = provider.into();
    source.host = host.into();
    start
}

fn join(work_id: &str, session: &str, role: WorkRole) -> WorkStart {
    start_with(None, "join", session, Some(work_id), role)
}

fn takeover(work_id: &str, session: &str, generation: Option<i64>) -> WorkStart {
    let mut start = start_with(None, "take over", session, Some(work_id), WorkRole::Lead);
    start.takeover_generation = generation;
    start
}

fn checkpoint(
    work_id: &str,
    operation_id: &str,
    generation: i64,
    session: &str,
    kind: CheckpointKind,
    summary: &str,
) -> WorkCheckpoint {
    WorkCheckpoint {
        work_id: work_id.into(),
        operation_id: operation_id.into(),
        generation,
        session: session.into(),
        expected_updated_seq: None,
        kind,
        summary: summary.into(),
        next_action: Some("next".into()),
        scope: None,
        branch: None,
        worktree: None,
        resolve_blocker: false,
        source_refs: vec![],
        depends_on: vec![],
        related_work: vec![],
    }
}

fn work_event_count(store: &dyn Store, kind: &str) -> usize {
    store
        .events_after(0, 1000)
        .unwrap()
        .iter()
        .filter(|event| event.kind == kind)
        .count()
}

fn assert_invalid<T>(result: Result<T, HubError>, message: &str) {
    match result {
        Err(HubError::Invalid(actual)) => assert_eq!(actual, message),
        Err(HubError::Rejected(rejection)) => assert_eq!(rejection.message(), message),
        _ => panic!("expected invalid {message:?}"),
    }
}

fn concurrent_forge_start_has_one_work_and_one_lead(db: &Db) {
    let setup = db.open();
    let alice = agent(&*setup, 1);
    let bob = agent(&*setup, 2);
    drop(setup);

    let stores = [db.open(), db.open()];
    let barrier = Arc::new(Barrier::new(2));
    let handles = [alice, bob]
        .into_iter()
        .zip(stores)
        .map(|(who, store)| {
            let barrier = barrier.clone();
            thread::spawn(move || {
                barrier.wait();
                store.work_start(
                    &who.token,
                    &forge_start("42", &format!("session-{}", who.owner)),
                    unix_now(),
                )
            })
        })
        .collect::<Vec<_>>();
    let results = handles
        .into_iter()
        .map(|handle| handle.join().unwrap().unwrap())
        .collect::<Vec<_>>();

    assert_eq!(results[0].item.id, results[1].item.id);
    assert_eq!(results.iter().filter(|result| result.created).count(), 1);
    assert_eq!(
        results
            .iter()
            .filter(|result| result.conflict.is_some())
            .count(),
        1
    );
    let item = results.iter().find(|result| result.created).unwrap();
    assert_eq!(
        item.item
            .participants
            .iter()
            .filter(|participant| participant.role == WorkRole::Lead)
            .count(),
        1
    );
    assert_eq!(work_event_count(&*db.open(), "work_started"), 1);
}

fn native_and_automatic_starts_have_explicit_retry_rules(db: &Db) {
    let store = db.open();
    let alice = agent(&*store, 1);
    let now = unix_now();

    let explicit = native_start("request-1", "explicit", "explicit-session");
    let first = store.work_start(&alice.token, &explicit, now).unwrap();
    assert!(first.created);
    let retry = store.work_start(&alice.token, &explicit, now + 1).unwrap();
    assert_eq!(retry.item.id, first.item.id);
    assert!(!retry.created && retry.conflict.is_none());
    let changed = store
        .work_start(
            &alice.token,
            &native_start("request-1", "changed", "explicit-session"),
            now + 2,
        )
        .unwrap();
    assert_eq!(
        changed.conflict.unwrap().kind,
        WorkConflictKind::MetadataChanged
    );

    let mut automatic = automatic_start("automatic", "auto-session");
    automatic.scope = vec!["automatic/**".into()];
    let auto_first = store
        .work_start_auto(&alice.token, &automatic, now + 3)
        .unwrap();
    let auto_retry = store
        .work_start_auto(&alice.token, &automatic, now + 4)
        .unwrap();
    assert_eq!(auto_retry.item.id, auto_first.item.id);
    assert!(!auto_retry.created && auto_retry.conflict.is_none());
    let auto_changed = store
        .work_start_auto(
            &alice.token,
            &{
                let mut changed = automatic_start("different", "auto-session");
                changed.scope = vec!["automatic/**".into()];
                changed
            },
            now + 5,
        )
        .unwrap();
    assert_eq!(
        auto_changed.conflict.unwrap().kind,
        WorkConflictKind::SessionGoalChanged
    );

    let other_session = store
        .work_start_auto(
            &alice.token,
            &{
                let mut other = automatic_start("automatic", "other-session");
                other.scope = vec!["other/**".into()];
                other
            },
            now + 6,
        )
        .unwrap();
    assert!(other_session.created);
    assert_ne!(other_session.item.id, auto_first.item.id);
    assert_eq!(work_event_count(&*store, "work_started"), 3);
}

fn a_description_is_kept_from_creation_and_read_back(db: &Db) {
    let store = db.open();
    let alice = agent(&*store, 1);
    let now = unix_now();
    let text = "Adds a plain description to Work so a teammate can tell what it changes and why.";
    let mut described = native_start("described", "Describe Work", "described-session");
    described.description = Some(text.into());
    let created = store.work_start(&alice.token, &described, now).unwrap();
    assert!(created.created);
    assert_eq!(created.item.description.as_deref(), Some(text));
    let id = created.item.id.clone();

    let read = store.work_get(&alice.token, &id, now + 1).unwrap().unwrap();
    assert_eq!(read.description.as_deref(), Some(text));
    let overview = store
        .work_overview(&alice.token, &WorkOverviewQuery::default(), now + 1)
        .unwrap();
    let listed = overview.current.iter().find(|item| item.id == id).unwrap();
    assert_eq!(listed.description.as_deref(), Some(text));
    let briefing = store
        .work_briefing(&alice.token, &BriefingQuery::default(), now + 1)
        .unwrap();
    assert!(
        briefing
            .events
            .iter()
            .any(|event| event.work_id == id && event.item.description.as_deref() == Some(text))
    );

    // A later start by the lead is not refused for a changed or missing description, and the one
    // from creation stays, so Work made before descriptions existed can still be resumed.
    let retry = store.work_start(&alice.token, &described, now + 2).unwrap();
    assert!(!retry.created && retry.conflict.is_none());
    for changed in [None, Some("Something else.".to_owned())] {
        let mut start = described.clone();
        start.description = changed;
        let result = store.work_start(&alice.token, &start, now + 3).unwrap();
        assert!(result.conflict.is_none());
        assert_eq!(result.item.description.as_deref(), Some(text));
    }

    // A task-only start keeps it the same way.
    let mut automatic = automatic_start("Describe automatic Work", "auto-session");
    automatic.scope = vec!["automatic/**".into()];
    automatic.description = Some(text.into());
    let auto_first = store
        .work_start_auto(&alice.token, &automatic, now + 4)
        .unwrap();
    assert_eq!(auto_first.item.description.as_deref(), Some(text));
    automatic.description = Some("Something else.".into());
    let auto_changed = store
        .work_start_auto(&alice.token, &automatic, now + 5)
        .unwrap();
    assert!(auto_changed.conflict.is_none());
    assert_eq!(auto_changed.item.description.as_deref(), Some(text));

    // Without one, the Work has none and its JSON has no key.
    let mut plain = native_start("plain", "Plain Work", "plain-session");
    plain.scope = vec!["plain/**".into()];
    let plain = store.work_start(&alice.token, &plain, now + 6).unwrap();
    assert!(plain.created && plain.item.description.is_none());
    assert!(
        serde_json::to_value(&plain.item)
            .unwrap()
            .get("description")
            .is_none()
    );

    for refused in ["x".repeat(501), "line\nbreak".into(), String::new()] {
        let mut start = native_start("refused", "Refused Work", "refused-session");
        start.scope = vec!["refused/**".into()];
        start.description = Some(refused);
        assert_invalid(
            store.work_start(&alice.token, &start, now + 7),
            "description must be 1-500 bytes without control characters",
        );
    }
    assert_eq!(work_event_count(&*store, "work_started"), 3);
}

fn authorization_and_claim_projection_keep_old_claims_visible(db: &Db) {
    let store = db.open();
    let lead = agent(&*store, 1);
    let viewer = agent(&*store, 2);
    let read_only = agent_with(
        &*store,
        AgentSpec {
            provider: "github",
            host: "github.com",
            user_id: 3,
            repository: REPO,
            repository_id: Some(REPOSITORY_ID),
            read_only: true,
            ttl: TTL,
        },
    );
    let now = unix_now();
    let started = store
        .work_start(&lead.token, &native_start("claim", "claim", "lead"), now)
        .unwrap();
    let id = started.item.id;

    assert_eq!(
        store
            .work_get(&viewer.token, &id, now)
            .unwrap()
            .unwrap()
            .claim_status,
        ClaimStatus::Active
    );
    assert_eq!(
        store
            .work_get(&viewer.token, &id, now - 1)
            .unwrap()
            .unwrap()
            .claim_status,
        ClaimStatus::Unknown
    );
    let stale_at = now + CLAIM_STALE_SECONDS + 1;
    assert_eq!(
        store
            .work_get(&viewer.token, &id, stale_at)
            .unwrap()
            .unwrap()
            .claim_status,
        ClaimStatus::Stale
    );
    // A stale report alone does not open takeover while the lead's lease is live.
    let contested = store
        .work_start(&viewer.token, &takeover(&id, "viewer", None), stale_at)
        .unwrap();
    assert_eq!(
        contested.conflict.unwrap().kind,
        WorkConflictKind::ClaimHeld
    );
    let lapsed_at = now + LEASE + 1;
    tick(&*store, now, lapsed_at);
    let contested = store
        .work_start(&viewer.token, &takeover(&id, "viewer", None), lapsed_at)
        .unwrap();
    assert_eq!(
        contested.conflict.unwrap().kind,
        WorkConflictKind::StaleTakeoverRequired
    );

    assert!(store.revoke_agent_token(&lead.token).unwrap());
    assert_eq!(
        store
            .work_get(&viewer.token, &id, now + 1)
            .unwrap()
            .unwrap()
            .claim_status,
        ClaimStatus::Stale
    );
    assert!(matches!(
        store.work_checkpoint(
            &lead.token,
            &checkpoint(&id, "revoked", 1, "lead", CheckpointKind::Progress, "x"),
            now + 1,
        ),
        Err(HubError::Unauthorized)
    ));

    let expiring = agent_with(
        &*store,
        AgentSpec {
            provider: "github",
            host: "github.com",
            user_id: 4,
            repository: REPO,
            repository_id: Some(REPOSITORY_ID),
            read_only: false,
            ttl: 1,
        },
    );
    let expiring_now = expiring.expires_at - 1;
    let expired_work = store
        .work_start(
            &expiring.token,
            &native_start("expires", "expires", "expires"),
            expiring_now,
        )
        .unwrap();
    assert!(matches!(
        store.work_get(&expiring.token, &expired_work.item.id, expiring.expires_at),
        Err(HubError::Unauthorized)
    ));
    assert_eq!(
        store
            .work_get(&viewer.token, &expired_work.item.id, expiring.expires_at)
            .unwrap()
            .unwrap()
            .claim_status,
        ClaimStatus::Stale
    );

    let disabled = agent(&*store, 5);
    let disabled_work = store
        .work_start(
            &disabled.token,
            &native_start("disabled", "disabled", "disabled"),
            now,
        )
        .unwrap();
    store.set_user_disabled(&disabled.owner, true).unwrap();
    assert!(matches!(
        store.work_get(&disabled.token, &disabled_work.item.id, now + 1),
        Err(HubError::Unauthorized)
    ));
    assert_eq!(
        store
            .work_get(&viewer.token, &disabled_work.item.id, now + 1)
            .unwrap()
            .unwrap()
            .claim_status,
        ClaimStatus::Stale
    );

    assert!(matches!(
        store.work_start(
            &read_only.token,
            &native_start("readonly", "readonly", "readonly"),
            now,
        ),
        Err(HubError::Unauthorized)
    ));
    assert!(matches!(
        store.work_checkpoint(
            &read_only.token,
            &checkpoint(
                &id,
                "readonly-checkpoint",
                1,
                "readonly",
                CheckpointKind::Progress,
                "readonly",
            ),
            now,
        ),
        Err(HubError::Unauthorized)
    ));
    let briefing = store
        .work_briefing(&read_only.token, &BriefingQuery::default(), now)
        .unwrap();
    assert!(briefing.delivery.is_none());
    assert!(matches!(
        store.work_ack(
            &read_only.token,
            &BriefingAck {
                delivery: format!("dlv_{:032x}", 1),
            },
            now,
        ),
        Err(HubError::Unauthorized)
    ));
}

fn repository_namespaces_fence_work_and_relations(db: &Db) {
    let store = db.open();
    let alice = agent(&*store, 1);
    let now = unix_now();
    let mut target_start = native_start("target", "target", "target");
    target_start.scope = vec!["target/**".into()];
    let target = store
        .work_start(&alice.token, &target_start, now)
        .unwrap()
        .item;
    let mut local_start = native_start("local", "local", "local");
    local_start.scope = vec!["local/**".into()];
    let local = store
        .work_start(&alice.token, &local_start, now)
        .unwrap()
        .item;

    let outsiders = [
        AgentSpec {
            provider: "gitlab",
            host: "gitlab.com",
            user_id: 2,
            repository: REPO,
            repository_id: Some(REPOSITORY_ID),
            read_only: false,
            ttl: TTL,
        },
        AgentSpec {
            provider: "github",
            host: "github.example",
            user_id: 3,
            repository: REPO,
            repository_id: Some(REPOSITORY_ID),
            read_only: false,
            ttl: TTL,
        },
        AgentSpec {
            provider: "github",
            host: "github.com",
            user_id: 4,
            repository: REPO,
            repository_id: Some(REPOSITORY_ID + 1),
            read_only: false,
            ttl: TTL,
        },
        AgentSpec {
            provider: "github",
            host: "github.com",
            user_id: 5,
            repository: REPO,
            repository_id: None,
            read_only: false,
            ttl: TTL,
        },
        AgentSpec {
            provider: "github",
            host: "github.com",
            user_id: 6,
            repository: "acme/other",
            repository_id: Some(REPOSITORY_ID),
            read_only: false,
            ttl: TTL,
        },
    ]
    .map(|spec| agent_with(&*store, spec));
    for (index, outsider) in outsiders.iter().enumerate() {
        assert!(
            store
                .work_get(&outsider.token, &target.id, now)
                .unwrap()
                .is_none()
        );
        let foreign = store
            .work_start(
                &outsider.token,
                &native_start(
                    &format!("foreign-{index}"),
                    "foreign",
                    &format!("foreign-{index}"),
                ),
                now,
            )
            .unwrap()
            .item;
        for (suffix, source_refs, related_work) in [
            (
                "source",
                vec![SourceRef::Work {
                    work_id: foreign.id.clone(),
                }],
                vec![],
            ),
            ("relation", vec![], vec![foreign.id.clone()]),
        ] {
            let mut cross_scope = checkpoint(
                &target.id,
                &format!("cross-{index}-{suffix}"),
                1,
                "target",
                CheckpointKind::Progress,
                "cross scope",
            );
            cross_scope.expected_updated_seq = Some(target.updated_seq);
            cross_scope.source_refs = source_refs;
            cross_scope.related_work = related_work;
            assert_invalid(
                store.work_checkpoint(&alice.token, &cross_scope, now + 1),
                &format!(
                    "related work {} is outside the repository or unknown",
                    foreign.id
                ),
            );
        }
    }

    let mut same_scope = checkpoint(
        &target.id,
        "same-scope",
        1,
        "target",
        CheckpointKind::Progress,
        "same scope",
    );
    same_scope.expected_updated_seq = Some(target.updated_seq);
    same_scope.source_refs = vec![SourceRef::Work {
        work_id: local.id.clone(),
    }];
    same_scope.depends_on = vec![local.id.clone()];
    let updated = store
        .work_checkpoint(&alice.token, &same_scope, now + 2)
        .unwrap();
    assert_eq!(updated.item.depends_on, [local.id]);
    assert_eq!(
        updated.item.current.unwrap().source_refs,
        same_scope.source_refs
    );
}

fn intent_provenance_refs_require_exact_same_work_versions_and_replay(db: &Db) {
    let store = db.open();
    let lead = agent(&*store, 1);
    let now = unix_now();
    let work = store
        .work_start(
            &lead.token,
            &native_start("intent-provenance", "intent provenance", "intent-session"),
            now,
        )
        .unwrap()
        .item;
    let mut foreign_start = native_start("other-intent", "other intent", "other-session");
    foreign_start.scope = vec!["other/**".into()];
    let foreign_start = store.work_start(&lead.token, &foreign_start, now).unwrap();
    assert!(foreign_start.created);
    assert!(foreign_start.conflict.is_none());
    let foreign = foreign_start.item;
    assert_ne!(foreign.id, work.id);

    let declared_event = SourceRef::CapturedEvent {
        session_id: "conversation-session".into(),
        event_id: "message:42".into(),
    };
    let mut observed = checkpoint(
        &work.id,
        "intent-observed",
        1,
        "intent-session",
        CheckpointKind::Progress,
        "captured the declared intent event",
    );
    observed.expected_updated_seq = Some(work.updated_seq);
    observed.source_refs = vec![declared_event.clone()];
    let observed = store
        .work_checkpoint(&lead.token, &observed, now + 1)
        .unwrap();
    assert_eq!(
        observed.item.current.as_ref().unwrap().source_refs,
        [declared_event]
    );

    let superseded = SourceRef::PriorWorkVersion {
        work_id: work.id.clone(),
        event_seq: observed.receipt.event_seq,
    };
    let mut correction = checkpoint(
        &work.id,
        "intent-correction",
        1,
        "intent-session",
        CheckpointKind::Decision,
        "correct the prior intent checkpoint",
    );
    correction.expected_updated_seq = Some(observed.item.updated_seq);
    correction.source_refs = vec![superseded.clone()];
    let corrected = store
        .work_checkpoint(&lead.token, &correction, now + 2)
        .unwrap();
    assert_eq!(
        corrected.item.current.as_ref().unwrap().source_refs,
        [superseded]
    );

    let mut later = checkpoint(
        &work.id,
        "intent-later",
        1,
        "intent-session",
        CheckpointKind::Progress,
        "continued after the correction",
    );
    later.expected_updated_seq = Some(corrected.item.updated_seq);
    let later = store.work_checkpoint(&lead.token, &later, now + 3).unwrap();

    let replay = store
        .work_checkpoint(&lead.token, &correction, now + 4)
        .unwrap();
    assert!(replay.receipt.replayed);
    assert_eq!(replay.receipt.event_seq, corrected.receipt.event_seq);
    assert_eq!(replay.item.updated_seq, later.item.updated_seq);

    let mut foreign_version = checkpoint(
        &work.id,
        "foreign-prior-version",
        1,
        "intent-session",
        CheckpointKind::Decision,
        "invalid cross-work correction",
    );
    foreign_version.expected_updated_seq = Some(later.item.updated_seq);
    foreign_version.source_refs = vec![SourceRef::PriorWorkVersion {
        work_id: foreign.id,
        event_seq: foreign.updated_seq,
    }];
    assert_invalid(
        store.work_checkpoint(&lead.token, &foreign_version, now + 5),
        "prior work version must reference the checkpoint work",
    );

    let mut missing_version = foreign_version;
    missing_version.operation_id = "missing-prior-version".into();
    missing_version.summary = "invalid missing correction target".into();
    missing_version.source_refs = vec![SourceRef::PriorWorkVersion {
        work_id: work.id,
        event_seq: i64::MAX,
    }];
    assert_invalid(
        store.work_checkpoint(&lead.token, &missing_version, now + 6),
        "prior work version is unavailable",
    );
}

fn participants_replays_and_takeovers_are_fenced(db: &Db) {
    let store = db.open();
    let alice = agent(&*store, 1);
    let helper = agent(&*store, 2);
    let reviewer = agent(&*store, 3);
    let successor = agent(&*store, 4);
    let now = unix_now();
    let started = store
        .work_start(&alice.token, &native_start("shared", "shared", "lead"), now)
        .unwrap()
        .item;

    assert!(
        store
            .work_start(
                &helper.token,
                &join(&started.id, "helper", WorkRole::Helper),
                now + 1,
            )
            .unwrap()
            .joined
    );
    assert!(
        store
            .work_start(
                &reviewer.token,
                &join(&started.id, "reviewer", WorkRole::Reviewer),
                now + 1,
            )
            .unwrap()
            .joined
    );
    let joined = store
        .work_get(&alice.token, &started.id, now + 1)
        .unwrap()
        .unwrap();
    for (who, session) in [(&helper, "helper"), (&reviewer, "reviewer")] {
        let mut attempted = checkpoint(
            &started.id,
            &format!("{session}-write"),
            1,
            session,
            CheckpointKind::Progress,
            "not allowed",
        );
        attempted.expected_updated_seq = Some(joined.updated_seq);
        assert_invalid(
            store.work_checkpoint(&who.token, &attempted, now + 2),
            "LEAD_SESSION_MISMATCH",
        );
    }

    let mut operation = checkpoint(
        &started.id,
        "operation-1",
        1,
        "lead",
        CheckpointKind::Progress,
        "made progress",
    );
    operation.expected_updated_seq = Some(joined.updated_seq);
    let first = store
        .work_checkpoint(&alice.token, &operation, now + 3)
        .unwrap();
    let event_count = work_event_count(&*store, "work_checkpoint");
    let replay = store
        .work_checkpoint(&alice.token, &operation, now + 4)
        .unwrap();
    assert_eq!(replay.item.updated_seq, first.item.updated_seq);
    assert!(!first.receipt.replayed);
    assert!(replay.receipt.replayed);
    assert_eq!(replay.receipt.event_seq, first.receipt.event_seq);
    assert_eq!(work_event_count(&*store, "work_checkpoint"), event_count);
    assert_invalid(
        store.work_checkpoint(&helper.token, &operation, now + 4),
        "checkpoint operation belongs to another lead session",
    );
    let mut changed_operation = operation.clone();
    changed_operation.summary = "different payload".into();
    assert_invalid(
        store.work_checkpoint(&alice.token, &changed_operation, now + 5),
        "checkpoint operation_id was already used for another request",
    );

    let held = store
        .work_start(
            &successor.token,
            &takeover(&started.id, "successor", Some(1)),
            now + 6,
        )
        .unwrap();
    assert_eq!(held.conflict.unwrap().kind, WorkConflictKind::ClaimHeld);
    let mut handover = checkpoint(
        &started.id,
        "handover",
        1,
        "lead",
        CheckpointKind::Handover,
        "ready for takeover",
    );
    handover.expected_updated_seq = Some(first.item.updated_seq);
    store
        .work_checkpoint(&alice.token, &handover, now + 7)
        .unwrap();
    let wrong_generation = store
        .work_start(
            &successor.token,
            &takeover(&started.id, "successor", Some(2)),
            now + 8,
        )
        .unwrap();
    assert_eq!(
        wrong_generation.conflict.unwrap().kind,
        WorkConflictKind::GenerationChanged
    );
    let taken = store
        .work_start(
            &successor.token,
            &takeover(&started.id, "successor", Some(1)),
            now + 9,
        )
        .unwrap();
    assert!(taken.took_over);
    assert_eq!(taken.item.generation, 2);
    assert_eq!(taken.item.lead_token, successor.token);

    let historical = store
        .work_checkpoint(&alice.token, &operation, now + 10)
        .unwrap();
    assert_eq!(historical.item.generation, 2);
    assert_eq!(historical.item.lead_token, successor.token);
    assert_eq!(historical.receipt.generation, 1);
    assert_eq!(historical.receipt.event_seq, first.receipt.event_seq);
    assert!(historical.receipt.replayed);
    let mut old_generation = checkpoint(
        &started.id,
        "new-old-generation",
        1,
        "lead",
        CheckpointKind::Progress,
        "late new operation",
    );
    old_generation.expected_updated_seq = Some(taken.item.updated_seq);
    assert_invalid(
        store.work_checkpoint(&alice.token, &old_generation, now + 10),
        "checkpoint generation is fenced; current generation is 2",
    );
    let mut late = checkpoint(
        &started.id,
        "late",
        2,
        "lead",
        CheckpointKind::Progress,
        "late",
    );
    late.expected_updated_seq = Some(taken.item.updated_seq);
    assert_invalid(
        store.work_checkpoint(&alice.token, &late, now + 10),
        &format!(
            "LEAD_SESSION_MISMATCH: recorded lead agent agent4, session successor. Take over with agentscm_start work_id {} takeover_generation 2.",
            started.id
        ),
    );

    let read_only = agent_with(
        &*store,
        AgentSpec {
            provider: "github",
            host: "github.com",
            user_id: 5,
            repository: REPO,
            repository_id: Some(REPOSITORY_ID),
            read_only: true,
            ttl: TTL,
        },
    );
    assert!(matches!(
        store.work_checkpoint(&read_only.token, &operation, now + 11),
        Err(HubError::Unauthorized)
    ));
    let outsider = agent_with(
        &*store,
        AgentSpec {
            provider: "github",
            host: "github.com",
            user_id: 6,
            repository: "acme/other",
            repository_id: Some(REPOSITORY_ID),
            read_only: false,
            ttl: TTL,
        },
    );
    assert_invalid(
        store.work_checkpoint(&outsider.token, &operation, now + 11),
        "unknown work item",
    );
    assert!(store.revoke_agent_token(&alice.token).unwrap());
    assert!(matches!(
        store.work_checkpoint(&alice.token, &operation, now + 11),
        Err(HubError::Unauthorized)
    ));
}

fn blocker_metadata_and_restart_are_durable(db: &Db) {
    let store = db.open();
    let alice = agent(&*store, 1);
    let now = unix_now();
    let started = store
        .work_start(
            &alice.token,
            &native_start("durable", "durable", "lead"),
            now,
        )
        .unwrap()
        .item;

    let mut blocked = checkpoint(
        &started.id,
        "blocked",
        1,
        "lead",
        CheckpointKind::Blocker,
        "waiting for a decision",
    );
    blocked.scope = Some(vec!["docs/**".into(), "src/lib.rs".into()]);
    blocked.expected_updated_seq = Some(started.updated_seq);
    blocked.branch = Some("feature/updated".into());
    blocked.worktree = Some("worktree-b".into());
    let blocked_item = store
        .work_checkpoint(&alice.token, &blocked, now + 1)
        .unwrap()
        .item;
    assert_eq!(blocked_item.state, WorkState::Blocked);
    assert_eq!(blocked_item.scope, ["docs/", "src/lib.rs"]);
    assert_eq!(blocked_item.branch.as_deref(), Some("feature/updated"));
    assert_eq!(blocked_item.worktree.as_deref(), Some("worktree-b"));
    assert_eq!(
        blocked_item.blocker.as_ref().unwrap().summary,
        "waiting for a decision"
    );

    let mut pause = checkpoint(
        &started.id,
        "pause",
        1,
        "lead",
        CheckpointKind::Pause,
        "paused",
    );
    pause.expected_updated_seq = Some(blocked_item.updated_seq);
    let paused = store
        .work_checkpoint(&alice.token, &pause, now + 2)
        .unwrap()
        .item;
    assert_eq!(paused.state, WorkState::Paused);
    assert_eq!(
        paused.blocker.as_ref().unwrap().summary,
        "waiting for a decision"
    );

    let mut resolved = checkpoint(
        &started.id,
        "resolve",
        1,
        "lead",
        CheckpointKind::Progress,
        "decision received",
    );
    resolved.resolve_blocker = true;
    resolved.expected_updated_seq = Some(paused.updated_seq);
    let resolved = store
        .work_checkpoint(&alice.token, &resolved, now + 3)
        .unwrap()
        .item;
    assert_eq!(resolved.state, WorkState::Active);
    assert!(resolved.blocker.is_none());
    let final_seq = resolved.updated_seq;
    drop(store);

    let reopened = db.open();
    let durable = reopened
        .work_get(&alice.token, &started.id, now + 4)
        .unwrap()
        .unwrap();
    assert_eq!(durable.updated_seq, final_seq);
    assert_eq!(durable.state, WorkState::Active);
    assert!(durable.blocker.is_none());
    assert_eq!(durable.scope, ["docs/", "src/lib.rs"]);
    assert_eq!(durable.branch.as_deref(), Some("feature/updated"));
    assert_eq!(durable.worktree.as_deref(), Some("worktree-b"));
    let (used, aggregate) = db.work_usage_and_aggregate();
    assert_eq!(used, aggregate);
}

fn stale_observed_pause_cannot_overwrite_newer_checkpoint(db: &Db) {
    let store = db.open();
    let lead = agent(&*store, 1);
    let now = unix_now();

    for (index, kind) in [
        CheckpointKind::Progress,
        CheckpointKind::Blocker,
        CheckpointKind::Finish,
    ]
    .into_iter()
    .enumerate()
    {
        let session = format!("cas-{index}");
        let mut start = native_start(&format!("cas-{index}"), "CAS work", &session);
        start.scope = vec![format!("src/cas-{index}.rs")];
        let started = store
            .work_start(&lead.token, &start, now + index as i64 * 10)
            .unwrap()
            .item;
        let observed = started.updated_seq;
        let mut fresh = checkpoint(
            &started.id,
            &format!("fresh-{index}"),
            1,
            &session,
            kind,
            &format!("fresh {kind:?}"),
        );
        fresh.scope = Some(vec![format!("src/cas-{index}.rs")]);
        fresh.next_action = Some(format!("fresh next {index}"));
        fresh.expected_updated_seq = Some(observed);
        let fresh = store
            .work_checkpoint(&lead.token, &fresh, now + index as i64 * 10 + 1)
            .unwrap()
            .item;

        let mut stale_pause = checkpoint(
            &started.id,
            &format!("stale-pause-{index}"),
            1,
            &session,
            CheckpointKind::Pause,
            "automatic session-end pause",
        );
        stale_pause.expected_updated_seq = Some(observed);
        let attempt = store.work_checkpoint(&lead.token, &stale_pause, now + index as i64 * 10 + 2);
        if kind == CheckpointKind::Finish {
            // Finished work refuses every new checkpoint before the sequence is compared.
            assert_invalid(
                attempt,
                "finished work accepts no checkpoint; reopen it first",
            );
        } else {
            assert!(matches!(
                attempt,
                Err(HubError::WorkCheckpointSequenceConflict { current_seq }) if current_seq == fresh.updated_seq
            ));
        }

        let retained = store
            .work_get(&lead.token, &started.id, now + index as i64 * 10 + 3)
            .unwrap()
            .unwrap();
        assert_eq!(retained.updated_seq, fresh.updated_seq);
        assert_eq!(retained.state, fresh.state);
        assert_eq!(retained.scope, fresh.scope);
        assert_eq!(retained.next_action, fresh.next_action);
        assert_eq!(retained.blocker, fresh.blocker);
        assert_eq!(retained.current, fresh.current);
    }

    let session = "cas-retry";
    let mut retry_start = native_start("cas-retry", "CAS retry", session);
    retry_start.scope = vec!["src/cas-retry.rs".into()];
    let started = store
        .work_start(&lead.token, &retry_start, now + 40)
        .unwrap()
        .item;
    assert_invalid(
        store.work_checkpoint(
            &lead.token,
            &checkpoint(
                &started.id,
                "missing-cas",
                1,
                session,
                CheckpointKind::Progress,
                "missing CAS",
            ),
            now + 40,
        ),
        "expected_updated_seq is required for a new checkpoint",
    );
    let mut pause = checkpoint(
        &started.id,
        "cas-pause",
        1,
        session,
        CheckpointKind::Pause,
        "pause",
    );
    pause.expected_updated_seq = Some(started.updated_seq);
    let first = store
        .work_checkpoint(&lead.token, &pause, now + 41)
        .unwrap();
    let replay = store
        .work_checkpoint(&lead.token, &pause, now + 42)
        .unwrap();
    assert_eq!(replay.item.updated_seq, first.item.updated_seq);
    assert_eq!(replay.receipt.event_seq, first.receipt.event_seq);
    assert!(!first.receipt.replayed);
    assert!(replay.receipt.replayed);
    assert_eq!(replay.receipt.work_id, started.id);
}

fn forge_issue_ids_are_canonical_positive_decimals(db: &Db) {
    let store = db.open();
    let now = unix_now();
    for (index, (provider, host)) in [("github", "github.com"), ("gitlab", "gitlab.com")]
        .into_iter()
        .enumerate()
    {
        let lead = agent_with(
            &*store,
            AgentSpec {
                provider,
                host,
                user_id: 50 + index as i64,
                repository: REPO,
                repository_id: Some(REPOSITORY_ID),
                read_only: false,
                ttl: TTL,
            },
        );
        let accepted = store
            .work_start(
                &lead.token,
                &forge_start_for(provider, host, "42", &format!("valid-{provider}")),
                now,
            )
            .unwrap();
        assert!(accepted.created);
        for (case, issue) in ["", "0", "01", "-1", "+1", "1.0", "abc"]
            .into_iter()
            .enumerate()
        {
            assert_invalid(
                store.work_start(
                    &lead.token,
                    &forge_start_for(provider, host, issue, &format!("invalid-{provider}-{case}")),
                    now + case as i64 + 1,
                ),
                "issue must be a canonical positive decimal id",
            );
        }
    }
}

fn concurrent_deliveries_and_acks_are_monotonic_and_retryable(db: &Db) {
    let setup = db.open();
    let first = agent(&*setup, 1);
    let second = another_token(&*setup, &first, "agent-second");
    let now = unix_now();
    let item = setup
        .work_start(
            &first.token,
            &native_start("deliveries", "Delivery concurrency", "delivery-session"),
            now,
        )
        .unwrap()
        .item;
    drop(setup);

    let barrier = Arc::new(Barrier::new(2));
    let handles = [first.token.clone(), second.token.clone()]
        .into_iter()
        .zip([db.open(), db.open()])
        .map(|(token, store)| {
            let barrier = barrier.clone();
            thread::spawn(move || {
                barrier.wait();
                store.work_briefing(&token, &BriefingQuery::default(), now + 1)
            })
        })
        .collect::<Vec<_>>();
    let initial = handles
        .into_iter()
        .map(|handle| handle.join().unwrap().unwrap())
        .collect::<Vec<_>>();
    assert!(initial.iter().all(|page| page.complete));
    assert_ne!(initial[0].delivery, initial[1].delivery);
    assert_eq!(initial[0].from, initial[1].from);
    assert_eq!(initial[0].through, initial[1].through);

    let store = db.open();
    let mut progress = checkpoint(
        &item.id,
        "delivery-progress",
        1,
        "delivery-session",
        CheckpointKind::Progress,
        "new event",
    );
    progress.expected_updated_seq = Some(item.updated_seq);
    store
        .work_checkpoint(&first.token, &progress, now + 2)
        .unwrap();
    let newest = store
        .work_briefing(&second.token, &BriefingQuery::default(), now + 3)
        .unwrap();
    assert!(newest.complete);
    assert!(newest.through > initial[0].through);

    let old_delivery = initial[0].delivery.clone().unwrap();
    let newest_delivery = newest.delivery.clone().unwrap();
    let ack_barrier = Arc::new(Barrier::new(2));
    let ack_handles = [
        (first.token.clone(), old_delivery.clone()),
        (second.token.clone(), newest_delivery),
    ]
    .into_iter()
    .zip([db.open(), db.open()])
    .map(|((token, delivery), store)| {
        let barrier = ack_barrier.clone();
        thread::spawn(move || {
            barrier.wait();
            store.work_ack(&token, &BriefingAck { delivery }, now + 4)
        })
    })
    .collect::<Vec<_>>();
    for handle in ack_handles {
        handle.join().unwrap().unwrap();
    }

    let after = store
        .work_briefing(&first.token, &BriefingQuery::default(), now + 5)
        .unwrap();
    assert_eq!(after.from, initial[0].through);
    assert_eq!(after.through, newest.through);
    assert!(after.complete);
    assert_invalid(
        store.work_ack(
            &second.token,
            &BriefingAck {
                delivery: old_delivery,
            },
            now + 6,
        ),
        "delivery is unknown or incomplete",
    );
    let (used, aggregate) = db.work_usage_and_aggregate();
    assert_eq!(used, aggregate);
    store
        .work_ack(
            &first.token,
            &BriefingAck {
                delivery: after.delivery.unwrap(),
            },
            now + 7,
        )
        .unwrap();

    // Acknowledged receipts remain retryable for 24 hours and do not consume the
    // token's eight-delivery bound.
    for offset in 0..9 {
        let page = store
            .work_briefing(
                &first.token,
                &BriefingQuery::default(),
                now + 10 + offset * 2,
            )
            .unwrap();
        assert!(page.complete);
        store
            .work_ack(
                &first.token,
                &BriefingAck {
                    delivery: page.delivery.unwrap(),
                },
                now + 11 + offset * 2,
            )
            .unwrap();
    }
}

fn compaction_preserves_baselines_and_expires_old_receipts(db: &Db) {
    let store = db.open();
    let lead = agent(&*store, 1);
    let reader = agent_with(
        &*store,
        AgentSpec {
            provider: "github",
            host: "github.com",
            user_id: 2,
            repository: REPO,
            repository_id: Some(REPOSITORY_ID),
            read_only: true,
            ttl: TTL,
        },
    );
    let now = unix_now();
    let old = now - 200_000;
    let started = store
        .work_start(
            &lead.token,
            &native_start("compact", "Compact work", "compact-session"),
            old,
        )
        .unwrap()
        .item;
    let mut baseline_request = checkpoint(
        &started.id,
        "compact-baseline",
        1,
        "compact-session",
        CheckpointKind::Progress,
        "baseline visible at the old watermark",
    );
    baseline_request.expected_updated_seq = Some(started.updated_seq);
    let baseline = store
        .work_checkpoint(&lead.token, &baseline_request, old + 1)
        .unwrap();
    let fixed = store
        .work_briefing(
            &reader.token,
            &BriefingQuery {
                limit: 1,
                ..BriefingQuery::default()
            },
            old + 2,
        )
        .unwrap();
    assert!(!fixed.complete);
    assert!(fixed.delivery.is_none());

    let mut recent_request = checkpoint(
        &started.id,
        "compact-recent",
        1,
        "compact-session",
        CheckpointKind::Progress,
        "recent progress",
    );
    recent_request.expected_updated_seq = Some(baseline.item.updated_seq);
    let recent = store
        .work_checkpoint(&lead.token, &recent_request, now - 10)
        .unwrap();

    let outsider = agent_with(
        &*store,
        AgentSpec {
            provider: "github",
            host: "github.com",
            user_id: 3,
            repository: "acme/other",
            repository_id: Some(REPOSITORY_ID + 1),
            read_only: false,
            ttl: TTL,
        },
    );
    let outside = store
        .work_start(
            &outsider.token,
            &native_start("outside-compact", "Outside", "outside-session"),
            old,
        )
        .unwrap()
        .item;
    let mut outside_request = checkpoint(
        &outside.id,
        "outside-progress",
        1,
        "outside-session",
        CheckpointKind::Progress,
        "outside progress",
    );
    outside_request.expected_updated_seq = Some(outside.updated_seq);
    let outside_result = store
        .work_checkpoint(&outsider.token, &outside_request, old + 1)
        .unwrap();

    let scope = WorkRepositoryScope {
        provider: "github".into(),
        host: "github.com".into(),
        repository: REPO.into(),
        repository_id: Some(REPOSITORY_ID),
    };
    let open = store
        .work_briefing(&lead.token, &BriefingQuery::default(), now)
        .unwrap();
    assert!(open.complete);
    // An open delivery no longer blocks compaction; the delivery it cuts is ended below.
    let open_delivery = open.delivery.unwrap();

    let preview = store
        .work_compact(&scope, now - 86_400, false, now)
        .unwrap();
    assert!(!preview.applied);
    assert_eq!(preview.deleted_operations, 1);
    assert_eq!(preview.deleted_versions, 1);
    assert_eq!(preview.retention_floor, baseline.receipt.event_seq);
    assert!(
        store
            .work_checkpoint(&lead.token, &baseline_request, now)
            .unwrap()
            .receipt
            .replayed
    );

    let compacted = store.work_compact(&scope, now - 86_400, true, now).unwrap();
    assert!(compacted.applied);
    assert_eq!(compacted.deleted_operations, 1);
    assert_eq!(compacted.deleted_versions, 1);
    assert_eq!(compacted.retention_floor, baseline.receipt.event_seq);
    // Its window started below the raised floor, so acknowledging it now fails.
    assert_invalid(
        store.work_ack(
            &lead.token,
            &BriefingAck {
                delivery: open_delivery,
            },
            now,
        ),
        "delivery is unknown or incomplete",
    );
    let (used, aggregate) = db.work_usage_and_aggregate();
    assert_eq!(used, aggregate);

    let continued = store
        .work_briefing(
            &reader.token,
            &BriefingQuery {
                delivery: None,
                subscriber: None,
                after: Some(fixed.next_after),
                work_after: fixed.next_work_after,
                work_done: fixed.next_work_done,
                through: Some(fixed.through),
                limit: 1,
            },
            now,
        )
        .unwrap();
    assert!(continued.events.iter().any(|event| {
        event
            .item
            .current
            .as_ref()
            .is_some_and(|checkpoint| checkpoint.summary == "baseline visible at the old watermark")
    }));
    // A compacted receipt must remain ambiguous, even when its baseline
    // snapshot survives. A caller must never mint a fresh retry from this error.
    assert_invalid(
        store.work_checkpoint(&lead.token, &baseline_request, now),
        &format!(
            "checkpoint receipt expired or work changed; current updated_seq is {}",
            recent.item.updated_seq
        ),
    );
    let mut definitely_uncommitted = recent_request.clone();
    definitely_uncommitted.operation_id = "post-compaction-stale-uncommitted".into();
    // expected=the retained baseline: every later receipt remains available.
    assert!(matches!(
        store.work_checkpoint(&lead.token, &definitely_uncommitted, now),
        Err(HubError::WorkCheckpointSequenceConflict { current_seq })
            if current_seq == recent.item.updated_seq
    ));
    assert!(
        store
            .work_checkpoint(&lead.token, &recent_request, now)
            .unwrap()
            .receipt
            .replayed
    );
    assert!(
        store
            .work_checkpoint(&outsider.token, &outside_request, now)
            .unwrap()
            .receipt
            .replayed
    );
    assert_eq!(
        store
            .work_get(&lead.token, &started.id, now)
            .unwrap()
            .unwrap()
            .updated_seq,
        recent.item.updated_seq
    );
    assert!(
        !store
            .work_briefing(
                &reader.token,
                &BriefingQuery {
                    after: Some(started.updated_seq),
                    through: Some(recent.item.updated_seq),
                    ..BriefingQuery::default()
                },
                now,
            )
            .unwrap()
            .retention_gap
    );
    assert!(
        store
            .work_briefing(&reader.token, &BriefingQuery::default(), now)
            .unwrap()
            .retention_gap
    );
    assert_invalid(
        store.work_briefing(
            &reader.token,
            &BriefingQuery {
                after: Some(0),
                through: Some(compacted.retention_floor - 1),
                limit: 1,
                ..BriefingQuery::default()
            },
            now,
        ),
        "briefing watermark is older than retained work history",
    );
    assert_eq!(outside_result.receipt.work_id, outside.id);
}

fn compaction_delete_exception_is_exact_and_updates_remain_prohibited(db: &Db) {
    drop(db.open());
    db.assert_event_delete_boundary();
}

fn postgres_read_briefings_share_the_repository_maintenance_lock(db: &Db) {
    if db._pg.is_none() {
        return;
    }
    let store = db.open();
    let reader = agent_with(
        &*store,
        AgentSpec {
            provider: "github",
            host: "github.com",
            user_id: 1,
            repository: REPO,
            repository_id: Some(REPOSITORY_ID),
            read_only: true,
            ttl: TTL,
        },
    );
    let mut lock_client = postgres::Client::connect(&db.url, NoTls).unwrap();
    let scope = serde_json::to_string(&(
        "github",
        "github.com",
        format!("@repo-id:{REPOSITORY_ID}"),
        Some(REPOSITORY_ID),
    ))
    .unwrap();
    for preview in [false, true] {
        lock_client
            .execute(
                "SELECT pg_advisory_lock($1, hashtext($2))",
                &[&0x6167_6579_i32, &scope],
            )
            .unwrap();
        let barrier = Arc::new(Barrier::new(2));
        let child_barrier = Arc::clone(&barrier);
        let child_store = Arc::clone(&store);
        let token = reader.token.clone();
        let (sent, received) = mpsc::channel();
        let handle = thread::spawn(move || {
            child_barrier.wait();
            let result = if preview {
                child_store.work_briefing_preview_for(&token, None, 1, unix_now())
            } else {
                child_store.work_briefing(&token, &BriefingQuery::default(), unix_now())
            };
            sent.send(result).unwrap();
        });
        barrier.wait();
        assert!(received.recv_timeout(Duration::from_millis(250)).is_err());
        assert!(
            lock_client
                .query_one(
                    "SELECT pg_advisory_unlock($1, hashtext($2))",
                    &[&0x6167_6579_i32, &scope],
                )
                .unwrap()
                .get::<_, bool>(0)
        );
        received
            .recv_timeout(Duration::from_secs(5))
            .unwrap()
            .unwrap();
        handle.join().unwrap();
    }
}

fn concurrent_delivery_reservations_enforce_capacity_and_roll_back(db: &Db) {
    const HEADROOM_AFTER_FIRST: i64 = 1024 * 1024;
    let store = db.open();
    let first = agent(&*store, 1);
    let second = agent(&*store, 2);
    db.set_work_usage(
        WORK_PAYLOAD_CAPACITY_BYTES - WORK_DELIVERY_RESERVATION_BYTES - HEADROOM_AFTER_FIRST,
    );

    let barrier = Arc::new(Barrier::new(3));
    let handles = [first.token.clone(), second.token.clone()]
        .into_iter()
        .zip([db.open(), db.open()])
        .map(|(token, store)| {
            let barrier = Arc::clone(&barrier);
            thread::spawn(move || {
                barrier.wait();
                let result = store.work_briefing(&token, &BriefingQuery::default(), unix_now());
                (token, result)
            })
        })
        .collect::<Vec<_>>();
    barrier.wait();
    let results = handles
        .into_iter()
        .map(|handle| handle.join().unwrap())
        .collect::<Vec<_>>();
    let mut winner = None;
    let mut loser = None;
    for (token, result) in results {
        match result {
            Ok(page) => winner = Some((token, page)),
            Err(HubError::WorkCapacity(c)) if c.limit == CapacityLimit::WorkStorage => {
                loser = Some(token);
            }
            other => panic!("unexpected capacity result: {other:?}"),
        }
    }
    let (winner_token, winner_page) = winner.expect("one delivery must win the reservation");
    let loser_token = loser.expect("one delivery must lose the reservation");
    assert_eq!(db.delivery_count(), 1);
    assert_eq!(
        db.work_usage_and_aggregate().0,
        WORK_PAYLOAD_CAPACITY_BYTES - HEADROOM_AFTER_FIRST
    );

    store
        .work_ack(
            &winner_token,
            &BriefingAck {
                delivery: winner_page.delivery.unwrap(),
            },
            unix_now(),
        )
        .unwrap();
    let retried = store
        .work_briefing(&loser_token, &BriefingQuery::default(), unix_now())
        .unwrap();
    assert!(retried.complete);
    assert_eq!(db.delivery_count(), 2);
}

fn briefing_sessions_on_one_token_have_independent_cursors(db: &Db) {
    let store = db.open();
    let lead = agent(&*store, 1);
    let now = unix_now();
    let item = store
        .work_start(
            &lead.token,
            &native_start("session-cursors", "Session cursors", "lead-session"),
            now,
        )
        .unwrap()
        .item;
    let query = |subscriber: &str| BriefingQuery {
        subscriber: Some(subscriber.into()),
        ..BriefingQuery::default()
    };
    let first_a = store
        .work_briefing(&lead.token, &query("subscriber-a"), now + 1)
        .unwrap();
    let first_b = store
        .work_briefing(&lead.token, &query("subscriber-b"), now + 1)
        .unwrap();
    assert_eq!(first_a.subscriber.as_deref(), Some("subscriber-a"));
    assert_eq!(first_b.subscriber.as_deref(), Some("subscriber-b"));
    assert_eq!(first_a.from, 0);
    assert_eq!(first_b.from, 0);
    assert_eq!(first_a.through, first_b.through);
    store
        .work_ack(
            &lead.token,
            &BriefingAck {
                delivery: first_a.delivery.clone().unwrap(),
            },
            now + 2,
        )
        .unwrap();

    let mut progress = checkpoint(
        &item.id,
        "session-cursor-progress",
        1,
        "lead-session",
        CheckpointKind::Progress,
        "session cursor event",
    );
    progress.expected_updated_seq = Some(item.updated_seq);
    store
        .work_checkpoint(&lead.token, &progress, now + 3)
        .unwrap();

    let next_a = store
        .work_briefing(&lead.token, &query("subscriber-a"), now + 4)
        .unwrap();
    let next_b = store
        .work_briefing(&lead.token, &query("subscriber-b"), now + 4)
        .unwrap();
    assert_eq!(next_a.from, first_a.through);
    assert_eq!(next_b.from, 0);
    assert_eq!(next_a.through, next_b.through);

    let mut wrong_subscriber = query("subscriber-b");
    wrong_subscriber.delivery = next_a.delivery;
    wrong_subscriber.after = Some(next_a.after);
    wrong_subscriber.through = Some(next_a.through);
    assert_invalid(
        store.work_briefing(&lead.token, &wrong_subscriber, now + 5),
        "unknown briefing delivery",
    );
}

fn lead_credential_events(store: &dyn Store, work_id: &str) -> Vec<serde_json::Value> {
    store
        .events_after(0, 1000)
        .unwrap()
        .into_iter()
        .filter(|event| event.kind == "work_lead_credential" && event.subject == work_id)
        .map(|event| {
            assert!(!event.data.contains("tok_"), "{data}", data = event.data);
            serde_json::from_str(&event.data).unwrap()
        })
        .collect()
}

fn lead_credential_carries_for_the_same_owner_and_session(db: &Db) {
    let store = db.open();
    let alice = agent(&*store, 1);
    let replacement = another_token(&*store, &alice, "replacement");
    let outsider = agent(&*store, 2);
    let now = unix_now();
    let started = store
        .work_start(
            &alice.token,
            &native_start("lead-credential", "lead credential", "live"),
            now,
        )
        .unwrap()
        .item;
    assert_eq!(started.generation, 1);
    assert_eq!(started.lead_agent, "agent1");

    let mut fenced = checkpoint(
        &started.id,
        "fenced-generation",
        2,
        "live",
        CheckpointKind::Progress,
        "too new",
    );
    fenced.expected_updated_seq = Some(started.updated_seq);
    assert_invalid(
        store.work_checkpoint(&replacement.token, &fenced, now + 1),
        "checkpoint generation is fenced; current generation is 1",
    );
    let unchanged = store
        .work_get(&alice.token, &started.id, now + 1)
        .unwrap()
        .unwrap();
    assert_eq!(unchanged.updated_seq, started.updated_seq);
    assert_eq!(unchanged.lead_token, alice.token);
    assert!(lead_credential_events(&*store, &started.id).is_empty());

    let mut carried = checkpoint(
        &started.id,
        "carry-credential",
        1,
        "live",
        CheckpointKind::Progress,
        "same session, new token",
    );
    carried.expected_updated_seq = Some(started.updated_seq);
    let result = store
        .work_checkpoint(&replacement.token, &carried, now + 2)
        .unwrap();
    assert_eq!(result.item.generation, 1);
    assert_eq!(result.item.lead_token, replacement.token);
    assert_eq!(result.item.lead_agent, "replacement");
    assert_eq!(result.item.lead_session, "live");
    let lead = result
        .item
        .participants
        .iter()
        .find(|participant| participant.role == WorkRole::Lead)
        .unwrap();
    assert_eq!(lead.token, replacement.token);
    assert_eq!(lead.agent, "replacement");
    assert_eq!(lead.session, "live");
    let events = lead_credential_events(&*store, &started.id);
    assert_eq!(events.len(), 1);
    assert_eq!(events[0]["session"], "live");
    assert_eq!(events[0]["previous_agent"], "agent1");
    assert_eq!(events[0]["agent"], "replacement");
    assert_eq!(events[0]["generation"], 1);

    let mut foreign = checkpoint(
        &started.id,
        "other-owner",
        1,
        "live",
        CheckpointKind::Progress,
        "different owner",
    );
    foreign.expected_updated_seq = Some(result.item.updated_seq);
    assert_invalid(
        store.work_checkpoint(&outsider.token, &foreign, now + 3),
        "LEAD_SESSION_MISMATCH",
    );
    let error = store
        .work_checkpoint(&outsider.token, &foreign, now + 3)
        .unwrap_err();
    let envelope = agentscm::hub::error_envelope::from_hub_error(&error);
    assert_eq!(envelope.code, "lead_session_mismatch");
    assert_eq!(envelope.message, "LEAD_SESSION_MISMATCH");
    assert!(!envelope.render().contains(&started.id));
    assert!(!envelope.render().contains("replacement"));
    let after_foreign = store
        .work_get(&alice.token, &started.id, now + 3)
        .unwrap()
        .unwrap();
    assert_eq!(after_foreign.updated_seq, result.item.updated_seq);
    assert_eq!(lead_credential_events(&*store, &started.id).len(), 1);

    let mut other_session = checkpoint(
        &started.id,
        "other-session",
        1,
        "other",
        CheckpointKind::Progress,
        "same owner, different session",
    );
    other_session.expected_updated_seq = Some(result.item.updated_seq);
    let refusal = format!(
        "LEAD_SESSION_MISMATCH: recorded lead agent replacement, session live. Take over with agentscm_start work_id {} takeover_generation 1.",
        started.id
    );
    assert!(!refusal.contains("tok_"));
    assert_invalid(
        store.work_checkpoint(&alice.token, &other_session, now + 4),
        &refusal,
    );
    let after_other = store
        .work_get(&alice.token, &started.id, now + 4)
        .unwrap()
        .unwrap();
    assert_eq!(after_other.updated_seq, result.item.updated_seq);

    let mut touch_start = native_start("touch-credential", "touch credential", "touch");
    touch_start.scope = vec!["docs/**".into()];
    let touch_result = store
        .work_start(&alice.token, &touch_start, now + 5)
        .unwrap();
    assert!(touch_result.created);
    let touch_work = touch_result.item;
    db.sql(&format!(
        "INSERT INTO hub_sessions (token, session, via, created_at, updated_at) VALUES ('{token}', 'touch', '[\"hook:claude\"]', {at}, {at})",
        token = replacement.token,
        at = now + 5,
    ));
    let omitted = store
        .touch_session(
            &replacement.token,
            &agentscm::hub::session::SessionCall {
                session: None,
                via: "mcp".into(),
                task: None,
                paths: vec![],
                deny: false,
                report: true,
            },
            now + 6,
        )
        .unwrap();
    assert!(omitted.lead_notes.is_empty());
    let still = store
        .work_get(&alice.token, &touch_work.id, now + 6)
        .unwrap()
        .unwrap();
    assert_eq!(still.lead_token, alice.token);
    assert_eq!(still.generation, 1);
    assert!(lead_credential_events(&*store, &touch_work.id).is_empty());

    let named = store
        .touch_session(
            &replacement.token,
            &agentscm::hub::session::SessionCall {
                session: Some("touch".into()),
                via: "mcp".into(),
                task: None,
                paths: vec![],
                deny: false,
                report: true,
            },
            now + 7,
        )
        .unwrap();
    assert!(
        named
            .lead_notes
            .iter()
            .any(|note| note.contains(&touch_work.id) && note.contains("agent1"))
    );
    assert!(named.lead_notes.iter().all(|note| !note.contains("tok_")));
    let healed = store
        .work_get(&replacement.token, &touch_work.id, now + 7)
        .unwrap()
        .unwrap();
    assert_eq!(healed.lead_token, replacement.token);
    assert_eq!(healed.lead_agent, "replacement");
    assert_eq!(healed.lead_session, "touch");
    assert_eq!(healed.generation, 1);
    assert!(healed.updated_seq > still.updated_seq);
    assert_eq!(lead_credential_events(&*store, &touch_work.id).len(), 1);

    let mut follow = checkpoint(
        &touch_work.id,
        "after-touch",
        1,
        "touch",
        CheckpointKind::Progress,
        "credential already carried",
    );
    follow.expected_updated_seq = Some(healed.updated_seq);
    store
        .work_checkpoint(&replacement.token, &follow, now + 8)
        .unwrap();
    assert_eq!(lead_credential_events(&*store, &touch_work.id).len(), 1);
}

macro_rules! conformance {
    ($($case:ident),* $(,)?) => {
        mod on_sqlite {
            $(#[test] fn $case() { super::$case(&super::Db::sqlite()); })*
        }
        mod on_postgres {
            $(#[test] fn $case() {
                if let Some(db) = super::Db::postgres() { super::$case(&db); }
            })*
        }
    };
}

conformance!(
    pr_links_are_explicit_durable_and_snapshot_compared,
    pr_links_require_trusted_repository_and_current_lead,
    pr_links_fence_stale_updates_and_idempotency_collisions,
    pr_link_receipts_replay_after_takeover_changes_and_removal,
    pr_links_survive_compaction_with_correct_capacity,
    issue_links_survive_compaction_with_correct_capacity,
    pr_link_noops_leave_no_mutation_or_receipt,
    concurrent_pr_link_updates_fence_and_replay,
    concurrent_forge_start_has_one_work_and_one_lead,
    native_and_automatic_starts_have_explicit_retry_rules,
    a_description_is_kept_from_creation_and_read_back,
    authorization_and_claim_projection_keep_old_claims_visible,
    repository_namespaces_fence_work_and_relations,
    intent_provenance_refs_require_exact_same_work_versions_and_replay,
    participants_replays_and_takeovers_are_fenced,
    lead_credential_carries_for_the_same_owner_and_session,
    blocker_metadata_and_restart_are_durable,
    stale_observed_pause_cannot_overwrite_newer_checkpoint,
    forge_issue_ids_are_canonical_positive_decimals,
    concurrent_deliveries_and_acks_are_monotonic_and_retryable,
    compaction_preserves_baselines_and_expires_old_receipts,
    checkpoint_rejection_respects_removed_history_without_a_checkpoint_baseline,
    compaction_delete_exception_is_exact_and_updates_remain_prohibited,
    postgres_read_briefings_share_the_repository_maintenance_lock,
    concurrent_delivery_reservations_enforce_capacity_and_roll_back,
    briefing_sessions_on_one_token_have_independent_cursors,
    reservation_migration_backfills_unfinished_work_and_leaves_finished_unstated,
    finish_frees_scope_for_a_new_start_and_pause_keeps_blocking,
    finished_work_refuses_checkpoints_but_replays_the_finish_receipt,
    stale_finished_work_cannot_be_taken_over,
    announce_intent_refuses_fenced_sessions_but_not_finished_or_unlinked_ones,
    helper_join_on_finished_work_acquires_nothing,
    unreserved_work_needs_fresh_admission_to_be_taken_over,
    transition_matrix_covers_every_actor_and_kind,
    transition_state_rules_refuse_without_mutation,
    transition_refuses_foreign_read_only_and_unrelated_callers,
    owner_and_admin_act_after_the_lead_token_is_revoked,
    release_alone_leaves_no_outcome_and_frees_the_scope,
    old_session_after_release_is_fenced_and_not_reacquired,
    concurrent_release_checkpoint_and_reopen_have_one_winner,
    transition_retries_replay_after_later_transitions,
    reopen_conflicts_on_overlap_and_fences_the_old_session,
    responded_request_stays_responded_after_release,
    release_succeeds_at_payload_quota_and_retain_refuses,
    compaction_runs_after_transition_events_exist,
    abandoned_work_is_not_a_reported_completion,
    plain_announce_follows_the_token_sessions_after_release_abandon_and_takeover,
    plain_announce_allows_paths_covered_by_eligible_work,
    plain_announce_refuses_a_directory_that_overlaps_fenced_work_either_way,
    plain_announce_fails_closed_beyond_the_token_work_bound,
    cleanup_succeeds_for_a_token_with_more_work_than_the_announce_bound,
    a_helper_with_many_work_cannot_block_the_owners_release,
    plain_announce_ignores_empty_scope_work_in_its_bound,
    release_frees_session_and_sessionless_intents_of_the_scope,
    finish_checkpoint_cleans_up_like_a_terminal_transition,
    ending_work_spares_a_tokens_live_intents_elsewhere,
    finish_at_payload_quota_succeeds_in_its_cleanup,
    an_exact_finish_replay_repeats_no_release,
    ending_work_spares_a_session_that_is_eligible_on_other_work,
    ending_work_that_held_nothing_leaves_sessionless_intents,
    released_work_resumes_in_any_state_and_refuses_reports_until_then,
    resuming_released_work_conflicts_on_overlap_without_mutation,
    reopen_by_another_session_retargets_open_requests,
    resuming_released_work_retargets_requests_only_when_the_lead_changes,
    pause_and_handover_keep_a_retained_reservation,
    requests_that_propose_ended_work_are_marked_released_too,
    read_only_transition_on_an_unknown_id_is_unauthorized_in_both_stores,
    a_paused_item_frees_its_session_for_another_goal,
    an_idle_lease_lapses_and_the_next_admission_clears_it,
    lead_activity_renews_the_lease_without_reviving_it,
    every_basis_lapses_and_retain_is_capped,
    a_gap_in_the_clock_grants_one_lease_of_grace,
    lease_migration_starts_existing_reservations_now,
);

fn pr_link_update(item: &agentscm::hub::work::WorkItem, operation_id: &str) -> WorkPrLinkUpdate {
    WorkPrLinkUpdate {
        work_id: item.id.clone(),
        operation_id: operation_id.into(),
        generation: item.generation,
        session: item.lead_session.clone(),
        expected_updated_seq: item.updated_seq,
        link: Some(WorkPrLinkTarget {
            repository_id: REPOSITORY_ID,
            number: 17,
            head_sha: "a".repeat(40),
        }),
    }
}

fn pr_links_are_explicit_durable_and_snapshot_compared(db: &Db) {
    let store = db.open();
    let now = unix_now();
    let lead = agent(&*store, 1);
    let item = store
        .work_start(
            &lead.token,
            &native_start("pr-link", "Explicit PR", "lead"),
            now,
        )
        .unwrap()
        .item;
    assert!(
        store
            .work_pr_link_get(&lead.token, &item.id, &WorkPrLinkQuery::default(), now)
            .unwrap()
            .unwrap()
            .link
            .is_none()
    );
    let update = pr_link_update(&item, "link-first");
    let result = store
        .work_pr_link_set(&lead.token, &update, now + 1)
        .unwrap();
    let retained = result.view.link.as_ref().unwrap();
    assert_eq!(retained.author, lead.owner);
    assert_eq!(retained.token, lead.token);
    assert_eq!(retained.linked_at, now + 1);
    assert_eq!(retained.event_seq, result.receipt.event_seq);
    assert_eq!(retained.target, update.link.clone().unwrap());
    let events = store.events_after(item.updated_seq, 20).unwrap();
    let event = events
        .iter()
        .find(|event| event.kind == "work_pr_link")
        .unwrap();
    let data: serde_json::Value = serde_json::from_str(&event.data).unwrap();
    assert_eq!(data["operation"], "pr_link_set");
    assert_eq!(data["relation"]["head_sha"], "a".repeat(40));
    let preview_query = CatchupPreviewQuery {
        subscriber: "human".into(),
    };
    let before = store
        .work_catchup_preview(&lead.token, &preview_query, now + 2)
        .unwrap();
    let page = store
        .work_catchup(&lead.token, &CatchupQuery::default(), now + 2)
        .unwrap();
    assert!(
        page.work_events
            .iter()
            .any(|event| event.kind == "work_pr_link" && event.seq == result.receipt.event_seq)
    );
    let query = WorkPrLinkQuery {
        expected_updated_seq: Some(item.updated_seq),
        expected_head: Some("b".repeat(40)),
    };
    let current = store
        .work_pr_link_get(&lead.token, &item.id, &query, now + 2)
        .unwrap()
        .unwrap();
    assert_eq!(current.snapshot_status, WorkPrSnapshotStatus::WorkChanged);
    assert_eq!(current.head_status, WorkPrHeadStatus::DiffersFromLink);
    let after = store
        .work_catchup_preview(&lead.token, &preview_query, now + 2)
        .unwrap();
    assert_eq!(before.event_ack, after.event_ack);
    assert_eq!(after.event_ack, 0);
    let exact = WorkPrLinkQuery {
        expected_updated_seq: Some(result.view.updated_seq),
        expected_head: Some("a".repeat(40)),
    };
    let reopened = db
        .open()
        .work_pr_link_get(&lead.token, &item.id, &exact, now + 3)
        .unwrap()
        .unwrap();
    assert_eq!(reopened.snapshot_status, WorkPrSnapshotStatus::MatchesWork);
    assert_eq!(reopened.head_status, WorkPrHeadStatus::MatchesLink);
    assert_eq!(reopened.link, result.view.link);
}

fn pr_links_require_trusted_repository_and_current_lead(db: &Db) {
    let store = db.open();
    let now = unix_now();
    let lead = agent(&*store, 1);
    let item = store
        .work_start(
            &lead.token,
            &native_start("pr-auth", "PR authorization", "lead"),
            now,
        )
        .unwrap()
        .item;
    let update = pr_link_update(&item, "link-auth");
    let reader = agent_with(
        &*store,
        AgentSpec {
            provider: "github",
            host: "github.com",
            user_id: 2,
            repository: REPO,
            repository_id: Some(REPOSITORY_ID),
            read_only: true,
            ttl: TTL,
        },
    );
    assert!(
        store
            .work_pr_link_get(&reader.token, &item.id, &WorkPrLinkQuery::default(), now)
            .unwrap()
            .is_some()
    );
    assert!(matches!(
        store.work_pr_link_set(&reader.token, &update, now),
        Err(HubError::Unauthorized)
    ));
    let other = agent(&*store, 3);
    assert_invalid(
        store.work_pr_link_set(&other.token, &update, now),
        "only the current Work owner and lead session may change a PR link",
    );
    let wrong = agent_with(
        &*store,
        AgentSpec {
            provider: "github",
            host: "github.com",
            user_id: 4,
            repository: REPO,
            repository_id: Some(REPOSITORY_ID + 1),
            read_only: false,
            ttl: TTL,
        },
    );
    assert!(
        store
            .work_pr_link_get(&wrong.token, &item.id, &WorkPrLinkQuery::default(), now)
            .unwrap()
            .is_none()
    );
    assert_invalid(
        store.work_pr_link_set(&wrong.token, &update, now),
        "unknown work item",
    );
    let mut wrong_target = update.clone();
    wrong_target.link.as_mut().unwrap().repository_id += 1;
    assert_invalid(
        store.work_pr_link_set(&lead.token, &wrong_target, now),
        "PR link repository_id is outside the authorized Work repository",
    );
    let unbound = agent_with(
        &*store,
        AgentSpec {
            provider: "github",
            host: "github.com",
            user_id: 5,
            repository: REPO,
            repository_id: None,
            read_only: false,
            ttl: TTL,
        },
    );
    let mut unbound_start = native_start("unbound", "Missing identity", "unbound");
    unbound_start.scope.clear();
    let unbound_item = store
        .work_start(&unbound.token, &unbound_start, now)
        .unwrap()
        .item;
    assert_invalid(
        store.work_pr_link_get(
            &unbound.token,
            &unbound_item.id,
            &WorkPrLinkQuery::default(),
            now,
        ),
        "PR relationships require a trusted stable repository ID",
    );
    assert_invalid(
        store.work_pr_link_set(
            &unbound.token,
            &pr_link_update(&unbound_item, "unbound-link"),
            now,
        ),
        "PR relationships require a trusted stable repository ID",
    );
    let first = store
        .work_pr_link_set(&lead.token, &update, now + 1)
        .unwrap();
    assert!(
        store
            .work_pr_link_get(
                &reader.token,
                &item.id,
                &WorkPrLinkQuery::default(),
                now + 1
            )
            .unwrap()
            .unwrap()
            .link
            .is_some()
    );
    let successor = agent(&*store, 6);
    tick(&*store, now, now + LEASE + 2);
    let taken = store
        .work_start(
            &successor.token,
            &takeover(&item.id, "successor", Some(1)),
            now + LEASE + 2,
        )
        .unwrap();
    assert!(taken.took_over);
    let after_takeover = store
        .work_pr_link_get(
            &successor.token,
            &item.id,
            &WorkPrLinkQuery::default(),
            now + LEASE + 3,
        )
        .unwrap()
        .unwrap();
    assert_eq!(after_takeover.generation, 2);
    assert_eq!(after_takeover.link, first.view.link);
    let original_link = after_takeover.link.as_ref().unwrap();
    assert_eq!(original_link.author, lead.owner);
    assert_eq!(original_link.token, lead.token);
    assert_eq!(original_link.session, "lead");
    assert_eq!(original_link.generation, 1);
    assert_eq!(original_link.linked_at, now + 1);
    assert_eq!(original_link.event_seq, first.receipt.event_seq);
    let replayed = store
        .work_pr_link_set(&lead.token, &update, now + LEASE + 3)
        .unwrap();
    assert!(replayed.receipt.replayed);
    assert_eq!(replayed.receipt.generation, 1);
    assert_eq!(replayed.receipt.event_seq, first.receipt.event_seq);
    assert_eq!(replayed.view, after_takeover);
    let mut old_lead_new_operation = update.clone();
    old_lead_new_operation.operation_id = "old-lead-new-operation".into();
    assert_invalid(
        store.work_pr_link_set(&lead.token, &old_lead_new_operation, now + LEASE + 3),
        "PR link generation is fenced",
    );
    let mut removal = pr_link_update(&taken.item, "successor-remove");
    removal.link = None;
    let removed = store
        .work_pr_link_set(&successor.token, &removal, now + LEASE + 3)
        .unwrap();
    assert!(removed.view.link.is_none());
    assert!(removed.view.updated_seq > first.view.updated_seq);
}

fn pr_links_fence_stale_updates_and_idempotency_collisions(db: &Db) {
    let store = db.open();
    let now = unix_now();
    let lead = agent(&*store, 1);
    let item = store
        .work_start(
            &lead.token,
            &native_start("pr-fence", "PR fencing", "lead"),
            now,
        )
        .unwrap()
        .item;
    let update = pr_link_update(&item, "link-fence");
    let first = store
        .work_pr_link_set(&lead.token, &update, now + 1)
        .unwrap();
    let replay = store
        .work_pr_link_set(&lead.token, &update, now + 2)
        .unwrap();
    assert!(replay.receipt.replayed);
    assert_eq!(replay.receipt.event_seq, first.receipt.event_seq);
    assert_eq!(work_event_count(&*store, "work_pr_link"), 1);
    let mut conflict = update.clone();
    conflict.link.as_mut().unwrap().number += 1;
    assert_invalid(
        store.work_pr_link_set(&lead.token, &conflict, now + 2),
        "operation_id was already used for another request",
    );
    conflict.operation_id = "stale".into();
    assert_invalid(
        store.work_pr_link_set(&lead.token, &conflict, now + 2),
        "PR link receipt expired or Work changed",
    );
    conflict.expected_updated_seq = first.view.updated_seq;
    conflict.generation += 1;
    assert_invalid(
        store.work_pr_link_set(&lead.token, &conflict, now + 2),
        "PR link generation is fenced",
    );
    let mut cp = checkpoint(
        &item.id,
        "link-fence",
        item.generation,
        "lead",
        CheckpointKind::Progress,
        "progress",
    );
    cp.expected_updated_seq = Some(first.view.updated_seq);
    assert_invalid(
        store.work_checkpoint(&lead.token, &cp, now + 2),
        "checkpoint operation_id was already used for another request",
    );
    cp.operation_id = "checkpoint-key".into();
    let progress = store.work_checkpoint(&lead.token, &cp, now + 2).unwrap();
    let mut second = pr_link_update(&progress.item, "checkpoint-key");
    assert_invalid(
        store.work_pr_link_set(&lead.token, &second, now + 3),
        "operation_id was already used for another request",
    );
    second.operation_id = "new-head".into();
    second.link.as_mut().unwrap().head_sha = "b".repeat(40);
    let updated = store
        .work_pr_link_set(&lead.token, &second, now + 3)
        .unwrap();
    assert_eq!(
        updated.view.link.as_ref().unwrap().target.head_sha,
        "b".repeat(40)
    );
    let old_replay = store
        .work_pr_link_set(&lead.token, &update, now + 4)
        .unwrap();
    assert_eq!(old_replay.view.link, updated.view.link);
    assert_eq!(old_replay.receipt.event_seq, first.receipt.event_seq);
    let mut removal = second.clone();
    removal.operation_id = "remove".into();
    removal.expected_updated_seq = updated.view.updated_seq;
    removal.link = None;
    let removed = store
        .work_pr_link_set(&lead.token, &removal, now + 4)
        .unwrap();
    assert!(removed.view.link.is_none());
    assert!(
        store
            .work_pr_link_set(&lead.token, &removal, now + 5)
            .unwrap()
            .receipt
            .replayed
    );
    let event = store
        .events_after(updated.view.updated_seq, 10)
        .unwrap()
        .into_iter()
        .find(|e| e.kind == "work_pr_link")
        .unwrap();
    let data: serde_json::Value = serde_json::from_str(&event.data).unwrap();
    assert_eq!(data["operation"], "pr_link_remove");
    assert!(data["relation"].is_null());
}

fn pr_links_survive_compaction_with_correct_capacity(db: &Db) {
    let store = db.open();
    let now = unix_now();
    let old = now - 3 * 86_400;
    let lead = agent(&*store, 1);
    let item = store
        .work_start(
            &lead.token,
            &native_start("pr-retain", "PR retention", "lead"),
            old,
        )
        .unwrap()
        .item;
    let update = pr_link_update(&item, "retained-link");
    let linked = store
        .work_pr_link_set(&lead.token, &update, old + 1)
        .unwrap();
    let mut cp = checkpoint(
        &item.id,
        "retained-progress",
        1,
        "lead",
        CheckpointKind::Progress,
        "new baseline",
    );
    cp.expected_updated_seq = Some(linked.view.updated_seq);
    let progress = store.work_checkpoint(&lead.token, &cp, old + 2).unwrap();
    let (charged, aggregate) = db.work_usage_and_aggregate();
    assert_eq!(charged, aggregate);
    let scope = WorkRepositoryScope {
        provider: "github".into(),
        host: "github.com".into(),
        repository: REPO.into(),
        repository_id: Some(REPOSITORY_ID),
    };
    store
        .work_compact(&scope, now - 2 * 86_400, true, now)
        .unwrap();
    let current = store
        .work_pr_link_get(&lead.token, &item.id, &WorkPrLinkQuery::default(), now)
        .unwrap()
        .unwrap();
    assert_eq!(current.link, linked.view.link);
    assert_eq!(current.updated_seq, progress.item.updated_seq);
    assert!(
        !store
            .events_after(0, 100)
            .unwrap()
            .iter()
            .any(|e| e.seq == linked.receipt.event_seq)
    );
    assert_invalid(
        store.work_pr_link_set(&lead.token, &update, now),
        "PR link receipt expired or Work changed",
    );
    let (charged, aggregate) = db.work_usage_and_aggregate();
    assert_eq!(charged, aggregate);
    let mut removal = pr_link_update(&progress.item, "retained-remove");
    removal.link = None;
    store.work_pr_link_set(&lead.token, &removal, now).unwrap();
    let (charged, aggregate) = db.work_usage_and_aggregate();
    assert_eq!(charged, aggregate);
}

fn pr_link_noops_leave_no_mutation_or_receipt(db: &Db) {
    let store = db.open();
    let now = unix_now();
    let lead = agent(&*store, 1);
    let item = store
        .work_start(
            &lead.token,
            &native_start("pr-noop", "PR no-op", "lead"),
            now,
        )
        .unwrap()
        .item;
    let mut update = pr_link_update(&item, "noop-absent");
    update.link = None;
    let before_event = store.latest_event_seq().unwrap();
    let before_usage = db.work_usage_and_aggregate();
    assert_invalid(
        store.work_pr_link_set(&lead.token, &update, now + 1),
        "PR relationship is unchanged",
    );
    assert_eq!(store.latest_event_seq().unwrap(), before_event);
    assert_eq!(
        store.work_get(&lead.token, &item.id, now).unwrap().unwrap(),
        item
    );
    assert_eq!(db.work_usage_and_aggregate(), before_usage);

    // A rejected operation key has no receipt and remains usable for a real change.
    update.link = pr_link_update(&item, "unused").link;
    let linked = store
        .work_pr_link_set(&lead.token, &update, now + 2)
        .unwrap();
    assert!(!linked.receipt.replayed);
    let current_item = store
        .work_get(&lead.token, &item.id, now + 2)
        .unwrap()
        .unwrap();
    let before_event = store.latest_event_seq().unwrap();
    let before_usage = db.work_usage_and_aggregate();
    let mut repeated = update.clone();
    repeated.operation_id = "noop-present".into();
    repeated.expected_updated_seq = linked.view.updated_seq;
    assert_invalid(
        store.work_pr_link_set(&lead.token, &repeated, now + 3),
        "PR relationship is unchanged",
    );
    assert_eq!(store.latest_event_seq().unwrap(), before_event);
    assert_eq!(
        store
            .work_get(&lead.token, &item.id, now + 2)
            .unwrap()
            .unwrap(),
        current_item
    );
    assert_eq!(db.work_usage_and_aggregate(), before_usage);

    let replay = store
        .work_pr_link_set(&lead.token, &update, now + 3)
        .unwrap();
    assert!(replay.receipt.replayed);
    assert_eq!(replay.receipt.event_seq, linked.receipt.event_seq);
    repeated.link.as_mut().unwrap().number += 1;
    assert!(
        !store
            .work_pr_link_set(&lead.token, &repeated, now + 4)
            .unwrap()
            .receipt
            .replayed
    );
}

fn concurrent_pr_link_updates_fence_and_replay(db: &Db) {
    let store = db.open();
    let now = unix_now();
    let lead = agent(&*store, 1);
    let item = store
        .work_start(
            &lead.token,
            &native_start("pr-race", "Concurrent PR links", "lead"),
            now,
        )
        .unwrap()
        .item;
    let first = pr_link_update(&item, "racing-first");
    let mut second = pr_link_update(&item, "racing-second");
    second.link.as_mut().unwrap().number += 1;
    let barrier = Arc::new(Barrier::new(2));
    let handles = [first, second]
        .into_iter()
        .zip([db.open(), db.open()])
        .map(|(update, connection)| {
            let barrier = barrier.clone();
            let token = lead.token.clone();
            thread::spawn(move || {
                barrier.wait();
                connection.work_pr_link_set(&token, &update, now + 1)
            })
        })
        .collect::<Vec<_>>();
    let results = handles
        .into_iter()
        .map(|handle| handle.join().unwrap())
        .collect::<Vec<_>>();
    assert_eq!(results.iter().filter(|result| result.is_ok()).count(), 1);
    assert_eq!(results.iter().filter(|result| matches!(result, Err(HubError::Invalid(message)) if message == "PR link receipt expired or Work changed")).count(), 1);
    let winner = results.into_iter().find_map(Result::ok).unwrap();
    assert_eq!(work_event_count(&*store, "work_pr_link"), 1);
    let current_item = store
        .work_get(&lead.token, &item.id, now + 1)
        .unwrap()
        .unwrap();
    assert_eq!(current_item.updated_seq, winner.view.updated_seq);

    let mut identical = pr_link_update(&current_item, "racing-same-operation");
    identical.link.as_mut().unwrap().head_sha = "b".repeat(40);
    let barrier = Arc::new(Barrier::new(2));
    let handles = [db.open(), db.open()]
        .into_iter()
        .map(|connection| {
            let barrier = barrier.clone();
            let token = lead.token.clone();
            let update = identical.clone();
            thread::spawn(move || {
                barrier.wait();
                connection.work_pr_link_set(&token, &update, now + 2)
            })
        })
        .collect::<Vec<_>>();
    let results = handles
        .into_iter()
        .map(|handle| handle.join().unwrap().unwrap())
        .collect::<Vec<_>>();
    assert_eq!(results[0].receipt.event_seq, results[1].receipt.event_seq);
    assert_eq!(
        results[0].receipt.operation_id,
        results[1].receipt.operation_id
    );
    assert_eq!(results[0].view, results[1].view);
    assert_eq!(
        results
            .iter()
            .filter(|result| result.receipt.replayed)
            .count(),
        1
    );
    assert_eq!(work_event_count(&*store, "work_pr_link"), 2);
    let (charged, aggregate) = db.work_usage_and_aggregate();
    assert_eq!(charged, aggregate);
}

fn pr_link_receipts_replay_after_takeover_changes_and_removal(db: &Db) {
    let store = db.open();
    let now = unix_now();
    let lead = agent(&*store, 1);
    let item = store
        .work_start(
            &lead.token,
            &native_start(
                "pr-replay-takeover",
                "PR replay after takeover",
                "original-session",
            ),
            now,
        )
        .unwrap()
        .item;
    let original_request = pr_link_update(&item, "original-link");
    let original = store
        .work_pr_link_set(&lead.token, &original_request, now + 1)
        .unwrap();
    let successor = agent(&*store, 2);
    let takeover_at = now + LEASE + 2;
    tick(&*store, now, takeover_at);
    let taken = store
        .work_start(
            &successor.token,
            &takeover(&item.id, "successor-session", Some(1)),
            takeover_at,
        )
        .unwrap();
    assert!(taken.took_over);
    let verify_original_replay = |expected_link: Option<agentscm::hub::work_links::WorkPrLink>,
                                  expected_updated_seq: i64,
                                  replay_at: i64| {
        let before_seq = store.latest_event_seq().unwrap();
        let before_usage = db.work_usage_and_aggregate();
        let replay = store
            .work_pr_link_set(&lead.token, &original_request, replay_at)
            .unwrap();
        assert!(replay.receipt.replayed);
        assert_eq!(replay.receipt.work_id, original.receipt.work_id);
        assert_eq!(replay.receipt.operation_id, original.receipt.operation_id);
        assert_eq!(replay.receipt.event_seq, original.receipt.event_seq);
        assert_eq!(replay.receipt.generation, 1);
        assert_eq!(replay.view.generation, 2);
        assert_eq!(replay.view.updated_seq, expected_updated_seq);
        assert_eq!(replay.view.link, expected_link);
        assert_eq!(store.latest_event_seq().unwrap(), before_seq);
        assert_eq!(db.work_usage_and_aggregate(), before_usage);
    };
    verify_original_replay(
        original.view.link.clone(),
        taken.item.updated_seq,
        takeover_at + 1,
    );

    // Exact content alone is insufficient: the committing token must still be valid.
    let same_owner_other_token = another_token(&*store, &lead, "another-original-owner-token");
    for token in [&same_owner_other_token.token, &successor.token] {
        assert_invalid(
            store.work_pr_link_set(token, &original_request, takeover_at + 1),
            "only the current Work owner and lead session may change a PR link",
        );
    }
    let mut wrong_session = original_request.clone();
    wrong_session.session = "unrelated-session".into();
    assert_invalid(
        store.work_pr_link_set(&lead.token, &wrong_session, takeover_at + 1),
        "operation_id was already used for another request",
    );
    let mut wrong_target = original_request.clone();
    wrong_target.link.as_mut().unwrap().repository_id += 1;
    assert_invalid(
        store.work_pr_link_set(&lead.token, &wrong_target, takeover_at + 1),
        "operation_id was already used for another request",
    );
    let mut old_lead_new_operation = original_request.clone();
    old_lead_new_operation.operation_id = "old-lead-new-key".into();
    assert_invalid(
        store.work_pr_link_set(&lead.token, &old_lead_new_operation, takeover_at + 1),
        "PR link generation is fenced",
    );
    assert!(matches!(
        store.work_pr_link_set(&lead.token, &original_request, lead.expires_at),
        Err(HubError::Unauthorized)
    ));

    let mut changed_request = pr_link_update(&taken.item, "successor-change-link");
    changed_request.link.as_mut().unwrap().head_sha = "b".repeat(40);
    let changed = store
        .work_pr_link_set(&successor.token, &changed_request, takeover_at + 2)
        .unwrap();
    verify_original_replay(
        changed.view.link.clone(),
        changed.view.updated_seq,
        takeover_at + 3,
    );

    let mut remove_request = changed_request.clone();
    remove_request.operation_id = "successor-remove-link".into();
    remove_request.expected_updated_seq = changed.view.updated_seq;
    remove_request.link = None;
    let removed = store
        .work_pr_link_set(&successor.token, &remove_request, takeover_at + 3)
        .unwrap();
    verify_original_replay(None, removed.view.updated_seq, takeover_at + 4);

    let reader = agent_with(
        &*store,
        AgentSpec {
            provider: "github",
            host: "github.com",
            user_id: 3,
            repository: REPO,
            repository_id: Some(REPOSITORY_ID),
            read_only: true,
            ttl: TTL,
        },
    );
    assert!(matches!(
        store.work_pr_link_set(&reader.token, &original_request, takeover_at + 4),
        Err(HubError::Unauthorized)
    ));
    assert!(store.revoke_agent_token(&lead.token).unwrap());
    assert!(matches!(
        store.work_pr_link_set(&lead.token, &original_request, takeover_at + 4),
        Err(HubError::Unauthorized)
    ));
}

fn checkpoint_rejection_respects_removed_history_without_a_checkpoint_baseline(db: &Db) {
    let store = db.open();
    let lead = agent(&*store, 1);
    let now = unix_now();
    let old = now - 200_000;
    let started = store
        .work_start(
            &lead.token,
            &native_start(
                "removed-checkpoint",
                "Removed checkpoint",
                "retention-session",
            ),
            old,
        )
        .unwrap()
        .item;
    let mut request = checkpoint(
        &started.id,
        "removed-operation",
        1,
        "retention-session",
        CheckpointKind::Progress,
        "Committed operation whose receipt expires",
    );
    request.expected_updated_seq = Some(started.updated_seq);
    let committed = store
        .work_checkpoint(&lead.token, &request, old + 1)
        .unwrap();
    // Keep a PR-link baseline, so no retained checkpoint snapshot can reveal the
    // missing operation. Only the durable event floor proves history is incomplete.
    let linked = store
        .work_pr_link_set(
            &lead.token,
            &pr_link_update(&committed.item, "retention-pr-baseline"),
            old + 2,
        )
        .unwrap();
    let mut recent = request.clone();
    recent.operation_id = "recent-operation".into();
    recent.expected_updated_seq = Some(linked.view.updated_seq);
    let current = store
        .work_checkpoint(&lead.token, &recent, now - 1)
        .unwrap();
    let scope = WorkRepositoryScope {
        provider: "github".into(),
        host: "github.com".into(),
        repository: REPO.into(),
        repository_id: Some(REPOSITORY_ID),
    };
    let compacted = store.work_compact(&scope, now - 86_400, true, now).unwrap();
    // Checkpoints and PR-link changes share the operation receipt table. Both
    // old receipts expire, while the PR-link version is retained as the baseline.
    assert_eq!(compacted.deleted_operations, 2);
    assert_eq!(compacted.deleted_versions, 2);
    assert_eq!(compacted.retention_floor, linked.receipt.event_seq);
    assert_invalid(
        store.work_checkpoint(&lead.token, &request, now),
        &format!(
            "checkpoint receipt expired or work changed; current updated_seq is {}",
            current.item.updated_seq
        ),
    );
    let mut uncommitted = recent.clone();
    uncommitted.operation_id = "never-committed-after-baseline".into();
    assert!(
        matches!(store.work_checkpoint(&lead.token, &uncommitted, now),
        Err(HubError::WorkCheckpointSequenceConflict { current_seq }) if current_seq == current.item.updated_seq)
    );
    assert!(
        store
            .work_checkpoint(&lead.token, &recent, now)
            .unwrap()
            .receipt
            .replayed
    );
    assert_eq!(
        store
            .work_get(&lead.token, &started.id, now)
            .unwrap()
            .unwrap()
            .updated_seq,
        current.item.updated_seq
    );
}

fn lead_checkpoint(
    store: &dyn Store,
    who: &Agent,
    item: &agentscm::hub::work::WorkItem,
    operation_id: &str,
    session: &str,
    kind: CheckpointKind,
    now: i64,
) -> agentscm::hub::work::WorkCheckpointResult {
    let mut request = checkpoint(
        &item.id,
        operation_id,
        item.generation,
        session,
        kind,
        "step",
    );
    request.expected_updated_seq = Some(item.updated_seq);
    store.work_checkpoint(&who.token, &request, now).unwrap()
}

fn assert_scope_overlap(result: &agentscm::hub::work::WorkStartResult) {
    assert!(!result.created);
    assert_eq!(
        result.conflict.as_ref().map(|conflict| conflict.kind),
        Some(WorkConflictKind::ScopeOverlap)
    );
}

fn reservation_migration_backfills_unfinished_work_and_leaves_finished_unstated(db: &Db) {
    let store = db.open();
    let alice = agent(&*store, 1);
    let now = unix_now();
    let start = |key: &str, session: &str, scope: &str| {
        let mut start = native_start(key, key, session);
        start.scope = vec![scope.into()];
        start
    };
    let active = store
        .work_start(&alice.token, &start("active", "s-active", "src/**"), now)
        .unwrap()
        .item;
    let paused = store
        .work_start(&alice.token, &start("paused", "s-paused", "lib/**"), now)
        .unwrap()
        .item;
    lead_checkpoint(
        &*store,
        &alice,
        &paused,
        "pause",
        "s-paused",
        CheckpointKind::Pause,
        now + 1,
    );
    let finished = store
        .work_start(
            &alice.token,
            &start("finished", "s-finished", "docs/**"),
            now,
        )
        .unwrap()
        .item;
    lead_checkpoint(
        &*store,
        &alice,
        &finished,
        "finish",
        "s-finished",
        CheckpointKind::Finish,
        now + 1,
    );
    drop(store);

    // Rebuild the schema a binary without this change leaves behind: no columns, no marker.
    db.sql(
        "DROP INDEX hub_work_reserved;
         ALTER TABLE hub_work_items DROP COLUMN reservation_basis;
         ALTER TABLE hub_work_items DROP COLUMN terminal_outcome;
         DELETE FROM hub_named_migrations WHERE name = 'work_reservations';",
    );

    let store = db.open();
    let implicit = (None, Some("legacy_implicit".to_owned()));
    assert_eq!(db.reservation(&active.id), implicit);
    assert_eq!(db.reservation(&paused.id), implicit);
    assert_eq!(db.reservation(&finished.id), (None, None));
    // Blocking is unchanged: the backfilled Work still holds its scope, finished Work does not.
    let blocked = store
        .work_start(&alice.token, &start("new-src", "s-new", "src/**"), now + 2)
        .unwrap();
    assert_scope_overlap(&blocked);
    let free = store
        .work_start(
            &alice.token,
            &start("new-docs", "s-docs", "docs/**"),
            now + 2,
        )
        .unwrap();
    assert!(free.created);
}

fn finish_frees_scope_for_a_new_start_and_pause_keeps_blocking(db: &Db) {
    let store = db.open();
    let alice = agent(&*store, 1);
    let bob = agent(&*store, 2);
    let carol = agent(&*store, 3);
    let now = unix_now();
    let first = store
        .work_start(&alice.token, &native_start("a", "a", "s-a"), now)
        .unwrap()
        .item;
    assert_eq!(db.reservation(&first.id), (None, Some("admitted".into())));
    assert_scope_overlap(
        &store
            .work_start(&bob.token, &native_start("b1", "b", "s-b"), now + 1)
            .unwrap(),
    );

    let finished = lead_checkpoint(
        &*store,
        &alice,
        &first,
        "finish",
        "s-a",
        CheckpointKind::Finish,
        now + 2,
    );
    assert_eq!(finished.item.state, WorkState::Finished);
    assert_eq!(db.reservation(&first.id), (Some("completed".into()), None));

    let second = store
        .work_start(&bob.token, &native_start("b2", "b", "s-b2"), now + 3)
        .unwrap();
    assert!(second.created);
    assert_eq!(
        db.reservation(&second.item.id),
        (None, Some("admitted".into()))
    );

    // A pause keeps the scope held, as an implicit reservation.
    lead_checkpoint(
        &*store,
        &bob,
        &second.item,
        "pause",
        "s-b2",
        CheckpointKind::Pause,
        now + 4,
    );
    assert_eq!(
        db.reservation(&second.item.id),
        (None, Some("legacy_implicit".into()))
    );
    assert_scope_overlap(
        &store
            .work_start(&carol.token, &native_start("c", "c", "s-c"), now + 5)
            .unwrap(),
    );
}

fn finished_work_refuses_checkpoints_but_replays_the_finish_receipt(db: &Db) {
    let store = db.open();
    let alice = agent(&*store, 1);
    let now = unix_now();
    let started = store
        .work_start(&alice.token, &native_start("a", "a", "s-a"), now)
        .unwrap()
        .item;
    let mut finish = checkpoint(
        &started.id,
        "finish",
        1,
        "s-a",
        CheckpointKind::Finish,
        "done",
    );
    finish.expected_updated_seq = Some(started.updated_seq);
    let first = store
        .work_checkpoint(&alice.token, &finish, now + 1)
        .unwrap();
    assert!(!first.receipt.replayed);
    let before = store
        .work_get(&alice.token, &started.id, now + 2)
        .unwrap()
        .unwrap();
    let events = work_event_count(&*store, "work_checkpoint");

    let mut resume = checkpoint(
        &started.id,
        "resume",
        1,
        "s-a",
        CheckpointKind::Progress,
        "again",
    );
    resume.expected_updated_seq = Some(before.updated_seq);
    assert_invalid(
        store.work_checkpoint(&alice.token, &resume, now + 3),
        "finished work accepts no checkpoint; reopen it first",
    );
    let after = store
        .work_get(&alice.token, &started.id, now + 4)
        .unwrap()
        .unwrap();
    assert_eq!(
        serde_json::to_value(&before).unwrap(),
        serde_json::to_value(&after).unwrap()
    );
    assert_eq!(after.state, WorkState::Finished);
    assert_eq!(work_event_count(&*store, "work_checkpoint"), events);
    assert_eq!(
        db.reservation(&started.id),
        (Some("completed".into()), None)
    );

    let replay = store
        .work_checkpoint(&alice.token, &finish, now + 5)
        .unwrap();
    assert!(replay.receipt.replayed);
    assert_eq!(replay.receipt.event_seq, first.receipt.event_seq);
}

fn stale_finished_work_cannot_be_taken_over(db: &Db) {
    let store = db.open();
    let alice = agent(&*store, 1);
    let bob = agent(&*store, 2);
    let now = unix_now();
    let started = store
        .work_start(&alice.token, &native_start("a", "a", "s-a"), now)
        .unwrap()
        .item;
    let finished = lead_checkpoint(
        &*store,
        &alice,
        &started,
        "finish",
        "s-a",
        CheckpointKind::Finish,
        now + 1,
    )
    .item;
    let message = "finished work cannot be taken over; reopen it instead";
    for at in [now + 2, now + 1 + CLAIM_STALE_SECONDS + 10] {
        let attempt = takeover(&started.id, "s-b", Some(finished.generation));
        let result = store.work_start(&bob.token, &attempt, at);
        assert!(
            matches!(result, Err(HubError::Invalid(ref actual)) if actual == message && actual.contains("reopen")),
            "takeover at {at} must name reopen"
        );
    }
    let after = store
        .work_get(&alice.token, &started.id, now + 3)
        .unwrap()
        .unwrap();
    assert_eq!(after.lead_token, alice.token);
    assert_eq!(after.generation, finished.generation);
    assert_eq!(after.updated_seq, finished.updated_seq);
    assert_eq!(
        db.reservation(&started.id),
        (Some("completed".into()), None)
    );
}

fn announce_intent_refuses_fenced_sessions_but_not_finished_or_unlinked_ones(db: &Db) {
    use agentscm::hub::intent::NewIntent;
    let store = db.open();
    let alice = agent(&*store, 1);
    let bob = agent(&*store, 2);
    let carol = agent(&*store, 3);
    let dave = agent(&*store, 4);
    let now = unix_now();
    let intent = |n: u128, path: &str| NewIntent {
        id: format!("int_{n:032x}"),
        task: "edit".into(),
        branch: None,
        base: None,
        machine: None,
        paths: vec![path.into()],
        ttl_seconds: 600,
    };
    // The announce route names no session, so it resolves to the default session.
    let started = store
        .work_start(&alice.token, &native_start("a", "a", "default"), now)
        .unwrap()
        .item;
    store
        .announce_intent(&alice.token, &intent(1, "src/a.rs"), now + 1)
        .unwrap();

    // Bob takes over after alice's lease lapsed; alice's session is now fenced.
    let later = now + LEASE + 10;
    tick(&*store, now, later);
    assert!(
        store
            .work_start(
                &bob.token,
                &takeover(&started.id, "default", Some(1)),
                later
            )
            .unwrap()
            .took_over
    );
    assert!(matches!(
        store.announce_intent(&alice.token, &intent(2, "src/b.rs"), later + 1),
        Err(HubError::Invalid(_))
    ));
    store
        .announce_intent(&bob.token, &intent(3, "src/c.rs"), later + 1)
        .unwrap();

    // A token linked only to finished Work may post advisory intents: the plain route carries no
    // session, an intent reserves nothing, and finishing acquired no reservation to lose.
    let mut lib_start = native_start("d", "d", "default");
    lib_start.scope = vec!["lib/**".into()];
    let lib = store
        .work_start(&dave.token, &lib_start, later)
        .unwrap()
        .item;
    store
        .announce_intent(&dave.token, &intent(4, "lib/a.rs"), later + 1)
        .unwrap();
    lead_checkpoint(
        &*store,
        &dave,
        &lib,
        "finish-lib",
        "default",
        CheckpointKind::Finish,
        later + 2,
    );
    store
        .announce_intent(&dave.token, &intent(5, "lib/b.rs"), later + 3)
        .unwrap();

    // A session never linked to Work keeps the legacy advisory behavior.
    store
        .announce_intent(&carol.token, &intent(6, "src/a.rs"), later + 4)
        .unwrap();
}

fn helper_join_on_finished_work_acquires_nothing(db: &Db) {
    let store = db.open();
    let alice = agent(&*store, 1);
    let helper = agent(&*store, 2);
    let carol = agent(&*store, 3);
    let now = unix_now();
    let started = store
        .work_start(&alice.token, &native_start("a", "a", "s-a"), now)
        .unwrap()
        .item;
    let finished = lead_checkpoint(
        &*store,
        &alice,
        &started,
        "finish",
        "s-a",
        CheckpointKind::Finish,
        now + 1,
    )
    .item;

    for (who, session, role) in [
        (&helper, "s-h", WorkRole::Helper),
        (&carol, "s-r", WorkRole::Reviewer),
    ] {
        let joined = store
            .work_start(&who.token, &join(&started.id, session, role), now + 2)
            .unwrap();
        assert!(joined.joined);
        assert_eq!(joined.item.state, WorkState::Finished);
        assert_eq!(joined.item.generation, finished.generation);
        assert_eq!(joined.item.lead_session, "s-a");
    }
    assert_eq!(
        db.reservation(&started.id),
        (Some("completed".into()), None)
    );
    // Participants of finished Work do not hold its scope against new Work.
    let fresh = store
        .work_start(&helper.token, &native_start("h", "h", "s-h2"), now + 3)
        .unwrap();
    assert!(fresh.created);
}

fn unreserved_work_needs_fresh_admission_to_be_taken_over(db: &Db) {
    let store = db.open();
    let alice = agent(&*store, 1);
    let bob = agent(&*store, 2);
    let carol = agent(&*store, 3);
    let now = unix_now();
    let started = store
        .work_start(&alice.token, &native_start("a", "a", "s-a"), now)
        .unwrap()
        .item;
    // A real release leaves the Work unfinished with no reservation.
    let released = store
        .work_transition(
            &by_agent(&alice),
            &transition_request(&started, "rel", WorkTransitionKind::Release, Some("s-a")),
            now,
        )
        .unwrap()
        .item;
    let holder = store
        .work_start(&bob.token, &native_start("b", "b", "s-b"), now + 1)
        .unwrap();
    assert!(holder.created, "unreserved Work does not block a new start");

    // Its own scope change checks nothing, so it may overlap the new holder.
    // A progress report on Work that holds nothing is refused; a pause is allowed.
    let mut widen = checkpoint(
        &released.id,
        "widen",
        released.generation,
        "s-a",
        CheckpointKind::Pause,
        "widen",
    );
    widen.expected_updated_seq = Some(released.updated_seq);
    widen.scope = Some(vec!["src/**".into(), "lib/**".into()]);
    let widened = store
        .work_checkpoint(&alice.token, &widen, now + 2)
        .unwrap()
        .item;
    assert_eq!(db.reservation(&released.id), (None, None));

    // Taking it over must win the scope first; the overlap leaves the Work untouched.
    let handover = lead_checkpoint(
        &*store,
        &alice,
        &widened,
        "handover",
        "s-a",
        CheckpointKind::Handover,
        now + 3,
    )
    .item;
    assert_eq!(db.reservation(&released.id), (None, None));
    let blocked = store
        .work_start(
            &carol.token,
            &takeover(&released.id, "s-c", Some(handover.generation)),
            now + 4,
        )
        .unwrap();
    assert_scope_overlap(&blocked);
    assert!(!blocked.took_over);
    let unchanged = store
        .work_get(&alice.token, &released.id, now + 5)
        .unwrap()
        .unwrap();
    assert_eq!(unchanged.generation, handover.generation);
    assert_eq!(unchanged.lead_session, "s-a");
    assert_eq!(db.reservation(&released.id), (None, None));

    // Once the holder finishes, the same takeover is admitted.
    lead_checkpoint(
        &*store,
        &bob,
        &holder.item,
        "finish-b",
        "s-b",
        CheckpointKind::Finish,
        now + 6,
    );
    let admitted = store
        .work_start(
            &carol.token,
            &takeover(&released.id, "s-c", Some(handover.generation)),
            now + 7,
        )
        .unwrap();
    assert!(admitted.took_over);
    assert_eq!(
        db.reservation(&released.id),
        (None, Some("admitted".into()))
    );
}

// Work transitions: release, retain, finish and reopen.

fn user_of(store: &dyn Store, who: &Agent) -> User {
    store.user_by_id(&who.owner).unwrap().unwrap()
}

fn person(user: &User, is_admin: bool) -> WorkTransitionActor<'_> {
    WorkTransitionActor::Person {
        user,
        repository: REPO,
        repository_id: REPOSITORY_ID,
        is_admin,
    }
}

fn by_agent(who: &Agent) -> WorkTransitionActor<'_> {
    WorkTransitionActor::Agent { token: &who.token }
}

fn transition_request(
    item: &agentscm::hub::work::WorkItem,
    operation_id: &str,
    transition: WorkTransitionKind,
    session: Option<&str>,
) -> WorkTransitionRequest {
    WorkTransitionRequest {
        work_id: item.id.clone(),
        operation_id: operation_id.into(),
        generation: item.generation,
        expected_updated_seq: item.updated_seq,
        session: session.map(str::to_owned),
        transition,
        reason: "synthetic reason".into(),
        superseded_by: None,
    }
}

fn start_in(
    store: &dyn Store,
    who: &Agent,
    key: &str,
    session: &str,
    scope: &str,
    now: i64,
) -> agentscm::hub::work::WorkItem {
    let mut start = native_start(key, key, session);
    start.scope = vec![scope.into()];
    let result = store.work_start(&who.token, &start, now).unwrap();
    assert!(result.created, "{key} should be created");
    result.item
}

fn current_item(
    store: &dyn Store,
    who: &Agent,
    id: &str,
    now: i64,
) -> agentscm::hub::work::WorkItem {
    store.work_get(&who.token, id, now).unwrap().unwrap()
}

fn transition_events(store: &dyn Store) -> Vec<serde_json::Value> {
    store
        .events_after(0, 1000)
        .unwrap()
        .iter()
        .filter(|event| event.kind == "work_transition")
        .map(|event| serde_json::from_str(&event.data).unwrap())
        .collect()
}

/// Holds `path` for the token's named session, the way a hook or MCP session does.
fn announce(store: &dyn Store, who: &Agent, session: &str, path: &str, now: i64) {
    store
        .touch_session(
            &who.token,
            &agentscm::hub::session::SessionCall {
                session: Some(session.into()),
                via: "mcp".into(),
                task: Some("edit".into()),
                paths: vec![path.into()],
                deny: false,
                report: false,
            },
            now,
        )
        .unwrap();
}

fn transition_matrix_covers_every_actor_and_kind(db: &Db) {
    use WorkTransitionKind::{Abandon, Complete, Release, Reopen, Retain, Supersede};
    let store = db.open();
    let now = unix_now();
    let alice = agent(&*store, 1);
    let alice_b = another_token(&*store, &alice, "agent-b");
    let admin_agent = agent(&*store, 9);
    let owner = user_of(&*store, &alice);
    let admin = user_of(&*store, &admin_agent);
    // Columns: lead, owner agent, owner person, administrator.
    let table = [
        (Release, [true, true, true, true]),
        (Retain, [true, true, true, false]),
        (Complete, [true, true, true, false]),
        (Abandon, [true, true, true, true]),
        (Supersede, [true, true, true, true]),
        (Reopen, [true, true, false, false]),
    ];
    let mut n = 0_i64;
    for (kind, allowed) in table {
        for (column, allowed) in allowed.into_iter().enumerate() {
            n += 1;
            let lead_session = format!("lead-s{n}");
            let mut item = start_in(
                &*store,
                &alice,
                &format!("m{n}"),
                &lead_session,
                &format!("m{n}/**"),
                now + n,
            );
            let other = start_in(
                &*store,
                &alice,
                &format!("o{n}"),
                &format!("other-s{n}"),
                &format!("o{n}/**"),
                now + n,
            );
            if kind == Reopen {
                item = lead_checkpoint(
                    &*store,
                    &alice,
                    &item,
                    "fin",
                    &lead_session,
                    CheckpointKind::Finish,
                    now + n,
                )
                .item;
            }
            let session = [Some(lead_session.as_str()), Some("owner-s"), None, None][column];
            let mut request = transition_request(&item, "op", kind, session);
            if kind == Supersede {
                request.superseded_by = Some(other.id.clone());
            }
            let actor = match column {
                0 => by_agent(&alice),
                1 => by_agent(&alice_b),
                2 => person(&owner, false),
                _ => person(&admin, true),
            };
            let before = serde_json::to_value(&item).unwrap();
            let before_columns = db.reservation(&item.id);
            let seq = store.latest_event_seq().unwrap();
            let outcome = store.work_transition(&actor, &request, now + 100 + n);
            let label = format!("{kind:?} column {column}");
            if !allowed {
                assert!(matches!(outcome, Err(HubError::Invalid(_))), "{label}");
                assert_eq!(
                    serde_json::to_value(current_item(&*store, &alice, &item.id, now + 200))
                        .unwrap(),
                    before,
                    "{label}"
                );
                assert_eq!(db.reservation(&item.id), before_columns, "{label}");
                assert_eq!(store.latest_event_seq().unwrap(), seq, "{label}");
                continue;
            }
            let result = outcome.unwrap_or_else(|error| panic!("{label}: {error:?}"));
            assert!(result.conflict.is_none(), "{label}");
            let receipt = result.receipt.unwrap();
            assert!(!receipt.replayed, "{label}");
            let after = result.item;
            assert_eq!(receipt.generation, after.generation, "{label}");
            // Reports stay where their author left them.
            assert_eq!(after.current, item.current, "{label}");
            assert_eq!(after.blocker, item.blocker, "{label}");
            let lead = column == 0;
            match kind {
                Release => {
                    assert_eq!(after.state, WorkState::Paused, "{label}");
                    assert_eq!(after.generation, item.generation + 1, "{label}");
                    assert_eq!(db.reservation(&item.id), (None, None), "{label}");
                }
                Retain => {
                    assert_eq!(after.state, WorkState::Active, "{label}");
                    assert_eq!(after.generation, item.generation, "{label}");
                    assert_eq!(
                        db.reservation(&item.id),
                        (None, Some("retained".into())),
                        "{label}"
                    );
                }
                Complete | Abandon | Supersede => {
                    assert_eq!(after.state, WorkState::Finished, "{label}");
                    assert_eq!(
                        after.generation,
                        item.generation + i64::from(!lead),
                        "{label}"
                    );
                    let name = match kind {
                        Complete => "completed",
                        Abandon => "abandoned",
                        _ => "superseded",
                    };
                    assert_eq!(
                        db.reservation(&item.id),
                        (Some(name.into()), None),
                        "{label}"
                    );
                    assert_eq!(
                        after.related_work.contains(&other.id),
                        kind == Supersede,
                        "{label}"
                    );
                }
                Reopen => {
                    assert_eq!(after.state, WorkState::Active, "{label}");
                    assert_eq!(after.generation, item.generation + 1, "{label}");
                    assert_eq!(
                        db.reservation(&item.id),
                        (None, Some("admitted".into())),
                        "{label}"
                    );
                    assert_eq!(after.lead_session, session.unwrap(), "{label}");
                    assert_eq!(
                        after.lead_token,
                        if lead { &alice.token } else { &alice_b.token }.as_str(),
                        "{label}"
                    );
                }
            }
        }
    }
}

fn transition_state_rules_refuse_without_mutation(db: &Db) {
    use WorkTransitionKind::{Complete, Release, Reopen, Retain, Supersede};
    let store = db.open();
    let now = unix_now();
    let alice = agent(&*store, 1);
    let item = start_in(&*store, &alice, "a", "s-a", "src/**", now);
    let other = start_in(&*store, &alice, "b", "s-b", "lib/**", now);
    let try_it = |item: &agentscm::hub::work::WorkItem,
                  op: &str,
                  kind,
                  edit: &dyn Fn(&mut WorkTransitionRequest)| {
        let mut request = transition_request(item, op, kind, Some("s-a"));
        edit(&mut request);
        store.work_transition(&by_agent(&alice), &request, now + 5)
    };
    let invalid =
        |result: Result<_, HubError>| assert!(matches!(result, Err(HubError::Invalid(_))));
    // Reopen needs finished Work; a stale generation or sequence is fenced.
    invalid(try_it(&item, "r0", Reopen, &|_| {}));
    invalid(try_it(&item, "g", Retain, &|r| r.generation += 1));
    invalid(try_it(&item, "q", Retain, &|r| r.expected_updated_seq -= 1));
    // Supersede names an existing, different Work of the repository.
    invalid(try_it(&item, "s0", Supersede, &|_| {}));
    invalid(try_it(&item, "s1", Supersede, &|r| {
        r.superseded_by = Some(item.id.clone())
    }));
    invalid(try_it(&item, "s2", Supersede, &|r| {
        r.superseded_by = Some(format!("wrk_{:032x}", 7))
    }));
    invalid(try_it(&item, "s3", Complete, &|r| {
        r.superseded_by = Some(other.id.clone())
    }));
    invalid(try_it(&item, "e", Complete, &|r| r.reason = String::new()));
    assert_eq!(
        current_item(&*store, &alice, &item.id, now + 6).updated_seq,
        item.updated_seq
    );
    // Released Work holds nothing to release or retain again.
    let released = store
        .work_transition(
            &by_agent(&alice),
            &transition_request(&item, "rel", Release, Some("s-a")),
            now + 6,
        )
        .unwrap()
        .item;
    for (op, kind) in [("rel2", Release), ("ret", Retain)] {
        let request = transition_request(&released, op, kind, Some("s-a"));
        invalid(store.work_transition(&by_agent(&alice), &request, now + 7));
    }
    let missing = WorkTransitionRequest {
        work_id: format!("wrk_{:032x}", 9),
        ..transition_request(&released, "missing", Release, Some("s-a"))
    };
    assert_invalid(
        store.work_transition(&by_agent(&alice), &missing, now + 7),
        "unknown work item",
    );
}

fn transition_refuses_foreign_read_only_and_unrelated_callers(db: &Db) {
    use WorkTransitionKind::{Abandon, Release, Retain};
    let store = db.open();
    let now = unix_now();
    let alice = agent(&*store, 1);
    let bob = agent(&*store, 2);
    let read_only = agent_with(
        &*store,
        AgentSpec {
            provider: "github",
            host: "github.com",
            user_id: 1,
            repository: REPO,
            repository_id: Some(REPOSITORY_ID),
            read_only: true,
            ttl: TTL,
        },
    );
    let carol = agent(&*store, 3);
    let carol_user = user_of(&*store, &carol);
    let bob_user = user_of(&*store, &bob);
    let item = start_in(&*store, &alice, "a", "s-a", "src/**", now);
    let before = serde_json::to_value(&item).unwrap();
    let before_columns = db.reservation(&item.id);
    let seq = store.latest_event_seq().unwrap();
    for kind in [Release, Retain, Abandon] {
        let mut request = transition_request(&item, "op", kind, Some("s-a"));
        let attempts = [
            store.work_transition(&by_agent(&bob), &request, now + 1),
            store.work_transition(&by_agent(&read_only), &request, now + 1),
            store.work_transition(
                &person(&carol_user, false),
                &{
                    request.session = None;
                    request.clone()
                },
                now + 1,
            ),
            // A foreign person who claims the flag still holds no standing in an owner check,
            // but an administrator is allowed to abandon and release.
        ];
        for (index, attempt) in attempts.into_iter().enumerate() {
            assert!(
                matches!(attempt, Err(HubError::Unauthorized)),
                "{kind:?} caller {index}"
            );
        }
    }
    // A cross-owner person without the admin flag is refused even for read access to the repository.
    let request = transition_request(&item, "op", Abandon, None);
    assert!(matches!(
        store.work_transition(&person(&bob_user, false), &request, now + 1),
        Err(HubError::Unauthorized)
    ));
    assert_eq!(
        serde_json::to_value(current_item(&*store, &alice, &item.id, now + 2)).unwrap(),
        before
    );
    assert_eq!(db.reservation(&item.id), before_columns);
    assert_eq!(store.latest_event_seq().unwrap(), seq);
}

fn owner_and_admin_act_after_the_lead_token_is_revoked(db: &Db) {
    use WorkTransitionKind::{Abandon, Release};
    let store = db.open();
    let now = unix_now();
    let alice = agent(&*store, 1);
    let helper = agent(&*store, 2);
    let admin_agent = agent(&*store, 9);
    let owner = user_of(&*store, &alice);
    let admin = user_of(&*store, &admin_agent);
    let first = start_in(&*store, &alice, "a", "default", "src/**", now);
    let second = start_in(&*store, &alice, "b", "default2", "lib/**", now);
    assert!(
        store
            .work_start(
                &helper.token,
                &join(&first.id, "default", WorkRole::Helper),
                now + 1
            )
            .unwrap()
            .joined
    );
    announce(&*store, &alice, "default", "src/a.rs", now + 2);
    announce(&*store, &helper, "default", "src/b.rs", now + 2);
    assert_eq!(store.live_intents(REPO, now + 3, 10).unwrap().len(), 2);
    assert!(store.revoke_agent_token(&alice.token).unwrap());

    let item = current_item(&*store, &helper, &first.id, now + 4);
    let released = store
        .work_transition(
            &person(&owner, false),
            &transition_request(&item, "owner-release", Release, None),
            now + 4,
        )
        .unwrap();
    assert_eq!(released.item.state, WorkState::Paused);
    assert_eq!(released.item.current, item.current);
    assert_eq!(released.item.blocker, item.blocker);
    // The lead's and the helper's claims are both gone.
    let live = store.live_intents(REPO, now + 5, 10).unwrap();
    assert!(
        live.is_empty(),
        "{:?}",
        live.iter()
            .map(|i| (i.agent.clone(), i.session.clone()))
            .collect::<Vec<_>>()
    );

    let second = current_item(&*store, &helper, &second.id, now + 5);
    store
        .work_transition(
            &person(&admin, true),
            &transition_request(&second, "admin-abandon", Abandon, None),
            now + 5,
        )
        .unwrap();
    let events = transition_events(&*store);
    assert_eq!(events.len(), 2);
    assert_eq!(
        events[0]["actor"],
        serde_json::json!({
            "kind": "owner", "id": owner.id, "login": owner.login, "via": "person"
        })
    );
    assert_eq!(events[0]["lead"]["agent"], "agent1");
    assert_eq!(events[0]["lead"]["session"], "default");
    assert_eq!(events[1]["actor"]["kind"], "admin");
    assert_eq!(events[1]["actor"]["login"], admin.login.as_str());
    assert_eq!(events[1]["reason"], "synthetic reason");
    assert_eq!(events[1]["prior_state"], "active");
    let text = serde_json::to_string(&events).unwrap();
    assert!(
        !text.contains(&alice.token),
        "the audit event must not hold a token id"
    );
    let reader = WorkLifecycleReader::Token(&helper.token);
    let view = lifecycle(&*store, &reader, &second.id, now + 6);
    assert_eq!(view.0, Some(TerminalOutcome::Abandoned));
    assert_eq!(view.1, Some(OutcomeProvenance::Admin));
}

/// The stored outcome, who stated it and the reservation basis of one Work item, read through
/// the operational projection.
fn lifecycle(
    store: &dyn Store,
    reader: &WorkLifecycleReader<'_>,
    id: &str,
    now: i64,
) -> (
    Option<TerminalOutcome>,
    Option<OutcomeProvenance>,
    Option<ReservationBasis>,
) {
    use agentscm::hub::work_lifecycle::OutcomeValue;
    let page = store
        .work_operational(reader, &OperationalSelect::One(id), now)
        .unwrap();
    let view = page.work.into_iter().next().unwrap();
    let outcome = match view.outcome.value {
        OutcomeValue::Completed => Some(TerminalOutcome::Completed),
        OutcomeValue::Abandoned => Some(TerminalOutcome::Abandoned),
        OutcomeValue::Superseded => Some(TerminalOutcome::Superseded),
        OutcomeValue::Unstated | OutcomeValue::None => None,
    };
    let provenance = outcome.map(|_| view.outcome.provenance);
    (outcome, provenance, view.reservation.basis)
}

fn release_alone_leaves_no_outcome_and_frees_the_scope(db: &Db) {
    use WorkTransitionKind::Release;
    let store = db.open();
    let now = unix_now();
    let alice = agent(&*store, 1);
    let bob = agent(&*store, 2);
    let item = start_in(&*store, &alice, "a", "s-a", "src/**", now);
    let released = store
        .work_transition(
            &by_agent(&alice),
            &transition_request(&item, "rel", Release, Some("s-a")),
            now + 1,
        )
        .unwrap()
        .item;
    assert_eq!(
        (released.state, released.generation),
        (WorkState::Paused, 2)
    );
    assert_eq!(db.reservation(&item.id), (None, None));
    // Unfinished and unreserved: another start on the scope succeeds.
    let taken = start_in(&*store, &bob, "b", "s-b", "src/**", now + 2);
    assert_ne!(taken.id, item.id);
    let still = current_item(&*store, &alice, &item.id, now + 3);
    assert_eq!(still.state, WorkState::Paused);
    assert_eq!(db.reservation(&item.id), (None, None));
    // Fresh admission of the released Work is now an overlap with bob.
    let resume = store
        .work_start(&alice.token, &takeover(&item.id, "s-a", Some(2)), now + 4)
        .unwrap();
    assert_eq!(
        resume.conflict.map(|conflict| conflict.kind),
        Some(WorkConflictKind::ScopeOverlap)
    );
    assert_eq!(db.reservation(&item.id), (None, None));
}

fn old_session_after_release_is_fenced_and_not_reacquired(db: &Db) {
    use WorkTransitionKind::Release;
    let store = db.open();
    let now = unix_now();
    let alice = agent(&*store, 1);
    let item = start_in(&*store, &alice, "a", "default", "src/**", now);
    let released = store
        .work_transition(
            &by_agent(&alice),
            &transition_request(&item, "rel", Release, Some("default")),
            now + 1,
        )
        .unwrap()
        .item;
    let mut stale = checkpoint(
        &item.id,
        "late",
        item.generation,
        "default",
        CheckpointKind::Progress,
        "late",
    );
    stale.expected_updated_seq = Some(released.updated_seq);
    assert!(matches!(
        store.work_checkpoint(&alice.token, &stale, now + 2),
        Err(HubError::Invalid(_))
    ));
    // An ordinary start by the same session returns the Work and takes nothing.
    let again = store
        .work_start(
            &alice.token,
            &start_with(None, "a", "default", Some(&item.id), WorkRole::Lead),
            now + 3,
        )
        .unwrap();
    assert!(!again.took_over);
    assert_eq!(db.reservation(&item.id), (None, None));
    assert!(matches!(
        store.announce_intent(
            &alice.token,
            &agentscm::hub::intent::NewIntent {
                id: format!("int_{:032x}", 5),
                task: "edit".into(),
                branch: None,
                base: None,
                machine: None,
                paths: vec!["src/a.rs".into()],
                ttl_seconds: 600
            },
            now + 4
        ),
        Err(HubError::Invalid(_))
    ));
    // An explicit resume at the new generation is a fresh admission.
    let resumed = store
        .work_start(
            &alice.token,
            &takeover(&item.id, "default", Some(released.generation)),
            now + 5,
        )
        .unwrap();
    assert!(resumed.took_over);
    assert_eq!(db.reservation(&item.id), (None, Some("admitted".into())));
}

fn concurrent_release_checkpoint_and_reopen_have_one_winner(db: &Db) {
    use WorkTransitionKind::{Release, Reopen};
    let store = db.open();
    let now = unix_now();
    let alice = agent(&*store, 1);
    let alice_b = another_token(&*store, &alice, "agent-b");
    let item = start_in(&*store, &alice, "a", "lead-s", "src/**", now);

    // Release against a checkpoint, both built from the same read.
    let release = transition_request(&item, "race-release", Release, Some("lead-s"));
    let mut progress = checkpoint(
        &item.id,
        "race-progress",
        item.generation,
        "lead-s",
        CheckpointKind::Progress,
        "newer report",
    );
    progress.expected_updated_seq = Some(item.updated_seq);
    let barrier = Arc::new(Barrier::new(2));
    let release_handle = {
        let (connection, barrier, token) = (db.open(), barrier.clone(), alice.token.clone());
        thread::spawn(move || {
            barrier.wait();
            connection.work_transition(
                &WorkTransitionActor::Agent { token: &token },
                &release,
                now + 1,
            )
        })
    };
    let progress_handle = {
        let (connection, barrier, token) = (db.open(), barrier.clone(), alice.token.clone());
        thread::spawn(move || {
            barrier.wait();
            connection.work_checkpoint(&token, &progress, now + 1)
        })
    };
    let (released, progressed) = (
        release_handle.join().unwrap(),
        progress_handle.join().unwrap(),
    );
    assert_eq!(
        usize::from(released.is_ok()) + usize::from(progressed.is_ok()),
        1
    );
    let after = current_item(&*store, &alice, &item.id, now + 2);
    if released.is_ok() {
        assert_eq!(db.reservation(&item.id), (None, None));
        assert_eq!(after.generation, 2);
        assert!(
            after.current.is_none(),
            "the refused report must not be written"
        );
    } else {
        assert_eq!(db.reservation(&item.id), (None, Some("admitted".into())));
        assert_eq!(after.generation, 1);
        assert_eq!(after.current.unwrap().summary, "newer report");
    }
    // Nothing resurrects a reservation afterwards.
    let before = db.reservation(&item.id);
    let _ = store.work_checkpoint(
        &alice.token,
        &checkpoint(
            &item.id,
            "again",
            1,
            "lead-s",
            CheckpointKind::Progress,
            "x",
        ),
        now + 3,
    );
    assert_eq!(db.reservation(&item.id).1.is_some(), before.1.is_some());

    // Two reopens of the same finished Work: one lead, one refusal.
    let other = start_in(&*store, &alice, "b", "lead-b", "lib/**", now + 4);
    let finished = lead_checkpoint(
        &*store,
        &alice,
        &other,
        "fin",
        "lead-b",
        CheckpointKind::Finish,
        now + 5,
    )
    .item;
    let barrier = Arc::new(Barrier::new(2));
    let handles = [(&alice, "lead-b"), (&alice_b, "owner-b")].map(|(who, session)| {
        let (connection, barrier, token) = (db.open(), barrier.clone(), who.token.clone());
        let request = transition_request(&finished, "race-reopen", Reopen, Some(session));
        let request = WorkTransitionRequest {
            operation_id: format!("race-reopen-{session}"),
            ..request
        };
        thread::spawn(move || {
            barrier.wait();
            connection.work_transition(
                &WorkTransitionActor::Agent { token: &token },
                &request,
                now + 6,
            )
        })
    });
    let results = handles.map(|handle| handle.join().unwrap());
    assert_eq!(results.iter().filter(|result| result.is_ok()).count(), 1);
    let winner = results.into_iter().find_map(Result::ok).unwrap();
    let settled = current_item(&*store, &alice, &other.id, now + 7);
    assert_eq!(settled.generation, finished.generation + 1);
    assert_eq!(settled.lead_session, winner.item.lead_session);
    assert_eq!(db.reservation(&other.id), (None, Some("admitted".into())));
    assert_eq!(
        settled
            .participants
            .iter()
            .filter(|p| p.role == WorkRole::Lead)
            .count(),
        1
    );
}

fn transition_retries_replay_after_later_transitions(db: &Db) {
    use WorkTransitionKind::{Complete, Release, Reopen, Retain, Supersede};
    let store = db.open();
    let now = unix_now();
    let alice = agent(&*store, 1);
    let alice_b = another_token(&*store, &alice, "agent-b");
    let admin_agent = agent(&*store, 9);
    let owner = user_of(&*store, &alice);
    let admin = user_of(&*store, &admin_agent);
    let item = start_in(&*store, &alice, "a", "s-a", "src/**", now);
    let target = start_in(&*store, &alice, "t", "s-t", "lib/**", now);
    let mut at = now;
    let mut step = |actor: WorkTransitionActor<'_>,
                    op: &str,
                    kind,
                    session: Option<&str>,
                    by: Option<&str>| {
        at += 1;
        let current = current_item(&*store, &alice_b, &item.id, at);
        let mut request = transition_request(&current, op, kind, session);
        request.superseded_by = by.map(str::to_owned);
        let result = store.work_transition(&actor, &request, at).unwrap();
        (request, result.receipt.unwrap())
    };
    let history = [
        step(by_agent(&alice), "t-retain", Retain, Some("s-a"), None),
        step(person(&owner, false), "t-release", Release, None, None),
        step(
            by_agent(&alice_b),
            "t-complete",
            Complete,
            Some("owner-s"),
            None,
        ),
        step(by_agent(&alice), "t-reopen", Reopen, Some("s-a"), None),
        step(
            person(&admin, true),
            "t-supersede",
            Supersede,
            None,
            Some(&target.id),
        ),
    ];
    let actors = [
        by_agent(&alice),
        person(&owner, false),
        by_agent(&alice_b),
        by_agent(&alice),
        person(&admin, true),
    ];
    let state = serde_json::to_value(current_item(&*store, &alice_b, &item.id, at + 1)).unwrap();
    let columns = db.reservation(&item.id);
    let seq = store.latest_event_seq().unwrap();
    let usage = db.work_usage_and_aggregate();
    for ((request, receipt), actor) in history.iter().zip(actors) {
        let replay = store.work_transition(&actor, request, at + 2).unwrap();
        let replayed = replay.receipt.unwrap();
        assert!(replayed.replayed, "{}", request.operation_id);
        assert_eq!(replayed.event_seq, receipt.event_seq);
        assert_eq!(replayed.generation, receipt.generation);
        assert_eq!(serde_json::to_value(&replay.item).unwrap(), state);
        // Changed fields under the same operation are refused.
        let mut changed = request.clone();
        changed.reason = "different reason".into();
        assert_invalid(
            store.work_transition(&actor, &changed, at + 3),
            "operation_id was already used for another request",
        );
    }
    assert_eq!(db.reservation(&item.id), columns);
    assert_eq!(columns, (Some("superseded".into()), None));
    assert_eq!(store.latest_event_seq().unwrap(), seq);
    assert_eq!(db.work_usage_and_aggregate(), usage);
    assert_eq!(
        db.work_usage_and_aggregate().0,
        db.work_usage_and_aggregate().1
    );
}

fn reopen_conflicts_on_overlap_and_fences_the_old_session(db: &Db) {
    use WorkTransitionKind::Reopen;
    let store = db.open();
    let now = unix_now();
    let alice = agent(&*store, 1);
    let alice_b = another_token(&*store, &alice, "agent-b");
    let bob = agent(&*store, 2);
    let item = start_in(&*store, &alice, "a", "s-a", "src/**", now);
    let finished = lead_checkpoint(
        &*store,
        &alice,
        &item,
        "fin",
        "s-a",
        CheckpointKind::Finish,
        now + 1,
    )
    .item;
    let blocker = start_in(&*store, &bob, "b", "s-b", "src/**", now + 2);

    let before = serde_json::to_value(&finished).unwrap();
    let seq = store.latest_event_seq().unwrap();
    let request = transition_request(&finished, "reopen", Reopen, Some("s-a"));
    let result = store
        .work_transition(&by_agent(&alice), &request, now + 3)
        .unwrap();
    assert!(result.receipt.is_none());
    assert_eq!(
        result.conflict.as_ref().map(|c| c.kind),
        Some(WorkConflictKind::ScopeOverlap)
    );
    assert_eq!(result.conflict.unwrap().overlaps[0].item.id, blocker.id);
    assert_eq!(serde_json::to_value(&result.item).unwrap(), before);
    assert_eq!(db.reservation(&item.id), (Some("completed".into()), None));
    assert_eq!(store.latest_event_seq().unwrap(), seq);

    // Once the overlap is gone, the owner's other agent reopens and leads under a new session.
    lead_checkpoint(
        &*store,
        &bob,
        &blocker,
        "fin-b",
        "s-b",
        CheckpointKind::Finish,
        now + 4,
    );
    let request = transition_request(&finished, "reopen-2", Reopen, Some("s-new"));
    let reopened = store
        .work_transition(&by_agent(&alice_b), &request, now + 5)
        .unwrap();
    assert!(reopened.conflict.is_none());
    let item_after = reopened.item;
    assert_eq!(
        (item_after.state, item_after.generation),
        (WorkState::Active, 2)
    );
    assert_eq!(item_after.lead_session, "s-new");
    assert_eq!(item_after.lead_token, alice_b.token);
    assert_eq!(db.reservation(&item.id), (None, Some("admitted".into())));
    let roles: Vec<_> = item_after
        .participants
        .iter()
        .map(|p| (p.session.as_str(), p.role))
        .collect();
    assert!(roles.contains(&("s-new", WorkRole::Lead)));
    assert!(roles.contains(&("s-a", WorkRole::Helper)));
    // The previous session is fenced.
    let mut stale = checkpoint(&item.id, "old", 1, "s-a", CheckpointKind::Progress, "old");
    stale.expected_updated_seq = Some(item_after.updated_seq);
    assert!(matches!(
        store.work_checkpoint(&alice.token, &stale, now + 6),
        Err(HubError::Invalid(_))
    ));

    // The reopened Work is ordinary: once its lease lapsed, another agent takes it over.
    let later = now + LEASE + 20;
    tick(&*store, now, later);
    let taken = store
        .work_start(&bob.token, &takeover(&item.id, "s-bob", Some(2)), later)
        .unwrap();
    assert!(taken.took_over);
    assert_eq!(taken.item.generation, 3);
    assert_eq!(db.reservation(&item.id), (None, Some("admitted".into())));
}

fn responded_request_stays_responded_after_release(db: &Db) {
    use WorkTransitionKind::Release;
    let store = db.open();
    let now = unix_now();
    let alice = agent(&*store, 1);
    let bob = agent(&*store, 2);
    let carol = agent(&*store, 3);
    let item = start_in(&*store, &alice, "a", "s-a", "src/**", now);
    let ask = |who: &Agent, key: &str, at: i64| {
        let mut start = native_start(key, key, &format!("s-{key}"));
        start.scope = vec!["src/**".into()];
        store
            .work_start(&who.token, &start, at)
            .unwrap()
            .coordination_request
            .unwrap()
    };
    let first = ask(&bob, "b", now + 1);
    let second = ask(&carol, "c", now + 2);
    let ack = CoordinationAck {
        request_id: first.id.clone(),
        target_work_id: Some(item.id.clone()),
        session: "s-a".into(),
        expected_updated_seq: first.updated_seq,
    };
    let acked = store.coordination_ack(&alice.token, &ack, now + 3).unwrap();
    let responded = store
        .coordination_reply(
            &alice.token,
            &CoordinationReply {
                request_id: first.id.clone(),
                operation_id: "reply".into(),
                target_work_id: item.id.clone(),
                session: "s-a".into(),
                expected_updated_seq: acked.updated_seq,
                kind: CoordinationResponseKind::Join,
                note: None,
                via: agentscm::hub::handoff::Via::Agent,
            },
            now + 4,
        )
        .unwrap();
    assert_eq!(responded.status, CoordinationStatus::Responded);

    let current = current_item(&*store, &alice, &item.id, now + 5);
    store
        .work_transition(
            &by_agent(&alice),
            &transition_request(&current, "rel", Release, Some("s-a")),
            now + 5,
        )
        .unwrap();
    let statuses = db.rows(&format!(
        "SELECT id, status FROM hub_coordination_requests WHERE id IN ('{}', '{}') ORDER BY id",
        first.id, second.id
    ));
    let mut expected = vec![
        vec![first.id.clone(), "responded".to_owned()],
        vec![second.id.clone(), "pending".to_owned()],
    ];
    expected.sort();
    assert_eq!(statuses, expected);
    let released: Vec<_> = store
        .events_after(0, 1000)
        .unwrap()
        .into_iter()
        .filter(|event| event.kind == "coordination_target_released")
        .map(|event| event.subject)
        .collect();
    let mut want = vec![first.id.clone(), second.id.clone()];
    want.sort();
    let mut got = released;
    got.sort();
    assert_eq!(got, want);
    // The requester can still withdraw its own request afterwards; nothing was closed for it.
    let refreshed = store
        .coordination_list(&bob.token, &Default::default(), now + 6)
        .unwrap();
    let mine = refreshed
        .requests
        .iter()
        .find(|r| r.id == first.id)
        .unwrap();
    store
        .coordination_withdraw(
            &bob.token,
            &CoordinationWithdraw {
                request_id: first.id.clone(),
                operation_id: "withdraw".into(),
                session: "s-b".into(),
                expected_updated_seq: mine.updated_seq,
                target: None,
                via_hub: false,
            },
            now + 7,
        )
        .unwrap();
}

fn release_succeeds_at_payload_quota_and_retain_refuses(db: &Db) {
    use WorkTransitionKind::{Release, Retain};
    let store = db.open();
    let now = unix_now();
    let alice = agent(&*store, 1);
    let item = start_in(&*store, &alice, "a", "s-a", "src/**", now);
    db.sql(&format!(
        "UPDATE hub_work_usage SET charged_bytes = {WORK_PAYLOAD_CAPACITY_BYTES}
         WHERE repository = '{REPO}'"
    ));
    let seq = store.latest_event_seq().unwrap();
    assert!(matches!(
        store.work_transition(
            &by_agent(&alice),
            &transition_request(&item, "ret", Retain, Some("s-a")),
            now + 1
        ),
        Err(HubError::WorkCapacity(c)) if c.limit == CapacityLimit::WorkStorage
    ));
    assert_eq!(store.latest_event_seq().unwrap(), seq);
    assert_eq!(db.reservation(&item.id), (None, Some("admitted".into())));

    let released = store
        .work_transition(
            &by_agent(&alice),
            &transition_request(&item, "rel", Release, Some("s-a")),
            now + 2,
        )
        .unwrap();
    assert_eq!(released.item.state, WorkState::Paused);
    assert_eq!(db.reservation(&item.id), (None, None));
    let charged: i64 = db.rows(&format!(
        "SELECT charged_bytes FROM hub_work_usage WHERE repository = '{REPO}'"
    ))[0][0]
        .parse()
        .unwrap();
    assert!(
        charged > WORK_PAYLOAD_CAPACITY_BYTES,
        "cleanup is recorded, not refused"
    );
}

fn compaction_runs_after_transition_events_exist(db: &Db) {
    use WorkTransitionKind::{Abandon, Release};
    let store = db.open();
    let now = unix_now();
    let old = now - 3 * 86_400;
    let alice = agent(&*store, 1);
    let bob = agent(&*store, 2);
    let item = start_in(&*store, &alice, "a", "s-a", "src/**", old);
    let mut start = native_start("b", "b", "s-b");
    start.scope = vec!["src/**".into()];
    let request = store
        .work_start(&bob.token, &start, old + 1)
        .unwrap()
        .coordination_request
        .unwrap();
    let current = current_item(&*store, &alice, &item.id, old + 2);
    let released = store
        .work_transition(
            &by_agent(&alice),
            &transition_request(&current, "rel", Release, Some("s-a")),
            old + 2,
        )
        .unwrap()
        .item;
    let mut abandon = transition_request(&released, "abandon", Abandon, Some("s-a"));
    abandon.generation = released.generation;
    store
        .work_transition(&by_agent(&alice), &abandon, old + 3)
        .unwrap();
    let mine = store
        .coordination_list(&bob.token, &Default::default(), old + 4)
        .unwrap();
    let mine = mine.requests.iter().find(|r| r.id == request.id).unwrap();
    store
        .coordination_withdraw(
            &bob.token,
            &CoordinationWithdraw {
                request_id: request.id.clone(),
                operation_id: "withdraw".into(),
                session: "s-b".into(),
                expected_updated_seq: mine.updated_seq,
                target: None,
                via_hub: false,
            },
            old + 5,
        )
        .unwrap();
    assert_eq!(transition_events(&*store).len(), 2);
    let scope = WorkRepositoryScope {
        provider: "github".into(),
        host: "github.com".into(),
        repository: REPO.into(),
        repository_id: Some(REPOSITORY_ID),
    };
    let compacted = store.work_compact(&scope, now - 86_400, true, now).unwrap();
    assert!(compacted.applied);
    assert!(compacted.deleted_operations >= 1);
    assert_eq!(compacted.deleted_coordination_requests, 1);
    assert!(
        store
            .events_after(0, 1000)
            .unwrap()
            .iter()
            .all(|event| event.kind != "coordination_target_released")
    );
}

fn abandoned_work_is_not_a_reported_completion(db: &Db) {
    use WorkTransitionKind::{Abandon, Complete, Supersede};
    let store = db.open();
    let now = unix_now();
    let alice = agent(&*store, 1);
    let alice_b = another_token(&*store, &alice, "agent-b");
    let admin_agent = agent(&*store, 9);
    let owner = user_of(&*store, &alice);
    let admin = user_of(&*store, &admin_agent);
    let reader = WorkLifecycleReader::Token(&alice.token);
    let make =
        |key: &str, scope: &str| start_in(&*store, &alice, key, &format!("s-{key}"), scope, now);
    let finished = make("f", "f/**");
    let abandoned = make("a", "a/**");
    let superseded = make("s", "s/**");
    let completed = make("c", "c/**");
    lead_checkpoint(
        &*store,
        &alice,
        &finished,
        "fin",
        "s-f",
        CheckpointKind::Finish,
        now + 1,
    );
    store
        .work_transition(
            &person(&owner, false),
            &transition_request(&abandoned, "x", Abandon, None),
            now + 1,
        )
        .unwrap();
    let mut request = transition_request(&superseded, "x", Supersede, None);
    request.superseded_by = Some(completed.id.clone());
    store
        .work_transition(&person(&admin, true), &request, now + 1)
        .unwrap();
    store
        .work_transition(
            &by_agent(&alice_b),
            &transition_request(&completed, "x", Complete, Some("owner-s")),
            now + 1,
        )
        .unwrap();
    let expect = [
        (
            &finished,
            Some(TerminalOutcome::Completed),
            OutcomeProvenance::AgentReport,
            true,
        ),
        (
            &abandoned,
            Some(TerminalOutcome::Abandoned),
            OutcomeProvenance::Person,
            false,
        ),
        (
            &superseded,
            Some(TerminalOutcome::Superseded),
            OutcomeProvenance::Admin,
            false,
        ),
        (
            &completed,
            Some(TerminalOutcome::Completed),
            OutcomeProvenance::AgentReport,
            true,
        ),
    ];
    for (item, outcome, provenance, reported) in expect {
        let view = lifecycle(&*store, &reader, &item.id, now + 2);
        assert_eq!(
            (view.0, view.1),
            (outcome, Some(provenance)),
            "{}",
            item.title
        );
        assert_eq!(view.2, None);
        let insight = store
            .work_insight(&alice.token, &item.id, now + 2)
            .unwrap()
            .unwrap();
        assert_eq!(insight.state, WorkState::Finished);
        assert_eq!(insight.reported_finished, reported, "{}", item.title);
    }
    // Recent outcomes list Work ended by any transition once, as a completion from the finished
    // snapshot; a finish checkpoint followed by a complete transition is not counted twice.
    let overview = store
        .work_overview(&alice.token, &Default::default(), now + 3)
        .unwrap();
    let ids: Vec<_> = overview
        .recent
        .iter()
        .map(|outcome| outcome.item.id.as_str())
        .collect();
    for item in [&finished, &abandoned, &superseded, &completed] {
        assert_eq!(
            ids.iter().filter(|id| **id == item.id).count(),
            1,
            "{}",
            item.title
        );
    }
    for outcome in &overview.recent {
        if outcome.item.id == completed.id {
            assert_eq!(
                outcome.kind,
                agentscm::hub::work::WorkOutcomeKind::Completion
            );
            assert_eq!(outcome.item.state, WorkState::Finished);
        }
    }
    // A decision checkpoint followed by a complete transition lists one completion, not the
    // decision row with its active snapshot.
    let decided = make("d", "d/**");
    let decision = lead_checkpoint(
        &*store,
        &alice,
        &decided,
        "dec",
        "s-d",
        CheckpointKind::Decision,
        now + 4,
    );
    store
        .work_transition(
            &by_agent(&alice),
            &transition_request(&decision.item, "x", Complete, Some("s-d")),
            now + 5,
        )
        .unwrap();
    let overview = store
        .work_overview(&alice.token, &Default::default(), now + 6)
        .unwrap();
    let rows: Vec<_> = overview
        .recent
        .iter()
        .filter(|outcome| outcome.item.id == decided.id)
        .collect();
    assert_eq!(rows.len(), 1);
    assert_eq!(
        rows[0].kind,
        agentscm::hub::work::WorkOutcomeKind::Completion
    );
    assert_eq!(rows[0].item.state, WorkState::Finished);
    let ended = rows[0].ended.as_ref().expect("transition row has ended");
    assert_eq!(ended.transition, "complete");
    assert_eq!(ended.reason, "synthetic reason");
    assert_eq!(
        ended.by.kind,
        agentscm::hub::work_lifecycle::WorkActorKind::Lead
    );
    assert_eq!(ended.by.via, agentscm::hub::work_lifecycle::ActorVia::Agent);
    assert_eq!(ended.superseded_by, None);
    assert!(
        overview
            .recent
            .iter()
            .filter(|outcome| outcome.item.id == finished.id)
            .all(|outcome| outcome.ended.is_none())
    );
    let sup = overview
        .recent
        .iter()
        .find(|o| o.item.id == superseded.id)
        .unwrap();
    assert_eq!(
        sup.ended.as_ref().unwrap().superseded_by.as_deref(),
        Some(completed.id.as_str())
    );
    // Pre-existing finished Work with no stored outcome reads as unstated.
    db.sql(&format!(
        "UPDATE hub_work_items SET terminal_outcome = NULL WHERE id = '{}'",
        abandoned.id
    ));
    let view = lifecycle(&*store, &reader, &abandoned.id, now + 4);
    assert_eq!((view.0, view.1), (None, None));
}

// Corrections: plain announces, session-less intents, resume, finish cleanup, reopen retarget.

fn plain_intent(n: u128, path: &str) -> agentscm::hub::intent::NewIntent {
    agentscm::hub::intent::NewIntent {
        id: format!("int_{n:032x}"),
        task: "edit".into(),
        branch: None,
        base: None,
        machine: None,
        paths: vec![path.into()],
        ttl_seconds: 600,
    }
}

fn live_intents(db: &Db) -> usize {
    db.rows("SELECT id FROM hub_intents WHERE released_at IS NULL")
        .len()
}

fn plain_intent_of(id: u128, paths: &[&str]) -> agentscm::hub::intent::NewIntent {
    let mut intent = plain_intent(id, paths[0]);
    intent.paths = paths.iter().map(|path| (*path).to_owned()).collect();
    intent
}

fn plain_announce_follows_the_token_sessions_after_release_abandon_and_takeover(db: &Db) {
    use WorkTransitionKind::{Abandon, Release};
    let store = db.open();
    let now = unix_now();
    let alice = agent(&*store, 1);
    let bob = agent(&*store, 2);
    let carol = agent(&*store, 3);
    let dave = agent(&*store, 4);
    let erin = agent(&*store, 5);
    let owner = user_of(&*store, &alice);
    let announce_plain = |who: &Agent, id: u128, paths: &[&str], at: i64| {
        store.announce_intent(&who.token, &plain_intent_of(id, paths), at)
    };

    // One id, the paths accumulated, as the `agentscm intent` hook posts them. A path outside
    // every linked scope is advisory and never refused.
    let item = start_in(&*store, &alice, "a", "abc", "src/**", now);
    announce_plain(&alice, 1, &["src/a.rs", "lib/x.rs"], now + 1).unwrap();

    // After a release the path inside the Work's scope is refused and names the Work; the path
    // outside it passes in the same token.
    let released = store
        .work_transition(
            &by_agent(&alice),
            &transition_request(&item, "rel", Release, Some("abc")),
            now + 2,
        )
        .unwrap()
        .item;
    let refused = announce_plain(&alice, 2, &["lib/x.rs", "src/b.rs"], now + 3);
    match refused {
        Err(HubError::Invalid(message)) => {
            assert!(message.contains(&item.id), "{message}");
            assert!(message.contains("Resume"), "{message}");
        }
        other => panic!("expected a refusal naming the Work, got {other:?}"),
    }
    announce_plain(&alice, 2, &["lib/x.rs"], now + 3).unwrap();
    // An explicit resume is a fresh admission and the refused path works again.
    let resumed = store
        .work_start(
            &alice.token,
            &takeover(&item.id, "abc", Some(released.generation)),
            now + 4,
        )
        .unwrap();
    assert!(resumed.took_over);
    announce_plain(&alice, 2, &["lib/x.rs", "src/b.rs"], now + 5).unwrap();

    // Abandoned Work is finished: its token may post advisory intents, but a released id stays
    // released, so the client mints a new one.
    let current = current_item(&*store, &alice, &item.id, now + 6);
    store
        .work_transition(
            &person(&owner, false),
            &transition_request(&current, "ab", Abandon, None),
            now + 6,
        )
        .unwrap();
    assert!(matches!(
        announce_plain(&alice, 2, &["src/c.rs"], now + 7),
        Err(HubError::Invalid(message)) if message == "intent was released"
    ));
    announce_plain(&alice, 3, &["src/c.rs"], now + 7).unwrap();

    // A takeover after the lease lapsed fences the previous holder's named session.
    let other = start_in(&*store, &bob, "b", "abc", "lib/**", now + 8);
    announce_plain(&bob, 6, &["lib/a.rs"], now + 9).unwrap();
    let later = now + 8 + LEASE + 10;
    tick(&*store, now, later);
    assert!(
        store
            .work_start(&carol.token, &takeover(&other.id, "abc", Some(1)), later)
            .unwrap()
            .took_over
    );
    assert!(matches!(
        announce_plain(&bob, 6, &["lib/a.rs", "lib/b.rs"], later + 1),
        Err(HubError::Invalid(_))
    ));
    announce_plain(&carol, 8, &["lib/c.rs"], later + 1).unwrap();

    // A token never linked to Work keeps the legacy advice.
    announce_plain(&dave, 9, &["lib/c.rs"], later + 2).unwrap();

    // Work with an empty scope refuses nothing.
    let mut empty = native_start("e", "e", "s-e");
    empty.scope = vec![];
    store.work_start(&erin.token, &empty, later + 3).unwrap();
    announce_plain(&erin, 10, &["src/z.rs"], later + 4).unwrap();
}

/// A path covered by Work on which a session is eligible passes even when other, fenced Work of
/// the same token also covers it.
fn plain_announce_allows_paths_covered_by_eligible_work(db: &Db) {
    use WorkTransitionKind::Release;
    let store = db.open();
    let now = unix_now();
    let alice = agent(&*store, 1);
    let first = start_in(&*store, &alice, "a", "s-a", "src/**", now);
    store
        .work_transition(
            &by_agent(&alice),
            &transition_request(&first, "rel", Release, Some("s-a")),
            now + 1,
        )
        .unwrap();
    // The released Work holds nothing, so the second may take a part of its scope.
    let second = start_in(&*store, &alice, "b", "s-b", "src/sub/**", now + 2);
    // `src/sub/` is still held by the second Work; the rest of `src/` is fenced.
    store
        .announce_intent(&alice.token, &plain_intent(1, "src/sub/a.rs"), now + 3)
        .unwrap();
    assert!(matches!(
        store.announce_intent(&alice.token, &plain_intent(2, "src/a.rs"), now + 3),
        Err(HubError::Invalid(_))
    ));
    assert_eq!(second.state, WorkState::Active);
}

/// A broader directory key against a narrower fenced scope, and the reverse, is refused; an
/// unrelated directory is not.
fn plain_announce_refuses_a_directory_that_overlaps_fenced_work_either_way(db: &Db) {
    use WorkTransitionKind::Release;
    let store = db.open();
    let now = unix_now();
    let alice = agent(&*store, 1);
    let narrow = start_in(&*store, &alice, "a", "s-a", "src/sub/**", now);
    store
        .work_transition(
            &by_agent(&alice),
            &transition_request(&narrow, "rel", Release, Some("s-a")),
            now + 1,
        )
        .unwrap();
    match store.announce_intent(&alice.token, &plain_intent(1, "src/**"), now + 2) {
        Err(HubError::Invalid(message)) => assert!(message.contains(&narrow.id), "{message}"),
        other => panic!("expected a refusal naming the Work, got {other:?}"),
    }
    store
        .announce_intent(&alice.token, &plain_intent(2, "lib/**"), now + 2)
        .unwrap();

    let bob = agent(&*store, 2);
    let broad = start_in(&*store, &bob, "b", "s-b", "docs/**", now + 3);
    store
        .work_transition(
            &by_agent(&bob),
            &transition_request(&broad, "rel-b", Release, Some("s-b")),
            now + 4,
        )
        .unwrap();
    match store.announce_intent(&bob.token, &plain_intent(3, "docs/deep/**"), now + 5) {
        Err(HubError::Invalid(message)) => assert!(message.contains(&broad.id), "{message}"),
        other => panic!("expected a refusal naming the Work, got {other:?}"),
    }
}

/// More unfinished Work for one token than the guard reads fails closed instead of letting a
/// partial read allow the announce; at the bound it still decides normally.
fn plain_announce_fails_closed_beyond_the_token_work_bound(db: &Db) {
    let store = db.open();
    let now = unix_now();
    let alice = agent(&*store, 1);
    for index in 0..100 {
        let key = format!("k{index}");
        start_in(&*store, &alice, &key, &key, &format!("d{index}/**"), now);
    }
    store
        .announce_intent(&alice.token, &plain_intent(1, "d1/a.rs"), now + 1)
        .unwrap();
    start_in(&*store, &alice, "k100", "k100", "d100/**", now);
    assert!(matches!(
        store.announce_intent(&alice.token, &plain_intent(2, "d1/a.rs"), now + 2),
        Err(HubError::WorkOverlapIncomplete)
    ));
}

/// Cleanup is not bounded by the announce guard: a token with more empty-scope Work than the
/// guard reads can still release, abandon and finish its Work.
fn cleanup_succeeds_for_a_token_with_more_work_than_the_announce_bound(db: &Db) {
    use WorkTransitionKind::{Abandon, Release};
    let store = db.open();
    let now = unix_now();
    let alice = agent(&*store, 1);
    let mut items = Vec::new();
    for index in 0..102 {
        let key = format!("k{index}");
        let mut start = native_start(&key, &key, &key);
        start.scope = vec![];
        items.push(store.work_start(&alice.token, &start, now).unwrap().item);
    }
    store
        .work_transition(
            &by_agent(&alice),
            &transition_request(&items[0], "rel", Release, Some("k0")),
            now + 1,
        )
        .unwrap();
    store
        .work_transition(
            &by_agent(&alice),
            &transition_request(&items[1], "ab", Abandon, Some("k1")),
            now + 2,
        )
        .unwrap();
    let finished = lead_checkpoint(
        &*store,
        &alice,
        &items[2],
        "fin",
        "k2",
        CheckpointKind::Finish,
        now + 3,
    );
    assert_eq!(finished.item.state, WorkState::Finished);
}

/// Another token's pile of empty-scope Work, joined as a helper to the victim's Work, cannot make
/// the victim's owner's release fail.
fn a_helper_with_many_work_cannot_block_the_owners_release(db: &Db) {
    use WorkTransitionKind::Release;
    let store = db.open();
    let now = unix_now();
    let alice = agent(&*store, 1);
    let bob = agent(&*store, 2);
    let victim = start_in(&*store, &alice, "v", "s-v", "src/**", now);
    for index in 0..102 {
        let key = format!("k{index}");
        let mut start = native_start(&key, &key, &key);
        start.scope = vec![];
        store.work_start(&bob.token, &start, now).unwrap();
    }
    let mut helper = join(&victim.id, "s-h", WorkRole::Helper);
    helper.scope = vec![];
    store.work_start(&bob.token, &helper, now + 1).unwrap();
    announce(&*store, &bob, "s-h", "src/h.rs", now + 2);
    let current = current_item(&*store, &alice, &victim.id, now + 3);
    store
        .work_transition(
            &by_agent(&alice),
            &transition_request(&current, "rel", Release, Some("s-v")),
            now + 4,
        )
        .unwrap();
}

/// Empty-scope Work cannot affect an announce, so it does not count toward the guard's bound.
fn plain_announce_ignores_empty_scope_work_in_its_bound(db: &Db) {
    let store = db.open();
    let now = unix_now();
    let alice = agent(&*store, 1);
    for index in 0..101 {
        let key = format!("k{index}");
        let mut start = native_start(&key, &key, &key);
        start.scope = vec![];
        store.work_start(&alice.token, &start, now).unwrap();
    }
    store
        .announce_intent(&alice.token, &plain_intent(1, "d1/a.rs"), now + 1)
        .unwrap();
}

/// A lead, a helper with a session intent, a session-less intent of each of them and of an
/// unrelated token, all inside `src/**`.
fn hold_scope_with_three_kinds_of_intent(
    db: &Db,
    store: &dyn Store,
    now: i64,
) -> (Agent, agentscm::hub::work::WorkItem) {
    let alice = agent(store, 1);
    let bob = agent(store, 2);
    let carol = agent(store, 3);
    let item = start_in(store, &alice, "a", "s-a", "src/**", now);
    store
        .work_start(
            &bob.token,
            &join(&item.id, "s-h", WorkRole::Helper),
            now + 1,
        )
        .unwrap();
    announce(store, &alice, "s-a", "src/q.rs", now + 2);
    announce(store, &bob, "s-h", "src/h.rs", now + 2);
    store
        .announce_intent(&alice.token, &plain_intent(1, "src/p.rs"), now + 3)
        .unwrap();
    store
        .announce_intent(&bob.token, &plain_intent(2, "src/p2.rs"), now + 3)
        .unwrap();
    // An unrelated token is not tied to the Work and is left alone.
    store
        .announce_intent(&carol.token, &plain_intent(3, "src/c.rs"), now + 3)
        .unwrap();
    assert_eq!(live_intents(db), 5);
    let item = current_item(store, &alice, &item.id, now + 4);
    (alice, item)
}

fn release_frees_session_and_sessionless_intents_of_the_scope(db: &Db) {
    use WorkTransitionKind::Release;
    let store = db.open();
    let now = unix_now();
    let (alice, item) = hold_scope_with_three_kinds_of_intent(db, &*store, now);
    store
        .work_transition(
            &by_agent(&alice),
            &transition_request(&item, "rel", Release, Some("s-a")),
            now + 5,
        )
        .unwrap();
    // Only the unrelated token's intent is left.
    assert_eq!(live_intents(db), 1);
}

fn live_keys(db: &Db) -> Vec<String> {
    db.rows("SELECT key FROM hub_intent_paths ORDER BY key")
        .into_iter()
        .map(|row| row[0].clone())
        .collect()
}

fn ending_work_spares_a_tokens_live_intents_elsewhere(db: &Db) {
    use WorkTransitionKind::Abandon;
    let store = db.open();
    let now = unix_now();
    let alice = agent(&*store, 1);
    let bob = agent(&*store, 2);
    let owner = user_of(&*store, &alice);
    // Bob helped on A and leads B now. Ending A takes only what belongs to A.
    let a = start_in(&*store, &alice, "a", "s-a", "src/**", now);
    store
        .work_start(&bob.token, &join(&a.id, "s-h", WorkRole::Helper), now + 1)
        .unwrap();
    start_in(&*store, &bob, "b", "s-b", "lib/**", now + 2);
    announce(&*store, &bob, "s-h", "src/h.rs", now + 3);
    announce(&*store, &bob, "s-b", "lib/b.rs", now + 3);
    for (id, path) in [(1, "lib/p.rs"), (2, "doc/q.rs"), (3, "src/p.rs")] {
        store
            .announce_intent(&bob.token, &plain_intent(id, path), now + 4)
            .unwrap();
    }
    let a = current_item(&*store, &alice, &a.id, now + 5);
    store
        .work_transition(
            &person(&owner, false),
            &transition_request(&a, "ab", Abandon, None),
            now + 5,
        )
        .unwrap();
    // A's helper session intent and the session-less intent inside A's scope are freed; B's
    // intent and the non-overlapping session-less intents stay.
    assert_eq!(live_keys(db), ["doc/q.rs", "lib/b.rs", "lib/p.rs"]);
}

fn ending_work_spares_a_session_that_is_eligible_on_other_work(db: &Db) {
    use WorkTransitionKind::Abandon;
    let store = db.open();
    let now = unix_now();
    let alice = agent(&*store, 1);
    let bob = agent(&*store, 2);
    let owner = user_of(&*store, &alice);
    let a = start_in(&*store, &alice, "a", "s-a", "src/**", now);
    store
        .work_start(&bob.token, &join(&a.id, "s-h", WorkRole::Helper), now + 1)
        .unwrap();
    announce(&*store, &bob, "s-h", "src/h.rs", now + 2);
    // The same session now leads other Work, so it is not ended with A.
    start_in(&*store, &bob, "b", "s-h", "lib/**", now + 3);
    let a = current_item(&*store, &alice, &a.id, now + 4);
    store
        .work_transition(
            &person(&owner, false),
            &transition_request(&a, "ab", Abandon, None),
            now + 4,
        )
        .unwrap();
    assert_eq!(live_keys(db), ["src/h.rs"]);
}

fn ending_work_that_held_nothing_leaves_sessionless_intents(db: &Db) {
    use WorkTransitionKind::Abandon;
    let store = db.open();
    let now = unix_now();
    let alice = agent(&*store, 1);
    let owner = user_of(&*store, &alice);
    let a = start_in(&*store, &alice, "a", "s-a", "src/**", now);
    store
        .announce_intent(&alice.token, &plain_intent(1, "src/x.rs"), now + 1)
        .unwrap();
    db.sql("UPDATE hub_work_items SET reservation_basis = NULL");
    let a = current_item(&*store, &alice, &a.id, now + 2);
    store
        .work_transition(
            &person(&owner, false),
            &transition_request(&a, "ab", Abandon, None),
            now + 2,
        )
        .unwrap();
    assert_eq!(live_keys(db), ["src/x.rs"]);
}

fn finish_checkpoint_cleans_up_like_a_terminal_transition(db: &Db) {
    let store = db.open();
    let now = unix_now();
    let (alice, item) = hold_scope_with_three_kinds_of_intent(db, &*store, now);
    let dave = agent(&*store, 4);
    let erin = agent(&*store, 5);
    // One request stays pending; one is answered. Neither may change status.
    let ask = |who: &Agent, key: &str, at: i64| {
        let mut start = native_start(key, key, &format!("s-{key}"));
        start.scope = vec!["src/**".into()];
        store
            .work_start(&who.token, &start, at)
            .unwrap()
            .coordination_request
            .unwrap()
    };
    let pending = ask(&dave, "d", now + 5);
    let answered = ask(&erin, "e", now + 6);
    let acked = store
        .coordination_ack(
            &alice.token,
            &CoordinationAck {
                request_id: answered.id.clone(),
                target_work_id: Some(item.id.clone()),
                session: "s-a".into(),
                expected_updated_seq: answered.updated_seq,
            },
            now + 7,
        )
        .unwrap();
    store
        .coordination_reply(
            &alice.token,
            &CoordinationReply {
                request_id: answered.id.clone(),
                operation_id: "reply".into(),
                target_work_id: item.id.clone(),
                session: "s-a".into(),
                expected_updated_seq: acked.updated_seq,
                kind: CoordinationResponseKind::Join,
                note: None,
                via: agentscm::hub::handoff::Via::Agent,
            },
            now + 8,
        )
        .unwrap();
    let current = current_item(&*store, &alice, &item.id, now + 9);
    let finished = lead_checkpoint(
        &*store,
        &alice,
        &current,
        "fin",
        "s-a",
        CheckpointKind::Finish,
        now + 9,
    );
    assert_eq!(finished.item.state, WorkState::Finished);
    assert_eq!(db.reservation(&item.id).1, None);
    // The lead's, the helper's and both session-less intents are gone; the stranger's stays.
    assert_eq!(live_intents(db), 1);
    let statuses = db.rows(&format!(
        "SELECT id, status FROM hub_coordination_requests WHERE id IN ('{}', '{}') ORDER BY id",
        pending.id, answered.id
    ));
    let mut expected = vec![
        vec![pending.id.clone(), "pending".to_owned()],
        vec![answered.id.clone(), "responded".to_owned()],
    ];
    expected.sort();
    assert_eq!(statuses, expected);
    let mut released: Vec<_> = store
        .events_after(0, 1000)
        .unwrap()
        .into_iter()
        .filter(|event| event.kind == "coordination_target_released")
        .map(|event| event.subject)
        .collect();
    released.sort();
    let mut want = vec![pending.id, answered.id];
    want.sort();
    assert_eq!(released, want);
    let event = store
        .events_after(0, 1000)
        .unwrap()
        .into_iter()
        .find(|event| event.kind == "coordination_target_released")
        .unwrap();
    let data: serde_json::Value = serde_json::from_str(&event.data).unwrap();
    assert_eq!(data["transition"], "finish");
    assert_eq!(data["actor"]["kind"], "lead");
}

/// An open request that targets Work of alice's, from bob, who declares an overlapping scope.
fn open_request_on(
    store: &dyn Store,
    bob: &Agent,
    scope: &str,
    now: i64,
) -> agentscm::hub::coordination::CoordinationRequest {
    let mut start = native_start("b", "b", "s-b");
    start.scope = vec![scope.into()];
    store
        .work_start(&bob.token, &start, now)
        .unwrap()
        .coordination_request
        .unwrap()
}

fn finish_at_payload_quota_succeeds_in_its_cleanup(db: &Db) {
    let store = db.open();
    let now = unix_now();
    let alice = agent(&*store, 1);
    let bob = agent(&*store, 2);
    let item = start_in(&*store, &alice, "a", "s-a", "src/**", now);
    open_request_on(&*store, &bob, "src/**", now + 1);
    let item = current_item(&*store, &alice, &item.id, now + 2);
    // Leave just enough room for the finish checkpoint itself; the cleanup then records its
    // request event past the quota and must not refuse the finish.
    let mut request = checkpoint(
        &item.id,
        "fin",
        item.generation,
        "s-a",
        CheckpointKind::Finish,
        "step",
    );
    request.expected_updated_seq = Some(item.updated_seq);
    let mut finished = None;
    for headroom in (25..40_000).step_by(25) {
        db.sql(&format!(
            "UPDATE hub_work_usage SET charged_bytes = {}
             WHERE repository = '{REPO}'",
            WORK_PAYLOAD_CAPACITY_BYTES - headroom
        ));
        match store.work_checkpoint(&alice.token, &request, now + 3) {
            Ok(result) => {
                finished = Some(result);
                break;
            }
            Err(HubError::WorkCapacity(c)) if c.limit == CapacityLimit::WorkStorage => {}
            Err(other) => panic!("unexpected {other:?}"),
        }
    }
    let finished = finished.expect("a finish fits some headroom");
    assert_eq!(finished.item.state, WorkState::Finished);
    let charged: i64 = db.rows(&format!(
        "SELECT charged_bytes FROM hub_work_usage WHERE repository = '{REPO}'"
    ))[0][0]
        .parse()
        .unwrap();
    assert!(
        charged > WORK_PAYLOAD_CAPACITY_BYTES,
        "cleanup is recorded past the quota, not refused"
    );
    assert_eq!(
        store
            .events_after(0, 1000)
            .unwrap()
            .iter()
            .filter(|event| event.kind == "coordination_target_released")
            .count(),
        1
    );
}

fn an_exact_finish_replay_repeats_no_release(db: &Db) {
    let store = db.open();
    let now = unix_now();
    let alice = agent(&*store, 1);
    let bob = agent(&*store, 2);
    let item = start_in(&*store, &alice, "a", "s-a", "src/**", now);
    open_request_on(&*store, &bob, "src/**", now + 1);
    let item = current_item(&*store, &alice, &item.id, now + 2);
    let mut request = checkpoint(
        &item.id,
        "fin",
        item.generation,
        "s-a",
        CheckpointKind::Finish,
        "step",
    );
    request.expected_updated_seq = Some(item.updated_seq);
    let first = store
        .work_checkpoint(&alice.token, &request, now + 3)
        .unwrap();
    assert!(!first.receipt.replayed);
    let released = |store: &dyn Store| {
        store
            .events_after(0, 1000)
            .unwrap()
            .iter()
            .filter(|event| event.kind == "coordination_target_released")
            .count()
    };
    assert_eq!(released(&*store), 1);
    // An advisory intent posted after the finish is not taken by a replay of the finish.
    store
        .announce_intent(&alice.token, &plain_intent(1, "src/z.rs"), now + 4)
        .unwrap();
    let seq = store.latest_event_seq().unwrap();
    let replay = store
        .work_checkpoint(&alice.token, &request, now + 5)
        .unwrap();
    assert!(replay.receipt.replayed);
    assert_eq!(replay.receipt.event_seq, first.receipt.event_seq);
    assert_eq!(released(&*store), 1);
    assert_eq!(store.latest_event_seq().unwrap(), seq);
    assert_eq!(live_keys(db), ["src/z.rs"]);
}

fn released_work_resumes_in_any_state_and_refuses_reports_until_then(db: &Db) {
    use WorkTransitionKind::Release;
    let store = db.open();
    let now = unix_now();
    let alice = agent(&*store, 1);
    let alice_b = another_token(&*store, &alice, "agent-b");
    let bob = agent(&*store, 2);
    let report =
        |who: &Agent, item: &agentscm::hub::work::WorkItem, op: &str, session: &str, kind, at| {
            let mut request = checkpoint(&item.id, op, item.generation, session, kind, "report");
            request.expected_updated_seq = Some(item.updated_seq);
            store.work_checkpoint(&who.token, &request, at)
        };

    // Blocked work, released, resumes by its own lead although its claim is current.
    let first = start_in(&*store, &alice, "a", "s-a", "src/**", now);
    let blocked = lead_checkpoint(
        &*store,
        &alice,
        &first,
        "blk",
        "s-a",
        CheckpointKind::Blocker,
        now + 1,
    )
    .item;
    assert_eq!(blocked.state, WorkState::Blocked);
    let released = store
        .work_transition(
            &by_agent(&alice),
            &transition_request(&blocked, "rel", Release, Some("s-a")),
            now + 2,
        )
        .unwrap()
        .item;
    assert_eq!(released.state, WorkState::Blocked);
    assert_eq!(db.reservation(&first.id), (None, None));
    // Progress, a decision and a blocker are refused with nothing held, and change nothing.
    for (op, kind) in [
        ("prog", CheckpointKind::Progress),
        ("dec", CheckpointKind::Decision),
        ("blk2", CheckpointKind::Blocker),
    ] {
        let seq = store.latest_event_seq().unwrap();
        let refused = report(&alice, &released, op, "s-a", kind, now + 3);
        assert!(
            matches!(&refused, Err(HubError::Invalid(message)) if message.contains("resume")),
            "{op}: {refused:?}"
        );
        assert_eq!(store.latest_event_seq().unwrap(), seq);
    }
    let resumed = store
        .work_start(
            &alice.token,
            &takeover(&first.id, "s-a", Some(released.generation)),
            now + 4,
        )
        .unwrap();
    assert!(resumed.took_over, "{:?}", resumed.conflict);
    assert_eq!(resumed.item.state, WorkState::Active);
    assert_eq!(db.reservation(&first.id), (None, Some("admitted".into())));
    report(
        &alice,
        &resumed.item,
        "prog-2",
        "s-a",
        CheckpointKind::Progress,
        now + 5,
    )
    .unwrap();

    // Paused work, released: a pause, handover or finish is still accepted, a progress report is not.
    let second = start_in(&*store, &alice, "b", "s-b", "lib/**", now + 6);
    let paused = lead_checkpoint(
        &*store,
        &alice,
        &second,
        "pause",
        "s-b",
        CheckpointKind::Pause,
        now + 7,
    )
    .item;
    let released_paused = store
        .work_transition(
            &by_agent(&alice),
            &transition_request(&paused, "rel-b", Release, Some("s-b")),
            now + 8,
        )
        .unwrap()
        .item;
    assert_eq!(released_paused.state, WorkState::Paused);
    assert!(matches!(
        report(
            &alice,
            &released_paused,
            "late",
            "s-b",
            CheckpointKind::Progress,
            now + 9
        ),
        Err(HubError::Invalid(_))
    ));
    let handed = report(
        &alice,
        &released_paused,
        "hand",
        "s-b",
        CheckpointKind::Handover,
        now + 9,
    )
    .unwrap()
    .item;
    assert_eq!(handed.state, WorkState::Handover);
    assert_eq!(db.reservation(&second.id), (None, None));

    // A rotated token of the same owner resumes it under a new session; another owner may not.
    let taken_over = store
        .work_start(
            &bob.token,
            &takeover(&second.id, "s-bob", Some(handed.generation)),
            now + 10,
        )
        .unwrap();
    // Handover work stays open to any agent, as before.
    assert!(taken_over.took_over);
    let third = start_in(&*store, &alice, "c", "s-c", "docs/**", now + 11);
    let released_third = store
        .work_transition(
            &by_agent(&alice),
            &transition_request(&third, "rel-c", Release, Some("s-c")),
            now + 12,
        )
        .unwrap()
        .item;
    assert_eq!(released_third.state, WorkState::Paused);
    let refused = store
        .work_start(
            &bob.token,
            &takeover(&third.id, "s-bob2", Some(released_third.generation)),
            now + 13,
        )
        .unwrap();
    assert_eq!(
        refused.conflict.map(|conflict| conflict.kind),
        Some(WorkConflictKind::ClaimHeld)
    );
    let rotated = store
        .work_start(
            &alice_b.token,
            &takeover(&third.id, "s-rot", Some(released_third.generation)),
            now + 14,
        )
        .unwrap();
    assert!(rotated.took_over, "{:?}", rotated.conflict);
    assert_eq!(rotated.item.lead_session, "s-rot");
    assert_eq!(db.reservation(&third.id), (None, Some("admitted".into())));
}

fn resuming_released_work_conflicts_on_overlap_without_mutation(db: &Db) {
    use WorkTransitionKind::Release;
    let store = db.open();
    let now = unix_now();
    let alice = agent(&*store, 1);
    let bob = agent(&*store, 2);
    let item = start_in(&*store, &alice, "a", "s-a", "src/**", now);
    let released = store
        .work_transition(
            &by_agent(&alice),
            &transition_request(&item, "rel", Release, Some("s-a")),
            now + 1,
        )
        .unwrap()
        .item;
    start_in(&*store, &bob, "b", "s-b", "src/**", now + 2);
    let seq = store.latest_event_seq().unwrap();
    let blocked = store
        .work_start(
            &alice.token,
            &takeover(&item.id, "s-a", Some(released.generation)),
            now + 3,
        )
        .unwrap();
    assert_scope_overlap(&blocked);
    assert!(!blocked.took_over);
    assert_eq!(store.latest_event_seq().unwrap(), seq);
    let unchanged = current_item(&*store, &alice, &item.id, now + 4);
    assert_eq!(unchanged.generation, released.generation);
    assert_eq!(unchanged.updated_seq, released.updated_seq);
    assert_eq!(db.reservation(&item.id), (None, None));
}

fn resuming_released_work_retargets_requests_only_when_the_lead_changes(db: &Db) {
    use WorkTransitionKind::Release;
    let store = db.open();
    let now = unix_now();
    let alice = agent(&*store, 1);
    let alice_b = another_token(&*store, &alice, "agent-b");
    let bob = agent(&*store, 2);
    let item = start_in(&*store, &alice, "a", "s-a", "src/**", now);
    let request = open_request_on(&*store, &bob, "src/**", now + 1);
    let acked = store
        .coordination_ack(
            &alice.token,
            &CoordinationAck {
                request_id: request.id.clone(),
                target_work_id: Some(item.id.clone()),
                session: "s-a".into(),
                expected_updated_seq: request.updated_seq,
            },
            now + 2,
        )
        .unwrap();
    store
        .coordination_reply(
            &alice.token,
            &CoordinationReply {
                request_id: request.id.clone(),
                operation_id: "reply".into(),
                target_work_id: item.id.clone(),
                session: "s-a".into(),
                expected_updated_seq: acked.updated_seq,
                kind: CoordinationResponseKind::Join,
                note: None,
                via: agentscm::hub::handoff::Via::Agent,
            },
            now + 3,
        )
        .unwrap();
    let status = || {
        db.rows(&format!(
            "SELECT status FROM hub_coordination_requests WHERE id = '{}'",
            request.id
        ))[0][0]
            .clone()
    };
    let current = current_item(&*store, &alice, &item.id, now + 4);
    let released = store
        .work_transition(
            &by_agent(&alice),
            &transition_request(&current, "rel", Release, Some("s-a")),
            now + 4,
        )
        .unwrap()
        .item;
    assert_eq!(status(), "responded");

    // The same token and session resume: the lead is unchanged, so the answer stands.
    let resumed = store
        .work_start(
            &alice.token,
            &takeover(&item.id, "s-a", Some(released.generation)),
            now + 5,
        )
        .unwrap();
    assert!(resumed.took_over);
    assert_eq!(status(), "responded");
    assert_eq!(work_event_count(&*store, "coordination_retargeted"), 0);

    // Another token of the owner resumes after a second release: the lead changes and the
    // request waits for the new lead again.
    let current = current_item(&*store, &alice, &item.id, now + 6);
    let released = store
        .work_transition(
            &by_agent(&alice),
            &transition_request(&current, "rel-2", Release, Some("s-a")),
            now + 6,
        )
        .unwrap()
        .item;
    store
        .work_start(
            &alice_b.token,
            &takeover(&item.id, "s-b", Some(released.generation)),
            now + 7,
        )
        .unwrap();
    assert_eq!(status(), "pending");
    assert_eq!(work_event_count(&*store, "coordination_retargeted"), 1);
}

fn reopen_by_another_session_retargets_open_requests(db: &Db) {
    use WorkTransitionKind::{Complete, Reopen};
    let store = db.open();
    let now = unix_now();
    let alice = agent(&*store, 1);
    let alice_b = another_token(&*store, &alice, "agent-b");
    let bob = agent(&*store, 2);
    let item = start_in(&*store, &alice, "a", "s-a", "src/**", now);
    let mut start = native_start("b", "b", "s-b");
    start.scope = vec!["src/**".into()];
    let request = store
        .work_start(&bob.token, &start, now + 1)
        .unwrap()
        .coordination_request
        .unwrap();
    let acked = store
        .coordination_ack(
            &alice.token,
            &CoordinationAck {
                request_id: request.id.clone(),
                target_work_id: Some(item.id.clone()),
                session: "s-a".into(),
                expected_updated_seq: request.updated_seq,
            },
            now + 2,
        )
        .unwrap();
    store
        .coordination_reply(
            &alice.token,
            &CoordinationReply {
                request_id: request.id.clone(),
                operation_id: "reply".into(),
                target_work_id: item.id.clone(),
                session: "s-a".into(),
                expected_updated_seq: acked.updated_seq,
                kind: CoordinationResponseKind::Join,
                note: None,
                via: agentscm::hub::handoff::Via::Agent,
            },
            now + 3,
        )
        .unwrap();
    let status = || {
        db.rows(&format!(
            "SELECT status FROM hub_coordination_requests WHERE id = '{}'",
            request.id
        ))[0][0]
            .clone()
    };
    assert_eq!(status(), "responded");
    let current = current_item(&*store, &alice, &item.id, now + 4);
    store
        .work_transition(
            &by_agent(&alice),
            &transition_request(&current, "done", Complete, Some("s-a")),
            now + 4,
        )
        .unwrap();
    assert_eq!(status(), "responded", "ending the Work resolves nothing");
    assert_eq!(work_event_count(&*store, "coordination_retargeted"), 0);

    // The same session reopening keeps the same lead, so nothing is retargeted.
    let finished = current_item(&*store, &alice, &item.id, now + 5);
    store
        .work_transition(
            &by_agent(&alice),
            &transition_request(&finished, "same", Reopen, Some("s-a")),
            now + 5,
        )
        .unwrap();
    assert_eq!(status(), "responded");
    assert_eq!(work_event_count(&*store, "coordination_retargeted"), 0);

    // Another session reopening changes the lead: the answered request waits for it again.
    let current = current_item(&*store, &alice, &item.id, now + 6);
    store
        .work_transition(
            &by_agent(&alice),
            &transition_request(&current, "done-2", Complete, Some("s-a")),
            now + 6,
        )
        .unwrap();
    let finished = current_item(&*store, &alice, &item.id, now + 7);
    let reopened = store
        .work_transition(
            &by_agent(&alice_b),
            &transition_request(&finished, "other", Reopen, Some("s-new")),
            now + 7,
        )
        .unwrap();
    assert_eq!(reopened.item.lead_session, "s-new");
    assert_eq!(status(), "pending");
    assert_eq!(work_event_count(&*store, "coordination_retargeted"), 1);
}

fn pause_and_handover_keep_a_retained_reservation(db: &Db) {
    use WorkTransitionKind::Retain;
    let store = db.open();
    let now = unix_now();
    let alice = agent(&*store, 1);
    let item = start_in(&*store, &alice, "a", "s-a", "src/**", now);
    let retained = store
        .work_transition(
            &by_agent(&alice),
            &transition_request(&item, "keep", Retain, Some("s-a")),
            now + 1,
        )
        .unwrap()
        .item;
    assert_eq!(db.reservation(&item.id).1, Some("retained".into()));
    // The automatic pause that ends a session is an ordinary pause checkpoint.
    let paused = lead_checkpoint(
        &*store,
        &alice,
        &retained,
        "auto",
        "s-a",
        CheckpointKind::Pause,
        now + 2,
    )
    .item;
    assert_eq!(db.reservation(&item.id).1, Some("retained".into()));
    let handed = lead_checkpoint(
        &*store,
        &alice,
        &paused,
        "hand",
        "s-a",
        CheckpointKind::Handover,
        now + 3,
    )
    .item;
    assert_eq!(db.reservation(&item.id).1, Some("retained".into()));
    // An admitted reservation still becomes implicit, and a takeover admits it again.
    let other = start_in(&*store, &alice, "b", "s-b", "lib/**", now + 4);
    lead_checkpoint(
        &*store,
        &alice,
        &other,
        "pause-b",
        "s-b",
        CheckpointKind::Pause,
        now + 5,
    );
    assert_eq!(db.reservation(&other.id).1, Some("legacy_implicit".into()));
    let taken = store
        .work_start(
            &alice.token,
            &takeover(&item.id, "s-new", Some(handed.generation)),
            now + 6,
        )
        .unwrap();
    assert!(taken.took_over);
    assert_eq!(db.reservation(&item.id).1, Some("admitted".into()));
}

fn requests_that_propose_ended_work_are_marked_released_too(db: &Db) {
    use WorkTransitionKind::Release;
    let store = db.open();
    let now = unix_now();
    let alice = agent(&*store, 1);
    let bob = agent(&*store, 2);
    let first = start_in(&*store, &alice, "a", "s-a", "src/**", now);
    let mut start = native_start("b", "b", "s-b");
    start.scope = vec!["src/**".into()];
    let request = store
        .work_start(&bob.token, &start, now + 1)
        .unwrap()
        .coordination_request
        .unwrap();
    // Alice's release frees the scope; bob's retried start then creates the Work the request
    // proposed, and the request is bound to it.
    store
        .work_transition(
            &by_agent(&alice),
            &transition_request(&first, "rel", Release, Some("s-a")),
            now + 2,
        )
        .unwrap();
    let mine = store.work_start(&bob.token, &start, now + 3).unwrap();
    assert!(mine.created, "{:?}", mine.conflict);
    let bound = db.rows(&format!(
        "SELECT proposed_work FROM hub_coordination_requests WHERE id = '{}'",
        request.id
    ));
    assert_eq!(bound[0][0], mine.item.id);
    let released_events = |store: &dyn Store| {
        store
            .events_after(0, 1000)
            .unwrap()
            .into_iter()
            .filter(|e| e.kind == "coordination_target_released" && e.subject == request.id)
            .count()
    };
    assert_eq!(released_events(&*store), 1);
    // Ending the proposing Work marks the request again; its status is not touched.
    lead_checkpoint(
        &*store,
        &bob,
        &mine.item,
        "fin",
        "s-b",
        CheckpointKind::Finish,
        now + 4,
    );
    assert_eq!(released_events(&*store), 2);
    let status = db.rows(&format!(
        "SELECT status FROM hub_coordination_requests WHERE id = '{}'",
        request.id
    ));
    assert_eq!(status[0][0], "pending");
}

fn read_only_transition_on_an_unknown_id_is_unauthorized_in_both_stores(db: &Db) {
    let store = db.open();
    let now = unix_now();
    let reader = agent_with(
        &*store,
        AgentSpec {
            provider: "github",
            host: "github.com",
            user_id: 7,
            repository: REPO,
            repository_id: Some(REPOSITORY_ID),
            read_only: true,
            ttl: TTL,
        },
    );
    let request = WorkTransitionRequest {
        work_id: "wrk_00000000000000000000000000000001".into(),
        operation_id: "op".into(),
        generation: 1,
        expected_updated_seq: 1,
        session: Some("s".into()),
        transition: WorkTransitionKind::Release,
        reason: "synthetic reason".into(),
        superseded_by: None,
    };
    assert!(matches!(
        store.work_transition(&by_agent(&reader), &request, now),
        Err(HubError::Unauthorized)
    ));
    // A writer sees an unknown id as invalid.
    let writer = agent(&*store, 8);
    assert!(matches!(
        store.work_transition(&by_agent(&writer), &request, now),
        Err(HubError::Invalid(_))
    ));
}

fn a_paused_item_frees_its_session_for_another_goal(db: &Db) {
    let store = db.open();
    let alice = agent(&*store, 1);
    let now = unix_now();
    let mut first_start = automatic_start("first goal", "s-1");
    first_start.scope = vec!["first/**".into()];
    let first = store
        .work_start_auto(&alice.token, &first_start, now)
        .unwrap()
        .item;
    let paused = lead_checkpoint(
        &*store,
        &alice,
        &first,
        "pause",
        "s-1",
        CheckpointKind::Pause,
        now + 1,
    )
    .item;
    assert_eq!(paused.state, WorkState::Paused);
    // A task-only start is identified by its session, so a repeat finds the paused goal.
    let repeat = store
        .work_start_auto(&alice.token, &first_start, now + 2)
        .unwrap();
    assert_eq!(repeat.item.id, first.id);
    assert!(!repeat.created);

    // A different explicit goal in the same session starts, where it used to be refused.
    let second = store
        .work_start(
            &alice.token,
            &{
                let mut start = native_start("second", "second goal", "s-1");
                start.scope = vec!["second/**".into()];
                start
            },
            now + 3,
        )
        .unwrap();
    assert!(second.created && second.conflict.is_none());
    assert_eq!(
        store
            .work_for_session(&alice.token, "s-1", now + 3)
            .unwrap()
            .unwrap()
            .id,
        second.item.id
    );

    // Resuming the paused goal while the session leads another is refused without mutation.
    let refused = store
        .work_start(
            &alice.token,
            &takeover(&first.id, "s-1", Some(paused.generation)),
            now + 4,
        )
        .unwrap();
    assert_eq!(
        refused.conflict.map(|conflict| conflict.kind),
        Some(WorkConflictKind::SessionGoalChanged)
    );
    assert_eq!(refused.item.id, second.item.id);
    assert_eq!(work_event_count(&*store, "work_takeover"), 0);

    // Once the second goal is paused too, the first resumes.
    lead_checkpoint(
        &*store,
        &alice,
        &second.item,
        "pause-second",
        "s-1",
        CheckpointKind::Pause,
        now + 5,
    );
    let resumed = store
        .work_start(
            &alice.token,
            &takeover(&first.id, "s-1", Some(paused.generation)),
            now + 6,
        )
        .unwrap();
    assert!(resumed.took_over && resumed.conflict.is_none());
    assert_eq!(resumed.item.state, WorkState::Active);
}

/// The Hub's clock heartbeat once a minute from `from` through `to`, as `serve` runs it.
fn tick(store: &dyn Store, from: i64, to: i64) {
    let mut at = from;
    while at < to {
        store.observe_lease_clock(at).unwrap();
        at += 60;
    }
    store.observe_lease_clock(to).unwrap();
}

/// The stored lease: last renewal and expiry, empty when no reservation is held.
fn lease_of(db: &Db, id: &str) -> (String, String) {
    let rows = db.rows(&format!(
        "SELECT reservation_renewed_at, reservation_expires_at FROM hub_work_items
         WHERE id = '{id}'"
    ));
    (rows[0][0].clone(), rows[0][1].clone())
}

fn activity(
    store: &dyn Store,
    who: &Agent,
    id: &str,
    now: i64,
) -> agentscm::hub::work_lifecycle::ActivityV1 {
    let page = store
        .work_operational(
            &agentscm::hub::work_transition::WorkLifecycleReader::Token(&who.token),
            &OperationalSelect::One(id),
            now,
        )
        .unwrap();
    assert_eq!(page.schema_version, 2);
    page.work.into_iter().next().unwrap().activity
}

/// An agent whose token outlives the hours these lease tests move the clock through.
fn lasting_agent(store: &dyn Store, user_id: i64) -> Agent {
    agent_with(
        store,
        AgentSpec {
            provider: "github",
            host: "github.com",
            user_id,
            repository: REPO,
            repository_id: Some(REPOSITORY_ID),
            read_only: false,
            ttl: 3 * 86_400,
        },
    )
}

fn scoped(key: &str, session: &str, scope: &str) -> WorkStart {
    let mut start = native_start(key, key, session);
    start.scope = vec![scope.into()];
    start
}

fn an_idle_lease_lapses_and_the_next_admission_clears_it(db: &Db) {
    use agentscm::hub::work_lifecycle::{ActivityStatus, LEASE_SECONDS};
    let store = db.open();
    let alice = lasting_agent(&*store, 1);
    let bob = lasting_agent(&*store, 2);
    let now = unix_now();
    store.observe_lease_clock(now).unwrap();
    let held = start_in(&*store, &alice, "a", "s-a", "src/**", now);
    announce(&*store, &alice, "s-a", "src/lib.rs", now);
    assert_eq!(
        activity(&*store, &alice, &held.id, now).status,
        ActivityStatus::Live
    );

    // Within the lease the reservation blocks.
    let edge = now + LEASE_SECONDS - 1;
    tick(&*store, now, edge);
    assert_scope_overlap(
        &store
            .work_start(&bob.token, &scoped("b1", "s-b", "src/**"), edge)
            .unwrap(),
    );
    let after = now + LEASE_SECONDS + 1;
    tick(&*store, edge, after);
    assert_eq!(
        activity(&*store, &alice, &held.id, after).status,
        ActivityStatus::Lapsed
    );
    // A lapsed reservation is advice: it stays until an admission over it.
    assert_eq!(db.reservation(&held.id), (None, Some("admitted".into())));
    let admitted = store
        .work_start(&bob.token, &scoped("b2", "s-b2", "src/**"), after)
        .unwrap();
    assert!(admitted.created && admitted.conflict.is_none());

    // The same transaction cleared it, fenced the old lead and recorded what the Hub saw.
    assert_eq!(db.reservation(&held.id), (None, None));
    assert_eq!(lease_of(db, &held.id), (String::new(), String::new()));
    let lapsed = current_item(&*store, &alice, &held.id, after);
    assert_eq!(
        (lapsed.state, lapsed.generation),
        (WorkState::Paused, held.generation + 1)
    );
    let events: Vec<serde_json::Value> = store
        .events_after(0, 1000)
        .unwrap()
        .iter()
        .filter(|event| event.kind == "reservation_lapsed")
        .map(|event| serde_json::from_str(&event.data).unwrap())
        .collect();
    assert_eq!(events.len(), 1);
    let event = &events[0];
    assert_eq!(event["lead"]["session"], "s-a");
    assert_eq!(event["renewed_at"], now);
    assert_eq!(event["expires_at"], now + LEASE_SECONDS);
    assert_eq!(event["branch"], "feature/work");
    assert_eq!(event["scope"], serde_json::json!(held.scope));
    assert_eq!(event["seen_paths"], serde_json::json!(["src/lib.rs"]));
    assert_eq!(event["push_state"], "unknown");
    assert_eq!(event["admitted_by"], admitted.item.id.as_str());
    assert_eq!(event["generation"], held.generation + 1);

    // A late checkpoint at the old generation is fenced.
    assert!(matches!(
        store.work_checkpoint(
            &alice.token,
            &checkpoint(
                &held.id,
                "late",
                held.generation,
                "s-a",
                CheckpointKind::Progress,
                "late"
            ),
            after + 1,
        ),
        Err(HubError::Invalid(_))
    ));
    // The woken lead's edits are refused every time: lapse refusals never pass the refusal limit.
    let edit = agentscm::hub::session::SessionCall {
        session: Some("s-a".into()),
        via: "hook:claude".into(),
        task: None,
        paths: vec!["src/lib.rs".into()],
        deny: true,
        report: true,
    };
    for minute in 0..5 {
        assert!(matches!(
            store.touch_session(&alice.token, &edit, after + 2 + minute * 60),
            Err(HubError::Invalid(_) | HubError::Rejected(_))
        ));
    }
    // Resuming needs fresh admission, and the scope is held again.
    assert_scope_overlap(
        &store
            .work_start(
                &alice.token,
                &takeover(&held.id, "s-a", Some(held.generation + 1)),
                after + 400,
            )
            .unwrap(),
    );
}

fn lead_activity_renews_the_lease_without_reviving_it(db: &Db) {
    use agentscm::hub::work_lifecycle::{ActivityStatus, LEASE_SECONDS};
    let store = db.open();
    let alice = lasting_agent(&*store, 1);
    let bob = lasting_agent(&*store, 2);
    let now = unix_now();
    store.observe_lease_clock(now).unwrap();
    let held = start_in(&*store, &alice, "a", "s-a", "src/**", now);
    // The lead session works for 50 minutes, past the 30-minute intent cap; no absolute cap
    // applies to the lease.
    let mut at = now;
    for step in 1..=5 {
        let next = now + step * 600;
        tick(&*store, at, next);
        at = next;
        announce(&*store, &alice, "s-a", "src/x.rs", at);
        assert_scope_overlap(
            &store
                .work_start(
                    &bob.token,
                    &scoped(&format!("b{step}"), "s-b", "src/**"),
                    at,
                )
                .unwrap(),
        );
    }
    let renewed = at;
    let expected = (renewed.to_string(), (renewed + LEASE_SECONDS).to_string());
    assert_eq!(lease_of(db, &held.id), expected);
    // At most one renewal a minute.
    announce(&*store, &alice, "s-a", "src/x.rs", renewed + 30);
    assert_eq!(lease_of(db, &held.id), expected);
    // Neither another session of the lead token nor another token renews it.
    tick(&*store, renewed, renewed + 600);
    announce(&*store, &alice, "s-other", "docs/y.md", renewed + 600);
    announce(&*store, &bob, "s-b", "docs/z.md", renewed + 600);
    assert_eq!(lease_of(db, &held.id), expected);
    // Once lapsed, a late touch of the lead does not revive it, and its edit needs fresh admission.
    let late = renewed + LEASE_SECONDS + 1;
    tick(&*store, renewed + 600, late);
    store
        .touch_session(
            &alice.token,
            &agentscm::hub::session::SessionCall {
                session: Some("s-a".into()),
                via: "mcp".into(),
                task: None,
                paths: Vec::new(),
                deny: false,
                report: false,
            },
            late,
        )
        .unwrap();
    assert_eq!(lease_of(db, &held.id), expected);
    assert_eq!(
        activity(&*store, &alice, &held.id, late).status,
        ActivityStatus::Lapsed
    );
    assert!(
        store
            .work_start(&bob.token, &scoped("b-late", "s-b", "src/**"), late)
            .unwrap()
            .created
    );
}

fn every_basis_lapses_and_retain_is_capped(db: &Db) {
    use agentscm::hub::work_lifecycle::{LEASE_SECONDS, RETAIN_CAP_SECONDS};
    let store = db.open();
    let alice = lasting_agent(&*store, 1);
    let bob = lasting_agent(&*store, 2);
    let now = unix_now();
    store.observe_lease_clock(now).unwrap();
    let retained = start_in(&*store, &alice, "r", "s-r", "kept/**", now);
    let retain = store
        .work_transition(
            &by_agent(&alice),
            &transition_request(&retained, "retain", WorkTransitionKind::Retain, Some("s-r")),
            now,
        )
        .unwrap();
    assert_eq!(db.reservation(&retained.id).1.as_deref(), Some("retained"));
    assert_eq!(
        lease_of(db, &retained.id).1,
        (now + RETAIN_CAP_SECONDS).to_string()
    );
    let paused = start_in(&*store, &alice, "p", "s-p", "paused/**", now);
    lead_checkpoint(
        &*store,
        &alice,
        &paused,
        "pause",
        "s-p",
        CheckpointKind::Pause,
        now,
    );
    assert_eq!(
        db.reservation(&paused.id).1.as_deref(),
        Some("legacy_implicit")
    );

    // A pause's implicit reservation lapses on its own clock, even while its lead session keeps
    // working; a retain outlasts the idle lease.
    tick(&*store, now, now + 600);
    let heartbeat = agentscm::hub::session::SessionCall {
        session: Some("s-p".into()),
        via: "mcp".into(),
        task: None,
        paths: Vec::new(),
        deny: false,
        report: false,
    };
    store
        .touch_session(&alice.token, &heartbeat, now + 600)
        .unwrap();
    let idle = now + LEASE_SECONDS + 1;
    tick(&*store, now + 600, idle);
    assert!(
        store
            .work_start(&bob.token, &scoped("bp", "s-bp", "paused/**"), idle)
            .unwrap()
            .created
    );
    assert_eq!(db.reservation(&paused.id), (None, None));
    assert_scope_overlap(
        &store
            .work_start(&bob.token, &scoped("br1", "s-br", "kept/**"), idle)
            .unwrap(),
    );
    // The retain is capped; it never holds indefinitely.
    let capped = now + RETAIN_CAP_SECONDS + 1;
    tick(&*store, idle, capped);
    assert!(
        store
            .work_start(&bob.token, &scoped("br2", "s-br2", "kept/**"), capped)
            .unwrap()
            .created
    );
    assert_eq!(db.reservation(&retained.id), (None, None));
    assert!(retain.conflict.is_none());
}

fn a_gap_in_the_clock_grants_one_lease_of_grace(db: &Db) {
    use agentscm::hub::work_lifecycle::LEASE_SECONDS;
    let store = db.open();
    let alice = lasting_agent(&*store, 1);
    let bob = lasting_agent(&*store, 2);
    let now = unix_now();
    store.observe_lease_clock(now).unwrap();
    let held = start_in(&*store, &alice, "a", "s-a", "src/**", now);
    // The Hub is down for two hours. Before its first heartbeat the clock is stale, so a request
    // lapses nothing.
    let restart = now + 7200;
    assert_scope_overlap(
        &store
            .work_start(&bob.token, &scoped("b1", "s-b", "src/**"), restart)
            .unwrap(),
    );
    // The first heartbeat grants the reservation that was live one lease of grace.
    store.observe_lease_clock(restart).unwrap();
    assert_eq!(
        lease_of(db, &held.id).1,
        (restart + LEASE_SECONDS).to_string()
    );
    let edge = restart + LEASE_SECONDS - 1;
    tick(&*store, restart, edge);
    assert_scope_overlap(
        &store
            .work_start(&bob.token, &scoped("b2", "s-b", "src/**"), edge)
            .unwrap(),
    );
    let after = restart + LEASE_SECONDS + 1;
    tick(&*store, edge, after);
    assert!(
        store
            .work_start(&bob.token, &scoped("b3", "s-b3", "src/**"), after)
            .unwrap()
            .created
    );
}

fn lease_migration_starts_existing_reservations_now(db: &Db) {
    use agentscm::hub::work_lifecycle::{LEASE_SECONDS, RETAIN_CAP_SECONDS};
    let store = db.open();
    let alice = lasting_agent(&*store, 1);
    let now = unix_now();
    let active = start_in(&*store, &alice, "a", "s-a", "src/**", now - 86_400);
    let retained = start_in(&*store, &alice, "r", "s-r", "kept/**", now - 86_400);
    store
        .work_transition(
            &by_agent(&alice),
            &transition_request(&retained, "retain", WorkTransitionKind::Retain, Some("s-r")),
            now - 86_400,
        )
        .unwrap();
    drop(store);

    // Rebuild the schema a binary without this change leaves behind.
    db.sql(
        "ALTER TABLE hub_work_items DROP COLUMN reservation_renewed_at;
         ALTER TABLE hub_work_items DROP COLUMN reservation_expires_at;
         DROP TABLE hub_lease_clock;
         DELETE FROM hub_named_migrations WHERE name = 'work_reservation_leases';",
    );
    let before = unix_now();
    let _store = db.open();
    let after = unix_now();
    // The clock starts at migration time, not at the day-old admission.
    for (id, lease) in [
        (&active.id, LEASE_SECONDS),
        (&retained.id, RETAIN_CAP_SECONDS),
    ] {
        let (renewed, expires) = lease_of(db, id);
        let renewed: i64 = renewed.parse().unwrap();
        assert!((before..=after).contains(&renewed), "{renewed}");
        assert_eq!(expires.parse::<i64>().unwrap(), renewed + lease);
    }
    let observed = db.rows("SELECT observed_at FROM hub_lease_clock");
    assert!((before..=after).contains(&observed[0][0].parse::<i64>().unwrap()));
}

fn issue_links_survive_compaction_with_correct_capacity(db: &Db) {
    use agentscm::hub::planned_issues::IssueIdentity;
    use agentscm::hub::work_issues::{WorkIssueChoice, WorkIssueLinkUpdate};
    let store = db.open();
    let now = unix_now();
    let old = now - 3 * 86_400;
    let lead = agent(&*store, 1);
    let issue = IssueIdentity {
        provider: "github".into(),
        host: "github.com".into(),
        repository_id: REPOSITORY_ID,
        number: 7,
    };
    let choice = WorkIssueChoice::NewIssue {
        title: "New issue intention".into(),
    };
    let mut start = native_start("issue-retain", "Issue retention", "lead");
    start.issue_choice = Some(choice.clone());
    let item = store.work_start(&lead.token, &start, old).unwrap().item;
    let update = WorkIssueLinkUpdate {
        work_id: item.id.clone(),
        operation_id: "retained-issue".into(),
        generation: 1,
        session: "lead".into(),
        expected_updated_seq: item.updated_seq,
        link: Some(issue.clone()),
    };
    let linked = store
        .work_issue_link_set(&lead.token, &update, old + 1)
        .unwrap();
    let mut cp = checkpoint(
        &item.id,
        "retained-issue-progress",
        1,
        "lead",
        CheckpointKind::Progress,
        "new baseline",
    );
    cp.expected_updated_seq = Some(linked.item.updated_seq);
    let progress = store.work_checkpoint(&lead.token, &cp, old + 2).unwrap();
    let (charged, aggregate) = db.work_usage_and_aggregate();
    assert_eq!(charged, aggregate);
    let scope = WorkRepositoryScope {
        provider: "github".into(),
        host: "github.com".into(),
        repository: REPO.into(),
        repository_id: Some(REPOSITORY_ID),
    };
    store
        .work_compact(&scope, now - 2 * 86_400, true, now)
        .unwrap();
    let current = store.work_get(&lead.token, &item.id, now).unwrap().unwrap();
    assert_eq!(current.issue_link, Some(issue));
    assert_eq!(current.initial_issue_choice, choice);
    assert_eq!(current.updated_seq, progress.item.updated_seq);
    assert!(
        !store
            .events_after(0, 100)
            .unwrap()
            .iter()
            .any(|e| e.seq == linked.receipt.event_seq)
    );
    assert_invalid(
        store.work_issue_link_set(&lead.token, &update, now),
        "issue link receipt expired or Work changed",
    );
    let mut finish = checkpoint(
        &item.id,
        "issue-finished",
        1,
        "lead",
        CheckpointKind::Finish,
        "finished report",
    );
    finish.expected_updated_seq = Some(current.updated_seq);
    let finished = store
        .work_checkpoint(&lead.token, &finish, now)
        .unwrap()
        .item;
    let before = lifecycle(
        &*store,
        &WorkLifecycleReader::Token(&lead.token),
        &item.id,
        now,
    );
    let removal = WorkIssueLinkUpdate {
        work_id: item.id.clone(),
        operation_id: "finished-issue-remove".into(),
        generation: 1,
        session: "lead".into(),
        expected_updated_seq: finished.updated_seq,
        link: None,
    };
    let removed = store
        .work_issue_link_set(&lead.token, &removal, now + 1)
        .unwrap()
        .item;
    assert_eq!(removed.current, finished.current);
    assert_eq!(
        lifecycle(
            &*store,
            &WorkLifecycleReader::Token(&lead.token),
            &item.id,
            now + 1
        ),
        before
    );
    let (charged, aggregate) = db.work_usage_and_aggregate();
    assert_eq!(charged, aggregate);
}
