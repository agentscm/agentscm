//! Public coordination response bounds that must stay below the Hub JSON limit.
use crate::hub_pg;
use agentscm::hub::capacity::CapacityLimit;

use std::sync::Arc;

use agentscm::hub::HubError;
use agentscm::hub::coordination::{
    CoordinationAck, CoordinationAcknowledgement, CoordinationAgent, CoordinationInboxQuery,
    CoordinationListQuery, CoordinationProposal, CoordinationReply, CoordinationRequest,
    CoordinationResolve, CoordinationResponse, CoordinationResponseKind, CoordinationStatus,
    CoordinationTarget, CoordinationWithdraw, CoordinationWithdrawTarget,
};
use agentscm::hub::session::SessionCall;
use agentscm::hub::store::{self, ForgeIdentity, Store, mint_bound_agent_token, unix_now};
use agentscm::hub::work::{
    BriefingQuery, CheckpointKind, WorkCheckpoint, WorkPathOverlap, WorkRepositoryScope, WorkRole,
    WorkStart,
};
use hub_pg::PgSchema;
use tempfile::TempDir;

const REPOSITORY: &str = "acme/widgets";
const REPOSITORY_ID: i64 = 101;

struct Db {
    url: String,
    _pg: Option<PgSchema>,
    _directory: Option<TempDir>,
}

impl Db {
    fn sqlite() -> Self {
        let directory = TempDir::new().unwrap();
        Self {
            url: format!("sqlite://{}", directory.path().join("hub.sqlite").display()),
            _pg: None,
            _directory: Some(directory),
        }
    }

    fn postgres() -> Option<Self> {
        let pg = PgSchema::create()?;
        Some(Self {
            url: pg.url.clone(),
            _pg: Some(pg),
            _directory: None,
        })
    }

    fn open(&self) -> Arc<dyn Store> {
        store::open(&self.url).unwrap()
    }

