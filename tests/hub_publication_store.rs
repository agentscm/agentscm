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

    // Listing is newest first by created time, then id, and pages strictly before a cursor.
    let all = store.publications(REPO, None, 100).unwrap();
    assert!(all.len() > 2);
    assert!(
        all.windows(2)
            .all(|w| (w[0].created_at, &w[0].id) > (w[1].created_at, &w[1].id))
    );
    assert_eq!(store.publications(REPO, None, 1).unwrap(), all[..1]);
    assert_eq!(
        store.publications(REPO, Some(&all[1].id), 100).unwrap(),
        all[2..]
    );
    assert!(
        store
            .publications(REPO, Some("pub_missing"), 100)
            .unwrap()
            .is_empty()
    );
    assert!(
        store
            .publications("acme/other", None, 100)
            .unwrap()
            .is_empty()
    );

    // Search: case-insensitive substring over branch, base, id, owner, agent and title, plus the
    // pull request number; LIKE wildcards are literal; paging keeps the filter.
    let titled = done(
        store
            .create_publication(&token, &new_on("Quarterly Rollup", "feat/title"), now)
            .unwrap(),
    );
    let found = |q: &str| -> Vec<String> {
        store
            .publication_search(REPO, None, q, 100)
            .unwrap()
            .rows
            .into_iter()
            .map(|p| p.id)
            .collect()
    };
    // Matching folds ASCII case on both backends; PostgreSQL's ILIKE may also fold non-ASCII by locale.
    let accented = done(
        store
            .create_publication(&token, &new_on("Café Ünï report", "feat/uni"), now)
            .unwrap(),
    );
    assert_eq!(found("Café Ünï"), std::slice::from_ref(&accented.id));
    if store.backend() == "sqlite" {
        assert_eq!(found("ünï").len(), 0);
    }
    assert_eq!(found("CAFé"), std::slice::from_ref(&accented.id));
    assert_eq!(found("QUARTERLY roll"), std::slice::from_ref(&titled.id));
    assert_eq!(found("FEAT/TITLE"), std::slice::from_ref(&titled.id));
    assert_eq!(found(&titled.id[4..12]), std::slice::from_ref(&titled.id));
    assert_eq!(found("#12"), std::slice::from_ref(&id.to_owned()));
    assert!(found("100%").is_empty() && found("p_b").is_empty() && found("\\").is_empty());
    assert_eq!(
        found("alice").len(),
        store.publications(REPO, None, 100).unwrap().len()
    );
    let page = store
        .publication_search(REPO, None, "feat/n", 2)
        .unwrap()
        .rows;
    let rest = store
        .publication_search(REPO, Some(&page[1].id), "feat/n", 100)
        .unwrap()
        .rows;
    assert_eq!(page.len() + rest.len(), found("feat/n").len());
    assert!(rest.iter().all(|p| p.branch.starts_with("feat/n")));
    assert!(
        store
            .publication_search("acme/other", None, "alice", 100)
            .unwrap()
            .rows
            .is_empty()
    );

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

fn candidate(sequence: u64, branch: &str, previous: Option<&str>) -> NewPublication {
    use agentscm::candidate::CandidateSubject;
    use agentscm::hub::publish::{Candidate, derive_candidate_id};
    let mut m = manifest("candidate", branch);
    m.event_seq = None;
    m.commit = format!("{sequence:040x}");
    let mut subject = CandidateSubject {
        id: String::new(),
        attachment: "1".repeat(64),
        endpoint: "https://github.com/acme/widgets.git".into(),
        output: branch.into(),
        change: agentscm::context::ChangeId {
            repository: "2".repeat(64),
            id: "change".into(),
        },
        change_version: sequence,
        context_digest: "3".repeat(64),
        policy_epoch: 0,
        revision: m.revision.clone(),
        target: m.base.clone(),
        baseline_commit: "4".repeat(40),
        parent_commit: m.parent.clone(),
        source_commit: "5".repeat(40),
        source_fingerprint: "6".repeat(64),
        comparison_fingerprint: "7".repeat(64),
    };
    subject.id = subject.canonical_id().unwrap();
    let id = derive_candidate_id(REPO, &subject.id, &m.commit, branch);
    m.candidate = Some(Candidate {
        endpoint: subject.endpoint.clone(),
        subject,
        previous_publication: previous.map(str::to_owned),
    });
    NewPublication { id, manifest: m }
}

