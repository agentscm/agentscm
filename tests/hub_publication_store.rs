//! Publication persistence, run against SQLite always and PostgreSQL when
//! AGENTSCM_TEST_DATABASE_URL is set.
#[path = "common/hub_pg.rs"]
mod hub_pg;

use std::sync::Arc;

use agentscm::hub::HubError;
use agentscm::hub::publish::{Create, MAX_UNFINISHED, Manifest, NewPublication, Update, derive_id};
use agentscm::hub::store::{self, ForgeIdentity, Store, mint_agent_token, unix_now};
use hub_pg::PgSchema;
use serde_json::{Value, json};
use tempfile::TempDir;

const REPO: &str = "acme/widgets";
fn manifest(title: &str, branch: &str) -> Manifest {
    serde_json::from_value(json!({
        "revision": "a".repeat(64), "event_seq": 3, "commit": "b".repeat(40),
        "tree": "c".repeat(40), "parent": "d".repeat(40), "branch": branch, "base": "main",
        "title": title, "body": "", "message": "msg\n",
        "author": {"name": "A", "email": "a@example.invalid", "date": "2026-05-28T20:26:40Z"},
        "committer": {"name": "A", "email": "a@example.invalid", "date": "2026-05-28T20:26:40Z"},
        "changes": [{"path": "a.txt", "mode": "100644", "blob": "e".repeat(40),
            "sha256": "f".repeat(64), "size": 5}]
    }))
    .unwrap()
}

fn new_on(title: &str, branch: &str) -> NewPublication {
    NewPublication {
        id: derive_id(REPO, 3, &"b".repeat(40), branch),
        manifest: manifest(title, branch),
    }
}

fn new(title: &str) -> NewPublication {
    new_on(title, "feat/x")
}

fn done(created: Create) -> agentscm::hub::publish::Publication {
    match created {
        Create::Done(publication) => publication,
        other => panic!("{other:?}"),
    }
}

fn events(store: &dyn Store) -> Vec<(String, Value)> {
    store
        .events_after(0, 1000)
        .unwrap()
        .into_iter()
        .filter(|e| e.kind.starts_with("publication_"))
        .map(|e| (e.kind, serde_json::from_str(&e.data).unwrap()))
        .collect()
}