    fn exec(&self, sql: &str) {
        if self._pg.is_some() {
            postgres::Client::connect(&self.url, postgres::NoTls)
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

    fn participant_generations(&self) -> Vec<(String, i64)> {
        if self._pg.is_some() {
            let mut client = postgres::Client::connect(&self.url, postgres::NoTls).unwrap();
            client
                .query(
                    "SELECT session,generation FROM hub_work_participants ORDER BY session",
                    &[],
                )
                .unwrap()
                .into_iter()
                .map(|row| (row.get(0), row.get(1)))
                .collect()
        } else {
            let path = self.url.strip_prefix("sqlite://").unwrap();
            let connection = rusqlite::Connection::open(path).unwrap();
            let mut statement = connection
                .prepare("SELECT session,generation FROM hub_work_participants ORDER BY session")
                .unwrap();
            statement
                .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
                .unwrap()
                .map(Result::unwrap)
                .collect()
        }
    }

    fn expire_token(&self, token: &str) {
        if self._pg.is_some() {
            postgres::Client::connect(&self.url, postgres::NoTls)
                .unwrap()
                .execute(
                    "UPDATE hub_agent_tokens SET expires_at=0 WHERE id=$1",
                    &[&token],
                )
                .unwrap();
        } else {
            let path = self.url.strip_prefix("sqlite://").unwrap();
            rusqlite::Connection::open(path)
                .unwrap()
                .execute(
                    "UPDATE hub_agent_tokens SET expires_at=0 WHERE id=?1",
                    [token],
                )
                .unwrap();
        }
    }

    fn disable_token_owner(&self, token: &str) {
        if self._pg.is_some() {
            postgres::Client::connect(&self.url, postgres::NoTls)
                .unwrap()
                .execute(
                    "UPDATE hub_users SET disabled=TRUE
                     WHERE id=(SELECT owner FROM hub_agent_tokens WHERE id=$1)",
                    &[&token],
                )
                .unwrap();
        } else {
            let path = self.url.strip_prefix("sqlite://").unwrap();
            rusqlite::Connection::open(path)
                .unwrap()
                .execute(
                    "UPDATE hub_users SET disabled=1
                     WHERE id=(SELECT owner FROM hub_agent_tokens WHERE id=?1)",
                    [token],
                )
                .unwrap();
        }
    }

    fn charged_bytes(&self) -> i64 {
        let sql = "SELECT charged_bytes FROM hub_work_usage
                   WHERE provider='github' AND host='github.com'
                     AND repository='acme/widgets' AND repository_id=101";
        if self._pg.is_some() {
            postgres::Client::connect(&self.url, postgres::NoTls)
                .unwrap()
                .query_one(sql, &[])
                .unwrap()
                .get(0)
        } else {
            let path = self.url.strip_prefix("sqlite://").unwrap();
            rusqlite::Connection::open(path)
                .unwrap()
                .query_row(sql, [], |row| row.get(0))
                .unwrap()
        }
    }
}

fn token(store: &dyn Store, user_id: i64, name: &str) -> String {
    mint(store, user_id, name, false)
}

fn mint(store: &dyn Store, user_id: i64, name: &str, read_only: bool) -> String {
    let user = store
        .upsert_user(
            &ForgeIdentity {
                provider: "github".into(),
                host: "github.com".into(),
                user_id,
            },
            &format!("user{user_id}"),
        )
        .unwrap();
    mint_bound_agent_token(
        store,
        &user.id,
        name,
        REPOSITORY,
        REPOSITORY_ID,
        3600,
        read_only,
    )
    .unwrap()
    .0
}

fn start(key: &str, session: &str) -> WorkStart {
    start_scope(key, session, WorkRole::Lead, None, &["src/**"])
}

fn start_scope(
    key: &str,
    session: &str,
    role: WorkRole,
    work_id: Option<&str>,
    scope: &[&str],
) -> WorkStart {
    WorkStart {
        issue_choice: None,
        work_id: work_id.map(str::to_owned),
        source: None,
        request_key: work_id.is_none().then(|| key.into()),
        title: key.into(),
        description: None,
        session: session.into(),
        role,
        scope: scope.iter().map(|path| (*path).into()).collect(),
        branch: None,
        worktree: None,
        next_action: Some("continue".into()),
        depends_on: Vec::new(),
        related_work: Vec::new(),
        takeover_generation: None,
        preflight: None,
    }
}

fn checkpoint(
    work_id: &str,
    operation_id: &str,
    session: &str,
    expected_updated_seq: i64,
    kind: CheckpointKind,
    scope: Option<&[&str]>,
) -> WorkCheckpoint {
    WorkCheckpoint {
        work_id: work_id.into(),
        operation_id: operation_id.into(),
        generation: 1,
        session: session.into(),
        expected_updated_seq: Some(expected_updated_seq),
        kind,
        summary: "coordination checkpoint".into(),
        next_action: Some("continue".into()),
        scope: scope.map(|paths| paths.iter().map(|path| (*path).into()).collect()),
        branch: None,
        worktree: None,
        resolve_blocker: false,
        source_refs: Vec::new(),
        depends_on: Vec::new(),
        related_work: Vec::new(),
    }
}

fn durable_refusal_replays_on_both_stores(db: &Db) {
    let store = db.open();
    let incumbent = token(&*store, 1, "incumbent");
    let requester = token(&*store, 2, "requester");
    let now = unix_now();
    store
        .work_start(&incumbent, &start("incumbent", "incumbent-session"), now)
        .unwrap();
    let first = store
        .work_start(
            &requester,
            &start("requester", "requester-session"),
            now + 1,
        )
        .unwrap();
    let request = first.coordination_request.unwrap();
    assert!(!first.created);
    assert!(first.conflict.is_some());

    let replay = store
        .work_start(
            &requester,
            &start("requester", "requester-session"),
            now + 2,
        )
        .unwrap()
        .coordination_request
        .unwrap();
    assert_eq!(replay.id, request.id);
    let inbox = store
        .coordination_inbox(
            &incumbent,
            &CoordinationInboxQuery {
                session: Some("incumbent-session".into()),
                ..CoordinationInboxQuery::default()
            },
            now + 3,
        )
        .unwrap();
    assert_eq!(inbox.requests.len(), 1);
    assert_eq!(inbox.requests[0].id, request.id);

    let ack = CoordinationAck {
        request_id: request.id.clone(),
        target_work_id: Some(request.targets[0].work_id.clone()),
        session: "incumbent-session".into(),
        expected_updated_seq: request.updated_seq,
    };
    let acknowledged = store.coordination_ack(&incumbent, &ack, now + 4).unwrap();
    let replayed_ack = store.coordination_ack(&incumbent, &ack, now + 5).unwrap();
    assert_eq!(replayed_ack.updated_seq, acknowledged.updated_seq);
    let reply = CoordinationReply {
        request_id: request.id.clone(),
        operation_id: "reply-operation".into(),
        target_work_id: request.targets[0].work_id.clone(),
        session: "incumbent-session".into(),
        expected_updated_seq: acknowledged.updated_seq,
        kind: CoordinationResponseKind::Join,
        note: Some("Join the incumbent work as a helper".into()),
        via: agentscm::hub::handoff::Via::Agent,
    };
    let responded = store
        .coordination_reply(&incumbent, &reply, now + 6)
        .unwrap();
    let replayed_reply = store
        .coordination_reply(&incumbent, &reply, now + 7)
        .unwrap();
    assert_eq!(replayed_reply.updated_seq, responded.updated_seq);

    let premature = CoordinationResolve {
        request_id: request.id.clone(),
        operation_id: "premature-resolution".into(),
        session: "requester-session".into(),
        expected_updated_seq: responded.updated_seq,
    };
    assert!(matches!(
        store.coordination_resolve(&requester, &premature, now + 8),
        Err(HubError::WorkCoordinationRequired { .. })
    ));
    store
        .work_start(
            &requester,
            &start_scope(
                "join-request",
                "requester-session",
                WorkRole::Helper,
                Some(&request.targets[0].work_id),
                &["src/**"],
            ),
            now + 9,
        )
        .unwrap();
    let resolved = store
        .coordination_resolve(
            &requester,
            &CoordinationResolve {
                operation_id: "valid-resolution".into(),
                ..premature
            },
            now + 10,
        )
        .unwrap();
    assert_eq!(resolved.status, CoordinationStatus::Resolved);

    let withdrawing = token(&*store, 3, "withdrawing");
    let withdraw_request = store
        .work_start(
            &withdrawing,
            &start("withdraw-request", "withdraw-session"),
            now + 11,
        )
        .unwrap()
        .coordination_request
        .unwrap();
    let withdrawn = store
        .coordination_withdraw(
            &withdrawing,
            &CoordinationWithdraw {
                request_id: withdraw_request.id,
                operation_id: "withdraw-operation".into(),
                session: "withdraw-session".into(),
                expected_updated_seq: withdraw_request.updated_seq,
                target: None,
                via_hub: false,
            },
            now + 12,
        )
        .unwrap();
    assert_eq!(withdrawn.status, CoordinationStatus::Withdrawn);

    let splitting = token(&*store, 4, "splitting");
    let split_request = store
        .work_start(
            &splitting,
            &start("split-request", "split-session"),
            now + 13,
        )
        .unwrap()
        .coordination_request
        .unwrap();
    let split_work = store
        .work_start(
            &splitting,
            &start_scope(
                "split-request",
                "split-session",
                WorkRole::Lead,
                None,
                &["docs/**"],
            ),
            now + 14,
        )
        .unwrap();
    assert!(split_work.created);
    let bound = store
        .coordination_inbox(
            &splitting,
            &CoordinationInboxQuery {
                session: Some("split-session".into()),
                ..CoordinationInboxQuery::default()
            },
            now + 15,
        )
        .unwrap()
        .requests
        .into_iter()
        .find(|candidate| candidate.id == split_request.id)
        .unwrap();
    assert_eq!(
        bound.proposal.work_id.as_deref(),
        Some(split_work.item.id.as_str())
    );
    let split_resolved = store
        .coordination_resolve(
            &splitting,
            &CoordinationResolve {
                request_id: bound.id,
                operation_id: "split-resolution".into(),
                session: "split-session".into(),
                expected_updated_seq: bound.updated_seq,
            },
            now + 16,
        )
        .unwrap();
    assert_eq!(split_resolved.status, CoordinationStatus::Resolved);
}

#[test]
fn durable_refusal_replays_on_sqlite() {
    durable_refusal_replays_on_both_stores(&Db::sqlite());
}

#[test]
fn durable_refusal_replays_on_postgres() {
    if let Some(db) = Db::postgres() {
        durable_refusal_replays_on_both_stores(&db);
    }
}

fn queued_expansion_requires_the_proposed_scope(db: &Db) {
    let store = db.open();
    let incumbent = token(&*store, 10, "expansion-incumbent");
    let requester = token(&*store, 11, "expansion-requester");
    let now = unix_now();
    let target = store
        .work_start(
            &incumbent,
            &start_scope(
                "expansion-target",
                "target-session",
                WorkRole::Lead,
                None,
                &["src/**"],
            ),
            now,
        )
        .unwrap()
        .item;
    let proposed = store
        .work_start(
            &requester,
            &start_scope(
                "expanding-work",
                "expanding-session",
                WorkRole::Lead,
                None,
                &["ui/**"],
            ),
            now + 1,
        )
        .unwrap()
        .item;
    let request = match store.work_checkpoint(
        &requester,
        &checkpoint(
            &proposed.id,
            "rejected-expansion",
            "expanding-session",
            proposed.updated_seq,
            CheckpointKind::Progress,
            Some(&["ui/**", "src/**"]),
        ),
        now + 2,
    ) {
        Err(HubError::WorkCoordinationRequired { request, .. }) => *request,
        other => panic!("expected durable expansion refusal, got {other:?}"),
    };
    let premature = store.coordination_resolve(
        &requester,
        &CoordinationResolve {
            request_id: request.id.clone(),
            operation_id: "scope-not-applied".into(),
            session: "expanding-session".into(),
            expected_updated_seq: request.updated_seq,
        },
        now + 3,
    );
    assert!(matches!(premature, Err(HubError::Invalid(message))
        if message.contains("proposal scope has not been applied")));

    store
        .work_checkpoint(
            &incumbent,
            &checkpoint(
                &target.id,
                "finish-target",
                "target-session",
                target.updated_seq,
                CheckpointKind::Finish,
                None,
            ),
            now + 4,
        )
        .unwrap();
    let expanded = store
        .work_checkpoint(
            &requester,
            &checkpoint(
                &proposed.id,
                "apply-expansion",
                "expanding-session",
                proposed.updated_seq,
                CheckpointKind::Progress,
                Some(&["ui/**", "src/**"]),
            ),
            now + 5,
        )
        .unwrap();
    assert_eq!(expanded.item.scope, vec!["src/", "ui/"]);
    // Finishing the target recorded a `coordination_target_released` event on the open request,
    // so the request has moved on from the version the requester first saw.
    let current = store
        .coordination_list(&requester, &CoordinationListQuery::default(), now + 5)
        .unwrap()
        .requests
        .into_iter()
        .find(|candidate| candidate.id == request.id)
        .unwrap();
    assert!(current.updated_seq > request.updated_seq);
    assert_eq!(current.status, request.status);
    let resolved = store
        .coordination_resolve(
            &requester,
            &CoordinationResolve {
                request_id: request.id,
                operation_id: "scope-applied".into(),
                session: "expanding-session".into(),
                expected_updated_seq: current.updated_seq,
            },
            now + 6,
        )
        .unwrap();
    assert_eq!(resolved.status, CoordinationStatus::Resolved);
}

fn response_projection_does_not_change_status(db: &Db) {
    let store = db.open();
    let first_lead = token(&*store, 20, "first-lead");
    let second_lead = token(&*store, 21, "second-lead");
    let requester = token(&*store, 22, "many-replies-requester");
    let now = unix_now();
    store
        .work_start(
            &first_lead,
            &start_scope(
                "first-target",
                "first-session",
                WorkRole::Lead,
                None,
                &["src/a/**"],
            ),
            now,
        )
        .unwrap();
    store
        .work_start(
            &second_lead,
            &start_scope(
                "second-target",
                "second-session",
                WorkRole::Lead,
                None,
                &["src/b/**"],
            ),
            now + 1,
        )
        .unwrap();
    let request = store
        .work_start(
            &requester,
            &start_scope(
                "many-replies",
                "request-session",
                WorkRole::Lead,
                None,
                &["src/**"],
            ),
            now + 2,
        )
        .unwrap()
        .coordination_request
        .unwrap();
    let first_target = request
        .targets
        .iter()
        .find(|target| target.lead.token == first_lead)
        .unwrap();
    let second_target = request
        .targets
        .iter()
        .find(|target| target.lead.token == second_lead)
        .unwrap();
    let mut current = store
        .coordination_reply(
            &second_lead,
            &CoordinationReply {
                request_id: request.id.clone(),
                operation_id: "second-only-reply".into(),
                target_work_id: second_target.work_id.clone(),
                session: "second-session".into(),
                expected_updated_seq: request.updated_seq,
                kind: CoordinationResponseKind::Decline,
                note: None,
                via: agentscm::hub::handoff::Via::Agent,
            },
            now + 3,
        )
        .unwrap();
    for index in 0..129 {
        current = store
            .coordination_reply(
                &first_lead,
                &CoordinationReply {
                    request_id: request.id.clone(),
                    operation_id: format!("first-repeat-{index}"),
                    target_work_id: first_target.work_id.clone(),
                    session: "first-session".into(),
                    expected_updated_seq: current.updated_seq,
                    kind: CoordinationResponseKind::ProposeSplit,
                    note: None,
                    via: agentscm::hub::handoff::Via::Agent,
                },
                now + 4 + index,
            )
            .unwrap();
    }
    assert_eq!(current.status, CoordinationStatus::Responded);
    assert!(!current.responses_complete);
    assert_eq!(current.responses.len(), 128);
    assert!(
        current
            .responses
            .iter()
            .all(|response| response.target_work_id == first_target.work_id)
    );
}

fn generation_upgrade_preserves_only_current_lead(db: &Db) {
    let migrations = if db._pg.is_some() {
        vec![
            include_str!("../../src/hub/migrations/0001_core.pg.sql"),
            include_str!("../../src/hub/migrations/0002_login.pg.sql"),
            include_str!("../../src/hub/migrations/0003_intents.pg.sql"),
            include_str!("../../src/hub/migrations/0004_github.pg.sql"),
            include_str!("../../src/hub/migrations/0005_publish.pg.sql"),
            include_str!("../../src/hub/migrations/0006_sessions.pg.sql"),
            include_str!("../../src/hub/migrations/0007_warn_once.pg.sql"),
            include_str!("../../src/hub/migrations/0008_read_only_tokens.pg.sql"),
            include_str!("../../src/hub/migrations/0009_handoffs.pg.sql"),
            include_str!("../../src/hub/migrations/0010_work.pg.sql"),
            include_str!("../../src/hub/migrations/0011_work_subscribers.pg.sql"),
            include_str!("../../src/hub/migrations/0012_work_subscriber_sessions.pg.sql"),
        ]
    } else {
        vec![
            include_str!("../../src/hub/migrations/0001_core.sqlite.sql"),
            include_str!("../../src/hub/migrations/0002_login.sqlite.sql"),
            include_str!("../../src/hub/migrations/0003_intents.sqlite.sql"),
            include_str!("../../src/hub/migrations/0004_github.sqlite.sql"),
            include_str!("../../src/hub/migrations/0005_publish.sqlite.sql"),
            include_str!("../../src/hub/migrations/0006_sessions.sqlite.sql"),
            include_str!("../../src/hub/migrations/0007_warn_once.sqlite.sql"),
            include_str!("../../src/hub/migrations/0008_read_only_tokens.sqlite.sql"),
            include_str!("../../src/hub/migrations/0009_handoffs.sqlite.sql"),
            include_str!("../../src/hub/migrations/0010_work.sqlite.sql"),
            include_str!("../../src/hub/migrations/0011_work_subscribers.sqlite.sql"),
            include_str!("../../src/hub/migrations/0012_work_subscriber_sessions.sqlite.sql"),
        ]
    };
    db.exec(&migrations.join("\n"));
    db.exec(
        "CREATE TABLE hub_schema_migrations (
             version BIGINT PRIMARY KEY, applied_at BIGINT NOT NULL
         );
         INSERT INTO hub_schema_migrations (version,applied_at) VALUES
             (1,0),(2,0),(3,0),(4,0),(5,0),(6,0),(7,0),(8,0),(9,0),(10,0),(11,0),(12,0);
         INSERT INTO hub_users (id,provider,host,forge_user_id,login,created_at)
             VALUES ('usr_upgrade','github','github.com',99,'upgrade',0);
         INSERT INTO hub_agent_tokens
             (id,owner,name,repository,digest,expires_at,created_at,repository_id)
             VALUES
             ('tok_old','usr_upgrade','old','acme/widgets','old-digest',4102444800,0,101),
             ('tok_current','usr_upgrade','current','acme/widgets','current-digest',4102444800,0,101),
             ('tok_helper','usr_upgrade','helper','acme/widgets','helper-digest',4102444800,0,101);
         INSERT INTO hub_work_items
             (id,provider,host,repository,repository_id,owner,lead_token,lead_agent,
              lead_session,generation,request_key,title,state,scope,snapshot,created_at,updated_at)
             VALUES
             ('wrk_upgrade','github','github.com','acme/widgets',101,'usr_upgrade',
              'tok_current','current','current-session',3,'upgrade-work','Upgrade work',
              'active','[\"src/\"]','{}',0,0);
         INSERT INTO hub_work_participants
             (work,token,owner,agent,session,role,created_at,updated_at)
             VALUES
             ('wrk_upgrade','tok_old','usr_upgrade','old','old-session','helper',0,0),
             ('wrk_upgrade','tok_current','usr_upgrade','current','current-session','lead',0,0),
             ('wrk_upgrade','tok_helper','usr_upgrade','helper','helper-session','helper',0,0);",
    );

    drop(db.open());
    let declaration_version: (i64, Option<i64>) = if db._pg.is_some() {
        let row = postgres::Client::connect(&db.url, postgres::NoTls)
            .unwrap()
            .query_one(
                "SELECT d.event_seq,w.updated_seq FROM hub_work_declarations d
                 JOIN hub_work_items w ON w.id=d.work WHERE w.id='wrk_upgrade'",
                &[],
            )
            .unwrap();
        (row.get(0), row.get(1))
    } else {
        rusqlite::Connection::open(db.url.strip_prefix("sqlite://").unwrap())
            .unwrap()
            .query_row(
                "SELECT d.event_seq,w.updated_seq FROM hub_work_declarations d
                 JOIN hub_work_items w ON w.id=d.work WHERE w.id='wrk_upgrade'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap()
    };
    // Upgrade succeeds without inventing a Work or declaration version for the legacy row.
    assert_eq!(declaration_version, (0, None));
    assert_eq!(
        db.participant_generations(),
        vec![
            ("current-session".into(), 3),
            ("helper-session".into(), 1),
            ("old-session".into(), 1),
        ]
    );
}

fn quotas_and_unavailable_requester_cleanup_are_enforced(db: &Db) {
    let store = db.open();
    let incumbent = token(&*store, 30, "quota-incumbent");
    let requester = token(&*store, 31, "quota-requester");
    let now = unix_now();
    store
        .work_start(
            &incumbent,
            &start("quota-incumbent", "incumbent-session"),
            now,
        )
        .unwrap();

    let mut requests = Vec::new();
    for index in 0..8 {
        requests.push(
            store
                .work_start(
                    &requester,
                    &start(&format!("quota-request-{index}"), "requester-session"),
                    now + index + 1,
                )
                .unwrap()
                .coordination_request
                .unwrap(),
        );
    }
    assert!(matches!(
        store.work_start(
            &requester,
            &start("quota-request-8", "requester-session"),
            now + 9,
        ),
        Err(HubError::WorkCapacity(c)) if c.limit == CapacityLimit::CoordinationRequests
    ));

    let cleanup = CoordinationWithdraw {
        request_id: requests[0].id.clone(),
        operation_id: "target-cleanup".into(),
        session: "incumbent-session".into(),
        expected_updated_seq: requests[0].updated_seq,
        target: None,
        via_hub: false,
    };
    assert!(matches!(
        store.coordination_withdraw(&incumbent, &cleanup, now + 10),
        Err(HubError::Unauthorized)
    ));
    assert!(store.revoke_agent_token(&requester).unwrap());
    let withdrawn = store
        .coordination_withdraw(&incumbent, &cleanup, now + 11)
        .unwrap();
    assert_eq!(withdrawn.status, CoordinationStatus::Withdrawn);
    let replay = store
        .coordination_withdraw(&incumbent, &cleanup, now + 12)
        .unwrap();
    assert_eq!(replay.updated_seq, withdrawn.updated_seq);

    let active = store
        .coordination_list(
            &incumbent,
            &CoordinationListQuery {
                limit: 1,
                active_only: true,
                ..CoordinationListQuery::default()
            },
            now + 13,
        )
        .unwrap();
    assert_eq!(active.requests.len(), 1);
    assert!(!active.complete);
    let later = CoordinationWithdraw {
        request_id: requests[2].id.clone(),
        operation_id: "target-cleanup-after-watermark".into(),
        session: "incumbent-session".into(),
        expected_updated_seq: requests[2].updated_seq,
        target: None,
        via_hub: false,
    };
    store
        .coordination_withdraw(&incumbent, &later, now + 14)
        .unwrap();
    let continuation = store
        .coordination_list(
            &incumbent,
            &CoordinationListQuery {
                after: Some(active.next_after),
                through: Some(active.through),
                active_only: true,
                ..CoordinationListQuery::default()
            },
            now + 15,
        )
        .unwrap();
    assert!(
        continuation
            .requests
            .iter()
            .any(|request| request.id == later.request_id)
    );
    let fresh = store
        .coordination_list(
            &incumbent,
            &CoordinationListQuery {
                active_only: true,
                ..CoordinationListQuery::default()
            },
            now + 16,
        )
        .unwrap();
    assert!(
        !fresh
            .requests
            .iter()
            .any(|request| request.id == later.request_id)
    );

    let expired = token(&*store, 32, "expired-requester");
    let expired_request = store
        .work_start(
            &expired,
            &start("expired-request", "expired-session"),
            now + 17,
        )
        .unwrap()
        .coordination_request
        .unwrap();
    db.expire_token(&expired);
    assert_eq!(
        store
            .coordination_withdraw(
                &incumbent,
                &CoordinationWithdraw {
                    request_id: expired_request.id,
                    operation_id: "expired-cleanup".into(),
                    session: "incumbent-session".into(),
                    expected_updated_seq: expired_request.updated_seq,
                    target: None,
                    via_hub: false,
                },
                now + 18,
            )
            .unwrap()
            .status,
        CoordinationStatus::Withdrawn
    );

    let disabled = token(&*store, 33, "disabled-requester");
    let disabled_request = store
        .work_start(
            &disabled,
            &start("disabled-request", "disabled-session"),
            now + 19,
        )
        .unwrap()
        .coordination_request
        .unwrap();
    db.disable_token_owner(&disabled);
    assert_eq!(
        store
            .coordination_withdraw(
                &incumbent,
                &CoordinationWithdraw {
                    request_id: disabled_request.id,
                    operation_id: "disabled-cleanup".into(),
                    session: "incumbent-session".into(),
                    expected_updated_seq: disabled_request.updated_seq,
                    target: None,
                    via_hub: false,
                },
                now + 20,
            )
            .unwrap()
            .status,
        CoordinationStatus::Withdrawn
    );

    for index in 0..8 {
        store
            .work_briefing(
                &incumbent,
                &BriefingQuery {
                    subscriber: Some(format!("subscriber-{index}")),
                    ..BriefingQuery::default()
                },
                now + 30 + index,
            )
            .unwrap();
    }
    assert!(matches!(
        store.work_briefing(
            &incumbent,
            &BriefingQuery {
                subscriber: Some("subscriber-8".into()),
                ..BriefingQuery::default()
            },
            now + 38,
        ),
        Err(HubError::Invalid(message))
            if message.contains("too many unacknowledged briefing deliveries")
    ));

    let second_incumbent_token = token(&*store, 30, "quota-incumbent-second");
    for index in 0..8 {
        store
            .work_briefing(
                &second_incumbent_token,
                &BriefingQuery {
                    subscriber: Some(format!("second-subscriber-{index}")),
                    ..BriefingQuery::default()
                },
                now + 40 + index,
            )
            .unwrap();
    }
    let third_incumbent_token = token(&*store, 30, "quota-incumbent-third");
    assert!(matches!(
        store.work_briefing(
            &third_incumbent_token,
            &BriefingQuery {
                subscriber: Some("owner-overflow".into()),
                ..BriefingQuery::default()
            },
            now + 48,
        ),
        Err(HubError::Invalid(message))
            if message.contains("too many unacknowledged briefing deliveries")
    ));

    let owner_requesters = (0..4)
        .map(|index| token(&*store, 34, &format!("owner-requester-{index}")))
        .collect::<Vec<_>>();
    for index in 0..25 {
        let request_token = &owner_requesters[index / 8];
        store
            .work_start(
                request_token,
                &start(
                    &format!("owner-request-{index}"),
                    &format!("owner-session-{}", index / 8),
                ),
                now + 50 + index as i64,
            )
            .unwrap();
    }
    assert!(matches!(
        store.work_start(
            &owner_requesters[3],
            &start("owner-request-25", "owner-session-3"),
            now + 75,
        ),
        Err(HubError::WorkCapacity(c)) if c.limit == CapacityLimit::CoordinationRequests
    ));
}

fn compacted_coordination_reports_a_gap_and_reclaims_capacity(db: &Db) {
    let store = db.open();
    let incumbent = token(&*store, 40, "compact-incumbent");
    let active_requester = token(&*store, 41, "compact-active");
    let closed_requester = token(&*store, 42, "compact-closed");
    let now = unix_now();
    let old = now - 200_000;
    store
        .work_start(
            &incumbent,
            &start("compact-incumbent", "incumbent-session"),
            old,
        )
        .unwrap();
    let active = store
        .work_start(
            &active_requester,
            &start("compact-active", "active-session"),
            old + 1,
        )
        .unwrap()
        .coordination_request
        .unwrap();
    let closed = store
        .work_start(
            &closed_requester,
            &start("compact-closed", "closed-session"),
            old + 2,
        )
        .unwrap()
        .coordination_request
        .unwrap();
    let withdrawal = CoordinationWithdraw {
        request_id: closed.id.clone(),
        operation_id: "compact-withdraw".into(),
        session: "closed-session".into(),
        expected_updated_seq: closed.updated_seq,
        target: None,
        via_hub: false,
    };
    store
        .coordination_withdraw(&closed_requester, &withdrawal, old + 3)
        .unwrap();
    let charged_before = db.charged_bytes();
    let scope = WorkRepositoryScope {
        provider: "github".into(),
        host: "github.com".into(),
        repository: REPOSITORY.into(),
        repository_id: Some(REPOSITORY_ID),
    };
    let cutoff = now - 100_000;
    let preview = store.work_compact(&scope, cutoff, false, now).unwrap();
    assert_eq!(preview.deleted_coordination_requests, 1);
    assert!(preview.coordination_retention_floor >= closed.updated_seq);
    let applied = store.work_compact(&scope, cutoff, true, now).unwrap();
    assert_eq!(applied.deleted_coordination_requests, 1);
    assert!(db.charged_bytes() < charged_before);

    let history = store
        .coordination_list(&incumbent, &CoordinationListQuery::default(), now + 1)
        .unwrap();
    assert!(history.retention_gap);
    assert!(
        history
            .requests
            .iter()
            .any(|request| request.id == active.id)
    );
    assert!(
        !history
            .requests
            .iter()
            .any(|request| request.id == closed.id)
    );
    assert!(matches!(
        store.coordination_withdraw(&closed_requester, &withdrawal, now + 2),
        Err(HubError::Invalid(message)) if message.contains("unknown coordination request")
    ));
    let after_deleted_high_water = store
        .coordination_list(
            &incumbent,
            &CoordinationListQuery {
                after: Some(applied.coordination_retention_floor),
                ..CoordinationListQuery::default()
            },
            now + 3,
        )
        .unwrap();
    assert!(after_deleted_high_water.through >= after_deleted_high_water.from);
    assert!(!after_deleted_high_water.retention_gap);
}

fn default_work_session_is_durably_aliased_to_the_first_hook(db: &Db) {
    let store = db.open();
    let lead = token(&*store, 50, "alias-lead");
    let now = unix_now();
    store
        .touch_session(
            &lead,
            &SessionCall {
                session: None,
                via: "mcp".into(),
                task: None,
                paths: Vec::new(),
                deny: false,
                report: false,
            },
            now,
        )
        .unwrap();
    let item = store
        .work_start(&lead, &start("alias-work", "default"), now + 1)
        .unwrap()
        .item;
    let separate = store
        .touch_session(
            &lead,
            &SessionCall {
                session: Some("named-mcp".into()),
                via: "mcp".into(),
                task: None,
                paths: Vec::new(),
                deny: false,
                report: false,
            },
            now + 2,
        )
        .unwrap();
    assert_eq!(separate.session, "named-mcp");
    let separate_hook = store
        .touch_session(
            &lead,
            &SessionCall {
                session: Some("named-mcp".into()),
                via: "hook:claude".into(),
                task: None,
                paths: vec!["independent/file.rs".into()],
                deny: true,
                report: true,
            },
            now + 3,
        )
        .unwrap();
    assert_eq!(separate_hook.session, "named-mcp");
    let delayed = now + 1_201;
    assert!(matches!(
        store.touch_session(
            &lead,
            &SessionCall {
                session: Some("observed-hook".into()),
                via: "hook:claude".into(),
                task: None,
                paths: vec!["outside/file.rs".into()],
                deny: true,
                report: true,
            },
            delayed,
        ),
        Err(HubError::Invalid(message)) if message.contains("not covered")
    ));
    let touched = store
        .touch_session(
            &lead,
            &SessionCall {
                session: Some("observed-hook".into()),
                via: "hook:claude".into(),
                task: None,
                paths: vec!["src/lib.rs".into()],
                deny: true,
                report: true,
            },
            delayed + 1,
        )
        .unwrap();
    assert_eq!(touched.session, "default");
    // Once `default` is adopted, a later new native session is its own session, not refused.
    let alternate = store
        .touch_session(
            &lead,
            &SessionCall {
                session: Some("alternate-hook".into()),
                via: "hook:claude".into(),
                task: None,
                paths: Vec::new(),
                deny: false,
                report: true,
            },
            delayed + 2,
        )
        .unwrap();
    assert_eq!(alternate.session, "alternate-hook");
    assert!(store.default_session_bound(&lead).unwrap());
    assert_eq!(
        store
            .session_name_for(&lead, Some("observed-hook"), delayed + 3)
            .unwrap(),
        "default"
    );
    let paused = store
        .work_checkpoint(
            &lead,
            &checkpoint(
                &item.id,
                "pause-alias-work",
                "default",
                item.updated_seq,
                CheckpointKind::Pause,
                None,
            ),
            delayed + 4,
        )
        .unwrap();
    assert!(matches!(
        store.touch_session(
            &lead,
            &SessionCall {
                session: Some("observed-hook".into()),
                via: "hook:claude".into(),
                task: None,
                paths: vec!["src/lib.rs".into()],
                deny: true,
                report: true,
            },
            delayed + 5,
        ),
        Err(HubError::Invalid(message)) if message.contains("not covered")
    ));
    assert_eq!(paused.item.state, agentscm::hub::work::WorkState::Paused);

    let mut resume = start_scope(
        "alias-work",
        "default",
        WorkRole::Lead,
        Some(&item.id),
        &["src/**"],
    );
    resume.takeover_generation = Some(paused.item.generation);
    let resumed = store.work_start(&lead, &resume, delayed + 6).unwrap().item;
    let mut finish = checkpoint(
        &resumed.id,
        "finish-alias-work",
        "default",
        resumed.updated_seq,
        CheckpointKind::Finish,
        None,
    );
    finish.generation = resumed.generation;
    store.work_checkpoint(&lead, &finish, delayed + 7).unwrap();
    assert!(matches!(
        store.touch_session(
            &lead,
            &SessionCall {
                session: Some("observed-hook".into()),
                via: "hook:claude".into(),
                task: None,
                paths: vec!["src/lib.rs".into()],
                deny: true,
                report: true,
            },
            delayed + 8,
        ),
        Err(HubError::Invalid(message)) if message.contains("not covered")
    ));

    let old_lead = token(&*store, 51, "alias-old-lead");
    let new_lead = token(&*store, 52, "alias-new-lead");
    store
        .touch_session(
            &old_lead,
            &SessionCall {
                session: None,
                via: "mcp".into(),
                task: None,
                paths: Vec::new(),
                deny: false,
                report: false,
            },
            delayed + 9,
        )
        .unwrap();
    let handed = store
        .work_start(
            &old_lead,
            &start_scope(
                "alias-takeover",
                "default",
                WorkRole::Lead,
                None,
                &["takeover/**"],
            ),
            delayed + 10,
        )
        .unwrap()
        .item;
    store
        .touch_session(
            &old_lead,
            &SessionCall {
                session: Some("old-hook".into()),
                via: "hook:claude".into(),
                task: None,
                paths: Vec::new(),
                deny: false,
                report: true,
            },
            delayed + 11,
        )
        .unwrap();
    store
        .work_checkpoint(
            &old_lead,
            &checkpoint(
                &handed.id,
                "handover-alias-work",
                "default",
                handed.updated_seq,
                CheckpointKind::Handover,
                None,
            ),
            delayed + 12,
        )
        .unwrap();
    let mut takeover = start_scope(
        "alias-takeover",
        "new-session",
        WorkRole::Lead,
        Some(&handed.id),
        &["takeover/**"],
    );
    takeover.takeover_generation = Some(handed.generation);
    store
        .work_start(&new_lead, &takeover, delayed + 13)
        .unwrap();
    assert!(matches!(
        store.touch_session(
            &old_lead,
            &SessionCall {
                session: Some("old-hook".into()),
                via: "hook:claude".into(),
                task: None,
                paths: vec!["takeover/file.rs".into()],
                deny: true,
                report: true,
            },
            delayed + 14,
        ),
        Err(HubError::Invalid(message)) if message.contains("not covered")
    ));
}

/// A person's answer through the Hub keeps the responding agent as the responder and says
/// `hub` beside it; the requester's inbox sees it, and an agent's own answer carries no mark.
fn a_person_reply_through_the_hub_is_marked_and_delivered(db: &Db) {
    let store = db.open();
    let incumbent = token(&*store, 1, "incumbent");
    let requester = token(&*store, 2, "requester");
    let now = unix_now();
    store
        .work_start(&incumbent, &start("incumbent", "incumbent-session"), now)
        .unwrap();
    let request = store
        .work_start(
            &requester,
            &start("requester", "requester-session"),
            now + 1,
        )
        .unwrap()
        .coordination_request
        .unwrap();
    let target = request.targets[0].work_id.clone();
    let reply = |operation: &str, seq: i64, via_hub: bool| CoordinationReply {
        request_id: request.id.clone(),
        operation_id: operation.into(),
        target_work_id: target.clone(),
        session: "incumbent-session".into(),
        expected_updated_seq: seq,
        kind: CoordinationResponseKind::ProposeSplit,
        note: Some("I take tax, you take rounding".into()),
        via: if via_hub {
            agentscm::hub::handoff::Via::Hub
        } else {
            agentscm::hub::handoff::Via::Agent
        },
    };
    let by_person = reply("person", request.updated_seq, true);
    let first = store
        .coordination_reply(&incumbent, &by_person, now + 2)
        .unwrap();
    let response = &first.responses[0];
    assert_eq!(response.via.as_deref(), Some("hub"));
    // The responder is still the agent that leads the Work; its owner is the person.
    assert_eq!(response.responder.token, incumbent);
    assert_eq!(response.responder.login, "user1");
    // A retry of the same operation replays instead of answering twice.
    let again = store
        .coordination_reply(&incumbent, &by_person, now + 3)
        .unwrap();
    assert_eq!(again.responses.len(), 1);
    assert_eq!(again.updated_seq, first.updated_seq);

    // The requester's session finds it in its inbox at its next touch, with the mark.
    let inbox = store
        .coordination_inbox(
            &requester,
            &CoordinationInboxQuery {
                session: Some("requester-session".into()),
                ..CoordinationInboxQuery::default()
            },
            now + 4,
        )
        .unwrap();
    assert_eq!(inbox.requests[0].responses[0].via.as_deref(), Some("hub"));
    let data: Vec<String> = store
        .events_after(0, 1000)
        .unwrap()
        .into_iter()
        .filter(|event| event.kind == "coordination_responded")
        .map(|event| event.data)
        .collect();
    assert_eq!(data.len(), 1);
    assert!(data[0].contains("\"via\":\"hub\""), "{data:?}");

    // The agent answering for itself (a different target generation is not needed): no mark.
    let own = reply("agent", first.updated_seq, false);
    let second = store.coordination_reply(&incumbent, &own, now + 5).unwrap();
    assert!(
        second
            .responses
            .iter()
            .any(|response| response.via.is_none() && response.id != first.responses[0].id)
    );
}

/// The lead of an affected Work, or the person who owns it, closes a request whose requester can
/// no longer act. Both stores show the flag for each way a requester becomes unavailable, refuse
/// everyone else, fence on the request sequence and the target generation, and replay safely.
fn a_lead_closes_a_request_whose_requester_is_gone(db: &Db) {
    let store = db.open();
    let incumbent = token(&*store, 1, "incumbent");
    let viewer = mint(&*store, 1, "viewer", true);
    let stranger = token(&*store, 5, "stranger");
    let now = unix_now();
    store
        .work_start(&incumbent, &start("incumbent", "incumbent-session"), now)
        .unwrap();
    let requesters = [
        token(&*store, 2, "revoked"),
        token(&*store, 3, "expired"),
        token(&*store, 4, "disabled"),
        token(&*store, 6, "alive"),
    ];
    let requests: Vec<CoordinationRequest> = requesters
        .iter()
        .enumerate()
        .map(|(index, requester)| {
            store
                .work_start(
                    requester,
                    &start_scope(
                        &format!("requester-{index}"),
                        "requester-session",
                        WorkRole::Lead,
                        None,
                        &[&format!("src/area{index}/**")],
                    ),
                    now + 1 + index as i64,
                )
                .unwrap()
                .coordination_request
                .unwrap()
        })
        .collect();
    let list = |at: i64| {
        store
            .coordination_list(&incumbent, &CoordinationListQuery::default(), at)
            .unwrap()
            .requests
    };
    // Every requester can act, so nothing is flagged.
    assert!(list(now + 10).iter().all(|r| !r.requester_unavailable));

    // Revoked, expired and disabled-owner requesters are each reported as unavailable.
    assert!(store.revoke_agent_token(&requesters[0]).unwrap());
    db.expire_token(&requesters[1]);
    db.disable_token_owner(&requesters[2]);
    let flagged: Vec<(String, bool)> = list(now + 11)
        .into_iter()
        .map(|r| (r.id, r.requester_unavailable))
        .collect();
    for (index, request) in requests.iter().enumerate() {
        assert_eq!(
            flagged.iter().find(|(id, _)| *id == request.id).unwrap().1,
            index < 3,
            "request {index}"
        );
    }

    let current = |request: &CoordinationRequest| {
        list(now + 12)
            .into_iter()
            .find(|r| r.id == request.id)
            .unwrap()
    };
    let revoked = current(&requests[0]);
    let led = revoked
        .targets
        .iter()
        .find(|target| target.lead.token == incumbent)
        .unwrap();
    let withdraw = |request: &CoordinationRequest, operation: &str, via_hub: bool| {
        let led = request
            .targets
            .iter()
            .find(|target| target.lead.token == incumbent)
            .unwrap();
        CoordinationWithdraw {
            request_id: request.id.clone(),
            operation_id: operation.into(),
            session: "incumbent-session".into(),
            expected_updated_seq: request.updated_seq,
            target: Some(CoordinationWithdrawTarget {
                work_id: led.work_id.clone(),
                generation: led.generation,
            }),
            via_hub,
        }
    };
    let by_person = withdraw(&revoked, "person", true);

    // Nobody else may close it: not another owner, not a read-only token of the lead's owner,
    // not the lead's token under another session, and not while the requester can still act.
    for (caller, call) in [
        (&stranger, by_person.clone()),
        (&viewer, by_person.clone()),
        (
            &incumbent,
            CoordinationWithdraw {
                session: "someone-elses-session".into(),
                ..by_person.clone()
            },
        ),
        (&incumbent, withdraw(&current(&requests[3]), "alive", true)),
    ] {
        assert!(
            matches!(
                store.coordination_withdraw(caller, &call, now + 13),
                Err(HubError::Unauthorized)
            ),
            "{caller}"
        );
    }

    // A person never closes a request as its requester, even one that can act; the agent still can.
    let alive = current(&requests[3]);
    let own = CoordinationWithdraw {
        request_id: alive.id.clone(),
        operation_id: "own".into(),
        session: "requester-session".into(),
        expected_updated_seq: alive.updated_seq,
        target: None,
        via_hub: true,
    };
    assert!(matches!(
        store.coordination_withdraw(&requesters[3], &own, now + 13),
        Err(HubError::Unauthorized)
    ));
    let own = CoordinationWithdraw {
        via_hub: false,
        ..own
    };
    assert_eq!(
        store
            .coordination_withdraw(&requesters[3], &own, now + 13)
            .unwrap()
            .status,
        CoordinationStatus::Withdrawn
    );

    // A stale sequence, another generation and another Work are each refused, and change nothing.
    for (stale, message) in [
        (
            CoordinationWithdraw {
                expected_updated_seq: revoked.updated_seq + 99,
                ..by_person.clone()
            },
            "request changed",
        ),
        (
            CoordinationWithdraw {
                target: Some(CoordinationWithdrawTarget {
                    work_id: led.work_id.clone(),
                    generation: led.generation + 1,
                }),
                ..by_person.clone()
            },
            "target changed",
        ),
        (
            CoordinationWithdraw {
                target: Some(CoordinationWithdrawTarget {
                    work_id: format!("wrk_{:032x}", 1),
                    generation: led.generation,
                }),
                ..by_person.clone()
            },
            "target changed",
        ),
    ] {
        match store.coordination_withdraw(&incumbent, &stale, now + 14) {
            Err(HubError::Invalid(reason)) => assert!(reason.contains(message), "{reason}"),
            other => panic!("expected a refusal, got {other:?}"),
        }
    }
    assert_eq!(current(&requests[0]).status, revoked.status);

    // The lead's owner closes it for the lead; the record says a person did, and why.
    let closed = store
        .coordination_withdraw(&incumbent, &by_person, now + 15)
        .unwrap();
    assert_eq!(closed.status, CoordinationStatus::Withdrawn);
    let withdrawn_events = || -> Vec<String> {
        store
            .events_after(0, 1000)
            .unwrap()
            .into_iter()
            .filter(|event| event.kind == "coordination_withdrawn")
            .map(|event| event.data)
            .filter(|data| data.contains("requester_unavailable"))
            .collect()
    };
    let events = withdrawn_events();
    assert_eq!(events.len(), 1);
    assert!(
        events[0].contains("\"reason\":\"requester_unavailable\""),
        "{events:?}"
    );
    assert!(events[0].contains("\"via\":\"hub\""), "{events:?}");

    // A retry replays the same close; a new attempt on the closed request is refused.
    let again = store
        .coordination_withdraw(&incumbent, &by_person, now + 16)
        .unwrap();
    assert_eq!(again.updated_seq, closed.updated_seq);
    assert_eq!(withdrawn_events().len(), 1);
    assert!(matches!(
        store.coordination_withdraw(&incumbent, &withdraw(&closed, "second", true), now + 17),
        Err(HubError::Invalid(reason)) if reason.contains("closed")
    ));

    // Nothing was released: the lead still holds its Work, and the request left the active list.
    let item = store
        .work_get(&incumbent, &led.work_id, now + 18)
        .unwrap()
        .unwrap();
    assert_eq!(item.lead_token, incumbent);
    let active = store
        .coordination_list(
            &incumbent,
            &CoordinationListQuery {
                active_only: true,
                ..CoordinationListQuery::default()
            },
            now + 19,
        )
        .unwrap();
    assert!(active.requests.iter().all(|r| r.id != revoked.id));

    // The lead's own agent closes another one directly, with no fence and no mark.
    let expired = current(&requests[1]);
    let direct = CoordinationWithdraw {
        target: None,
        ..withdraw(&expired, "agent", false)
    };
    let closed = store
        .coordination_withdraw(&incumbent, &direct, now + 20)
        .unwrap();
    assert_eq!(closed.status, CoordinationStatus::Withdrawn);
    let events = withdrawn_events();
    assert_eq!(events.len(), 2);
    assert!(!events[1].contains("\"via\""), "{events:?}");
}

macro_rules! parity_case {
    ($name:ident, $case:ident) => {
        mod $name {
            #[test]
            fn sqlite() {
                super::$case(&super::Db::sqlite());
            }

            #[test]
            fn postgres() {
                if let Some(db) = super::Db::postgres() {
                    super::$case(&db);
                }
            }
        }
    };
}

parity_case!(
    person_reply_through_the_hub,
    a_person_reply_through_the_hub_is_marked_and_delivered
);
parity_case!(
    close_orphaned_request,
    a_lead_closes_a_request_whose_requester_is_gone
);
parity_case!(
    queued_expansion,
    queued_expansion_requires_the_proposed_scope
);
parity_case!(
    response_projection,
    response_projection_does_not_change_status
);
parity_case!(
    generation_upgrade,
    generation_upgrade_preserves_only_current_lead
);
parity_case!(
    quotas_and_cleanup,
    quotas_and_unavailable_requester_cleanup_are_enforced
);
parity_case!(
    coordination_compaction,
    compacted_coordination_reports_a_gap_and_reclaims_capacity
);
parity_case!(
    default_session_alias,
    default_work_session_is_durably_aliased_to_the_first_hook
);

fn agent(seed: &str) -> CoordinationAgent {
    CoordinationAgent {
        token: format!("tok_{seed:0<32}"),
        agent: seed.repeat(255 / seed.len().max(1)),
        session: seed.repeat(255 / seed.len().max(1)),
        owner: format!("usr_{seed:0<32}"),
        login: seed.repeat(255 / seed.len().max(1)),
    }
}

#[test]
fn maximum_projected_coordination_request_fits_the_json_budget() {
    let path = "p".repeat(1024);
    let requester = agent("r");
    let targets = (0..100)
        .map(|index| CoordinationTarget {
            work_id: format!("wrk_{index:032x}"),
            lead: agent("l"),
            generation: 1,
            reasons: (0..4)
                .map(|_| WorkPathOverlap {
                    your_path: path.clone(),
                    their_path: path.clone(),
                })
                .collect(),
            reasons_complete: false,
        })
        .collect::<Vec<_>>();
    let acknowledgements = (0..128)
        .map(|index| CoordinationAcknowledgement {
            target_work_id: Some(format!("wrk_{:032x}", index % 100)),
            agent: agent("a"),
            generation: 1,
            through_seq: index,
            acknowledged_at: index,
            seq: index,
        })
        .collect::<Vec<_>>();
    let responses = (0..128)
        .map(|index| CoordinationResponse {
            id: format!("crp_{index:032x}"),
            operation_id: format!("op_{index:032x}"),
            target_work_id: format!("wrk_{:032x}", index % 100),
            responder: agent("s"),
            target_generation: 1,
            kind: CoordinationResponseKind::ProposeSplit,
            note: Some("n".repeat(500)),
            via: None,
            created_at: index,
            seq: index,
        })
        .collect::<Vec<_>>();
    let request = CoordinationRequest {
        id: format!("crq_{:032x}", 1),
        requester,
        proposal: CoordinationProposal {
            work_id: None,
            source: None,
            request_key: Some(format!("req_{:032x}", 1)),
            title: "t".repeat(500),
            scope: (0..256)
                .map(|index| format!("{index:03}{}", "p".repeat(1021)))
                .collect(),
        },
        targets,
        status: CoordinationStatus::Responded,
        acknowledgements,
        acknowledgements_complete: false,
        responses,
        responses_complete: false,
        created_at: 1,
        updated_at: 2,
        created_seq: 1,
        updated_seq: 2,
        requester_unavailable: true,
    };

    let encoded = serde_json::to_vec(&request).unwrap();
    assert!(encoded.len() < 2 * 1024 * 1024, "{} bytes", encoded.len());
}

fn finished_default_work_binds_no_hook_session(db: &Db) {
    let store = db.open();
    let lead = token(&*store, 53, "alias-finished");
    let now = unix_now();
    let item = store
        .work_start(&lead, &start("alias-finished-work", "default"), now)
        .unwrap()
        .item;
    let mut finish = checkpoint(
        &item.id,
        "finish-alias-finished-work",
        "default",
        item.updated_seq,
        CheckpointKind::Finish,
        None,
    );
    finish.generation = item.generation;
    store.work_checkpoint(&lead, &finish, now + 1).unwrap();
    // Nothing unfinished is led by `default`, so a new native session stays its own.
    let touched = store
        .touch_session(
            &lead,
            &SessionCall {
                session: Some("after-finish-hook".into()),
                via: "hook:claude".into(),
                task: None,
                paths: Vec::new(),
                deny: false,
                report: true,
            },
            now + 2,
        )
        .unwrap();
    assert_eq!(touched.session, "after-finish-hook");
    assert!(!store.default_session_bound(&lead).unwrap());
}

parity_case!(
    finished_default_alias,
    finished_default_work_binds_no_hook_session
);

fn named_overlap_consent_is_fenced_and_not_an_admission(db: &Db) {
    use agentscm::hub::work_overlap::{WorkOverlapConsentRequest, WorkOverlapConsentState};
    let store = db.open();
    let incumbent = token(&*store, 1, "incumbent");
    let requester = token(&*store, 2, "requester");
    let other = token(&*store, 3, "other");
    let now = unix_now();
    let target = store
        .work_start(&incumbent, &start("incumbent", "lead"), now)
        .unwrap()
        .item;
    let proposal = start("requester", "requester");
    let request = store
        .work_start(&requester, &proposal, now + 1)
        .unwrap()
        .coordination_request
        .unwrap();
    let consent = WorkOverlapConsentRequest {
        request_id: request.id.clone(),
        target_work_id: target.id.clone(),
        operation_id: "consent-1".into(),
        session: "lead".into(),
        expected_updated_seq: request.updated_seq,
        target_generation: target.generation,
        target_updated_seq: target.updated_seq,
        paths: vec!["src/a.rs".into()],
    };
    assert!(matches!(
        store.work_overlap_consent(&requester, &consent, now + 2),
        Err(HubError::Unauthorized)
    ));
    assert!(matches!(
        store.work_overlap_consent(&other, &consent, now + 2),
        Err(HubError::Unauthorized)
    ));
    for paths in [
        vec!["src/**".into()],
        vec!["outside.rs".into()],
        vec!["src/a.rs".into(), "src/a.rs".into()],
    ] {
        assert!(
            store
                .work_overlap_consent(
                    &incumbent,
                    &WorkOverlapConsentRequest {
                        paths,
                        ..consent.clone()
                    },
                    now + 2
                )
                .is_err()
        );
    }
    let scope = WorkRepositoryScope {
        provider: "github".into(),
        host: "github.com".into(),
        repository: REPOSITORY.into(),
        repository_id: Some(REPOSITORY_ID),
    };
    let before_charge = db.charged_bytes();
    let before_recount = store.work_payload_used(&scope).unwrap();
    let granted = store
        .work_overlap_consent(&incumbent, &consent, now + 3)
        .unwrap();
    assert_eq!(
        db.charged_bytes() - before_charge,
        store.work_payload_used(&scope).unwrap() - before_recount
    );
    assert!(db.charged_bytes() > before_charge);
    assert_eq!(granted.state, WorkOverlapConsentState::AwaitingRequester);
    assert_eq!(granted.consent.paths, ["src/a.rs"]);
    assert!(granted.consent.accepted.is_none());
    let replay = store
        .work_overlap_consent(&incumbent, &consent, now + 4)
        .unwrap();
    assert_eq!(granted, replay);
    assert!(
        store
            .work_overlap_consent(
                &incumbent,
                &WorkOverlapConsentRequest {
                    paths: vec!["src/b.rs".into()],
                    ..consent.clone()
                },
                now + 4
            )
            .is_err()
    );
    assert_eq!(
        store
            .work_overlap_consents(&requester, &request.id, "requester", now + 4)
            .unwrap(),
        vec![granted.clone()]
    );
    assert!(matches!(
        store.work_overlap_consents(&other, &request.id, "other", now + 4),
        Err(HubError::Unauthorized)
    ));
    // Target consent by itself still grants no admission or reservation.
    assert!(
        !store
            .work_start(&requester, &proposal, now + 5)
            .unwrap()
            .created
    );
    let progress = store
        .work_checkpoint(
            &incumbent,
            &checkpoint(
                &target.id,
                "progress",
                "lead",
                target.updated_seq,
                CheckpointKind::Progress,
                None,
            ),
            now + 6,
        )
        .unwrap();
    assert_eq!(
        store
            .work_overlap_consent(&incumbent, &consent, now + 7)
            .unwrap()
            .state,
        WorkOverlapConsentState::AwaitingRequester
    );
    // A scope change away and back must never revive this grant, even with the same lead.
    let narrowed = store
        .work_checkpoint(
            &incumbent,
            &checkpoint(
                &target.id,
                "narrow",
                "lead",
                progress.item.updated_seq,
                CheckpointKind::Progress,
                Some(&["src/a.rs"]),
            ),
            now + 8,
        )
        .unwrap();
    let _restored = store
        .work_checkpoint(
            &incumbent,
            &checkpoint(
                &target.id,
                "restore",
                "lead",
                narrowed.item.updated_seq,
                CheckpointKind::Progress,
                Some(&["src/**"]),
            ),
            now + 9,
        )
        .unwrap();
    assert_eq!(
        store
            .work_overlap_consent(&incumbent, &consent, now + 10)
            .unwrap()
            .state,
        WorkOverlapConsentState::Invalidated
    );
}

fn named_overlap_consent_lapses_and_decline_revokes(db: &Db) {
    use agentscm::hub::work_overlap::{WorkOverlapConsentRequest, WorkOverlapConsentState};
    let store = db.open();
    let incumbent = token(&*store, 11, "incumbent");
    let requester = token(&*store, 12, "requester");
    let now = unix_now();
    let target = store
        .work_start(&incumbent, &start("target", "lead"), now)
        .unwrap()
        .item;
    let request = store
        .work_start(&requester, &start("requester", "requester"), now + 1)
        .unwrap()
        .coordination_request
        .unwrap();
    let consent = WorkOverlapConsentRequest {
        request_id: request.id.clone(),
        target_work_id: target.id.clone(),
        operation_id: "grant".into(),
        session: "lead".into(),
        expected_updated_seq: request.updated_seq,
        target_generation: target.generation,
        target_updated_seq: target.updated_seq,
        paths: vec!["src/a.rs".into()],
    };
    let granted = store
        .work_overlap_consent(&incumbent, &consent, now + 2)
        .unwrap();
    store
        .coordination_reply(
            &incumbent,
            &CoordinationReply {
                request_id: request.id.clone(),
                operation_id: "decline".into(),
                target_work_id: target.id.clone(),
                session: "lead".into(),
                expected_updated_seq: granted.consent.event_seq,
                kind: CoordinationResponseKind::Decline,
                note: Some("keep separate".into()),
                via: agentscm::hub::handoff::Via::Agent,
            },
            now + 3,
        )
        .unwrap();
    assert_eq!(
        store
            .work_overlap_consents(&requester, &request.id, "requester", now + 4)
            .unwrap()[0]
            .state,
        WorkOverlapConsentState::Invalidated
    );
    let request2 = store
        .work_start(&requester, &start("requester2", "requester2"), now + 5)
        .unwrap()
        .coordination_request
        .unwrap();
    let grant2 = WorkOverlapConsentRequest {
        request_id: request2.id.clone(),
        operation_id: "grant2".into(),
        expected_updated_seq: request2.updated_seq,
        ..consent.clone()
    };
    store
        .work_overlap_consent(&incumbent, &grant2, now + 6)
        .unwrap();
    assert_eq!(
        store
            .work_overlap_consents(&requester, &request2.id, "requester2", now + 1201)
            .unwrap()[0]
            .state,
        WorkOverlapConsentState::Invalidated
    );
    // Simulate a reservation release/re-admission through the actual durable columns. The epoch
    // must change even if the same principal and scope return before a later reader.
    db.exec(&format!("UPDATE hub_work_items SET reservation_basis=NULL WHERE id='{}'; UPDATE hub_work_items SET reservation_basis='admitted',reservation_renewed_at={},reservation_expires_at={} WHERE id='{}'",target.id,now+1300,now+2500,target.id));
    assert_eq!(
        store
            .work_overlap_consent(&incumbent, &grant2, now + 1301)
            .unwrap()
            .state,
        WorkOverlapConsentState::Invalidated
    );
}

parity_case!(
    named_overlap_consent,
    named_overlap_consent_is_fenced_and_not_an_admission
);
parity_case!(
    named_overlap_invalidation,
    named_overlap_consent_lapses_and_decline_revokes
);

fn named_overlap_concurrent_retries_keep_one_consent(db: &Db) {
    use agentscm::hub::work_overlap::{WorkOverlapConsentRequest, WorkOverlapConsentState};
    let store = db.open();
    let incumbent = token(&*store, 21, "incumbent");
    let helper = token(&*store, 21, "helper");
    let requester = token(&*store, 22, "requester");
    let now = unix_now();
    let target = store
        .work_start(&incumbent, &start("target", "lead"), now)
        .unwrap()
        .item;
    let joined = store
        .work_start(
            &helper,
            &start_scope(
                "helper",
                "helper",
                WorkRole::Helper,
                Some(&target.id),
                &["src/**"],
            ),
            now + 1,
        )
        .unwrap();
    assert!(joined.joined);
    let target = joined.item;
    let request = store
        .work_start(&requester, &start("requester", "requester"), now + 2)
        .unwrap()
        .coordination_request
        .unwrap();
    let consent = WorkOverlapConsentRequest {
        request_id: request.id.clone(),
        target_work_id: target.id.clone(),
        operation_id: "concurrent".into(),
        session: "lead".into(),
        expected_updated_seq: request.updated_seq,
        target_generation: target.generation,
        target_updated_seq: target.updated_seq,
        paths: vec!["src/a.rs".into()],
    };
    assert!(matches!(
        store.work_overlap_consent(
            &helper,
            &WorkOverlapConsentRequest {
                session: "helper".into(),
                ..consent.clone()
            },
            now + 3
        ),
        Err(HubError::Unauthorized)
    ));
    let barrier = Arc::new(std::sync::Barrier::new(2));
    let results = std::thread::scope(|scope| {
        let tasks = (0..2)
            .map(|_| {
                let store = db.open();
                let barrier = barrier.clone();
                let consent = consent.clone();
                let incumbent = incumbent.clone();
                scope.spawn(move || {
                    barrier.wait();
                    store
                        .work_overlap_consent(&incumbent, &consent, now + 4)
                        .unwrap()
                })
            })
            .collect::<Vec<_>>();
        tasks
            .into_iter()
            .map(|task| task.join().unwrap())
            .collect::<Vec<_>>()
    });
    assert_eq!(results[0], results[1]);
    assert_eq!(
        store
            .work_overlap_consents(&requester, &request.id, "requester", now + 5)
            .unwrap()
            .len(),
        1
    );
    store
        .session_activity(
            &incumbent,
            &SessionCall {
                session: Some("lead".into()),
                via: "mcp".into(),
                task: None,
                paths: Vec::new(),
                deny: false,
                report: true,
            },
            agentscm::hub::work_lifecycle::CallPhase::Start,
            now + 90,
        )
        .unwrap();
    assert_eq!(
        store
            .work_overlap_consent(&incumbent, &consent, now + 91)
            .unwrap()
            .state,
        WorkOverlapConsentState::AwaitingRequester
    );
}

parity_case!(
    named_overlap_concurrency,
    named_overlap_concurrent_retries_keep_one_consent
);

fn agreed_overlap_admits_and_checks_only_named_files(db: &Db) {
    use agentscm::hub::work_overlap::{
        WorkOverlapConsentRequest, WorkOverlapConsentState, WorkOverlapStart,
    };
    let store = db.open();
    let now = unix_now();
    let a = token(&*store, 201, "first");
    let b = token(&*store, 202, "second");
    let third = token(&*store, 203, "third");
    let first = start_scope(
        "first",
        "first",
        WorkRole::Lead,
        None,
        &["src/a.rs", "src/b.rs"],
    );
    let target = store.work_start(&a, &first, now).unwrap().item;
    let second = start_scope("second", "second", WorkRole::Lead, None, &["src/a.rs"]);
    let request = store
        .work_start(&b, &second, now + 1)
        .unwrap()
        .coordination_request
        .unwrap();
    let grant = store
        .work_overlap_consent(
            &a,
            &WorkOverlapConsentRequest {
                request_id: request.id.clone(),
                target_work_id: target.id.clone(),
                operation_id: "grant".into(),
                session: "first".into(),
                expected_updated_seq: request.updated_seq,
                target_generation: target.generation,
                target_updated_seq: target.updated_seq,
                paths: vec!["src/a.rs".into()],
            },
            now + 2,
        )
        .unwrap();
    let accepted = WorkOverlapStart {
        start: second.clone(),
        consent_ids: vec![grant.consent.id.clone()],
    };
    let preflight = store
        .work_overlap_preflight(&b, &accepted, now + 3)
        .unwrap();
    assert_eq!(preflight.scope_overlaps.len(), 1);
    assert_eq!(preflight.scope_overlaps[0].agreed_paths, ["src/a.rs"]);
    assert!(!preflight.scope_overlaps[0].unresolved());
    let admitted = store.work_overlap_start(&b, &accepted, now + 4).unwrap();
    assert!(admitted.created);
    assert_eq!(
        store
            .work_overlap_consents(&b, &request.id, "second", now + 5)
            .unwrap()[0]
            .state,
        WorkOverlapConsentState::Agreed
    );
    let replay = store.work_overlap_start(&b, &accepted, now + 5).unwrap();
    assert_eq!(replay.item.id, admitted.item.id);
    assert!(!replay.created);
    let ordinary = store.work_preflight(&b, &second, now + 5).unwrap();
    assert_eq!(ordinary.scope_overlaps[0].agreed_paths, ["src/a.rs"]);
    assert!(
        !store
            .work_start(
                &third,
                &start_scope("third", "third", WorkRole::Lead, None, &["src/a.rs"]),
                now + 5
            )
            .unwrap()
            .created
    );
    let touch = |token: &str, session: &str, paths: &[&str], deny: bool, at| {
        store.touch_session(
            token,
            &SessionCall {
                session: Some(session.into()),
                via: "hook:codex".into(),
                task: None,
                paths: paths.iter().map(|s| (*s).into()).collect(),
                deny,
                report: true,
            },
            at,
        )
    };
    assert!(
        touch(&a, "first", &["src/a.rs", "src/b.rs"], false, now + 6)
            .unwrap()
            .denied
            .is_empty()
    );
    let agreed = touch(&b, "second", &["src/a.rs"], true, now + 7).unwrap();
    assert!(agreed.denied.is_empty());
    assert_eq!(agreed.overlaps[0].agreed_paths, ["src/a.rs"]);
    // Consent does not cover another file or a third lead's intent on the agreed file.
    let unrelated = touch(&third, "third", &["src/a.rs"], true, now + 8);
    assert_eq!(unrelated.unwrap().denied[0].your_paths, ["src/a.rs"]);
    let other = touch(&b, "second", &["src/b.rs"], true, now + 9);
    assert!(other.is_err());
    let helper = token(&*store, 201, "helper");
    store
        .work_start(
            &helper,
            &start_scope(
                "helper",
                "helper",
                WorkRole::Helper,
                Some(&target.id),
                &["src/a.rs"],
            ),
            now + 9,
        )
        .unwrap();
    touch(&helper, "helper", &["src/a.rs"], false, now + 9).unwrap();
    let third_holder = touch(&b, "second", &["src/a.rs"], true, now + 9).unwrap();
    assert!(
        !third_holder.denied.is_empty(),
        "a helper cannot borrow its lead's agreement"
    );
    // Invalidated acceptance cannot use an old receipt to restore an agreement.
    store
        .work_checkpoint(
            &a,
            &checkpoint(
                &target.id,
                "narrow",
                "first",
                store
                    .work_for_session(&a, "first", now + 10)
                    .unwrap()
                    .unwrap()
                    .updated_seq,
                CheckpointKind::Progress,
                Some(&["src/b.rs"]),
            ),
            now + 10,
        )
        .unwrap();
    assert!(store.work_overlap_start(&b, &accepted, now + 11).is_err());
    let invalidated = touch(&b, "second", &["src/a.rs"], true, now + 11).unwrap();
    assert!(
        !invalidated.denied.is_empty(),
        "an already held edit must recheck ended consent"
    );
}

fn consent_cannot_resolve_inconsistent_checked_scope(db: &Db) {
    use agentscm::hub::work_overlap::{
        WorkOverlapConsentRequest, WorkOverlapConsentState, WorkOverlapStart,
    };
    let store = db.open();
    let now = unix_now();
    let a = token(&*store, 204, "first");
    let b = token(&*store, 205, "second");
    let target = store
        .work_start(
            &a,
            &start_scope("first", "first", WorkRole::Lead, None, &["src/a.rs"]),
            now,
        )
        .unwrap()
        .item;
    let proposed = start_scope("second", "second", WorkRole::Lead, None, &["src/a.rs"]);
    let request = store
        .work_start(&b, &proposed, now + 1)
        .unwrap()
        .coordination_request
        .unwrap();
    let grant = store
        .work_overlap_consent(
            &a,
            &WorkOverlapConsentRequest {
                request_id: request.id.clone(),
                target_work_id: target.id.clone(),
                operation_id: "grant".into(),
                session: "first".into(),
                expected_updated_seq: request.updated_seq,
                target_generation: target.generation,
                target_updated_seq: target.updated_seq,
                paths: vec!["src/a.rs".into()],
            },
            now + 2,
        )
        .unwrap();
    let accepted = WorkOverlapStart {
        start: proposed.clone(),
        consent_ids: vec![grant.consent.id.clone()],
    };
    store.work_overlap_start(&b, &accepted, now + 3).unwrap();

    // Plant inconsistent indexed evidence, retaining the old epoch to isolate this check from
    // normal scope-change invalidation. A snapshot-based grant cannot resolve different scope.
    db.exec(&format!(
        "UPDATE hub_work_items SET scope='[\"src/a.rs\",\"src/other.rs\"]' WHERE id='{}'; UPDATE hub_work_items SET overlap_epoch={} WHERE id='{}'",
        target.id, grant.consent.target.epoch, target.id,
    ));
    assert_eq!(
        store
            .work_overlap_consents(&b, &request.id, "second", now + 4)
            .unwrap()[0]
            .state,
        WorkOverlapConsentState::Agreed,
        "the fixture must reach the checked-scope guard rather than epoch invalidation"
    );
    for preflight in [
        store.work_preflight(&b, &proposed, now + 4).unwrap(),
        store
            .work_overlap_preflight(&b, &accepted, now + 4)
            .unwrap(),
    ] {
        let overlap = &preflight.scope_overlaps[0];
        assert_eq!(overlap.their_scope, ["src/a.rs", "src/other.rs"]);
        assert_eq!(overlap.item.scope, ["src/a.rs"]);
        assert!(overlap.agreed_paths.is_empty());
        assert!(overlap.unresolved());
    }
    assert!(store.work_overlap_start(&b, &accepted, now + 5).is_err());
}

parity_case!(
    checked_scope_consent,
    consent_cannot_resolve_inconsistent_checked_scope
);

fn agreed_overlap_scope_expansion_and_partial_consent(db: &Db) {
    use agentscm::hub::work_overlap::{WorkOverlapCheckpoint, WorkOverlapConsentRequest};
    let store = db.open();
    let now = unix_now();
    let a = token(&*store, 211, "first");
    let b = token(&*store, 212, "second");
    let target = store
        .work_start(
            &a,
            &start_scope(
                "first",
                "first",
                WorkRole::Lead,
                None,
                &["src/a.rs", "src/b.rs"],
            ),
            now,
        )
        .unwrap()
        .item;
    let own = store
        .work_start(
            &b,
            &start_scope("second", "second", WorkRole::Lead, None, &["docs/a.md"]),
            now,
        )
        .unwrap()
        .item;
    let change = checkpoint(
        &own.id,
        "expand",
        "second",
        own.updated_seq,
        CheckpointKind::Progress,
        Some(&["docs/a.md", "src/a.rs", "src/b.rs"]),
    );
    let request = match store.work_checkpoint(&b, &change, now + 1).unwrap_err() {
        HubError::WorkCoordinationRequired { request, .. } => *request,
        error => panic!("{error:?}"),
    };
    let mut consent = WorkOverlapConsentRequest {
        request_id: request.id.clone(),
        target_work_id: target.id.clone(),
        operation_id: "partial".into(),
        session: "first".into(),
        expected_updated_seq: request.updated_seq,
        target_generation: target.generation,
        target_updated_seq: target.updated_seq,
        paths: vec!["src/a.rs".into()],
    };
    let grant = store.work_overlap_consent(&a, &consent, now + 2).unwrap();
    let mut accept = WorkOverlapCheckpoint {
        checkpoint: change,
        consent_ids: vec![grant.consent.id.clone()],
    };
    assert!(store.work_overlap_checkpoint(&b, &accept, now + 3).is_err());
    assert_eq!(
        store
            .work_for_session(&b, "second", now + 3)
            .unwrap()
            .unwrap()
            .scope,
        ["docs/a.md"]
    );
    consent.operation_id = "full".into();
    consent.expected_updated_seq = grant.consent.event_seq;
    consent.paths.push("src/b.rs".into());
    let full = store.work_overlap_consent(&a, &consent, now + 4).unwrap();
    accept.consent_ids = vec![full.consent.id];
    let expanded = store.work_overlap_checkpoint(&b, &accept, now + 5).unwrap();
    assert_eq!(expanded.item.scope, ["docs/a.md", "src/a.rs", "src/b.rs"]);
    let replay = store.work_overlap_checkpoint(&b, &accept, now + 6).unwrap();
    assert!(replay.receipt.replayed);
    accept.consent_ids = vec![grant.consent.id];
    assert!(
        store.work_overlap_checkpoint(&b, &accept, now + 7).is_err(),
        "changed consent IDs must not replay then bind"
    );
}
parity_case!(
    agreed_overlap_admission,
    agreed_overlap_admits_and_checks_only_named_files
);
parity_case!(
    agreed_overlap_expansion,
    agreed_overlap_scope_expansion_and_partial_consent
);

fn agreed_overlap_concurrent_admission_and_takeover_fences(db: &Db) {
    use agentscm::hub::work_overlap::{WorkOverlapConsentRequest, WorkOverlapStart};
    let store = db.open();
    let now = unix_now();
    let a = token(&*store, 221, "first");
    let b = token(&*store, 222, "second");
    let first = start_scope("first", "first", WorkRole::Lead, None, &["src/a.rs"]);
    let target = store.work_start(&a, &first, now).unwrap().item;
    let second = start_scope("second", "second", WorkRole::Lead, None, &["src/a.rs"]);
    let request = store
        .work_start(&b, &second, now + 1)
        .unwrap()
        .coordination_request
        .unwrap();
    let grant = store
        .work_overlap_consent(
            &a,
            &WorkOverlapConsentRequest {
                request_id: request.id,
                target_work_id: target.id.clone(),
                operation_id: "grant".into(),
                session: "first".into(),
                expected_updated_seq: request.updated_seq,
                target_generation: target.generation,
                target_updated_seq: target.updated_seq,
                paths: vec!["src/a.rs".into()],
            },
            now + 2,
        )
        .unwrap();
    let acceptance = WorkOverlapStart {
        start: second,
        consent_ids: vec![grant.consent.id],
    };
    let barrier = Arc::new(std::sync::Barrier::new(2));
    let threads: Vec<_> = (0..2)
        .map(|_| {
            let store = store.clone();
            let b = b.clone();
            let input = acceptance.clone();
            let barrier = barrier.clone();
            std::thread::spawn(move || {
                barrier.wait();
                store.work_overlap_start(&b, &input, now + 3).unwrap()
            })
        })
        .collect();
    let results: Vec<_> = threads.into_iter().map(|t| t.join().unwrap()).collect();
    assert_eq!(results[0].item.id, results[1].item.id);
    assert_eq!(results.iter().filter(|r| r.created).count(), 1);
    let mut borrowed = acceptance.clone();
    borrowed.start.request_key = Some("unrelated".into());
    assert!(store.work_overlap_start(&b, &borrowed, now + 4).is_err());
    borrowed = acceptance.clone();
    borrowed.start.scope = vec!["src/**".into()];
    assert!(store.work_overlap_start(&b, &borrowed, now + 4).is_err());
    // A new lead cannot inherit the old pair's permission when re-admitting this Work.
    store
        .work_checkpoint(
            &a,
            &checkpoint(
                &target.id,
                "handover",
                "first",
                target.updated_seq,
                CheckpointKind::Handover,
                None,
            ),
            now + 5,
        )
        .unwrap();
    let mut takeover = first;
    takeover.work_id = Some(target.id);
    takeover.request_key = None;
    takeover.session = "new-lead".into();
    takeover.takeover_generation = Some(1);
    let refused = store.work_start(&a, &takeover, now + 6).unwrap();
    assert!(!refused.took_over);
    assert!(refused.conflict.is_some());
    assert!(
        store.work_overlap_start(&b, &acceptance, now + 7).is_ok(),
        "a refused takeover changes neither lead nor existing agreement"
    );
}
parity_case!(
    agreed_overlap_concurrency,
    agreed_overlap_concurrent_admission_and_takeover_fences
);

fn agreed_overlap_retention_keeps_live_agreement_and_reclaims_invalidated(db: &Db) {
    use agentscm::hub::work_overlap::{
        WorkOverlapConsentRequest, WorkOverlapConsentState, WorkOverlapStart,
    };
    let store = db.open();
    let now = unix_now();
    let old = now - 100_000;
    let a = token(&*store, 231, "first");
    let b = token(&*store, 232, "second");
    let first = start_scope("first", "first", WorkRole::Lead, None, &["src/a.rs"]);
    let target = store.work_start(&a, &first, old).unwrap().item;
    let second = start_scope("second", "second", WorkRole::Lead, None, &["src/a.rs"]);
    let request = store
        .work_start(&b, &second, old + 1)
        .unwrap()
        .coordination_request
        .unwrap();
    let grant = store
        .work_overlap_consent(
            &a,
            &WorkOverlapConsentRequest {
                request_id: request.id.clone(),
                target_work_id: target.id.clone(),
                operation_id: "grant".into(),
                session: "first".into(),
                expected_updated_seq: request.updated_seq,
                target_generation: target.generation,
                target_updated_seq: target.updated_seq,
                paths: vec!["src/a.rs".into()],
            },
            old + 2,
        )
        .unwrap();
    store
        .work_overlap_start(
            &b,
            &WorkOverlapStart {
                start: second,
                consent_ids: vec![grant.consent.id],
            },
            old + 3,
        )
        .unwrap();
    let request = store
        .coordination_list(&b, &CoordinationListQuery::default(), old + 4)
        .unwrap()
        .requests
        .into_iter()
        .find(|r| r.id == request.id)
        .unwrap();
    store
        .coordination_resolve(
            &b,
            &CoordinationResolve {
                request_id: request.id.clone(),
                operation_id: "resolved".into(),
                session: "second".into(),
                expected_updated_seq: request.updated_seq,
            },
            old + 5,
        )
        .unwrap();
    // Renew before every expiry, without rewriting the old resolved coordination request.
    for at in ((old + 500)..=now).step_by(500) {
        for (token, session) in [(&a, "first"), (&b, "second")] {
            store
                .touch_session(
                    token,
                    &SessionCall {
                        session: Some(session.into()),
                        via: "mcp".into(),
                        task: None,
                        paths: vec![],
                        deny: false,
                        report: false,
                    },
                    at,
                )
                .unwrap();
        }
    }
    let scope = WorkRepositoryScope {
        provider: "github".into(),
        host: "github.com".into(),
        repository: REPOSITORY.into(),
        repository_id: Some(REPOSITORY_ID),
    };
    let kept = store.work_compact(&scope, now - 90_000, true, now).unwrap();
    assert_eq!(kept.deleted_coordination_requests, 0);
    assert_eq!(
        store
            .work_overlap_consents(&b, &request.id, "second", now)
            .unwrap()[0]
            .state,
        WorkOverlapConsentState::Agreed
    );
    assert_eq!(db.charged_bytes(), store.work_payload_used(&scope).unwrap());
    let current = store.work_for_session(&a, "first", now).unwrap().unwrap();
    store
        .work_checkpoint(
            &a,
            &checkpoint(
                &target.id,
                "complete",
                "first",
                current.updated_seq,
                CheckpointKind::Finish,
                None,
            ),
            now + 1,
        )
        .unwrap();
    let removed = store
        .work_compact(&scope, now + 2, true, now + 86_403)
        .unwrap();
    assert_eq!(removed.deleted_coordination_requests, 1);
    assert_eq!(db.charged_bytes(), store.work_payload_used(&scope).unwrap());
}
parity_case!(
    agreed_overlap_retention,
    agreed_overlap_retention_keeps_live_agreement_and_reclaims_invalidated
);

fn pending_requester_lifecycle_requires_fresh_consent(db: &Db) {
    use agentscm::hub::work_overlap::{
        WorkOverlapCheckpoint, WorkOverlapConsentRequest, WorkOverlapConsentState,
    };
    use agentscm::hub::work_transition::{WorkTransitionActor, WorkTransitionRequest};
    let store = db.open();
    let now = unix_now();
    for (index, change) in ["scope", "release", "expiry", "legacy"]
        .into_iter()
        .enumerate()
    {
        let a = token(&*store, 801 + index as i64 * 2, "target");
        let b = token(&*store, 802 + index as i64 * 2, "requester");
        let target_session = format!("target-{change}");
        let own_session = format!("requester-{change}");
        let shared = format!("src/{change}.rs");
        let own_path = format!("docs/{change}.md");
        let changed_path = format!("docs/{change}-changed.md");
        let own_start = start_scope(
            &own_session,
            &own_session,
            WorkRole::Lead,
            None,
            &[&own_path],
        );
        let target = store
            .work_start(
                &a,
                &start_scope(
                    &target_session,
                    &target_session,
                    WorkRole::Lead,
                    None,
                    &[&shared],
                ),
                now,
            )
            .unwrap()
            .item;
        let own = store.work_start(&b, &own_start, now).unwrap().item;
        let desired = [own_path.as_str(), shared.as_str()];
        let proposed = checkpoint(
            &own.id,
            "propose",
            &own_session,
            own.updated_seq,
            CheckpointKind::Progress,
            Some(&desired),
        );
        let request = match store.work_checkpoint(&b, &proposed, now + 1).unwrap_err() {
            HubError::WorkCoordinationRequired { request, .. } => *request,
            e => panic!("{e:?}"),
        };
        let input = WorkOverlapConsentRequest {
            request_id: request.id.clone(),
            target_work_id: target.id.clone(),
            operation_id: "grant".into(),
            session: target_session.clone(),
            expected_updated_seq: request.updated_seq,
            target_generation: target.generation,
            target_updated_seq: target.updated_seq,
            paths: vec![shared.clone()],
        };
        let grant = store.work_overlap_consent(&a, &input, now + 2).unwrap();
        assert!(grant.consent.pending_requester.is_some());
        let progressed = store
            .work_checkpoint(
                &b,
                &checkpoint(
                    &own.id,
                    "progress",
                    &own_session,
                    own.updated_seq,
                    CheckpointKind::Progress,
                    None,
                ),
                now + 3,
            )
            .unwrap()
            .item;
        assert_eq!(
            store
                .work_overlap_consent(&a, &input, now + 4)
                .unwrap()
                .state,
            WorkOverlapConsentState::AwaitingRequester,
            "progress must preserve pending consent"
        );
        let (current, at) = match change {
            "legacy" => {
                let expression = if db._pg.is_some() {
                    "(snapshot::jsonb - 'pending_requester')::text"
                } else {
                    "json_remove(snapshot, '$.pending_requester')"
                };
                db.exec(&format!(
                    "UPDATE hub_work_overlap_consents SET snapshot={expression} WHERE id='{}'",
                    grant.consent.id
                ));
                (progressed.clone(), now + 6)
            }
            "scope" => (
                store
                    .work_checkpoint(
                        &b,
                        &checkpoint(
                            &own.id,
                            "change",
                            &own_session,
                            progressed.updated_seq,
                            CheckpointKind::Progress,
                            Some(&[&changed_path]),
                        ),
                        now + 5,
                    )
                    .unwrap()
                    .item,
                now + 6,
            ),
            "release" => {
                let release: WorkTransitionRequest = serde_json::from_value(serde_json::json!({"work_id":own.id,"operation_id":"release","generation":progressed.generation,"expected_updated_seq":progressed.updated_seq,"session":own_session,"transition":"release","reason":"synthetic release"})).unwrap();
                let released = store
                    .work_transition(&WorkTransitionActor::Agent { token: &b }, &release, now + 5)
                    .unwrap()
                    .item;
                let resume = WorkStart {
                    work_id: Some(own.id.clone()),
                    request_key: None,
                    takeover_generation: Some(released.generation),
                    ..own_start.clone()
                };
                let resumed = store.work_start(&b, &resume, now + 6).unwrap().item;
                assert!(resumed.generation > own.generation);
                (resumed, now + 7)
            }
            "expiry" => {
                // Keep only the target live, then exercise the requester's expired lease and revival.
                store
                    .work_checkpoint(
                        &a,
                        &checkpoint(
                            &target.id,
                            "renew-target",
                            &target_session,
                            target.updated_seq,
                            CheckpointKind::Progress,
                            None,
                        ),
                        now + 1100,
                    )
                    .unwrap();
                store.observe_lease_clock(now + 1100).unwrap();
                assert_eq!(
                    store
                        .work_overlap_consent(&a, &input, now + 1204)
                        .unwrap()
                        .state,
                    WorkOverlapConsentState::Invalidated
                );
                let revived = store.work_start(&b, &own_start, now + 1205).unwrap();
                assert!(revived.conflict.is_none());
                (revived.item, now + 1206)
            }
            _ => unreachable!(),
        };
        assert_eq!(
            store.work_overlap_consent(&a, &input, at).unwrap().state,
            WorkOverlapConsentState::Invalidated,
            "old receipt cannot restore requester consent after {change}"
        );
        let accepting = WorkOverlapCheckpoint {
            checkpoint: WorkCheckpoint {
                generation: current.generation,
                ..checkpoint(
                    &own.id,
                    "accept-old",
                    &own_session,
                    current.updated_seq,
                    CheckpointKind::Progress,
                    Some(&desired),
                )
            },
            consent_ids: vec![grant.consent.id],
        };
        assert!(
            store.work_overlap_checkpoint(&b, &accepting, at).is_err(),
            "old consent must refuse {change}"
        );
        let request = store
            .coordination_inbox(
                &a,
                &CoordinationInboxQuery {
                    session: Some(target_session.clone()),
                    ..Default::default()
                },
                at,
            )
            .unwrap()
            .requests
            .into_iter()
            .find(|r| r.id == request.id)
            .unwrap();
        let target = store
            .work_for_session(&a, &target_session, at)
            .unwrap()
            .unwrap();
        let fresh = store
            .work_overlap_consent(
                &a,
                &WorkOverlapConsentRequest {
                    operation_id: "fresh".into(),
                    expected_updated_seq: request.updated_seq,
                    target_updated_seq: target.updated_seq,
                    ..input
                },
                at + 1,
            )
            .unwrap();
        let accepting = WorkOverlapCheckpoint {
            consent_ids: vec![fresh.consent.id],
            ..accepting
        };
        let accepted = store
            .work_overlap_checkpoint(&b, &accepting, at + 2)
            .unwrap();
        assert_eq!(accepted.item.scope, desired);
        assert!(
            store
                .work_overlap_checkpoint(&b, &accepting, at + 3)
                .unwrap()
                .receipt
                .replayed
        );
    }
}
parity_case!(
    pending_requester_lifecycle,
    pending_requester_lifecycle_requires_fresh_consent
);