fn candidate_suite(store: Arc<dyn Store>) {
    let owner = store
        .upsert_user(
            &ForgeIdentity {
                provider: "github".into(),
                host: "github.com".into(),
                user_id: 7,
            },
            "alice",
        )
        .unwrap();
    let (token, _) = mint_agent_token(&*store, &owner.id, "candidate", REPO, 3600).unwrap();
    let now = unix_now();
    let bound = candidate(90, "agents/bound", None);
    let binding = r#"{"server_binding":"synthetic"}"#;
    let row = done(
        store
            .create_bound_publication(&token, &bound, Some(binding), now)
            .unwrap(),
    );
    assert_eq!(row.authorization_binding.as_deref(), Some(binding));
    assert_eq!(
        store.create_publication(&token, &bound, now).unwrap(),
        Create::Conflict
    );
    assert_eq!(
        store
            .create_bound_publication(&token, &bound, Some("{}"), now)
            .unwrap(),
        Create::Conflict
    );
    assert_eq!(
        done(
            store
                .create_bound_publication(&token, &bound, Some(binding), now)
                .unwrap()
        ),
        row
    );
    assert!(
        store
            .create_bound_publication(&token, &bound, Some(&"x".repeat(65537)), now)
            .is_err()
    );
    assert!(
        store
            .create_bound_publication(&token, &bound, Some("not json"), now)
            .is_err()
    );
    let a = candidate(1, "agents/lineage", None);
    done(store.create_publication(&token, &a, now).unwrap());
    store
        .update_publication(&a.id, &Update::BranchCreated, now)
        .unwrap();
    store
        .update_publication(&a.id, &Update::PullRequestAttempt, now)
        .unwrap();
    // A stale local branch update cannot erase the attempted write, nor can rejection.
    let attempted = store
        .update_publication(&a.id, &Update::BranchCreated, now)
        .unwrap();
    assert_eq!(attempted.branch_state, "pr_attempting");
    assert_eq!(attempted.states()["branch"]["state"], "created");
    assert!(
        store
            .update_publication(&a.id, &Update::PullRequestAttempt, now)
            .is_err()
    );
    let b = candidate(2, "agents/lineage", Some(&a.id));
    assert_eq!(
        store.create_publication(&token, &b, now).unwrap(),
        Create::Conflict
    );
    store
        .update_publication(
            &a.id,
            &Update::PullRequest {
                number: 7,
                node_id: "PR_7".into(),
                html_url: "https://github.com/acme/widgets/pull/7".into(),
            },
            now,
        )
        .unwrap();
    done(store.create_publication(&token, &b, now).unwrap());
    store
        .update_publication(
            &b.id,
            &Update::Error(Some("target advanced before attempt".into())),
            now,
        )
        .unwrap();
    let c = candidate(3, "agents/lineage", Some(&b.id));
    done(store.create_publication(&token, &c, now).unwrap());
    assert_eq!(
        store.publication(REPO, &b.id).unwrap().unwrap().manifest,
        b.manifest.canonical()
    );
    store
        .update_publication(&b.id, &Update::BranchCreated, now)
        .unwrap();
    assert!(
        store
            .update_publication(&b.id, &Update::PullRequestAttempt, now)
            .is_err(),
        "superseded B cannot dispatch"
    );
    // No PR in the chain is also safe to supersede before an attempt.
    let initial = candidate(4, "agents/initial", None);
    done(store.create_publication(&token, &initial, now).unwrap());
    let successor = candidate(5, "agents/initial", Some(&initial.id));
    done(store.create_publication(&token, &successor, now).unwrap());
    let other = store
        .upsert_user(
            &ForgeIdentity {
                provider: "github".into(),
                host: "github.com".into(),
                user_id: 8,
            },
            "bob",
        )
        .unwrap();
    let (other_token, _) = mint_agent_token(&*store, &other.id, "other", REPO, 3600).unwrap();
    let wrong_owner = candidate(6, "agents/initial", Some(&successor.id));
    assert_eq!(
        store
            .create_publication(&other_token, &wrong_owner, now)
            .unwrap(),
        Create::Conflict
    );
    let wrong_parent = candidate(7, "agents/initial", Some(&initial.id));
    assert_eq!(
        store
            .create_publication(&token, &wrong_parent, now)
            .unwrap(),
        Create::Conflict
    );
    let mut wrong_lineage = candidate(10, "agents/initial", Some(&successor.id));
    let subject = &mut wrong_lineage.manifest.candidate.as_mut().unwrap().subject;
    subject.change.id = "different-change".into();
    subject.id = subject.canonical_id().unwrap();
    wrong_lineage.id = agentscm::hub::publish::derive_candidate_id(
        REPO,
        &subject.id,
        &wrong_lineage.manifest.commit,
        &wrong_lineage.manifest.branch,
    );
    assert_eq!(
        store
            .create_publication(&token, &wrong_lineage, now)
            .unwrap(),
        Create::Conflict
    );
    // A rejected quota check must roll back the candidate head reservation too.
    let (capped, _) = mint_agent_token(&*store, &owner.id, "capped-candidate", REPO, 3600).unwrap();
    for index in 0..MAX_UNFINISHED {
        done(
            store
                .create_publication(&capped, &new_on("quota", &format!("quota/{index}")), now)
                .unwrap(),
        );
    }
    let quota_candidate = candidate(11, "agents/quota", None);
    assert_eq!(
        store
            .create_publication(&capped, &quota_candidate, now)
            .unwrap(),
        Create::TooMany
    );
    store
        .update_publication(
            &new_on("quota", "quota/0").id,
            &Update::BranchRejected {
                status: 422,
                message: "refused".into(),
            },
            now,
        )
        .unwrap();
    done(
        store
            .create_publication(&capped, &quota_candidate, now)
            .unwrap(),
    );
    // Race: either the old write reserves first, or the successor reserves first;
    // both must never succeed. This exercises the actual backend transaction locks.
    let race = candidate(8, "agents/race", None);
    done(store.create_publication(&token, &race, now).unwrap());
    store
        .update_publication(&race.id, &Update::BranchCreated, now)
        .unwrap();
    let next = candidate(9, "agents/race", Some(&race.id));
    let barrier = Arc::new(std::sync::Barrier::new(2));
    let writer = {
        let store = store.clone();
        let barrier = barrier.clone();
        let id = race.id.clone();
        std::thread::spawn(move || {
            barrier.wait();
            store
                .update_publication(&id, &Update::PullRequestAttempt, now)
                .is_ok()
        })
    };
    barrier.wait();
    let reserved = matches!(
        store.create_publication(&token, &next, now).unwrap(),
        Create::Done(_)
    );
    let dispatched = writer.join().unwrap();
    assert_ne!(reserved, dispatched, "exactly one wins the branch fence");
}

