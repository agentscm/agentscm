//! Durable person-level catch-up storage contracts.

use crate::hub_pg;

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
    BriefingQuery, CheckpointKind, WorkCheckpoint, WorkItem, WorkOverviewQuery,
    WorkRepositoryScope, WorkRole, WorkStart, WorkState,
};
use agentscm::hub::work_lifecycle::{
    MergeStatus, OperationalOverviewV1, OutcomeValue, ReservationStatus,
};
use agentscm::hub::work_preflight::{
    WorkPreflightReason, WorkPreflightRequest, WorkPreflightStatus,
};
use agentscm::hub::work_transition::{
    OperationalSelect, WorkLifecycleReader, WorkTransitionActor, WorkTransitionKind,
    WorkTransitionRequest,
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

    fn merge_ids(&self, repository_id: i64) -> Vec<String> {
        const SQL: &str = "SELECT id FROM hub_catchup_merges WHERE repository_id=";
        if let Some(database) = &self.database {
            let connection = rusqlite::Connection::open(database).unwrap();
            let mut statement = connection
                .prepare(&format!("{SQL}{repository_id} ORDER BY seq"))
                .unwrap();
            return statement
                .query_map([], |row| row.get(0))
                .unwrap()
                .collect::<Result<_, _>>()
                .unwrap();
        }
        Client::connect(&self._pg.as_ref().unwrap().url, NoTls)
            .unwrap()
            .query(&format!("{SQL}{repository_id} ORDER BY seq"), &[])
            .unwrap()
            .iter()
            .map(|row| row.get(0))
            .collect()
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
            same_repository_head: true,
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
                issue_choice: None,
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
                issue_choice: None,
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
                issue_choice: None,
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
                window_from: Some(now - 14 * 86_400),
            },
        )
        .unwrap();
    let initial = fixture
        .store
        .work_catchup(&old_token, &CatchupQuery::default(), now + 1)
        .unwrap();
    let forge = |source: CatchupMergeSource| {
        initial
            .source_coverage
            .iter()
            .find(|c| c.source == source)
            .unwrap()
            .window_from
    };
    assert_eq!(
        forge(CatchupMergeSource::ForgePull),
        Some(now - 14 * 86_400)
    );
    assert_eq!(forge(CatchupMergeSource::TrackedPublication), None);
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
                issue_choice: None,
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
                issue_choice: None,
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
                issue_choice: None,
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
                issue_choice: None,
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
                    issue_choice: None,
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
    assert_eq!(fixture.merge_ids(257), vec![retained_forge.id]);

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
                issue_choice: None,
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
                window_from: None,
            },
        )
        .unwrap();
    let preflight = fixture
        .store
        .work_preflight(
            &token,
            &WorkStart {
                issue_choice: None,
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
                window_from: None,
            },
        )
        .unwrap();
    let start = WorkStart {
        issue_choice: None,
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

fn repository_namespace_repair(fixture: &Fixture, collision: bool) {
    use agentscm::hub::connection::NewConnection;
    use agentscm::hub::work::{ForgeWorkRef, WorkRepositoryRename, WorkSource};
    let now = unix_now();
    let owner = fixture.owner(845);
    let old = "acme/previous";
    let new = "acme/current";
    let old_token = fixture.token_for_repository(&owner, "before", old, 845, false);
    let new_token = fixture.token_for_repository(&owner, "after", new, 845, false);
    let start = |key: &str, path: &str| WorkStart {
        issue_choice: None,
        work_id: None,
        source: None,
        request_key: Some(key.into()),
        title: key.into(),
        session: format!("session-{key}"),
        role: WorkRole::Lead,
        scope: vec![path.into()],
        branch: None,
        worktree: None,
        next_action: None,
        depends_on: vec![],
        related_work: vec![],
        takeover_generation: None,
        preflight: None,
    };
    let original = fixture
        .store
        .work_start(&old_token, &start("retained", "src/retained.rs"), now)
        .unwrap()
        .item;
    let publication = publication(old, "feature/retained", "retained evidence");
    fixture
        .store
        .create_publication(&old_token, &publication, now)
        .unwrap();
    let original = fixture
        .store
        .work_checkpoint(
            &old_token,
            &WorkCheckpoint {
                work_id: original.id.clone(),
                operation_id: "before-rename".into(),
                generation: original.generation,
                session: original.lead_session.clone(),
                expected_updated_seq: Some(original.updated_seq),
                kind: CheckpointKind::Progress,
                summary: "Recorded publication".into(),
                next_action: None,
                scope: None,
                branch: None,
                worktree: None,
                resolve_blocker: false,
                source_refs: vec![agentscm::hub::work::SourceRef::Commit {
                    oid: publication.manifest.commit.clone(),
                    path: None,
                }],
                depends_on: vec![],
                related_work: vec![],
            },
            now,
        )
        .unwrap()
        .item;
    let mut forge = start("unused", "src/issue.rs");
    forge.request_key = None;
    forge.source = Some(ForgeWorkRef {
        provider: "github".into(),
        host: "github.com".into(),
        repository: old.into(),
        issue: "179".into(),
    });
    let issue = fixture
        .store
        .work_start(&old_token, &forge, now)
        .unwrap()
        .item;
    let occupied = fixture
        .store
        .work_start(&new_token, &start("destination", "src/other.rs"), now)
        .unwrap()
        .item;
    if collision {
        fixture
            .store
            .work_start(&new_token, &start("retained", "src/collision.rs"), now)
            .unwrap();
    }
    // An outstanding proposal must retain its identity and target after migration.
    let blocked = fixture
        .store
        .work_start(&old_token, &start("proposal", "src/retained.rs"), now)
        .unwrap();
    let request_id = blocked.coordination_request.unwrap().id;
    let connection = fixture
        .store
        .insert_connection(
            &NewConnection {
                id: "con_84500000000000000000000000000000".into(),
                provider: "github".into(),
                host: "github.com".into(),
                kind: "pat".into(),
                app_id: None,
                app_slug: None,
                acts_as: Some("synthetic".into()),
            },
            &owner,
        )
        .unwrap();
    fixture
        .store
        .upsert_repository(&connection.id, "github.com", 845, new, None, None)
        .unwrap();
    let mut repair = WorkRepositoryRename {
        scope: WorkRepositoryScope {
            provider: "github".into(),
            host: "github.com".into(),
            repository: old.into(),
            repository_id: Some(845),
        },
        destination: new.into(),
        apply: false,
    };
    for invalid in [
        WorkRepositoryRename {
            destination: "acme/unverified".into(),
            ..repair.clone()
        },
        WorkRepositoryRename {
            scope: WorkRepositoryScope {
                repository_id: None,
                ..repair.scope.clone()
            },
            ..repair.clone()
        },
        WorkRepositoryRename {
            scope: WorkRepositoryScope {
                repository_id: Some(846),
                ..repair.scope.clone()
            },
            ..repair.clone()
        },
        WorkRepositoryRename {
            scope: WorkRepositoryScope {
                host: "other.example".into(),
                ..repair.scope.clone()
            },
            ..repair.clone()
        },
    ] {
        assert!(
            fixture
                .store
                .work_repair_repository(&invalid, now + 1)
                .is_err()
        );
    }
    let before = fixture.store.latest_event_seq().unwrap();
    if collision {
        assert!(
            fixture
                .store
                .work_repair_repository(&repair, now + 1)
                .is_err()
        );
        repair.apply = true;
        assert!(
            fixture
                .store
                .work_repair_repository(&repair, now + 1)
                .is_err()
        );
        assert_eq!(fixture.store.latest_event_seq().unwrap(), before);
        assert!(
            fixture
                .store
                .work_get(&new_token, &issue.id, now + 1)
                .unwrap()
                .is_none()
        );
        return;
    }
    let preview = fixture
        .store
        .work_repair_repository(&repair, now + 1)
        .unwrap();
    assert_eq!(preview.work_items, 2);
    assert_eq!(preview.coordination_requests, 1);
    assert!(!preview.applied);
    assert_eq!(fixture.store.latest_event_seq().unwrap(), before);
    assert!(
        fixture
            .store
            .work_get(&new_token, &original.id, now + 1)
            .unwrap()
            .is_none()
    );
    repair.apply = true;
    fixture
        .store
        .work_repair_repository(&repair, now + 2)
        .unwrap();
    let repaired = fixture
        .store
        .work_get(&new_token, &original.id, now + 3)
        .unwrap()
        .unwrap();
    assert_eq!(repaired.repository, new);
    assert_eq!(repaired.generation, original.generation);
    assert_eq!(repaired.lead_token, original.lead_token);
    assert_eq!(repaired.lead_session, original.lead_session);
    assert_eq!(repaired.participants, original.participants);
    assert_eq!(repaired.created_seq, original.created_seq);
    assert_eq!(repaired.current, original.current);
    assert_eq!(
        repaired.claim_status,
        agentscm::hub::work::ClaimStatus::Stale
    );
    let insight = fixture
        .store
        .work_insight(&new_token, &original.id, now + 3)
        .unwrap()
        .unwrap();
    assert!(
        insight
            .publications
            .iter()
            .any(|p| p.publication_id == publication.id)
    );
    assert!(repaired.updated_seq > original.updated_seq);
    assert!(
        fixture
            .store
            .work_get(&old_token, &original.id, now + 3)
            .is_err()
    );
    assert_eq!(
        fixture
            .store
            .work_get(&new_token, &occupied.id, now + 3)
            .unwrap()
            .unwrap(),
        occupied
    );
    let repaired_issue = fixture
        .store
        .work_get(&new_token, &issue.id, now + 3)
        .unwrap()
        .unwrap();
    assert!(
        matches!(repaired_issue.source, WorkSource::Forge { repository, .. } if repository == new)
    );
    forge.source.as_mut().unwrap().repository = new.into();
    let resolved = fixture
        .store
        .work_start(&new_token, &forge, now + 3)
        .unwrap();
    assert_eq!(resolved.item.id, issue.id);
    assert!(!resolved.created);
    assert!(
        resolved.conflict.is_some(),
        "rename must not transfer claim authority"
    );
    let blocked = fixture
        .store
        .work_start(
            &new_token,
            &start("another-proposal", "src/retained.rs"),
            now + 3,
        )
        .unwrap();
    assert!(blocked.coordination_request.is_some());
    let inbox = fixture
        .store
        .coordination_list(&new_token, &Default::default(), now + 3)
        .unwrap();
    // Repository-scoped read remains available even though the old lead token owns the target.
    let request = inbox.requests.iter().find(|r| r.id == request_id).unwrap();
    assert_eq!(request.targets[0].work_id, original.id);
    let before_repeat = fixture.store.latest_event_seq().unwrap();
    assert_eq!(
        fixture
            .store
            .work_repair_repository(&repair, now + 4)
            .unwrap()
            .work_items,
        0
    );
    assert_eq!(fixture.store.latest_event_seq().unwrap(), before_repeat);
    // The old lead token can no longer renew, so the hold opens to takeover once its lease lapses.
    let lapsed = now + 4 + agentscm::hub::work_lifecycle::LEASE_SECONDS + 1;
    let mut at = now + 4;
    while at < lapsed {
        fixture.store.observe_lease_clock(at).unwrap();
        at += 60;
    }
    fixture.store.observe_lease_clock(lapsed).unwrap();
    let mut takeover = start("retained", "src/retained.rs");
    takeover.takeover_generation = Some(original.generation);
    let resumed = fixture
        .store
        .work_start(&new_token, &takeover, lapsed)
        .unwrap();
    assert!(resumed.took_over);
    assert_eq!(resumed.item.id, original.id);
    assert_eq!(resumed.item.generation, original.generation + 1);
    let requests = fixture
        .store
        .coordination_list(&new_token, &Default::default(), lapsed + 1)
        .unwrap();
    let request = requests
        .requests
        .iter()
        .find(|r| r.id == request_id)
        .unwrap();
    fixture
        .store
        .coordination_withdraw(
            &new_token,
            &agentscm::hub::coordination::CoordinationWithdraw {
                request_id: request.id.clone(),
                operation_id: "cleanup-renamed-requester".into(),
                session: resumed.item.lead_session.clone(),
                expected_updated_seq: request.updated_seq,
                target: None,
                via_hub: false,
            },
            lapsed + 1,
        )
        .unwrap();
    // A different repository reusing the old name cannot see the migrated Work.
    let other = fixture.token_for_repository(&owner, "reused", old, 846, false);
    assert!(
        fixture
            .store
            .work_get(&other, &original.id, now + 1001)
            .unwrap()
            .is_none()
    );
}

#[test]
fn sqlite_repository_namespace_repair() {
    repository_namespace_repair(&Fixture::sqlite(), false);
    repository_namespace_repair(&Fixture::sqlite(), true);
}
#[test]
fn postgres_repository_namespace_repair() {
    if let Some(fixture) = Fixture::postgres() {
        repository_namespace_repair(&fixture, false);
        repository_namespace_repair(&Fixture::postgres().unwrap(), true);
    }
}

// Work whose branch has a merged pull request is not listed as current work. These cases pin the
// read-time rule; the Work rows themselves never change.

const SETTLED_RID: i64 = 237;
const AUTO_PAUSE: &str = "Session ended without a reported final work outcome.";

fn begin(
    fixture: &Fixture,
    token: &str,
    key: &str,
    branch: Option<&str>,
    path: &str,
    at: i64,
) -> WorkItem {
    fixture
        .store
        .work_start(
            token,
            &WorkStart {
                issue_choice: None,
                work_id: None,
                source: None,
                request_key: Some(key.into()),
                title: format!("Work {key}"),
                session: format!("session-{key}"),
                role: WorkRole::Lead,
                scope: vec![path.into()],
                branch: branch.map(str::to_owned),
                worktree: None,
                next_action: None,
                depends_on: Vec::new(),
                related_work: Vec::new(),
                takeover_generation: None,
                preflight: None,
            },
            at,
        )
        .unwrap()
        .item
}

fn checkin(
    fixture: &Fixture,
    token: &str,
    item: &WorkItem,
    kind: CheckpointKind,
    summary: &str,
    at: i64,
) -> WorkItem {
    fixture
        .store
        .work_checkpoint(
            token,
            &WorkCheckpoint {
                work_id: item.id.clone(),
                operation_id: format!("op-{}-{at}", item.id),
                generation: item.generation,
                session: format!("session-{}", item.title.trim_start_matches("Work ")),
                expected_updated_seq: Some(item.updated_seq),
                kind,
                summary: summary.into(),
                next_action: None,
                scope: None,
                branch: None,
                worktree: None,
                resolve_blocker: false,
                source_refs: Vec::new(),
                depends_on: Vec::new(),
                related_work: Vec::new(),
            },
            at,
        )
        .unwrap()
        .item
}

fn merged(fixture: &Fixture, number: i64, branch: &str, base: &str, same_repo: bool, at: i64) {
    merged_observed(fixture, number, branch, base, same_repo, Some(at), at);
}

/// A merge whose provider time may be unknown, as a tracked publication's is.
fn merged_observed(
    fixture: &Fixture,
    number: i64,
    branch: &str,
    base: &str,
    same_repo: bool,
    occurred_at: Option<i64>,
    observed_at: i64,
) {
    let merge = NewCatchupMerge {
        branch: branch.into(),
        base: base.into(),
        same_repository_head: same_repo,
        occurred_at,
        ..merge(number, observed_at).0
    };
    fixture
        .store
        .record_catchup_merge(&scope(SETTLED_RID), &merge, observed_at)
        .unwrap();
}

fn listed(fixture: &Fixture, token: &str, limit: usize, now: i64) -> (Vec<String>, u64, bool) {
    let overview = fixture
        .store
        .work_overview(
            token,
            &WorkOverviewQuery {
                recent_since: None,
                limit,
            },
            now,
        )
        .unwrap();
    (
        overview.current.into_iter().map(|item| item.id).collect(),
        overview.settled_count,
        overview.current_complete,
    )
}

fn briefing_ids(fixture: &Fixture, token: &str, now: i64) -> Vec<String> {
    let mut query = BriefingQuery {
        limit: 1,
        ..BriefingQuery::default()
    };
    let mut ids = Vec::new();
    loop {
        let page = fixture.store.work_briefing(token, &query, now).unwrap();
        ids.extend(page.items.iter().map(|item| item.id.clone()));
        if page.complete {
            return ids;
        }
        query = BriefingQuery {
            after: Some(page.next_after),
            work_after: page.next_work_after,
            work_done: page.next_work_done,
            through: Some(page.through),
            limit: 1,
            ..BriefingQuery::default()
        };
    }
}

fn settled_work(fixture: &Fixture) -> (String, String, i64) {
    let owner = fixture.owner(SETTLED_RID);
    let token = fixture.token(&owner, "settled-lead", SETTLED_RID, false);
    let reader = fixture.token(&owner, "settled-reader", SETTLED_RID, true);
    (token, reader, unix_now() - 10_000)
}

fn settles_and_returns_on_a_later_check_in(fixture: &Fixture) {
    let (token, reader, t0) = settled_work(fixture);
    let now = t0 + 5_000;
    let item = begin(fixture, &token, "a", Some("feat/a"), "src/a.rs", t0);
    assert_eq!(
        listed(fixture, &reader, 100, now),
        (vec![item.id.clone()], 0, true)
    );

    merged(fixture, 1, "feat/a", "main", true, t0 + 100);
    assert_eq!(
        listed(fixture, &reader, 100, now),
        (vec![item.id.clone()], 0, true)
    );
    assert_eq!(briefing_ids(fixture, &reader, now), vec![item.id.clone()]);
    // The record is untouched and still readable.
    let kept = fixture
        .store
        .work_get(&reader, &item.id, now)
        .unwrap()
        .unwrap();
    assert_ne!(kept.state, WorkState::Finished);

    let again = checkin(
        fixture,
        &token,
        &item,
        CheckpointKind::Progress,
        "more work",
        t0 + 200,
    );
    assert_eq!(
        listed(fixture, &reader, 100, now),
        (vec![item.id.clone()], 0, true)
    );
    assert_eq!(briefing_ids(fixture, &reader, now), vec![item.id.clone()]);
    assert_eq!(again.state, WorkState::Active);
}

fn nothing_settles_without_merge_data_or_a_branch(fixture: &Fixture) {
    let (token, reader, t0) = settled_work(fixture);
    let branchless = begin(fixture, &token, "nb", None, "src/nb.rs", t0);
    let unmerged = begin(
        fixture,
        &token,
        "um",
        Some("feat/unmerged"),
        "src/um.rs",
        t0,
    );
    let mut expected = vec![branchless.id, unmerged.id];
    expected.sort();
    assert_eq!(listed(fixture, &reader, 100, t0 + 500), (expected, 0, true));
}

fn blocked_work_never_settles(fixture: &Fixture) {
    let (token, reader, t0) = settled_work(fixture);
    let item = begin(fixture, &token, "blk", Some("feat/blk"), "src/blk.rs", t0);
    let blocked = checkin(
        fixture,
        &token,
        &item,
        CheckpointKind::Blocker,
        "waiting",
        t0 + 10,
    );
    assert!(blocked.blocker.is_some());
    merged(fixture, 2, "feat/blk", "main", true, t0 + 100);
    assert_eq!(
        listed(fixture, &reader, 100, t0 + 500),
        (vec![item.id.clone()], 0, true)
    );
    assert_eq!(briefing_ids(fixture, &reader, t0 + 500), vec![item.id]);
}

fn forks_and_bases_never_settle(fixture: &Fixture) {
    let (token, reader, t0) = settled_work(fixture);
    let forked = begin(
        fixture,
        &token,
        "fork",
        Some("feat/fork"),
        "src/fork.rs",
        t0,
    );
    let default = begin(fixture, &token, "main", Some("main"), "src/main.rs", t0);
    let release = begin(fixture, &token, "rel", Some("release"), "src/rel.rs", t0);
    // A fork's branch name says nothing about same-named Work here.
    merged(fixture, 3, "feat/fork", "main", false, t0 + 100);
    // A branch that is a merge base never settles, whichever order the merges arrive in.
    merged(fixture, 4, "main", "develop", true, t0 + 100);
    merged(fixture, 5, "release", "main", true, t0 + 100);
    merged(fixture, 6, "feat/into-release", "release", true, t0 + 101);
    let mut expected = vec![forked.id, default.id, release.id];
    expected.sort();
    assert_eq!(listed(fixture, &reader, 100, t0 + 500), (expected, 0, true));
}

fn a_merge_at_the_check_in_instant_settles(fixture: &Fixture) {
    let (token, reader, t0) = settled_work(fixture);
    let item = begin(fixture, &token, "tie", Some("feat/tie"), "src/tie.rs", t0);
    let item = checkin(
        fixture,
        &token,
        &item,
        CheckpointKind::Progress,
        "p",
        t0 + 50,
    );
    assert_eq!(item.last_checkin_at, t0 + 50);
    merged(fixture, 7, "feat/tie", "main", true, t0 + 49);
    assert_eq!(
        listed(fixture, &reader, 100, t0 + 500).0,
        vec![item.id.clone()]
    );
    merged(fixture, 8, "feat/tie", "main", true, t0 + 50);
    assert_eq!(
        listed(fixture, &reader, 100, t0 + 500),
        (vec![item.id.clone()], 0, true)
    );
}

fn an_automatic_pause_after_the_merge_settles_until_resumed(fixture: &Fixture) {
    let (token, reader, t0) = settled_work(fixture);
    let auto = begin(
        fixture,
        &token,
        "auto",
        Some("feat/auto"),
        "src/auto.rs",
        t0,
    );
    let manual = begin(fixture, &token, "man", Some("feat/man"), "src/man.rs", t0);
    merged(fixture, 9, "feat/auto", "main", true, t0 + 100);
    merged(fixture, 10, "feat/man", "main", true, t0 + 100);
    // The session outlived the merge, so ending it writes a check-in after the merge.
    let auto = checkin(
        fixture,
        &token,
        &auto,
        CheckpointKind::Pause,
        AUTO_PAUSE,
        t0 + 200,
    );
    let manual = checkin(
        fixture,
        &token,
        &manual,
        CheckpointKind::Pause,
        "stepping away",
        t0 + 200,
    );
    assert_eq!(auto.state, WorkState::Paused);
    assert_eq!(
        listed(fixture, &reader, 100, t0 + 500),
        (
            {
                let mut ids = vec![manual.id.clone(), auto.id.clone()];
                ids.sort();
                ids
            },
            0,
            true
        )
    );

    // A resume through takeover keeps the pause summary but changes the state.
    let resumed = fixture
        .store
        .work_start(
            &token,
            &WorkStart {
                issue_choice: None,
                work_id: Some(auto.id.clone()),
                source: None,
                request_key: None,
                title: auto.title.clone(),
                session: "resumed-session".into(),
                role: WorkRole::Lead,
                scope: auto.scope.clone(),
                branch: auto.branch.clone(),
                worktree: None,
                next_action: None,
                depends_on: Vec::new(),
                related_work: Vec::new(),
                takeover_generation: Some(auto.generation),
                preflight: None,
            },
            t0 + 300,
        )
        .unwrap();
    assert!(resumed.took_over);
    assert_ne!(resumed.item.state, WorkState::Paused);
    assert!(listed(fixture, &reader, 100, t0 + 500).0.contains(&auto.id));
}

fn the_limit_counts_only_visible_items_and_paging_skips_hidden_ones(fixture: &Fixture) {
    let (token, reader, t0) = settled_work(fixture);
    let mut visible = Vec::new();
    for n in 0..5 {
        let branch = format!("feat/page-{n}");
        let item = begin(
            fixture,
            &token,
            &format!("pg{n}"),
            Some(&branch),
            &format!("src/pg{n}.rs"),
            t0,
        );
        if n < 3 {
            merged(fixture, 20 + n, &branch, "main", true, t0 + 100);
        }
        visible.push(item.id);
    }
    visible.sort();
    assert_eq!(
        listed(fixture, &reader, 2, t0 + 500),
        (visible[..2].to_vec(), 0, false)
    );
    let (first, settled, complete) = listed(fixture, &reader, 1, t0 + 500);
    assert_eq!((first.len(), settled, complete), (1, 0, false));
    assert_eq!(briefing_ids(fixture, &reader, t0 + 500), visible);
}

fn a_hidden_item_still_conflicts_with_a_new_start(fixture: &Fixture) {
    let (token, reader, t0) = settled_work(fixture);
    let item = begin(fixture, &token, "adm", Some("feat/adm"), "src/adm.rs", t0);
    merged(fixture, 30, "feat/adm", "main", true, t0 + 100);
    assert_eq!(listed(fixture, &reader, 100, t0 + 500).1, 0);
    let other_owner = fixture.owner(SETTLED_RID + 1);
    let other = fixture.token(&other_owner, "settled-other", SETTLED_RID, false);
    let result = fixture
        .store
        .work_start(
            &other,
            &WorkStart {
                issue_choice: None,
                work_id: None,
                source: None,
                request_key: Some("adm-other".into()),
                title: "Overlapping start".into(),
                session: "other-session".into(),
                role: WorkRole::Lead,
                scope: vec!["src/adm.rs".into()],
                branch: None,
                worktree: None,
                next_action: None,
                depends_on: Vec::new(),
                related_work: Vec::new(),
                takeover_generation: None,
                preflight: None,
            },
            t0 + 500,
        )
        .unwrap();
    let conflict = result
        .conflict
        .expect("admission still sees the hidden item");
    assert!(
        conflict
            .overlaps
            .iter()
            .any(|overlap| overlap.item.id == item.id)
    );
}

fn settled_state_survives_compaction_of_merge_observations(fixture: &Fixture) {
    let owner = fixture.owner(SETTLED_RID);
    let token = fixture.token(&owner, "settled-lead", SETTLED_RID, false);
    let reader = fixture.token(&owner, "settled-reader", SETTLED_RID, true);
    let now = unix_now();
    let t0 = now - 3 * 86_400;
    let item = begin(fixture, &token, "cmp", Some("feat/cmp"), "src/cmp.rs", t0);
    merged(fixture, 40, "feat/cmp", "main", true, t0 + 100);
    assert_eq!(
        listed(fixture, &reader, 100, now),
        (vec![item.id.clone()], 0, true)
    );

    let work_scope = WorkRepositoryScope {
        provider: "github".into(),
        host: "github.com".into(),
        repository: REPOSITORY.into(),
        repository_id: Some(SETTLED_RID),
    };
    let compacted = fixture
        .store
        .work_compact(&work_scope, now - 86_400, true, now)
        .unwrap();
    assert_eq!(compacted.deleted_merge_observations, 1);
    assert!(fixture.merge_ids(SETTLED_RID).is_empty());

    // The observation is gone, the derived branch record is not.
    assert_eq!(
        listed(fixture, &reader, 100, now),
        (vec![item.id.clone()], 0, true)
    );
    assert!(
        fixture
            .store
            .work_get(&reader, &item.id, now)
            .unwrap()
            .is_some()
    );
}

/// Rows of the derived table as (branch, is_base, merged_at), after dropping it and its
/// migration record and reopening the database, which replays the migration over the
/// merge observations that already exist.
fn replayed_branch_merges(fixture: &Fixture) -> Vec<(String, i64, i64)> {
    const READ: &str = "SELECT branch, CAST(is_base AS BIGINT), merged_at
        FROM hub_catchup_branch_merges ORDER BY branch";
    const DROP: [&str; 2] = [
        "DROP TABLE hub_catchup_branch_merges",
        "DELETE FROM hub_named_migrations WHERE name='work_branch_merges'",
    ];
    if let Some(database) = &fixture.database {
        let connection = rusqlite::Connection::open(database).unwrap();
        DROP.iter().for_each(|sql| {
            connection.execute(sql, []).unwrap();
        });
        drop(connection);
        store::open(&format!("sqlite://{}", database.display())).unwrap();
        let connection = rusqlite::Connection::open(database).unwrap();
        let mut statement = connection.prepare(READ).unwrap();
        return statement
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
    }
    let url = &fixture._pg.as_ref().unwrap().url;
    let mut client = Client::connect(url, NoTls).unwrap();
    DROP.iter().for_each(|sql| {
        client.execute(*sql, &[]).unwrap();
    });
    store::open(url).unwrap();
    client
        .query(READ, &[])
        .unwrap()
        .iter()
        .map(|row| (row.get(0), row.get(1), row.get(2)))
        .collect()
}

fn the_migration_backfills_branch_merges_from_existing_observations(fixture: &Fixture) {
    let t0 = unix_now() - 10_000;
    merged(fixture, 50, "feat/x", "main", true, t0 + 100);
    merged(fixture, 51, "feat/y", "feat/x", true, t0 + 200);
    merged(fixture, 52, "feat/x", "main", true, t0 + 300);
    merged(fixture, 53, "feat/z", "main", false, t0 + 150);
    // A tracked publication has no merge time: its base is recorded, its branch is not.
    merged_observed(fixture, 54, "feat/n", "release", true, None, t0 + 400);
    assert_eq!(
        replayed_branch_merges(fixture),
        vec![
            // A base of any merge is marked, with the latest merge that named it.
            ("feat/x".to_owned(), 1, t0 + 300),
            ("feat/y".to_owned(), 0, t0 + 200),
            // Legacy rows cannot tell a fork, so the head branch settles.
            ("feat/z".to_owned(), 0, t0 + 150),
            ("main".to_owned(), 1, t0 + 300),
            ("release".to_owned(), 1, t0 + 400),
        ]
    );
}

fn a_merge_without_a_provider_time_does_not_settle(fixture: &Fixture) {
    let (token, reader, t0) = settled_work(fixture);
    let item = begin(fixture, &token, "nt", Some("feat/nt"), "src/nt.rs", t0);
    // Observed long after the check-in, but with no merge time it cannot say the Work ended.
    merged_observed(fixture, 60, "feat/nt", "main", true, None, t0 + 400);
    assert_eq!(
        listed(fixture, &reader, 100, t0 + 500),
        (vec![item.id.clone()], 0, true)
    );
    assert_eq!(
        briefing_ids(fixture, &reader, t0 + 500),
        vec![item.id.clone()]
    );
    // A later observation that carries the time settles it.
    merged(fixture, 61, "feat/nt", "main", true, t0 + 100);
    assert_eq!(
        listed(fixture, &reader, 100, t0 + 500),
        (vec![item.id.clone()], 0, true)
    );
}

fn a_branch_that_is_also_a_base_stays_listed_in_either_order(fixture: &Fixture) {
    let (token, reader, t0) = settled_work(fixture);
    let merged_first = begin(fixture, &token, "mf", Some("feat/mf"), "src/mf.rs", t0);
    let base_first = begin(fixture, &token, "bf", Some("feat/bf"), "src/bf.rs", t0);
    merged(fixture, 62, "feat/mf", "main", true, t0 + 100);
    merged(fixture, 63, "feat/mf-child", "feat/mf", true, t0 + 110);
    merged(fixture, 64, "feat/bf-child", "feat/bf", true, t0 + 100);
    merged(fixture, 65, "feat/bf", "main", true, t0 + 110);
    let mut expected = vec![merged_first.id, base_first.id];
    expected.sort();
    assert_eq!(
        listed(fixture, &reader, 100, t0 + 500),
        (expected.clone(), 0, true)
    );
    assert_eq!(briefing_ids(fixture, &reader, t0 + 500), expected);
}

fn settling_works_in_an_unbound_repository(fixture: &Fixture) {
    let owner = fixture.owner(SETTLED_RID);
    let token = mint_agent_token(&*fixture.store, &owner, "unbound", REPOSITORY, 3600)
        .unwrap()
        .0;
    let t0 = unix_now() - 10_000;
    let item = begin(fixture, &token, "ub", Some("feat/ub"), "src/ub.rs", t0);
    let unbound = CatchupRepositoryScope {
        repository_id: None,
        ..scope(SETTLED_RID)
    };
    let record = NewCatchupMerge {
        branch: "feat/ub".into(),
        base: "main".into(),
        occurred_at: Some(t0 + 100),
        ..merge(66, t0 + 100).0
    };
    assert_eq!(
        listed(fixture, &token, 100, t0 + 500),
        (vec![item.id.clone()], 0, true)
    );
    fixture
        .store
        .record_catchup_merge(&unbound, &record, t0 + 100)
        .unwrap();
    assert_eq!(
        listed(fixture, &token, 100, t0 + 500),
        (vec![item.id.clone()], 0, true)
    );
}

fn a_check_in_after_the_merge_survives_the_automatic_pause(fixture: &Fixture) {
    let (token, reader, t0) = settled_work(fixture);
    let late = begin(
        fixture,
        &token,
        "late",
        Some("feat/late"),
        "src/late.rs",
        t0,
    );
    let early = begin(
        fixture,
        &token,
        "early",
        Some("feat/early"),
        "src/early.rs",
        t0,
    );
    merged(fixture, 67, "feat/late", "main", true, t0 + 100);
    merged(fixture, 68, "feat/early", "main", true, t0 + 100);
    // late: real check-in after the merge, then the session ends.
    let late = checkin(
        fixture,
        &token,
        &late,
        CheckpointKind::Progress,
        "more",
        t0 + 150,
    );
    let late = checkin(
        fixture,
        &token,
        &late,
        CheckpointKind::Pause,
        AUTO_PAUSE,
        t0 + 300,
    );
    assert_eq!(late.reported_at, Some(t0 + 150));
    // early: the last real check-in preceded the merge.
    let early = checkin(
        fixture,
        &token,
        &early,
        CheckpointKind::Progress,
        "p",
        t0 + 50,
    );
    let early = checkin(
        fixture,
        &token,
        &early,
        CheckpointKind::Pause,
        AUTO_PAUSE,
        t0 + 300,
    );
    assert_eq!(early.reported_at, Some(t0 + 50));
    assert_eq!(
        listed(fixture, &reader, 100, t0 + 500),
        (
            {
                let mut ids = vec![late.id.clone(), early.id.clone()];
                ids.sort();
                ids
            },
            0,
            true
        )
    );
    assert_eq!(briefing_ids(fixture, &reader, t0 + 500), {
        let mut ids = vec![late.id.clone(), early.id.clone()];
        ids.sort();
        ids
    });

    // A repeated automatic pause keeps the value; a real checkpoint clears it.
    let again = checkin(
        fixture,
        &token,
        &late,
        CheckpointKind::Pause,
        AUTO_PAUSE,
        t0 + 400,
    );
    assert_eq!(again.reported_at, Some(t0 + 150));
    let real = checkin(
        fixture,
        &token,
        &again,
        CheckpointKind::Progress,
        "back",
        t0 + 450,
    );
    assert_eq!(real.reported_at, None);
}

fn operational_read(
    fixture: &Fixture,
    token: &str,
    select: &OperationalSelect<'_>,
    now: i64,
) -> OperationalOverviewV1 {
    fixture
        .store
        .work_operational(&WorkLifecycleReader::Token(token), select, now)
        .unwrap()
}

fn operational_covers_reserving_work_hidden_by_a_merged_pr(fixture: &Fixture) {
    let (token, reader, t0) = settled_work(fixture);
    let now = t0 + 5_000;
    let hidden = begin(fixture, &token, "oph", Some("feat/oph"), "src/oph.rs", t0);
    let plain = begin(fixture, &token, "opp", Some("feat/opp"), "src/opp.rs", t0);
    let freed = begin(fixture, &token, "opf", None, "src/opf.rs", t0);
    fixture
        .store
        .work_transition(
            &WorkTransitionActor::Agent { token: &token },
            &WorkTransitionRequest {
                work_id: freed.id.clone(),
                operation_id: "release-opf".into(),
                generation: freed.generation,
                expected_updated_seq: freed.updated_seq,
                session: Some("session-opf".into()),
                transition: WorkTransitionKind::Release,
                reason: "synthetic release".into(),
                superseded_by: None,
            },
            t0 + 10,
        )
        .unwrap();
    merged(fixture, 60, "feat/oph", "main", true, t0 + 100);

    // Held scope remains visible, while the legacy merge observation stays inspectable.
    let (ids, settled, _) = listed(fixture, &reader, 100, now);
    assert!(ids.contains(&hidden.id), "{ids:?}");
    assert_eq!(settled, 0);

    let holders = operational_read(fixture, &reader, &OperationalSelect::Holders, t0 + 120);
    assert_eq!((holders.schema_version, holders.complete), (2, true));
    let mut expected = vec![hidden.id.clone(), plain.id.clone()];
    expected.sort();
    let listed_ids: Vec<_> = holders.work.iter().map(|w| w.work_id.clone()).collect();
    assert_eq!(listed_ids, expected, "ordered by id, freed work absent");
    let entry = holders
        .work
        .iter()
        .find(|w| w.work_id == hidden.id)
        .unwrap();
    assert!(entry.merge.hidden_by_merged_pr);
    assert_eq!(entry.merge.status, MergeStatus::Observed);
    // A merge changes neither the state, the outcome nor the reservation.
    assert_eq!(entry.state, WorkState::Active);
    assert_eq!(entry.outcome.value, OutcomeValue::None);
    assert_eq!(entry.reservation.status, ReservationStatus::Held);
    assert_eq!(entry.reservation.scope, vec!["src/oph.rs".to_owned()]);
    assert_eq!(entry.generation, hidden.generation);
    let other = holders.work.iter().find(|w| w.work_id == plain.id).unwrap();
    assert!(!other.merge.hidden_by_merged_pr);
    assert_eq!(
        other.merge.status,
        MergeStatus::Unknown,
        "no coverage known"
    );

    // Unfinished and released: readable by id with no reservation, never a holder.
    let one = operational_read(fixture, &reader, &OperationalSelect::One(&freed.id), now);
    let one = &one.work[0];
    assert_eq!(one.reservation.status, ReservationStatus::None);
    assert!(one.reservation.scope.is_empty() && one.reservation.basis.is_none());
    assert_eq!(one.state, WorkState::Paused);
    // Unknown and foreign ids read as empty.
    let missing = format!("wrk_{}", "0".repeat(32));
    assert!(
        operational_read(fixture, &reader, &OperationalSelect::One(&missing), now)
            .work
            .is_empty()
    );
    assert!(matches!(
        fixture.store.work_operational(
            &WorkLifecycleReader::Token(&reader),
            &OperationalSelect::One("not-an-id"),
            now
        ),
        Err(HubError::Invalid(_))
    ));

    // No token identifier appears, and only `freshness` and the lease `activity` depend on the
    // request time.
    let early = serde_json::to_value(&holders).unwrap();
    let text = early.to_string();
    assert!(!text.contains(&token) && !text.contains(&reader), "{text}");
    fixture.store.revoke_agent_token(&token).unwrap();
    let later = operational_read(fixture, &reader, &OperationalSelect::Holders, now + 600);
    let mut later = serde_json::to_value(&later).unwrap();
    let mut early = early;
    assert_ne!(early, later, "a revoked lead changes freshness");
    for value in [&mut early, &mut later] {
        for work in value["work"].as_array_mut().unwrap() {
            let work = work.as_object_mut().unwrap();
            work.remove("freshness");
            work.remove("activity");
        }
    }
    assert_eq!(early, later);
}

fn merge_status_is_none_only_when_forge_coverage_is_known(fixture: &Fixture) {
    let (token, reader, t0) = settled_work(fixture);
    let now = t0 + 5_000;
    let branch = begin(fixture, &token, "mnb", Some("feat/mnb"), "src/mnb.rs", t0);
    let loose = begin(fixture, &token, "mnl", None, "src/mnl.rs", t0);
    let status_of = |id: &str| {
        operational_read(fixture, &reader, &OperationalSelect::One(id), now).work[0]
            .merge
            .status
    };
    assert_eq!(status_of(&branch.id), MergeStatus::Unknown);
    let record = |state, window_from| {
        fixture
            .store
            .record_catchup_source_status(
                &scope(SETTLED_RID),
                &CatchupSourceStatus {
                    source: CatchupMergeSource::ForgePull,
                    state,
                    checked_at: t0 + 1_000,
                    unread_pages: 0,
                    deferred: 0,
                    window_from,
                },
            )
            .unwrap();
    };
    // A read that does not reach the item's creation, or an incomplete one, proves nothing.
    record(CatchupCoverageState::ObservedOnly, Some(t0 + 500));
    assert_eq!(status_of(&branch.id), MergeStatus::Unknown);
    record(CatchupCoverageState::Incomplete, Some(t0 - 100));
    assert_eq!(status_of(&branch.id), MergeStatus::Unknown);
    record(CatchupCoverageState::ObservedOnly, Some(t0 - 100));
    assert_eq!(status_of(&branch.id), MergeStatus::None);
    // Work with no branch is never judged by the merge rule.
    assert_eq!(status_of(&loose.id), MergeStatus::Unknown);
    merged(fixture, 61, "feat/mnb", "main", true, t0 + 100);
    let after = operational_read(fixture, &reader, &OperationalSelect::One(&branch.id), now);
    assert_eq!(after.work[0].merge.status, MergeStatus::Observed);
    assert!(after.work[0].merge.hidden_by_merged_pr);
}

fn more_than_a_hundred_holders_are_bounded_and_flagged(fixture: &Fixture) {
    let (token, reader, t0) = settled_work(fixture);
    for index in 0..101 {
        begin(
            fixture,
            &token,
            &format!("many{index}"),
            None,
            &format!("src/many{index}.rs"),
            t0,
        );
    }
    let page = operational_read(fixture, &reader, &OperationalSelect::Holders, t0 + 10);
    assert_eq!((page.work.len(), page.complete), (100, false));
    let ids: Vec<_> = page.work.iter().map(|w| w.work_id.clone()).collect();
    let mut sorted = ids.clone();
    sorted.sort();
    assert_eq!(ids, sorted);
}

macro_rules! settled_cases {
    ($($case:ident),* $(,)?) => {
        mod settled {
            use super::*;
            $(
                #[test]
                fn $case() {
                    super::$case(&Fixture::sqlite());
                }
            )*
            mod postgres {
                use super::*;
                $(
                    #[test]
                    fn $case() {
                        let Some(fixture) = Fixture::postgres() else {
                            return;
                        };
                        super::super::$case(&fixture);
                    }
                )*
            }
        }
    };
}

settled_cases!(
    settles_and_returns_on_a_later_check_in,
    nothing_settles_without_merge_data_or_a_branch,
    blocked_work_never_settles,
    forks_and_bases_never_settle,
    a_merge_at_the_check_in_instant_settles,
    an_automatic_pause_after_the_merge_settles_until_resumed,
    the_limit_counts_only_visible_items_and_paging_skips_hidden_ones,
    a_hidden_item_still_conflicts_with_a_new_start,
    settled_state_survives_compaction_of_merge_observations,
    the_migration_backfills_branch_merges_from_existing_observations,
    a_merge_without_a_provider_time_does_not_settle,
    a_branch_that_is_also_a_base_stays_listed_in_either_order,
    settling_works_in_an_unbound_repository,
    a_check_in_after_the_merge_survives_the_automatic_pause,
    operational_covers_reserving_work_hidden_by_a_merged_pr,
    merge_status_is_none_only_when_forge_coverage_is_known,
    more_than_a_hundred_holders_are_bounded_and_flagged,
);

fn released_work_can_still_use_legacy_branch_settling(fixture: &Fixture) {
    let (token, reader, t0) = settled_work(fixture);
    let item = begin(
        fixture,
        &token,
        "released-merge",
        Some("feat/released"),
        "src/released.rs",
        t0,
    );
    fixture
        .store
        .work_transition(
            &WorkTransitionActor::Agent { token: &token },
            &WorkTransitionRequest {
                work_id: item.id.clone(),
                operation_id: "release-before-merge".into(),
                generation: item.generation,
                expected_updated_seq: item.updated_seq,
                session: Some("session-released-merge".into()),
                transition: WorkTransitionKind::Release,
                reason: "No more scope is reserved".into(),
                superseded_by: None,
            },
            t0 + 10,
        )
        .unwrap();
    merged(fixture, 700, "feat/released", "main", true, t0 + 100);
    assert_eq!(listed(fixture, &reader, 100, t0 + 500), (vec![], 1, true));
    assert!(briefing_ids(fixture, &reader, t0 + 500).is_empty());
}
#[test]
fn sqlite_released_work_can_still_use_legacy_branch_settling() {
    released_work_can_still_use_legacy_branch_settling(&Fixture::sqlite());
}
#[test]
fn postgres_released_work_can_still_use_legacy_branch_settling() {
    if let Some(f) = Fixture::postgres() {
        released_work_can_still_use_legacy_branch_settling(&f);
    }
}
