//! Durable person-level catch-up storage contracts.

#[path = "common/hub_pg.rs"]
mod hub_pg;

use std::path::PathBuf;
use std::sync::{Arc, Barrier};

use agentscm::hub::HubError;
use agentscm::hub::catchup::{
    CatchupAck, CatchupAckStatus, CatchupCoverageState, CatchupMergeSource, CatchupPreviewQuery,
    CatchupPublicationRepair, CatchupQuery, CatchupRepositoryScope, CatchupSourceStatus,
    NewCatchupMerge,
};
use agentscm::hub::publish::{Create, NewPublication, Update, derive_id};
use agentscm::hub::store::{
    self, ForgeIdentity, Store, mint_agent_token, mint_bound_agent_token, unix_now,
};
use agentscm::hub::work::{
    CheckpointKind, WorkCheckpoint, WorkRepositoryScope, WorkRole, WorkStart,
};
use agentscm::hub::work_preflight::{
    WorkPreflightReason, WorkPreflightRequest, WorkPreflightStatus,
};
use hub_pg::PgSchema;
use postgres::{Client, NoTls};
use serde_json::json;
use tempfile::TempDir;

const REPOSITORY: &str = "acme/widgets";
const REPOSITORY_ID: i64 = 42;

struct Fixture {
    store: Arc<dyn Store>,
    database: Option<PathBuf>,
    _directory: Option<TempDir>,
    _pg: Option<PgSchema>,
}

impl Fixture {
    fn sqlite() -> Self {
        let directory = TempDir::new().unwrap();
        let database = directory.path().join("hub.sqlite");
        let store = store::open(&format!("sqlite://{}", database.display())).unwrap();
        Self {
            store,
            database: Some(database),
            _directory: Some(directory),
            _pg: None,
        }
    }

    fn postgres() -> Option<Self> {
        let pg = PgSchema::create()?;
        let store = store::open(&pg.url).unwrap();
        Some(Self {
            store,
            database: None,
            _directory: None,
            _pg: Some(pg),
        })
    }

    fn owner(&self, user_id: i64) -> String {
        self.store
            .upsert_user(
                &ForgeIdentity {
                    provider: "github".into(),
                    host: "github.com".into(),
                    user_id,
                },
                &format!("user-{user_id}"),
            )
            .unwrap()
            .id
    }

    fn token(&self, owner: &str, name: &str, repository_id: i64, read_only: bool) -> String {
        self.token_for_repository(owner, name, REPOSITORY, repository_id, read_only)
    }

    fn token_for_repository(
        &self,
        owner: &str,
        name: &str,
        repository: &str,
        repository_id: i64,
        read_only: bool,
    ) -> String {
        mint_bound_agent_token(
            &*self.store,
            owner,
            name,
            repository,
            repository_id,
            3600,
            read_only,
        )
        .unwrap()
        .0
    }

    fn delivery_count(&self) -> i64 {
        if let Some(database) = &self.database {
            return rusqlite::Connection::open(database)
                .unwrap()
                .query_row("SELECT COUNT(*) FROM hub_catchup_deliveries", [], |row| {
                    row.get(0)
                })
                .unwrap();
        }
        Client::connect(&self._pg.as_ref().unwrap().url, NoTls)
            .unwrap()
            .query_one("SELECT COUNT(*) FROM hub_catchup_deliveries", &[])
            .unwrap()
            .get(0)
    }

    fn publication_failure_count(&self) -> i64 {
        if let Some(database) = &self.database {
            return rusqlite::Connection::open(database)
                .unwrap()
                .query_row(
                    "SELECT COUNT(*) FROM hub_catchup_publication_failures",
                    [],
                    |row| row.get(0),
                )
                .unwrap();
        }
        Client::connect(&self._pg.as_ref().unwrap().url, NoTls)
            .unwrap()
            .query_one("SELECT COUNT(*) FROM hub_catchup_publication_failures", &[])
            .unwrap()
            .get(0)
    }

    fn events(&self, kind: &str) -> Vec<serde_json::Value> {
        self.store
            .events_after(0, 1000)
            .unwrap()
            .into_iter()
            .filter(|event| event.kind == kind)
            .map(|event| serde_json::from_str(&event.data).unwrap())
            .collect()
    }