#[test]
fn candidate_lineage_on_sqlite() {
    let dir = TempDir::new().unwrap();
    candidate_suite(
        store::open(&format!(
            "sqlite://{}",
            dir.path().join("hub.sqlite").display()
        ))
        .unwrap(),
    );
}

#[test]
fn candidate_lineage_on_postgres() {
    let Some(pg) = PgSchema::create() else { return };
    candidate_suite(store::open(&pg.url).unwrap());
}

#[test]
fn a_corrupt_manifest_row_still_lists_and_does_not_break_search() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("hub.sqlite");
    let store = store::open(&format!("sqlite://{}", path.display())).unwrap();
    let identity = ForgeIdentity {
        provider: "github".into(),
        host: "github.com".into(),
        user_id: 7,
    };
    let owner = store.upsert_user(&identity, "alice").unwrap();
    let (token, _) = mint_agent_token(&*store, &owner.id, "builder", REPO, 3600).unwrap();
    let now = unix_now();
    let good = done(
        store
            .create_publication(&token, &new_on("Findable title", "feat/good"), now)
            .unwrap(),
    );
    let bad = done(
        store
            .create_publication(&token, &new_on("t", "feat/bad"), now)
            .unwrap(),
    );
    rusqlite::Connection::open(&path)
        .unwrap()
        .execute(
            "UPDATE hub_publications SET manifest = '{not json' WHERE id = ?1",
            [&bad.id],
        )
        .unwrap();
    let ids = |q: Option<&str>| -> Vec<String> {
        match q {
            None => store.publications(REPO, None, 100).unwrap(),
            Some(q) => store.publication_search(REPO, None, q, 100).unwrap().rows,
        }
        .into_iter()
        .map(|p| p.id)
        .collect()
    };
    assert_eq!(ids(None).len(), 2);
    assert_eq!(ids(Some("findable")), std::slice::from_ref(&good.id));
    assert_eq!(ids(Some("feat/bad")), std::slice::from_ref(&bad.id));
}