fn suite(store: Arc<dyn Store>) {
    let identity = ForgeIdentity {
        provider: "github".into(),
        host: "github.com".into(),
        user_id: 7,
    };
    let owner = store.upsert_user(&identity, "alice").unwrap();
    let (token, _) = mint_agent_token(&*store, &owner.id, "builder", REPO, 3600).unwrap();
    let (other_token, _) =
        mint_agent_token(&*store, &owner.id, "other", "acme/other", 3600).unwrap();
    let now = unix_now();

    // Creating is idempotent by id and manifest.
    let id = new("t").id;
    let id = id.as_str();
    let first = done(store.create_publication(&token, &new("t"), now).unwrap());
    assert_eq!(
        (first.repository.as_str(), first.agent.as_str()),
        (REPO, "builder")
    );
    assert_eq!(
        (first.owner_login.as_str(), first.branch_state.as_str()),
        ("alice", "pending")
    );
    assert_eq!(first.owner, owner.id);
    assert_eq!(
        store
            .create_publication(&token, &new("t"), now + 1)
            .unwrap(),
        Create::Done(first.clone())
    );
    assert_eq!(
        store
            .create_publication(&token, &new("other title"), now)
            .unwrap(),
        Create::Conflict
    );
    // An id is derived from the repository, so it cannot be used for another one.
    assert!(matches!(
        store.create_publication(&other_token, &new("t"), now),
        Err(HubError::Invalid(_))
    ));
    assert_eq!(events(&*store).len(), 1);
    assert_eq!(events(&*store)[0].0, "publication_requested");
    assert!(store.publication("acme/other", id).unwrap().is_none());
    assert_eq!(store.publication(REPO, id).unwrap(), Some(first));
    let mut bad = new("t");
    bad.manifest.changes[0].size = 9 * 1024 * 1024;
    assert!(matches!(
        store.create_publication(&token, &bad, now),
        Err(HubError::Invalid(_))
    ));

    // A pull request needs a created branch; a create attempt is state without an event.
    let attempt = store
        .update_publication(id, &Update::BranchAttempt, now)
        .unwrap();
    assert_eq!(attempt.branch_state, "attempting");
    store
        .update_publication(id, &Update::BranchAttempt, now)
        .unwrap();
    assert_eq!(events(&*store).len(), 1);
    let pull = || Update::PullRequest {
        number: 12,
        node_id: "PR_12".into(),
        html_url: "https://github.example/acme/widgets/pull/12".into(),
    };
    assert!(matches!(
        store.update_publication(id, &pull(), now),
        Err(HubError::Invalid(_))
    ));
    let rejected = Update::BranchRejected {
        status: 422,
        message: "Reference already exists".into(),
    };
    let after = store.update_publication(id, &rejected, now).unwrap();
    assert_eq!(after.branch_state, "rejected");
    store.update_publication(id, &rejected, now).unwrap();
    // Errors are state without an event.
    let errored = store
        .update_publication(id, &Update::Error(Some("502".into())), now)
        .unwrap();
    assert_eq!(errored.error.as_deref(), Some("502"));
    store
        .update_publication(id, &Update::Error(None), now)
        .unwrap();
    assert_eq!(events(&*store).len(), 2);

    // A retry after a rejection can succeed; forge facts never go backwards.
    let created = store
        .update_publication(id, &Update::BranchCreated, now)
        .unwrap();
    assert_eq!(
        (created.branch_state.as_str(), created.branch_message),
        ("created", None)
    );
    store
        .update_publication(id, &Update::BranchCreated, now)
        .unwrap();
    let still = store.update_publication(id, &rejected, now).unwrap();
    assert_eq!(still.branch_state, "created");
    let opened = store.update_publication(id, &pull(), now).unwrap();
    assert_eq!(opened.pr_number, Some(12));
    store.update_publication(id, &pull(), now).unwrap();
    let merge = Update::Merged {
        merge_commit: "9".repeat(40),
    };
    let merged = store.update_publication(id, &merge, now).unwrap();
    assert_eq!(merged.merge_commit, Some("9".repeat(40)));
    store.update_publication(id, &merge, now).unwrap();
    assert!(matches!(
        store.update_publication(
            "pub_ffffffffffffffffffffffffffffffff",
            &Update::BranchCreated,
            now
        ),
        Err(HubError::Invalid(_))
    ));

    let log = events(&*store);
    let kinds: Vec<&str> = log.iter().map(|(k, _)| k.as_str()).collect();
    assert_eq!(
        kinds,
        [
            "publication_requested",
            "publication_branch_rejected",
            "publication_branch_created",
            "publication_pr_opened",
            "publication_merged"
        ]
    );
    assert_eq!(log[2].1["commit"], "b".repeat(40));
    assert_eq!(log[2].1["revision"], "a".repeat(64));
    assert_eq!(log[3].1["number"], 12);
    assert_eq!(log[3].1["node_id"], "PR_12");
    assert_eq!(log[4].1["merge_commit"], "9".repeat(40));
    assert!(
        store
            .events_after(0, 1000)
            .unwrap()
            .iter()
            .filter(|e| e.kind.starts_with("publication_"))
            .all(|e| e.subject == id)
    );

    // A token has at most MAX_UNFINISHED open publications; repeats and rejected ones do not count.
    let (capped, _) = mint_agent_token(&*store, &owner.id, "capped", REPO, 3600).unwrap();
    for n in 0..MAX_UNFINISHED {
        done(
            store
                .create_publication(&capped, &new_on("t", &format!("feat/n{n}")), now)
                .unwrap(),
        );
    }
    let over = new_on("t", "feat/over");
    assert_eq!(
        store.create_publication(&capped, &over, now).unwrap(),
        Create::TooMany
    );
    done(
        store
            .create_publication(&capped, &new_on("t", "feat/n0"), now)
            .unwrap(),
    );
    let rejected = Update::BranchRejected {
        status: 422,
        message: "no".into(),
    };
    store
        .update_publication(&new_on("t", "feat/n0").id, &rejected, now)
        .unwrap();
    done(store.create_publication(&capped, &over, now).unwrap());

    // A revoked token cannot create.
    assert!(store.revoke_agent_token(&token).unwrap());
    let mut again = new("t");
    again.id = "pub_00000000000000000000000000000002".into();
    assert!(matches!(
        store.create_publication(&token, &again, now),
        Err(HubError::Unauthorized)
    ));
}

#[test]
fn publications_on_sqlite() {
    let dir = TempDir::new().unwrap();
    let url = format!("sqlite://{}", dir.path().join("hub.sqlite").display());
    suite(store::open(&url).unwrap());
}

#[test]
fn publications_on_postgres() {
    let Some(pg) = PgSchema::create() else { return };
    suite(store::open(&pg.url).unwrap());
}
