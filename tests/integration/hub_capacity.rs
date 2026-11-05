//! Capacity refusals recorded for the operator (#293), on both stores.
use agentscm::hub::capacity::{
    CapacityLimit, CapacityRefusal, CapacityRepository, MAX_REFUSALS, RETAIN_SECONDS,
};
use agentscm::hub::store::{self, Store, unix_now};

use crate::hub_pg::PgSchema;

fn refusal(repository: &str, id: Option<i64>, limit: CapacityLimit, at: i64) -> CapacityRefusal {
    CapacityRefusal {
        limit,
        repository: CapacityRepository {
            provider: "github".into(),
            host: "github.com".into(),
            repository: repository.into(),
            repository_id: id,
        },
        at,
        count: 1,
    }
}

fn records_one_row_per_repository_and_limit(store: &dyn Store) {
    let now = unix_now();
    let work = refusal(
        "acme/widgets",
        Some(101),
        CapacityLimit::WorkStorage,
        now - 10,
    );
    store.record_capacity_refusal(&work).unwrap();
    // A repeat counts; an out-of-order older report never moves `at` back.
    store
        .record_capacity_refusal(&CapacityRefusal {
            at: now,
            ..work.clone()
        })
        .unwrap();
    store
        .record_capacity_refusal(&CapacityRefusal {
            at: now - 20,
            ..work.clone()
        })
        .unwrap();
    store
        .record_capacity_refusal(&refusal(
            "acme/widgets",
            Some(101),
            CapacityLimit::CoordinationRequests,
            now - 5,
        ))
        .unwrap();
    store
        .record_capacity_refusal(&refusal(
            "acme/legacy",
            None,
            CapacityLimit::ChangeRecords,
            now - 1,
        ))
        .unwrap();
    let rows = store.capacity_refusals(now).unwrap();
    assert_eq!(
        rows,
        [
            CapacityRefusal {
                count: 3,
                at: now,
                ..work.clone()
            },
            refusal("acme/legacy", None, CapacityLimit::ChangeRecords, now - 1),
            refusal(
                "acme/widgets",
                Some(101),
                CapacityLimit::CoordinationRequests,
                now - 5
            ),
        ]
    );
    // A row older than the window is never read.
    store
        .record_capacity_refusal(&refusal(
            "acme/old",
            Some(7),
            CapacityLimit::MergeObservations,
            now - RETAIN_SECONDS - 1,
        ))
        .unwrap();
    assert_eq!(store.capacity_refusals(now).unwrap().len(), 3);
    // At most the newest MAX_REFUSALS rows are kept.
    for n in 0..MAX_REFUSALS {
        store
            .record_capacity_refusal(&refusal(
                &format!("acme/many-{n}"),
                Some(1000 + n),
                CapacityLimit::CatchupDeliveries,
                now + 1 + n,
            ))
            .unwrap();
    }
    let rows = store.capacity_refusals(now + MAX_REFUSALS).unwrap();
    assert_eq!(rows.len() as i64, MAX_REFUSALS);
    assert!(
        rows.iter()
            .all(|r| r.repository.repository.starts_with("acme/many-"))
    );
}

#[test]
fn capacity_refusals_are_kept_per_repository_and_limit_on_sqlite() {
    let dir = tempfile::tempdir().unwrap();
    let store = store::open(&format!(
        "sqlite://{}",
        dir.path().join("hub.sqlite").display()
    ))
    .unwrap();
    records_one_row_per_repository_and_limit(&*store);
}

#[test]
fn capacity_refusals_are_kept_per_repository_and_limit_on_postgres() {
    let Some(pg) = PgSchema::create() else {
        return;
    };
    records_one_row_per_repository_and_limit(&*store::open(&pg.url).unwrap());
}