/// A search reads at most 256 publications below the cursor and says where it stopped, so a
/// match further back is reached by following the cursor.
fn window_suite(store: Arc<dyn Store>) {
    let identity = ForgeIdentity {
        provider: "github".into(),
        host: "github.com".into(),
        user_id: 7,
    };
    let owner = store.upsert_user(&identity, "alice").unwrap();
    let base = unix_now();
    let mut tokens = Vec::new();
    for n in 0..15 {
        tokens.push(
            mint_agent_token(&*store, &owner.id, &format!("t{n}"), REPO, 3600)
                .unwrap()
                .0,
        );
    }
    let mut oldest = String::new();
    for i in 0..300usize {
        let branch = if i == 0 {
            "feat/needle".to_owned()
        } else {
            format!("feat/bulk{i}")
        };
        let made = done(
            store
                .create_publication(&tokens[i / 20], &new_on("t", &branch), base + i as i64)
                .unwrap(),
        );
        if i == 0 {
            oldest = made.id;
        }
    }
    let first = store.publication_search(REPO, None, "needle", 5).unwrap();
    assert!(first.rows.is_empty() && first.window_full);
    let stopped = first.searched_to.unwrap();
    // The window is the 256 newest: the 256th row from the top is the oldest examined.
    let newest = store.publications(REPO, None, 256).unwrap();
    assert_eq!(stopped, newest[255].id);
    let second = store
        .publication_search(REPO, Some(&stopped), "needle", 5)
        .unwrap();
    assert_eq!(second.rows.len(), 1);
    assert_eq!(second.rows[0].id, oldest);
    assert!(!second.window_full);
    // More matches than the page: the extra row is returned for the caller to cut.
    let many = store.publication_search(REPO, None, "bulk", 5).unwrap();
    assert_eq!(many.rows.len(), 5);
    assert!(many.window_full);
}

#[test]
fn publication_search_window_on_sqlite() {
    let dir = TempDir::new().unwrap();
    let url = format!("sqlite://{}", dir.path().join("hub.sqlite").display());
    window_suite(store::open(&url).unwrap());
}

#[test]
fn publication_search_window_on_postgres() {
    let Some(pg) = PgSchema::create() else { return };
    window_suite(store::open(&pg.url).unwrap());
}