    fn catchup_ack_receipt(&self, delivery: &str) -> (Option<String>, String) {
        if let Some(database) = &self.database {
            return rusqlite::Connection::open(database)
                .unwrap()
                .query_row(
                    "SELECT ack_actor,delivery_client FROM hub_catchup_deliveries WHERE id=?1",
                    [delivery],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .unwrap();
        }
        let row = Client::connect(&self._pg.as_ref().unwrap().url, NoTls)
            .unwrap()
            .query_one(
                "SELECT ack_actor,delivery_client FROM hub_catchup_deliveries WHERE id=$1",
                &[&delivery],
            )
            .unwrap();
        (row.get(0), row.get(1))
    }

    fn set_publication_merge(&self, publication: &str, merge_commit: &str, updated_at: i64) {
        if let Some(database) = &self.database {
            rusqlite::Connection::open(database)
                .unwrap()
                .execute(
                    "UPDATE hub_publications SET merge_commit=?2,updated_at=?3 WHERE id=?1",
                    rusqlite::params![publication, merge_commit, updated_at],
                )
                .unwrap();
            return;
        }
        Client::connect(&self._pg.as_ref().unwrap().url, NoTls)
            .unwrap()
            .execute(
                "UPDATE hub_publications SET merge_commit=$2,updated_at=$3 WHERE id=$1",
                &[&publication, &merge_commit, &updated_at],
            )
            .unwrap();
    }
}

fn publication(repository: &str, branch: &str, title: &str) -> NewPublication {
    let commit = "b".repeat(40);
    NewPublication {
        id: derive_id(repository, 3, &commit, branch),
        manifest: serde_json::from_value(json!({
            "revision": "a".repeat(64), "event_seq": 3, "commit": commit,
            "tree": "c".repeat(40), "parent": "d".repeat(40), "branch": branch,
            "base": "main", "title": title, "body": "", "message": "message\n",
            "author": {"name":"A","email":"a@example.invalid","date":"2026-10-25T00:00:00Z"},
            "committer": {"name":"A","email":"a@example.invalid","date":"2026-10-25T00:00:00Z"},
            "changes": [{"path":"src/a.rs","mode":"100644","blob":"e".repeat(40),
                "sha256":"f".repeat(64),"size":5}]
        }))
        .unwrap(),
    }
}

fn create_pull_publication(
    fixture: &Fixture,
    token: &str,
    branch: &str,
    title: &str,
    number: i64,
    now: i64,
) -> String {
    let publication = publication(REPOSITORY, branch, title);
    assert!(matches!(
        fixture
            .store
            .create_publication(token, &publication, now)
            .unwrap(),
        Create::Done(_)
    ));
    fixture
        .store
        .update_publication(&publication.id, &Update::BranchAttempt, now)
        .unwrap();
    fixture
        .store
        .update_publication(&publication.id, &Update::BranchCreated, now)
        .unwrap();
    fixture
        .store
        .update_publication(
            &publication.id,
            &Update::PullRequest {
                number,
                node_id: format!("PR_{number}"),
                html_url: format!("https://github.example/acme/widgets/pull/{number}"),
            },
            now,
        )
        .unwrap();
    publication.id
}

fn named_scope(repository: &str, repository_id: Option<i64>) -> CatchupRepositoryScope {
    CatchupRepositoryScope {
        provider: "github".into(),
        host: "github.com".into(),
        repository: repository.into(),
        repository_id,
    }
}

fn scope(repository_id: i64) -> CatchupRepositoryScope {
    CatchupRepositoryScope {
        provider: "github".into(),
        host: "github.com".into(),
        repository: REPOSITORY.into(),
        repository_id: Some(repository_id),
    }
}

fn merge(number: i64, observed_at: i64) -> (NewCatchupMerge, i64) {
    (
        NewCatchupMerge {
            source: CatchupMergeSource::ForgePull,
            source_id: format!("pull:{number}"),
            occurred_at: Some(observed_at - 100),
            title: Some(format!("Merge {number}")),
            branch: format!("feature-{number}"),
            base: "main".into(),
            head_commit: format!("{number:040x}"),
            merge_commit: format!("{:040x}", number + 100),
            pull_number: number,
            paths: vec![format!("src/{number}.rs")],
            paths_complete: true,
        },
        observed_at,
    )
}

fn continuation(page: &agentscm::hub::catchup::CatchupPage, limit: usize) -> CatchupQuery {
    CatchupQuery {
        subscriber: page.subscriber.clone(),
        delivery_client: page.delivery_client.clone(),
        delivery: Some(page.delivery.clone()),
        event_after: Some(page.next_event_after),
        event_through: Some(page.event_through),
        merge_after: Some(page.next_merge_after),
        merge_through: Some(page.merge_through),
        limit,
    }
}

fn human_ack_receipt_records_current_actor(fixture: &Fixture) {
    let now = unix_now();
    let owner = fixture.owner(58);
    let token_a = fixture.token(&owner, "human-reader-a", 258, false);
    let token_b = fixture.token(&owner, "human-reader-b", 258, true);
    let page = fixture
        .store
        .work_catchup(
            &token_a,
            &CatchupQuery {
                subscriber: "human".into(),
                delivery_client: "human-tab-a".into(),
                ..CatchupQuery::default()
            },
            now,
        )
        .unwrap();
    assert!(page.complete);
    let ack = CatchupAck {
        delivery: page.delivery.clone(),
        expected_event_from: page.event_from,
        expected_merge_from: page.merge_from,
    };
    let advanced = fixture
        .store
        .work_catchup_ack(&token_b, &ack, now + 1)
        .unwrap();
    assert_eq!(advanced.status, CatchupAckStatus::Advanced);
    assert_eq!(
        fixture.catchup_ack_receipt(&page.delivery),
        (Some(token_b.clone()), "human-tab-a".into())
    );

    let repeated = fixture
        .store
        .work_catchup_ack(&token_a, &ack, now + 2)
        .unwrap();
    assert_eq!(repeated.status, CatchupAckStatus::AlreadyAcknowledged);
    assert_eq!(
        fixture.catchup_ack_receipt(&page.delivery),
        (Some(token_b), "human-tab-a".into())
    );
}

fn contracts(fixture: &Fixture) {
    let now = unix_now();
    let owner = fixture.owner(1);
    let token_a = fixture.token(&owner, "device-a", REPOSITORY_ID, false);
    let token_b = fixture.token(&owner, "device-b", REPOSITORY_ID, false);
    let read_only = fixture.token(&owner, "reader", REPOSITORY_ID, true);
    let other_owner = fixture.owner(2);
    let other_token = fixture.token(&other_owner, "other", REPOSITORY_ID, false);
    let other_repo = fixture.token(&owner, "other-repo", REPOSITORY_ID + 1, false);

    let (first, first_at) = merge(1, now);
    let retained = fixture
        .store
        .record_catchup_merge(&scope(REPOSITORY_ID), &first, first_at)
        .unwrap();
    let duplicate = fixture
        .store
        .record_catchup_merge(&scope(REPOSITORY_ID), &first, first_at + 99)
        .unwrap();
    assert_eq!(
        duplicate, retained,
        "retry preserves immutable ingestion time"
    );
    assert_eq!(retained.occurred_at, Some(first_at - 100));
    assert_eq!(retained.observed_at, first_at);

    let (second, second_at) = merge(2, now + 1);
    fixture
        .store
        .record_catchup_merge(&scope(REPOSITORY_ID), &second, second_at)
        .unwrap();
    let query = CatchupQuery {
        delivery_client: "client-a".into(),
        limit: 1,
        ..CatchupQuery::default()
    };
    let first_page = fixture
        .store
        .work_catchup(&token_a, &query, now + 2)
        .unwrap();
    assert_eq!(first_page.merges.len(), 1);
    assert!(!first_page.complete);

    let replay = fixture
        .store
        .work_catchup(&token_b, &query, now + 3)
        .unwrap();
    assert_eq!(replay.delivery, first_page.delivery);
    assert_eq!(
        replay.merges, first_page.merges,
        "second human device replays from the start"
    );
    let independent = fixture
        .store
        .work_catchup(
            &token_a,
            &CatchupQuery {
                delivery_client: "client-b".into(),
                limit: 20,
                ..CatchupQuery::default()
            },
            now + 3,
        )
        .unwrap();
    assert_ne!(independent.delivery, first_page.delivery);
    assert_eq!(independent.merge_from, first_page.merge_from);
    assert!(independent.complete);

    let (later, later_at) = merge(3, now + 4);
    fixture
        .store
        .record_catchup_merge(&scope(REPOSITORY_ID), &later, later_at)
        .unwrap();
    let second_page = fixture
        .store
        .work_catchup(&token_a, &continuation(&first_page, 1), now + 5)
        .unwrap();
    assert!(second_page.complete);
    assert_eq!(second_page.merges.len(), 1);
    assert_eq!(
        second_page.merge_through, 2,
        "delivery watermark stays fixed"
    );

    let ack = CatchupAck {
        delivery: second_page.delivery.clone(),
        expected_event_from: second_page.event_from,
        expected_merge_from: second_page.merge_from,
    };
    let acknowledged = fixture
        .store
        .work_catchup_ack(&token_b, &ack, now + 6)
        .unwrap();
    assert_eq!(acknowledged.status, CatchupAckStatus::Advanced);
    let retried = fixture
        .store
        .work_catchup_ack(&token_a, &ack, now + 7)
        .unwrap();
    assert_eq!(retried.status, CatchupAckStatus::AlreadyAcknowledged);

    let stale_client_ack = CatchupAck {
        delivery: independent.delivery.clone(),
        expected_event_from: independent.event_from,
        expected_merge_from: independent.merge_from,
    };
    let recovered = fixture
        .store
        .work_catchup(
            &token_a,
            &CatchupQuery {
                delivery_client: "client-b".into(),
                ..CatchupQuery::default()
            },
            now + 8,
        )
        .unwrap();
    assert_ne!(recovered.delivery, independent.delivery);
    assert_eq!(recovered.merge_from, acknowledged.current_merge_ack);
    assert_eq!(recovered.merges.len(), 1);
    assert_eq!(recovered.merges[0].pull_number, 3);
    assert_eq!(
        fixture
            .store
            .work_catchup_ack(&token_a, &stale_client_ack, now + 8)
            .unwrap()
            .status,
        CatchupAckStatus::Conflict,
        "the superseded delivery remains an explicit conflict receipt"
    );
    let recovered_ack = CatchupAck {
        delivery: recovered.delivery.clone(),
        expected_event_from: recovered.event_from,
        expected_merge_from: recovered.merge_from,
    };
    assert_eq!(
        fixture
            .store
            .work_catchup_ack(&token_a, &recovered_ack, now + 9)
            .unwrap()
            .status,
        CatchupAckStatus::Advanced
    );
    let next = fixture
        .store
        .work_catchup(&token_a, &CatchupQuery::default(), now + 10)
        .unwrap();
    assert!(next.merges.is_empty());
    assert_eq!(next.merge_from, 3);
    assert_eq!(
        fixture
            .store
            .work_catchup_ack(&token_b, &ack, now + 10)
            .unwrap()
            .status,
        CatchupAckStatus::AlreadyAcknowledged,
        "a recent acknowledgement receipt survives a later read"
    );

    let agent_page = fixture
        .store
        .work_catchup(
            &token_a,
            &CatchupQuery {
                subscriber: "session-1".into(),
                ..CatchupQuery::default()
            },
            now + 9,
        )
        .unwrap();
    assert_eq!(
        agent_page.merges.len(),
        3,
        "agent subscriber is independent"
    );
    let other_person = fixture
        .store
        .work_catchup(&other_token, &CatchupQuery::default(), now + 9)
        .unwrap();
    assert_eq!(
        other_person.merges.len(),
        3,
        "person cursors are independent"
    );
    let other_scope = fixture
        .store
        .work_catchup(&other_repo, &CatchupQuery::default(), now + 9)
        .unwrap();
    assert!(
        other_scope.merges.is_empty(),
        "repository ids do not cross scopes"
    );

    let read_page = fixture
        .store
        .work_catchup(&read_only, &CatchupQuery::default(), now + 10)
        .unwrap();
    let read_ack = CatchupAck {
        delivery: read_page.delivery,
        expected_event_from: read_page.event_from,
        expected_merge_from: read_page.merge_from,
    };
    assert_eq!(
        fixture
            .store
            .work_catchup_ack(&read_only, &read_ack, now + 11)
            .unwrap()
            .status,
        CatchupAckStatus::Advanced
    );

    fixture.store.revoke_agent_token(&token_a).unwrap();
    assert!(
        fixture
            .store
            .work_catchup(&token_a, &CatchupQuery::default(), now + 12)
            .is_err()
    );

    assert!(
        fixture
            .store
            .catchup_merge_known(&scope(REPOSITORY_ID), 1, Some(&first.merge_commit))
            .unwrap()
    );
    assert!(
        !fixture
            .store
            .catchup_merge_known(&scope(REPOSITORY_ID), 1, Some(&later.merge_commit))
            .unwrap()
    );
    assert_eq!(
        fixture
            .store
            .catchup_merge_highwater(&scope(REPOSITORY_ID))
            .unwrap(),
        3
    );
    assert_eq!(
        fixture
            .store
            .catchup_merges_through(&scope(REPOSITORY_ID), 1, 3, 10)
            .unwrap()
            .len(),
        2
    );
}

fn compaction_delivery_guard(fixture: &Fixture) {
    let now = unix_now();
    let old = now - 200_000;
    let owner = fixture.owner(44);
    let token = fixture.token(&owner, "compaction-lead", 144, false);
    let (old_merge, _) = merge(144, old);
    fixture
        .store
        .record_catchup_merge(&scope(144), &old_merge, old)
        .unwrap();
    let started = fixture
        .store
        .work_start(
            &token,
            &WorkStart {
                work_id: None,
                source: None,
                request_key: Some("catchup-compaction".into()),
                title: "Catch-up compaction guard".into(),
                session: "compaction-session".into(),
                role: WorkRole::Lead,
                scope: vec!["src/compaction.rs".into()],
                branch: None,
                worktree: None,
                next_action: None,
                depends_on: Vec::new(),
                related_work: Vec::new(),
                takeover_generation: None,
                preflight: None,
            },
            old,
        )
        .unwrap();
    fixture
        .store
        .work_checkpoint(
            &token,
            &WorkCheckpoint {
                work_id: started.item.id.clone(),
                operation_id: "catchup-compaction-progress".into(),
                generation: started.item.generation,
                session: "compaction-session".into(),
                expected_updated_seq: Some(started.item.updated_seq),
                kind: CheckpointKind::Progress,
                summary: "old progress retained for catch-up".into(),
                next_action: None,
                scope: None,
                branch: None,
                worktree: None,
                resolve_blocker: false,
                source_refs: Vec::new(),
                depends_on: Vec::new(),
                related_work: Vec::new(),
            },
            old + 1,
        )
        .unwrap();

    let expired_owner = fixture.owner(45);
    let expired_token = fixture.token(&expired_owner, "expired-reader", 144, false);
    let expired_page = fixture
        .store
        .work_catchup(
            &expired_token,
            &CatchupQuery {
                delivery_client: "expired-client".into(),
                limit: 1,
                ..CatchupQuery::default()
            },
            old + 2,
        )
        .unwrap();
    assert!(!expired_page.complete);
    let expired_ack_page = fixture
        .store
        .work_catchup(
            &expired_token,
            &CatchupQuery {
                delivery_client: "expired-ack-client".into(),
                limit: 50,
                ..CatchupQuery::default()
            },
            old + 2,
        )
        .unwrap();
    assert!(expired_ack_page.complete);

    let mut page = fixture
        .store
        .work_catchup(
            &token,
            &CatchupQuery {
                delivery_client: "live-client".into(),
                limit: 1,
                ..CatchupQuery::default()
            },
            now,
        )
        .unwrap();
    assert!(!page.complete);
    let work_scope = WorkRepositoryScope {
        provider: "github".into(),
        host: "github.com".into(),
        repository: REPOSITORY.into(),
        repository_id: Some(144),
    };
    assert!(
        fixture
            .store
            .work_compact(&work_scope, now - 86_400, false, now)
            .unwrap_err()
            .to_string()
            .contains("work compaction is blocked by an unacknowledged catch-up delivery")
    );
    while !page.complete {
        page = fixture
            .store
            .work_catchup(&token, &continuation(&page, 1), now)
            .unwrap();
    }
    fixture
        .store
        .work_catchup_ack(
            &token,
            &CatchupAck {
                delivery: page.delivery,
                expected_event_from: page.event_from,
                expected_merge_from: page.merge_from,
            },
            now + 1,
        )
        .unwrap();
    let compacted = fixture
        .store
        .work_compact(&work_scope, now - 86_400, true, now + 1)
        .unwrap();
    assert_eq!(compacted.deleted_merge_observations, 1);
    assert_eq!(compacted.merge_retention_floor, 1);
    assert!(
        !fixture
            .store
            .catchup_merge_known(&scope(144), 144, Some(&old_merge.merge_commit))
            .unwrap()
    );
    let expired_continuation =
        fixture
            .store
            .work_catchup(&expired_token, &continuation(&expired_page, 1), now + 1);
    assert!(
        expired_continuation
            .unwrap_err()
            .to_string()
            .contains("unknown catch-up delivery")
    );
    let expired_ack = fixture.store.work_catchup_ack(
        &expired_token,
        &CatchupAck {
            delivery: expired_ack_page.delivery,
            expected_event_from: expired_ack_page.event_from,
            expected_merge_from: expired_ack_page.merge_from,
        },
        now + 1,
    );
    assert!(
        expired_ack
            .unwrap_err()
            .to_string()
            .contains("catch-up delivery is unknown or incomplete")
    );
    let after_compaction = fixture
        .store
        .work_catchup(
            &expired_token,
            &CatchupQuery {
                delivery_client: "expired-client".into(),
                limit: 1,
                ..CatchupQuery::default()
            },
            now + 1,
        )
        .unwrap();
    assert_ne!(after_compaction.delivery, expired_page.delivery);
    assert!(after_compaction.retention.work_gap);
    assert!(after_compaction.retention.merge_gap);
}

fn non_lead_cannot_use_explicit_lead_retry_bypass(fixture: &Fixture) {
    let now = unix_now();
    let owner = fixture.owner(46);
    let token = fixture.token(&owner, "preflight-role", 146, false);
    let started = fixture
        .store
        .work_start(
            &token,
            &WorkStart {
                work_id: None,
                source: None,
                request_key: Some("preflight-role-gate".into()),
                title: "Role gate".into(),
                session: "lead-session".into(),
                role: WorkRole::Lead,
                scope: vec!["src/role.rs".into()],
                branch: None,
                worktree: None,
                next_action: None,
                depends_on: Vec::new(),
                related_work: Vec::new(),
                takeover_generation: None,
                preflight: None,
            },
            now,
        )
        .unwrap();
    for role in [WorkRole::Helper, WorkRole::Reviewer] {
        let refused = fixture.store.work_start(
            &token,
            &WorkStart {
                work_id: Some(started.item.id.clone()),
                source: None,
                request_key: None,
                title: started.item.title.clone(),
                session: "lead-session".into(),
                role,
                scope: started.item.scope.clone(),
                branch: None,
                worktree: None,
                next_action: None,
                depends_on: Vec::new(),
                related_work: Vec::new(),
                takeover_generation: None,
                preflight: Some(WorkPreflightRequest {
                    source_revision: format!("{:040x}", 146),
                    merge_after: 0,
                    merge_through: 0,
                    source_relation_checked: false,
                    checked_merges: Vec::new(),
                }),
            },
            now + 1,
        );
        let Err(HubError::WorkPreflight(evidence)) = refused else {
            panic!("non-lead explicit retry must be refused: {refused:?}");
        };
        assert_eq!(evidence.status, WorkPreflightStatus::Unknown);
        let item = evidence.subject_match.unwrap();
        assert_eq!(item.participants, started.item.participants);
    }
}

fn stable_repository_id_survives_rename(fixture: &Fixture) {
    let now = unix_now();
    let owner = fixture.owner(47);
    let old = "acme/old-widgets";
    let new = "acme/new-widgets";
    let old_token = fixture.token_for_repository(&owner, "old", old, 247, false);
    let (first, at) = merge(247, now);
    fixture
        .store
        .record_catchup_merge(&named_scope(old, Some(247)), &first, at)
        .unwrap();
    fixture
        .store
        .record_catchup_source_status(
            &named_scope(old, Some(247)),
            &CatchupSourceStatus {
                source: CatchupMergeSource::ForgePull,
                state: CatchupCoverageState::ObservedOnly,
                checked_at: now,
                unread_pages: 0,
                deferred: 0,
            },
        )
        .unwrap();
    let initial = fixture
        .store
        .work_catchup(&old_token, &CatchupQuery::default(), now + 1)
        .unwrap();
    fixture
        .store
        .work_catchup_ack(
            &old_token,
            &CatchupAck {
                delivery: initial.delivery,
                expected_event_from: initial.event_from,
                expected_merge_from: initial.merge_from,
            },
            now + 2,
        )
        .unwrap();
    let started = fixture
        .store
        .work_start(
            &old_token,
            &WorkStart {
                work_id: None,
                source: None,
                request_key: Some("rename-active".into()),
                title: "Active across rename".into(),
                session: "rename-old".into(),
                role: WorkRole::Lead,
                scope: vec!["src/rename.rs".into()],
                branch: None,
                worktree: None,
                next_action: None,
                depends_on: vec![],
                related_work: vec![],
                takeover_generation: None,
                preflight: None,
            },
            now + 3,
        )
        .unwrap();
    let (second, at) = merge(248, now + 4);
    fixture
        .store
        .record_catchup_merge(&named_scope(new, Some(247)), &second, at)
        .unwrap();
    let new_token = fixture.token_for_repository(&owner, "new", new, 247, false);
    let renamed = fixture
        .store
        .work_catchup(&new_token, &CatchupQuery::default(), now + 5)
        .unwrap();
    assert_eq!(renamed.merge_from, 1);
    assert_eq!(
        renamed
            .merges
            .iter()
            .map(|m| m.pull_number)
            .collect::<Vec<_>>(),
        vec![248]
    );
    assert!(
        renamed
            .work_events
            .iter()
            .any(|event| event.work_id == started.item.id)
    );
    let preflight = fixture
        .store
        .work_preflight(
            &new_token,
            &WorkStart {
                work_id: None,
                source: None,
                request_key: Some("rename-next".into()),
                title: "New work after rename".into(),
                session: "rename-new".into(),
                role: WorkRole::Lead,
                scope: vec!["src/rename.rs".into()],
                branch: None,
                worktree: None,
                next_action: None,
                depends_on: vec![],
                related_work: vec![],
                takeover_generation: None,
                preflight: Some(WorkPreflightRequest {
                    source_revision: format!("{:040x}", 999),
                    merge_after: 1,
                    merge_through: 2,
                    source_relation_checked: true,
                    checked_merges: vec![],
                }),
            },
            now + 6,
        )
        .unwrap();
    assert_eq!(preflight.status, WorkPreflightStatus::ReviewRequired);
    assert!(
        preflight
            .reasons
            .contains(&WorkPreflightReason::RepositoryIdentityRenamed)
    );
    let disjoint = fixture
        .store
        .work_preflight(
            &new_token,
            &WorkStart {
                work_id: None,
                source: None,
                request_key: Some("rename-disjoint".into()),
                title: "Disjoint after rename".into(),
                session: "rename-disjoint".into(),
                role: WorkRole::Lead,
                scope: vec!["docs/disjoint.md".into()],
                branch: None,
                worktree: None,
                next_action: None,
                depends_on: vec![],
                related_work: vec![],
                takeover_generation: None,
                preflight: Some(WorkPreflightRequest {
                    source_revision: format!("{:040x}", 998),
                    merge_after: 1,
                    merge_through: 2,
                    source_relation_checked: true,
                    checked_merges: vec![],
                }),
            },
            now + 6,
        )
        .unwrap();
    assert!(
        !disjoint
            .reasons
            .contains(&WorkPreflightReason::RepositoryIdentityRenamed),
        "an unrelated old-name Work item does not block disjoint new Work"
    );
    fixture
        .store
        .work_checkpoint(
            &old_token,
            &WorkCheckpoint {
                work_id: started.item.id.clone(),
                operation_id: "rename-finish".into(),
                generation: started.item.generation,
                session: "rename-old".into(),
                expected_updated_seq: Some(started.item.updated_seq),
                kind: CheckpointKind::Finish,
                summary: "Finished before exact alias retry".into(),
                next_action: None,
                scope: None,
                branch: None,
                worktree: None,
                resolve_blocker: false,
                source_refs: vec![],
                depends_on: vec![],
                related_work: vec![],
            },
            now + 7,
        )
        .unwrap();
    let exact = fixture
        .store
        .work_preflight(
            &new_token,
            &WorkStart {
                work_id: None,
                source: None,
                request_key: Some("rename-active".into()),
                title: "Exact old subject".into(),
                session: "rename-exact".into(),
                role: WorkRole::Lead,
                scope: vec!["docs/disjoint.md".into()],
                branch: None,
                worktree: None,
                next_action: None,
                depends_on: vec![],
                related_work: vec![],
                takeover_generation: None,
                preflight: Some(WorkPreflightRequest {
                    source_revision: format!("{:040x}", 997),
                    merge_after: 1,
                    merge_through: 2,
                    source_relation_checked: true,
                    checked_merges: vec![],
                }),
            },
            now + 8,
        )
        .unwrap();
    assert!(
        exact
            .reasons
            .contains(&WorkPreflightReason::RepositoryIdentityRenamed),
        "an exact old-name subject remains visible after finish"
    );

    let taken = fixture.token_for_repository(&owner, "taken", old, 248, false);
    assert!(
        fixture
            .store
            .work_catchup(&taken, &CatchupQuery::default(), now + 6)
            .unwrap()
            .merges
            .is_empty()
    );
    let unbound_old = mint_agent_token(&*fixture.store, &owner, "unbound-old", old, 3600)
        .unwrap()
        .0;
    let (unbound_merge, at) = merge(249, now + 7);
    fixture
        .store
        .record_catchup_merge(&named_scope(old, None), &unbound_merge, at)
        .unwrap();
    let unbound_new = mint_agent_token(&*fixture.store, &owner, "unbound-new", new, 3600)
        .unwrap()
        .0;
    assert_eq!(
        fixture
            .store
            .work_catchup(&unbound_old, &CatchupQuery::default(), now + 8)
            .unwrap()
            .merges
            .len(),
        1
    );
    assert!(
        fixture
            .store
            .work_catchup(&unbound_new, &CatchupQuery::default(), now + 8)
            .unwrap()
            .merges
            .is_empty()
    );
}

fn agent_delivery_budget_reserves_human_capacity(fixture: &Fixture) {
    let now = unix_now();
    let owner = fixture.owner(48);
    let token = fixture.token(&owner, "budget", 248, false);
    for n in 0..16 {
        fixture
            .store
            .work_catchup(
                &token,
                &CatchupQuery {
                    subscriber: format!("agent:{n}"),
                    delivery_client: format!("client-{n}"),
                    ..CatchupQuery::default()
                },
                now + n,
            )
            .unwrap();
    }
    assert!(matches!(
        fixture.store.work_catchup(
            &token,
            &CatchupQuery {
                subscriber: "agent:overflow".into(),
                delivery_client: "overflow".into(),
                ..CatchupQuery::default()
            },
            now + 20
        ),
        Err(HubError::WorkCapacity)
    ));
    let human = fixture
        .store
        .work_catchup(
            &token,
            &CatchupQuery {
                delivery_client: "human-reserved".into(),
                ..CatchupQuery::default()
            },
            now + 21,
        )
        .unwrap();
    assert_eq!(human.subscriber, "human");
}

fn human_and_agent_delivery_row_budgets_are_independent(fixture: &Fixture) {
    let now = unix_now();
    let owner = fixture.owner(50);
    let tokens = (0..7)
        .map(|n| fixture.token(&owner, &format!("budget-boundary-{n}"), 250, false))
        .collect::<Vec<_>>();
    for n in 0..32 {
        fixture
            .store
            .work_catchup(
                &tokens[0],
                &CatchupQuery {
                    delivery_client: format!("human-boundary-{n}"),
                    ..CatchupQuery::default()
                },
                now,
            )
            .unwrap();
    }
    for (token_index, token) in tokens.iter().enumerate().take(6) {
        for subscriber_index in 0..16 {
            fixture
                .store
                .work_catchup(
                    token,
                    &CatchupQuery {
                        subscriber: format!("agent:{token_index}:{subscriber_index}"),
                        delivery_client: format!("agent-boundary-{token_index}-{subscriber_index}"),
                        ..CatchupQuery::default()
                    },
                    now,
                )
                .unwrap();
        }
    }
    assert!(matches!(
        fixture.store.work_catchup(
            &tokens[6],
            &CatchupQuery {
                subscriber: "agent:aggregate-overflow".into(),
                delivery_client: "agent-aggregate-overflow".into(),
                ..CatchupQuery::default()
            },
            now
        ),
        Err(HubError::WorkCapacity)
    ));
}

fn large_work_history_pages_without_skipping(fixture: &Fixture) {
    let now = unix_now();
    let owner = fixture.owner(49);
    let token = fixture.token(&owner, "large-pages", 249, false);
    let mut expected = Vec::new();
    for n in 0..35 {
        let scope = (0..256)
            .map(|i| format!("bulk/{n}/{i}/{}", "x".repeat(210)))
            .collect();
        let started = fixture
            .store
            .work_start(
                &token,
                &WorkStart {
                    work_id: None,
                    source: None,
                    request_key: Some(format!("large-{n}")),
                    title: format!("Large {n}"),
                    session: format!("large-{n}"),
                    role: WorkRole::Lead,
                    scope,
                    branch: None,
                    worktree: None,
                    next_action: Some("n".repeat(500)),
                    depends_on: vec![],
                    related_work: vec![],
                    takeover_generation: None,
                    preflight: None,
                },
                now + n,
            )
            .unwrap();
        expected.push(started.item.id);
    }
    let mut page = fixture
        .store
        .work_catchup(
            &token,
            &CatchupQuery {
                delivery_client: "large-client".into(),
                limit: 50,
                ..CatchupQuery::default()
            },
            now + 40,
        )
        .unwrap();
    assert!(
        !page.complete,
        "the byte ceiling splits a valid record-count page"
    );
    let mut actual = page
        .work_events
        .iter()
        .map(|event| event.work_id.clone())
        .collect::<Vec<_>>();
    while !page.complete {
        page = fixture
            .store
            .work_catchup(&token, &continuation(&page, 50), now + 41)
            .unwrap();
        actual.extend(page.work_events.iter().map(|event| event.work_id.clone()));
    }
    expected.sort();
    actual.sort();
    assert_eq!(
        actual, expected,
        "every retained Work event is paged exactly once"
    );
    assert_eq!(
        fixture
            .store
            .work_catchup_ack(
                &token,
                &CatchupAck {
                    delivery: page.delivery,
                    expected_event_from: page.event_from,
                    expected_merge_from: page.merge_from
                },
                now + 42
            )
            .unwrap()
            .status,
        CatchupAckStatus::Advanced
    );
}

fn preview_is_bounded_and_does_not_create_delivery_state(fixture: &Fixture) {
    let now = unix_now();
    let owner = fixture.owner(51);
    let token = fixture.token(&owner, "preview", 251, false);
    for number in 1..=21 {
        let (observed, at) = merge(number, now + number);
        fixture
            .store
            .record_catchup_merge(&scope(251), &observed, at)
            .unwrap();
    }
    let before = fixture.delivery_count();
    let preview = fixture
        .store
        .work_catchup_preview(
            &token,
            &CatchupPreviewQuery {
                subscriber: "agent:preview".into(),
            },
            now + 30,
        )
        .unwrap();
    assert_eq!(preview.event_ack, 0);
    assert_eq!(preview.merge_ack, 0);
    assert_eq!(preview.merge_through, 21);
    assert_eq!(preview.merges.len(), 20);
    assert!(!preview.complete);
    assert_eq!(fixture.delivery_count(), before);
    let repeated = fixture
        .store
        .work_catchup_preview(
            &token,
            &CatchupPreviewQuery {
                subscriber: "agent:preview".into(),
            },
            now + 31,
        )
        .unwrap();
    assert_eq!(repeated, preview);
    assert_eq!(fixture.delivery_count(), before);

    let durable = fixture
        .store
        .work_catchup(
            &token,
            &CatchupQuery {
                subscriber: "agent:preview".into(),
                delivery_client: "preview-tool".into(),
                ..CatchupQuery::default()
            },
            now + 32,
        )
        .unwrap();
    assert_eq!(fixture.delivery_count(), before + 1);
    assert_eq!(durable.merge_through, preview.merge_through);
}

fn failed_publication_projection_can_be_repaired(fixture: &Fixture) {
    let now = unix_now();
    let owner = fixture.owner(52);
    let token = fixture.token(&owner, "publication-repair", 252, false);
    let publication = create_pull_publication(
        fixture,
        &token,
        "repair-publication",
        "Repair projection",
        520,
        now,
    );
    let invalid_commit = format!("legacy\n{}", "x".repeat(4096));
    fixture.set_publication_merge(&publication, &invalid_commit, now + 1);
    let incomplete = fixture
        .store
        .work_catchup_preview(
            &token,
            &CatchupPreviewQuery {
                subscriber: "agent:repair".into(),
            },
            now + 2,
        )
        .unwrap();
    assert_eq!(fixture.publication_failure_count(), 1);
    let tracked = incomplete
        .source_coverage
        .iter()
        .find(|source| source.source == CatchupMergeSource::TrackedPublication)
        .unwrap();
    assert_eq!(tracked.state, CatchupCoverageState::Incomplete);
    assert_eq!(tracked.deferred, 1);

    let ordinary_replacement = "6".repeat(40);
    fixture
        .store
        .update_publication(
            &publication,
            &Update::Merged {
                merge_commit: ordinary_replacement,
            },
            now + 3,
        )
        .unwrap();
    assert_eq!(fixture.publication_failure_count(), 1);
    assert_eq!(
        fixture
            .store
            .publication(REPOSITORY, &publication)
            .unwrap()
            .unwrap()
            .merge_commit
            .as_deref(),
        Some(invalid_commit.as_str())
    );
    assert!(fixture.events("publication_merge_corrected").is_empty());

    let repaired_commit = "9".repeat(40);
    let proposal = fixture
        .store
        .repair_catchup_publication_merge(
            &CatchupPublicationRepair {
                repository: REPOSITORY.into(),
                publication: publication.clone(),
                merge_commit: repaired_commit.clone(),
                apply: false,
            },
            now + 3,
        )
        .unwrap();
    assert!(!proposal.applied);
    assert_eq!(
        proposal
            .previous_merge_commit_sha256
            .as_ref()
            .unwrap()
            .len(),
        64
    );
    assert_eq!(
        proposal.previous_merge_commit_sha256.as_deref(),
        Some(store::lower_hex_of(invalid_commit.as_bytes()).as_str())
    );
    assert_eq!(fixture.publication_failure_count(), 1);
    assert_eq!(
        fixture
            .store
            .publication(REPOSITORY, &publication)
            .unwrap()
            .unwrap()
            .merge_commit
            .as_deref(),
        Some(invalid_commit.as_str())
    );

    for invalid in [
        CatchupPublicationRepair {
            repository: "acme/other".into(),
            publication: publication.clone(),
            merge_commit: repaired_commit.clone(),
            apply: false,
        },
        CatchupPublicationRepair {
            repository: REPOSITORY.into(),
            publication: "pub_00000000000000000000000000000000".into(),
            merge_commit: repaired_commit.clone(),
            apply: false,
        },
        CatchupPublicationRepair {
            repository: REPOSITORY.into(),
            publication: publication.clone(),
            merge_commit: "A".repeat(40),
            apply: false,
        },
    ] {
        assert!(
            fixture
                .store
                .repair_catchup_publication_merge(&invalid, now + 3)
                .is_err()
        );
    }

    let repaired = fixture
        .store
        .repair_catchup_publication_merge(
            &CatchupPublicationRepair {
                repository: REPOSITORY.into(),
                publication: publication.clone(),
                merge_commit: repaired_commit.clone(),
                apply: true,
            },
            now + 4,
        )
        .unwrap();
    assert!(repaired.applied);
    assert_eq!(fixture.publication_failure_count(), 0);
    let correction_events = fixture.events("publication_merge_corrected");
    assert_eq!(correction_events.len(), 1);
    assert_eq!(
        correction_events[0],
        json!({
            "repository": REPOSITORY,
            "publication": publication,
            "previous_merge_commit_sha256": store::lower_hex_of(invalid_commit.as_bytes()),
            "merge_commit": repaired_commit,
        })
    );
    let complete = fixture
        .store
        .work_catchup_preview(
            &token,
            &CatchupPreviewQuery {
                subscriber: "agent:repair".into(),
            },
            now + 5,
        )
        .unwrap();
    assert_eq!(complete.merges.len(), 1);
    assert_eq!(complete.merges[0].source_id, publication);
    assert_eq!(complete.merges[0].merge_commit, repaired_commit);
    let tracked = complete
        .source_coverage
        .iter()
        .find(|source| source.source == CatchupMergeSource::TrackedPublication)
        .unwrap();
    assert_eq!(tracked.state, CatchupCoverageState::ObservedOnly);
    assert_eq!(tracked.deferred, 0);

    assert!(
        fixture
            .store
            .repair_catchup_publication_merge(
                &CatchupPublicationRepair {
                    repository: REPOSITORY.into(),
                    publication: publication.clone(),
                    merge_commit: repaired_commit.clone(),
                    apply: true,
                },
                now + 6,
            )
            .is_err()
    );
    assert_eq!(fixture.events("publication_merge_corrected").len(), 1);

    let valid_publication = create_pull_publication(
        fixture,
        &token,
        "valid-repair-refusal",
        "Valid projection has no failure",
        521,
        now + 7,
    );
    let valid_commit = "8".repeat(40);
    fixture
        .store
        .update_publication(
            &valid_publication,
            &Update::Merged {
                merge_commit: valid_commit.clone(),
            },
            now + 8,
        )
        .unwrap();
    for apply in [false, true] {
        let error = fixture
            .store
            .repair_catchup_publication_merge(
                &CatchupPublicationRepair {
                    repository: REPOSITORY.into(),
                    publication: valid_publication.clone(),
                    merge_commit: valid_commit.clone(),
                    apply,
                },
                now + 9,
            )
            .unwrap_err();
        assert_eq!(
            error.to_string(),
            "invalid: no retained catch-up projection failure for publication"
        );
    }
}

fn publication_repair_accepts_canonical_forge_observation(fixture: &Fixture) {
    let now = unix_now();
    let owner = fixture.owner(57);
    let token = fixture.token(&owner, "canonical-repair", 257, false);
    let publication = create_pull_publication(
        fixture,
        &token,
        "canonical-repair",
        "Canonical repair",
        570,
        now,
    );
    fixture.set_publication_merge(&publication, "not-a-commit", now + 1);
    fixture
        .store
        .work_catchup_preview(
            &token,
            &CatchupPreviewQuery {
                subscriber: "agent:canonical-repair".into(),
            },
            now + 2,
        )
        .unwrap();
    assert_eq!(fixture.publication_failure_count(), 1);

    let repaired_commit = "5".repeat(40);
    let mut forge_merge = merge(570, now + 3).0;
    forge_merge.head_commit = "b".repeat(40);
    forge_merge.merge_commit = repaired_commit.clone();
    let retained_forge = fixture
        .store
        .record_catchup_merge(&scope(257), &forge_merge, now + 3)
        .unwrap();
    assert_eq!(retained_forge.source, CatchupMergeSource::ForgePull);

    let proposal = fixture
        .store
        .repair_catchup_publication_merge(
            &CatchupPublicationRepair {
                repository: REPOSITORY.into(),
                publication: publication.clone(),
                merge_commit: repaired_commit.clone(),
                apply: false,
            },
            now + 4,
        )
        .unwrap();
    assert!(!proposal.applied);
    assert_eq!(fixture.publication_failure_count(), 1);

    let applied = fixture
        .store
        .repair_catchup_publication_merge(
            &CatchupPublicationRepair {
                repository: REPOSITORY.into(),
                publication: publication.clone(),
                merge_commit: repaired_commit.clone(),
                apply: true,
            },
            now + 5,
        )
        .unwrap();
    assert!(applied.applied);
    assert_eq!(fixture.publication_failure_count(), 0);
    assert_eq!(
        fixture
            .store
            .publication(REPOSITORY, &publication)
            .unwrap()
            .unwrap()
            .merge_commit
            .as_deref(),
        Some(repaired_commit.as_str())
    );
    let observations = fixture
        .store
        .catchup_merges_through(
            &scope(257),
            0,
            fixture.store.catchup_merge_highwater(&scope(257)).unwrap(),
            10,
        )
        .unwrap();
    assert_eq!(observations, vec![retained_forge]);

    let immutable = fixture.store.repair_catchup_publication_merge(
        &CatchupPublicationRepair {
            repository: REPOSITORY.into(),
            publication: publication.clone(),
            merge_commit: "4".repeat(40),
            apply: true,
        },
        now + 6,
    );
    assert_eq!(
        immutable.unwrap_err().to_string(),
        "invalid: no retained catch-up projection failure for publication"
    );
    assert_eq!(
        fixture
            .store
            .publication(REPOSITORY, &publication)
            .unwrap()
            .unwrap()
            .merge_commit
            .as_deref(),
        Some(repaired_commit.as_str())
    );
}

fn strict_publication_repair_rolls_back_projection_failure(fixture: &Fixture) {
    let now = unix_now();
    let owner = fixture.owner(56);
    let token = fixture.token(&owner, "strict-repair", 256, false);
    let publication =
        create_pull_publication(fixture, &token, "strict-repair", "Strict repair", 560, now);
    fixture.set_publication_merge(&publication, "not-a-commit", now + 1);
    fixture
        .store
        .work_catchup_preview(
            &token,
            &CatchupPreviewQuery {
                subscriber: "agent:strict-repair".into(),
            },
            now + 2,
        )
        .unwrap();
    assert_eq!(fixture.publication_failure_count(), 1);
    if let Some(database) = &fixture.database {
        rusqlite::Connection::open(database)
            .unwrap()
            .execute_batch(
                "CREATE TRIGGER reject_strict_catchup_projection
                 BEFORE INSERT ON hub_catchup_merges
                 WHEN NEW.source='tracked_publication'
                 BEGIN SELECT RAISE(ABORT, 'synthetic strict projection failure'); END;",
            )
            .unwrap();
    } else {
        Client::connect(&fixture._pg.as_ref().unwrap().url, NoTls)
            .unwrap()
            .batch_execute(
                "CREATE FUNCTION reject_strict_catchup_projection() RETURNS trigger LANGUAGE plpgsql AS $$
                 BEGIN
                   IF NEW.source = 'tracked_publication' THEN
                     RAISE EXCEPTION 'synthetic strict projection failure';
                   END IF;
                   RETURN NEW;
                 END $$;
                 CREATE TRIGGER reject_strict_catchup_projection
                 BEFORE INSERT ON hub_catchup_merges
                 FOR EACH ROW EXECUTE FUNCTION reject_strict_catchup_projection();",
            )
            .unwrap();
    }

    let commit = "7".repeat(40);
    let repair = CatchupPublicationRepair {
        repository: REPOSITORY.into(),
        publication: publication.clone(),
        merge_commit: commit.clone(),
        apply: true,
    };
    assert!(
        fixture
            .store
            .repair_catchup_publication_merge(&repair, now + 3)
            .is_err()
    );
    assert_eq!(fixture.publication_failure_count(), 1);
    assert!(fixture.events("publication_merge_corrected").is_empty());
    assert_eq!(
        fixture
            .store
            .publication(REPOSITORY, &publication)
            .unwrap()
            .unwrap()
            .merge_commit
            .as_deref(),
        Some("not-a-commit")
    );

    if let Some(database) = &fixture.database {
        rusqlite::Connection::open(database)
            .unwrap()
            .execute_batch("DROP TRIGGER reject_strict_catchup_projection;")
            .unwrap();
    } else {
        Client::connect(&fixture._pg.as_ref().unwrap().url, NoTls)
            .unwrap()
            .batch_execute(
                "DROP TRIGGER reject_strict_catchup_projection ON hub_catchup_merges;
                 DROP FUNCTION reject_strict_catchup_projection();",
            )
            .unwrap();
    }
    let result = fixture
        .store
        .repair_catchup_publication_merge(&repair, now + 4)
        .unwrap();
    assert!(result.applied);
    assert_eq!(fixture.publication_failure_count(), 0);
    assert_eq!(fixture.events("publication_merge_corrected").len(), 1);
    assert_eq!(
        fixture
            .store
            .publication(REPOSITORY, &publication)
            .unwrap()
            .unwrap()
            .merge_commit
            .as_deref(),
        Some(commit.as_str())
    );
}

fn invisible_identity_is_rejected_and_projection_title_is_sanitized(fixture: &Fixture) {
    let now = unix_now();
    let owner = fixture.owner(53);
    let token = fixture.token(&owner, "invisible", 253, false);
    for mut invalid in [
        NewCatchupMerge {
            title: Some("hidden\u{200d}title".into()),
            ..merge(531, now).0
        },
        NewCatchupMerge {
            source_id: "pull:\u{2060}532".into(),
            ..merge(532, now).0
        },
        NewCatchupMerge {
            branch: "feature\u{200f}533".into(),
            ..merge(533, now).0
        },
        NewCatchupMerge {
            base: "ma\u{202e}in".into(),
            ..merge(534, now).0
        },
    ] {
        invalid.pull_number += 10_000;
        assert!(
            fixture
                .store
                .record_catchup_merge(&scope(253), &invalid, now)
                .is_err()
        );
    }

    let publication = create_pull_publication(
        fixture,
        &token,
        "visible-emoji",
        "Family 👨\u{200d}👩\u{200d}👧 release",
        535,
        now,
    );
    fixture
        .store
        .update_publication(
            &publication,
            &Update::Merged {
                merge_commit: "8".repeat(40),
            },
            now + 1,
        )
        .unwrap();
    let preview = fixture
        .store
        .work_catchup_preview(
            &token,
            &CatchupPreviewQuery {
                subscriber: "agent:invisible".into(),
            },
            now + 2,
        )
        .unwrap();
    let title = preview.merges[0].title.as_deref().unwrap();
    assert!(!title.contains('\u{200d}'));
    assert_eq!(title, "Family 👨👩👧 release");
}

#[test]
fn sqlite_catchup_contracts() {
    let fixture = Fixture::sqlite();
    contracts(&fixture);
    let database = fixture.database.as_ref().unwrap();
    let connection = rusqlite::Connection::open(database).unwrap();
    assert!(
        connection
            .execute("UPDATE hub_catchup_merges SET title='changed'", [])
            .is_err()
    );
    assert!(
        connection
            .execute("DELETE FROM hub_catchup_merges", [])
            .is_err()
    );
}

#[test]
fn sqlite_preview_is_bounded_and_does_not_create_delivery_state() {
    preview_is_bounded_and_does_not_create_delivery_state(&Fixture::sqlite());
}

#[test]
fn postgres_preview_is_bounded_and_does_not_create_delivery_state() {
    let Some(fixture) = Fixture::postgres() else {
        return;
    };
    preview_is_bounded_and_does_not_create_delivery_state(&fixture);
}

#[test]
fn sqlite_human_ack_receipt_records_current_actor() {
    human_ack_receipt_records_current_actor(&Fixture::sqlite());
}

#[test]
fn postgres_human_ack_receipt_records_current_actor() {
    let Some(fixture) = Fixture::postgres() else {
        return;
    };
    human_ack_receipt_records_current_actor(&fixture);
}

#[test]
fn sqlite_failed_publication_projection_can_be_repaired() {
    failed_publication_projection_can_be_repaired(&Fixture::sqlite());
}

#[test]
fn postgres_failed_publication_projection_can_be_repaired() {
    let Some(fixture) = Fixture::postgres() else {
        return;
    };
    failed_publication_projection_can_be_repaired(&fixture);
}

#[test]
fn sqlite_publication_repair_accepts_canonical_forge_observation() {
    publication_repair_accepts_canonical_forge_observation(&Fixture::sqlite());
}

#[test]
fn postgres_publication_repair_accepts_canonical_forge_observation() {
    let Some(fixture) = Fixture::postgres() else {
        return;
    };
    publication_repair_accepts_canonical_forge_observation(&fixture);
}

#[test]
fn sqlite_strict_publication_repair_rolls_back_projection_failure() {
    strict_publication_repair_rolls_back_projection_failure(&Fixture::sqlite());
}

#[test]
fn postgres_strict_publication_repair_rolls_back_projection_failure() {
    let Some(fixture) = Fixture::postgres() else {
        return;
    };
    strict_publication_repair_rolls_back_projection_failure(&fixture);
}

#[test]
fn sqlite_invisible_identity_is_rejected_and_projection_title_is_sanitized() {
    invisible_identity_is_rejected_and_projection_title_is_sanitized(&Fixture::sqlite());
}

#[test]
fn postgres_invisible_identity_is_rejected_and_projection_title_is_sanitized() {
    let Some(fixture) = Fixture::postgres() else {
        return;
    };
    invisible_identity_is_rejected_and_projection_title_is_sanitized(&fixture);
}

#[test]
fn postgres_projection_sql_failure_isolated_by_savepoint_and_retryable() {
    let Some(fixture) = Fixture::postgres() else {
        return;
    };
    let now = unix_now();
    let owner = fixture.owner(54);
    let token = fixture.token(&owner, "projection-savepoint", 254, false);
    let publication = create_pull_publication(
        &fixture,
        &token,
        "projection-savepoint",
        "Projection savepoint",
        540,
        now,
    );
    let mut client = Client::connect(&fixture._pg.as_ref().unwrap().url, NoTls).unwrap();
    client
        .batch_execute(
            "CREATE FUNCTION reject_tracked_catchup_projection() RETURNS trigger LANGUAGE plpgsql AS $$
             BEGIN
               IF NEW.source = 'tracked_publication' THEN
                 RAISE EXCEPTION 'synthetic projection failure';
               END IF;
               RETURN NEW;
             END $$;
             CREATE TRIGGER reject_tracked_catchup_projection
             BEFORE INSERT ON hub_catchup_merges
             FOR EACH ROW EXECUTE FUNCTION reject_tracked_catchup_projection();",
        )
        .unwrap();
    let commit = "7".repeat(40);
    let merged = fixture
        .store
        .update_publication(
            &publication,
            &Update::Merged {
                merge_commit: commit.clone(),
            },
            now + 1,
        )
        .expect("optional catch-up projection must not abort the publication update");
    assert_eq!(merged.merge_commit.as_deref(), Some(commit.as_str()));
    assert_eq!(fixture.publication_failure_count(), 1);
    assert_eq!(
        fixture
            .store
            .publication(REPOSITORY, &publication)
            .unwrap()
            .unwrap()
            .merge_commit
            .as_deref(),
        Some(commit.as_str())
    );
    client
        .batch_execute(
            "DROP TRIGGER reject_tracked_catchup_projection ON hub_catchup_merges;
             DROP FUNCTION reject_tracked_catchup_projection();",
        )
        .unwrap();
    fixture
        .store
        .update_publication(
            &publication,
            &Update::Merged {
                merge_commit: commit.clone(),
            },
            now + 2,
        )
        .unwrap();
    assert_eq!(fixture.publication_failure_count(), 0);
    let preview = fixture
        .store
        .work_catchup_preview(
            &token,
            &CatchupPreviewQuery {
                subscriber: "agent:savepoint".into(),
            },
            now + 3,
        )
        .unwrap();
    assert_eq!(preview.merges.len(), 1);
    assert_eq!(preview.merges[0].source_id, publication);
}

#[test]
fn sqlite_legacy_start_survives_optional_projection_storage_failure() {
    let fixture = Fixture::sqlite();
    let now = unix_now();
    let owner = fixture.owner(55);
    let token = fixture.token(&owner, "legacy-no-preflight", 255, false);
    rusqlite::Connection::open(fixture.database.as_ref().unwrap())
        .unwrap()
        .execute("DROP TABLE hub_publications", [])
        .unwrap();
    let started = fixture
        .store
        .work_start(
            &token,
            &WorkStart {
                work_id: None,
                source: None,
                request_key: Some("legacy-no-preflight".into()),
                title: "Legacy claim remains compatible".into(),
                session: "legacy-no-preflight".into(),
                role: WorkRole::Lead,
                scope: vec!["src/legacy.rs".into()],
                branch: None,
                worktree: None,
                next_action: None,
                depends_on: vec![],
                related_work: vec![],
                takeover_generation: None,
                preflight: None,
            },
            now,
        )
        .unwrap();
    assert!(started.created);
    assert_eq!(
        started.preflight.unwrap().status,
        WorkPreflightStatus::Unknown
    );
}

#[test]
fn sqlite_backfills_historical_publication_without_loading_manifest() {
    let fixture = Fixture::sqlite();
    let owner = fixture.owner(33);
    let token = fixture.token(&owner, "historical", REPOSITORY_ID, false);
    let now = unix_now();
    let connection = rusqlite::Connection::open(fixture.database.as_ref().unwrap()).unwrap();
    connection
        .execute(
            "INSERT INTO hub_publications
                (id,repository,token,revision,event_seq,commit_sha,branch,base,manifest,
                 branch_state,pr_number,merge_commit,created_at,updated_at)
             VALUES (?1,?2,?3,?4,NULL,?5,?6,?7,?8,'merged',?9,?10,?11,?11)",
            rusqlite::params![
                "pub_00000000000000000000000000000001",
                REPOSITORY,
                token,
                format!("{:040x}", 1),
                format!("{:040x}", 2),
                "historical-branch",
                "main",
                "not-json-and-deliberately-not-read",
                301,
                format!("{:040x}", 3),
                now - 100,
            ],
        )
        .unwrap();
    fixture
        .store
        .record_catchup_source_status(
            &scope(REPOSITORY_ID),
            &CatchupSourceStatus {
                source: CatchupMergeSource::ForgePull,
                state: CatchupCoverageState::ObservedOnly,
                checked_at: now,
                unread_pages: 0,
                deferred: 0,
            },
        )
        .unwrap();
    let preflight = fixture
        .store
        .work_preflight(
            &token,
            &WorkStart {
                work_id: None,
                source: None,
                request_key: Some("historical-preflight".into()),
                title: "Historical preflight".into(),
                session: "historical".into(),
                role: WorkRole::Lead,
                scope: vec!["src/historical.rs".into()],
                branch: None,
                worktree: None,
                next_action: None,
                depends_on: vec![],
                related_work: vec![],
                takeover_generation: None,
                preflight: Some(WorkPreflightRequest {
                    source_revision: format!("{:040x}", 99),
                    merge_after: 0,
                    merge_through: 0,
                    source_relation_checked: true,
                    checked_merges: vec![],
                }),
            },
            now,
        )
        .unwrap();
    assert_eq!(
        preflight.current_merge_through, 1,
        "preflight runs the durable historical backfill first"
    );
    assert!(
        preflight
            .reasons
            .contains(&WorkPreflightReason::MergeHighwaterChanged)
    );
    let page = fixture
        .store
        .work_catchup(&token, &CatchupQuery::default(), now)
        .unwrap();
    assert_eq!(page.merges.len(), 1);
    assert_eq!(page.merges[0].pull_number, 301);
    assert_eq!(page.merges[0].observed_at, now);
    assert_eq!(page.merges[0].occurred_at, None);
    assert!(!page.merges[0].paths_complete);
    assert!(page.merges[0].paths.is_empty());
    assert_eq!(page.merges[0].title, None);
    assert_eq!(
        page.source_coverage
            .iter()
            .find(|source| source.source == CatchupMergeSource::TrackedPublication)
            .unwrap()
            .state,
        CatchupCoverageState::ObservedOnly
    );
}

#[test]
fn postgres_catchup_contracts() {
    let Some(fixture) = Fixture::postgres() else {
        return;
    };
    contracts(&fixture);
}

#[test]
fn sqlite_catchup_delivery_blocks_compaction_until_ack_or_expiry() {
    compaction_delivery_guard(&Fixture::sqlite());
}

#[test]
fn postgres_catchup_delivery_blocks_compaction_until_ack_or_expiry() {
    let Some(fixture) = Fixture::postgres() else {
        return;
    };
    compaction_delivery_guard(&fixture);
}

#[test]
fn sqlite_non_lead_cannot_bypass_nonclear_preflight() {
    non_lead_cannot_use_explicit_lead_retry_bypass(&Fixture::sqlite());
}

#[test]
fn postgres_non_lead_cannot_bypass_nonclear_preflight() {
    let Some(fixture) = Fixture::postgres() else {
        return;
    };
    non_lead_cannot_use_explicit_lead_retry_bypass(&fixture);
}

#[test]
fn sqlite_stable_repository_id_survives_rename() {
    stable_repository_id_survives_rename(&Fixture::sqlite());
}

#[test]
fn postgres_stable_repository_id_survives_rename() {
    let Some(fixture) = Fixture::postgres() else {
        return;
    };
    stable_repository_id_survives_rename(&fixture);
}

#[test]
fn sqlite_agent_delivery_budget_reserves_human_capacity() {
    agent_delivery_budget_reserves_human_capacity(&Fixture::sqlite());
}

#[test]
fn postgres_agent_delivery_budget_reserves_human_capacity() {
    let Some(fixture) = Fixture::postgres() else {
        return;
    };
    agent_delivery_budget_reserves_human_capacity(&fixture);
}

#[test]
fn sqlite_human_and_agent_delivery_row_budgets_are_independent() {
    human_and_agent_delivery_row_budgets_are_independent(&Fixture::sqlite());
}

#[test]
fn postgres_human_and_agent_delivery_row_budgets_are_independent() {
    let Some(fixture) = Fixture::postgres() else {
        return;
    };
    human_and_agent_delivery_row_budgets_are_independent(&fixture);
}

#[test]
fn sqlite_large_work_history_pages_without_skipping() {
    large_work_history_pages_without_skipping(&Fixture::sqlite());
}

#[test]
fn postgres_large_work_history_pages_without_skipping() {
    let Some(fixture) = Fixture::postgres() else {
        return;
    };
    large_work_history_pages_without_skipping(&fixture);
}

#[test]
fn postgres_same_person_ack_is_atomic_across_connections() {
    let Some(pg) = PgSchema::create() else {
        return;
    };
    let first = store::open(&pg.url).unwrap();
    let second = store::open(&pg.url).unwrap();
    let owner = first
        .upsert_user(
            &ForgeIdentity {
                provider: "github".into(),
                host: "github.com".into(),
                user_id: 99,
            },
            "concurrent-reader",
        )
        .unwrap()
        .id;
    let token_a = mint_bound_agent_token(
        &*first,
        &owner,
        "device-a",
        REPOSITORY,
        REPOSITORY_ID,
        3600,
        false,
    )
    .unwrap()
    .0;
    let token_b = mint_bound_agent_token(
        &*first,
        &owner,
        "device-b",
        "acme/widgets-renamed",
        REPOSITORY_ID,
        3600,
        false,
    )
    .unwrap()
    .0;
    let now = unix_now();
    let (observed, at) = merge(91, now);
    first
        .record_catchup_merge(&scope(REPOSITORY_ID), &observed, at)
        .unwrap();
    let page = first
        .work_catchup(&token_a, &CatchupQuery::default(), now + 1)
        .unwrap();
    assert!(page.complete);
    let ack = CatchupAck {
        delivery: page.delivery,
        expected_event_from: page.event_from,
        expected_merge_from: page.merge_from,
    };
    let barrier = Arc::new(Barrier::new(3));
    let left_barrier = barrier.clone();
    let left_ack = ack.clone();
    let left = std::thread::spawn(move || {
        left_barrier.wait();
        first
            .work_catchup_ack(&token_a, &left_ack, now + 2)
            .unwrap()
            .status
    });
    let right_barrier = barrier.clone();
    let right = std::thread::spawn(move || {
        right_barrier.wait();
        second
            .work_catchup_ack(&token_b, &ack, now + 2)
            .unwrap()
            .status
    });
    barrier.wait();
    let statuses = [left.join().unwrap(), right.join().unwrap()];
    assert_eq!(
        statuses
            .iter()
            .filter(|status| **status == CatchupAckStatus::Advanced)
            .count(),
        1
    );
    assert_eq!(
        statuses
            .iter()
            .filter(|status| **status == CatchupAckStatus::AlreadyAcknowledged)
            .count(),
        1
    );
}

#[test]
fn postgres_merge_ingest_and_work_admission_share_a_repository_lock() {
    let Some(pg) = PgSchema::create() else {
        return;
    };
    let admission_store = store::open(&pg.url).unwrap();
    let ingest_store = store::open(&pg.url).unwrap();
    let owner = admission_store
        .upsert_user(
            &ForgeIdentity {
                provider: "github".into(),
                host: "github.com".into(),
                user_id: 100,
            },
            "atomic-admission",
        )
        .unwrap()
        .id;
    let token = mint_bound_agent_token(
        &*admission_store,
        &owner,
        "worker",
        REPOSITORY,
        REPOSITORY_ID,
        3600,
        false,
    )
    .unwrap()
    .0;
    let now = unix_now();
    admission_store
        .record_catchup_source_status(
            &scope(REPOSITORY_ID),
            &CatchupSourceStatus {
                source: CatchupMergeSource::ForgePull,
                state: CatchupCoverageState::ObservedOnly,
                checked_at: now,
                unread_pages: 0,
                deferred: 0,
            },
        )
        .unwrap();
    let start = WorkStart {
        work_id: None,
        source: None,
        request_key: Some("atomic-preflight".into()),
        title: "Atomic preflight".into(),
        session: "atomic-session".into(),
        role: WorkRole::Lead,
        scope: vec!["src/atomic.rs".into()],
        branch: None,
        worktree: None,
        next_action: None,
        depends_on: Vec::new(),
        related_work: Vec::new(),
        takeover_generation: None,
        preflight: Some(WorkPreflightRequest {
            source_revision: format!("{:040x}", 900),
            merge_after: 0,
            merge_through: 0,
            source_relation_checked: true,
            checked_merges: Vec::new(),
        }),
    };
    let (mut incoming, observed_at) = merge(92, now + 1);
    incoming.paths = vec!["src/atomic.rs".into()];
    let barrier = Arc::new(Barrier::new(3));
    let admission_barrier = barrier.clone();
    let admission_token = token.clone();
    let admission_start = start.clone();
    let admission = std::thread::spawn(move || {
        admission_barrier.wait();
        admission_store.work_start(&admission_token, &admission_start, now + 2)
    });
    let ingest_barrier = barrier.clone();
    let ingest = std::thread::spawn(move || {
        ingest_barrier.wait();
        ingest_store
            .record_catchup_merge(
                &named_scope("acme/widgets-renamed", Some(REPOSITORY_ID)),
                &incoming,
                observed_at,
            )
            .unwrap()
    });
    barrier.wait();
    let admission = admission.join().unwrap();
    let observed = ingest.join().unwrap();
    assert_eq!(observed.seq, 1);
    match admission {
        Ok(result) => {
            assert!(result.created);
            let preflight = result.preflight.unwrap();
            assert_eq!(
                preflight.status,
                WorkPreflightStatus::ClearWithinCheckedWindow
            );
            assert!(preflight.complete);
        }
        Err(HubError::WorkPreflight(result)) => {
            assert_eq!(result.status, WorkPreflightStatus::ReviewRequired);
            assert!(
                result
                    .reasons
                    .contains(&WorkPreflightReason::MergeHighwaterChanged)
            );
            let inspected = store::open(&pg.url)
                .unwrap()
                .work_preflight(&token, &start, now + 3)
                .unwrap();
            assert!(
                inspected.subject_match.is_none(),
                "a refused preflight must not create Work"
            );
        }
        Err(error) => panic!("unexpected admission error: {error}"),
    }
}
