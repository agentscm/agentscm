//! Durable work coordination store conformance. Every case runs on SQLite and,
//! when configured, PostgreSQL.
#[path = "common/hub_pg.rs"]
mod hub_pg;

use std::sync::{Arc, Barrier, mpsc};
use std::thread;
use std::time::Duration;

use agentscm::hub::HubError;
use agentscm::hub::store::{
    self, ForgeIdentity, Store, mint_agent_token, mint_bound_agent_token, unix_now,
};
use agentscm::hub::work::{
    BriefingAck, BriefingQuery, CLAIM_STALE_SECONDS, CheckpointKind, ClaimStatus, ForgeWorkRef,
    SourceRef, WORK_DELIVERY_RESERVATION_BYTES, WORK_PAYLOAD_CAPACITY_BYTES, WorkCheckpoint,
    WorkConflictKind, WorkRepositoryScope, WorkRole, WorkStart, WorkState,
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
                            ('work_started', 'work_joined', 'work_takeover', 'work_checkpoint')";
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
                                           'work_checkpoint')
                     )",
                    rusqlite::params![REPO, REPOSITORY_ID],
                    |row| row.get(0),
                )
                .unwrap();
            (used, aggregate)
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
        work_id: work_id.map(str::to_owned),
        source: None,
        request_key: request_key.map(str::to_owned),
        title: title.into(),
        session: session.into(),
        role,
        scope: vec!["src/**".into()],
        branch: Some("feature/work".into()),
        worktree: Some("worktree-a".into()),
        next_action: Some("continue".into()),
        depends_on: vec![],
        related_work: vec![],
        takeover_generation: None,
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
    assert!(
        matches!(result, Err(HubError::Invalid(ref actual)) if actual == message),
        "expected invalid {message:?}"
    );
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
    let contested = store
        .work_start(&viewer.token, &takeover(&id, "viewer", None), stale_at)
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
            "only the current lead session may checkpoint work",
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
        "only the current lead session may checkpoint work",
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
        assert_invalid(
            store.work_checkpoint(&lead.token, &stale_pause, now + index as i64 * 10 + 2),
            &format!(
                "checkpoint receipt expired or work changed; current updated_seq is {}",
                fresh.updated_seq
            ),
        );

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
    assert_invalid(
        store.work_compact(&scope, now - 86_400, false, now),
        "work compaction is blocked by an unacknowledged briefing delivery",
    );
    store
        .work_ack(
            &lead.token,
            &BriefingAck {
                delivery: open.delivery.unwrap(),
            },
            now,
        )
        .unwrap();

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
    assert_invalid(
        store.work_checkpoint(&lead.token, &baseline_request, now),
        &format!(
            "checkpoint receipt expired or work changed; current updated_seq is {}",
            recent.item.updated_seq
        ),
    );
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
    let scope =
        serde_json::to_string(&("github", "github.com", REPO, Some(REPOSITORY_ID))).unwrap();
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
                child_store.work_briefing_preview(&token, 1, unix_now())
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
            Err(HubError::WorkCapacity) => {
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
    concurrent_forge_start_has_one_work_and_one_lead,
    native_and_automatic_starts_have_explicit_retry_rules,
    authorization_and_claim_projection_keep_old_claims_visible,
    repository_namespaces_fence_work_and_relations,
    participants_replays_and_takeovers_are_fenced,
    blocker_metadata_and_restart_are_durable,
    stale_observed_pause_cannot_overwrite_newer_checkpoint,
    forge_issue_ids_are_canonical_positive_decimals,
    concurrent_deliveries_and_acks_are_monotonic_and_retryable,
    compaction_preserves_baselines_and_expires_old_receipts,
    compaction_delete_exception_is_exact_and_updates_remain_prohibited,
    postgres_read_briefings_share_the_repository_maintenance_lock,
    concurrent_delivery_reservations_enforce_capacity_and_roll_back,
    briefing_sessions_on_one_token_have_independent_cursors,
);
