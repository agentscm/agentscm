//! Public coordination response bounds that must stay below the Hub JSON limit.
use crate::hub_pg;

use std::sync::Arc;

use agentscm::hub::HubError;
use agentscm::hub::coordination::{
    CoordinationAck, CoordinationAcknowledgement, CoordinationAgent, CoordinationInboxQuery,
    CoordinationListQuery, CoordinationProposal, CoordinationReply, CoordinationRequest,
    CoordinationResolve, CoordinationResponse, CoordinationResponseKind, CoordinationStatus,
    CoordinationTarget, CoordinationWithdraw,
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
        false,
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
        work_id: work_id.map(str::to_owned),
        source: None,
        request_key: work_id.is_none().then(|| key.into()),
        title: key.into(),
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
    let owner = store
        .upsert_user(
            &ForgeIdentity {
                provider: "github".into(),
                host: "github.com".into(),
                user_id: 1,
            },
            "user1",
        )
        .unwrap();
    assert_eq!(
        store
            .repository_open_coordination_count(&owner, REPOSITORY, REPOSITORY_ID)
            .unwrap(),
        1
    );
    assert_eq!(
        store
            .repository_open_coordination_count(&owner, "acme/other", REPOSITORY_ID)
            .unwrap(),
        0
    );

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
        via_hub: false,
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
    assert_eq!(
        store
            .repository_open_coordination_count(&owner, REPOSITORY, REPOSITORY_ID)
            .unwrap(),
        0
    );

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
    let resolved = store
        .coordination_resolve(
            &requester,
            &CoordinationResolve {
                request_id: request.id,
                operation_id: "scope-applied".into(),
                session: "expanding-session".into(),
                expected_updated_seq: request.updated_seq,
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
                via_hub: false,
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
                    via_hub: false,
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
        Err(HubError::WorkCapacity)
    ));

    let cleanup = CoordinationWithdraw {
        request_id: requests[0].id.clone(),
        operation_id: "target-cleanup".into(),
        session: "incumbent-session".into(),
        expected_updated_seq: requests[0].updated_seq,
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
        Err(HubError::WorkCapacity)
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
    assert!(matches!(
        store.touch_session(
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
        ),
        Err(HubError::Invalid(message)) if message.contains("already bound")
    ));
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
        via_hub,
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
    };

    let encoded = serde_json::to_vec(&request).unwrap();
    assert!(encoded.len() < 2 * 1024 * 1024, "{} bytes", encoded.len());
}
