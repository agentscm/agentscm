//! Issue decisions are caller assertions; the same lifecycle contract runs on both stores.
use std::sync::{Arc, Barrier};
use std::thread;

use crate::hub_pg::PgSchema;
use agentscm::hub::connection::NewConnection;
use agentscm::hub::planned_issues::IssueIdentity;
use agentscm::hub::store::{
    self, ForgeIdentity, Store, mint_bound_agent_token, new_connection_id, unix_now,
};
use agentscm::hub::work::{
    WorkItem, WorkRepositoryRename, WorkRepositoryScope, WorkRole, WorkStart,
};
use agentscm::hub::work_issues::{WorkIssueChoice, WorkIssueLinkUpdate};
use tempfile::TempDir;

const REPO: &str = "acme/widgets";
const REPO_ID: i64 = 101;
struct Fixture {
    store: Arc<dyn Store>,
    _dir: Option<TempDir>,
    _pg: Option<PgSchema>,
}
impl Fixture {
    fn sqlite() -> Self {
        let dir = TempDir::new().unwrap();
        let store =
            store::open(&format!("sqlite://{}", dir.path().join("hub.db").display())).unwrap();
        Self {
            store,
            _dir: Some(dir),
            _pg: None,
        }
    }
    fn postgres() -> Option<Self> {
        let pg = PgSchema::create()?;
        Some(Self {
            store: store::open(&pg.url).unwrap(),
            _dir: None,
            _pg: Some(pg),
        })
    }
    fn token(&self, user: i64, read_only: bool) -> String {
        let owner = self
            .store
            .upsert_user(
                &ForgeIdentity {
                    provider: "github".into(),
                    host: "github.com".into(),
                    user_id: user,
                },
                &format!("user{user}"),
            )
            .unwrap();
        mint_bound_agent_token(
            &*self.store,
            &owner.id,
            "agent",
            REPO,
            REPO_ID,
            3600,
            read_only,
        )
        .unwrap()
        .0
    }
}
fn issue(number: i64) -> IssueIdentity {
    IssueIdentity {
        provider: "github".into(),
        host: "github.com".into(),
        repository_id: REPO_ID,
        number,
    }
}
fn start(key: &str) -> WorkStart {
    WorkStart {
        issue_choice: None,
        work_id: None,
        source: None,
        request_key: Some(key.into()),
        title: format!("Implement {key} #7"),
        session: key.into(),
        role: WorkRole::Lead,
        scope: vec![],
        branch: None,
        worktree: None,
        next_action: None,
        depends_on: vec![],
        related_work: vec![],
        takeover_generation: None,
        preflight: None,
    }
}
fn update(item: &WorkItem, operation: &str, number: i64) -> WorkIssueLinkUpdate {
    WorkIssueLinkUpdate {
        work_id: item.id.clone(),
        operation_id: operation.into(),
        generation: item.generation,
        session: item.lead_session.clone(),
        expected_updated_seq: item.updated_seq,
        link: Some(issue(number)),
    }
}
fn decisions_and_replay(f: Fixture) {
    let token = f.token(1, false);
    let now = unix_now();
    let mut request = start("choice");
    request.issue_choice = Some(WorkIssueChoice::Link { issue: issue(7) });
    let first = f.store.work_start(&token, &request, now).unwrap();
    assert!(first.created);
    assert_eq!(first.item.issue_link, Some(issue(7)));
    assert_eq!(
        first.item.initial_issue_choice,
        request.issue_choice.clone().unwrap()
    );
    let retry = f.store.work_start(&token, &request, now).unwrap();
    assert!(!retry.created);
    assert_eq!(retry.item.updated_seq, first.item.updated_seq);
    let mutation = update(&first.item, "later", 8);
    let changed = f
        .store
        .work_issue_link_set(&token, &mutation, now + 1)
        .unwrap();
    assert_eq!(changed.item.id, first.item.id);
    assert_eq!(changed.item.source, first.item.source);
    assert_eq!(
        changed.item.initial_issue_choice,
        first.item.initial_issue_choice
    );
    assert_eq!(changed.item.issue_link, Some(issue(8)));
    let repeated_start = f.store.work_start(&token, &request, now + 2).unwrap();
    assert_eq!(repeated_start.item.issue_link, Some(issue(8)));
    let mut remove = update(&changed.item, "remove", 9);
    remove.link = None;
    let removed = f
        .store
        .work_issue_link_set(&token, &remove, now + 3)
        .unwrap();
    let replay = f
        .store
        .work_issue_link_set(&token, &mutation, now + 4)
        .unwrap();
    assert!(replay.receipt.replayed);
    assert_eq!(replay.receipt.event_seq, changed.receipt.event_seq);
    assert_eq!(replay.item.updated_seq, removed.item.updated_seq);
    assert_eq!(replay.item.issue_link, None);
    let mut conflicting = mutation.clone();
    conflicting.link = Some(issue(99));
    assert!(
        f.store
            .work_issue_link_set(&token, &conflicting, now + 5)
            .is_err()
    );
    let mut altered_start = request.clone();
    altered_start.issue_choice = Some(WorkIssueChoice::NoneApplies {
        reason: "Separate task".into(),
    });
    assert!(f.store.work_start(&token, &altered_start, now + 5).is_err());
    let events = f
        .store
        .events_after(first.item.created_seq - 1, 100)
        .unwrap();
    let created = events.iter().find(|e| e.kind == "work_started").unwrap();
    assert!(created.data.contains("initial_issue_choice"));
    assert!(
        events
            .iter()
            .any(|e| e.kind == "work_issue_link" && e.data.contains("issue_link_set"))
    );
    let latest = f
        .store
        .work_get(&token, &first.item.id, now + 6)
        .unwrap()
        .unwrap();
    assert_eq!(latest.initial_issue_choice, first.item.initial_issue_choice);
}
fn compatibility_and_authorization(f: Fixture) {
    let token = f.token(1, false);
    let other = f.token(2, false);
    let reader = f.token(3, true);
    let now = unix_now();
    let initial = f
        .store
        .work_start(&token, &start("legacy"), now)
        .unwrap()
        .item;
    assert_eq!(
        initial.initial_issue_choice,
        WorkIssueChoice::NotProvided {}
    );
    let mutation = update(&initial, "link", 7);
    assert!(f.store.work_issue_link_set(&other, &mutation, now).is_err());
    assert!(
        f.store
            .work_issue_link_set(&reader, &mutation, now)
            .is_err()
    );
    for foreign in [
        IssueIdentity {
            repository_id: 102,
            ..issue(7)
        },
        IssueIdentity {
            host: "other.example".into(),
            ..issue(7)
        },
        IssueIdentity {
            provider: "gitlab".into(),
            ..issue(7)
        },
        issue(0),
    ] {
        let mut bad = mutation.clone();
        bad.link = Some(foreign.clone());
        assert!(f.store.work_issue_link_set(&token, &bad, now).is_err());
        let mut bad_start = start("invalid");
        bad_start.issue_choice = Some(WorkIssueChoice::Link { issue: foreign });
        assert!(f.store.work_start(&token, &bad_start, now).is_err());
    }
    let mut wrong = mutation.clone();
    wrong.session = "other".into();
    assert!(f.store.work_issue_link_set(&token, &wrong, now).is_err());
    let mut wrong = mutation.clone();
    wrong.generation += 1;
    assert!(f.store.work_issue_link_set(&token, &wrong, now).is_err());
    let result = f.store.work_issue_link_set(&token, &mutation, now).unwrap();
    let mut stale = update(&initial, "stale", 8);
    stale.expected_updated_seq = initial.updated_seq;
    assert!(f.store.work_issue_link_set(&token, &stale, now).is_err());
    assert_eq!(
        f.store
            .work_get(&token, &initial.id, now)
            .unwrap()
            .unwrap()
            .updated_seq,
        result.item.updated_seq
    );
    for (key, choice) in [
        (
            "new",
            WorkIssueChoice::NewIssue {
                title: "New widget issue".into(),
            },
        ),
        (
            "none",
            WorkIssueChoice::NoneApplies {
                reason: "Unrelated local maintenance".into(),
            },
        ),
    ] {
        let mut request = start(key);
        request.issue_choice = Some(choice.clone());
        let item = f.store.work_start(&token, &request, now).unwrap().item;
        assert_eq!(item.initial_issue_choice, choice);
        assert_eq!(item.issue_link, None);
    }
    let mut automatic = start("automatic");
    automatic.request_key = None;
    automatic.issue_choice = Some(WorkIssueChoice::NoneApplies {
        reason: "New goal".into(),
    });
    let first = f.store.work_start_auto(&token, &automatic, now).unwrap();
    assert!(first.created);
    automatic.issue_choice = Some(WorkIssueChoice::NewIssue {
        title: "Different goal".into(),
    });
    assert!(
        f.store
            .work_start_auto(&token, &automatic, now)
            .unwrap()
            .conflict
            .is_some()
    );
}
fn races(f: Fixture) {
    let token = f.token(1, false);
    let now = unix_now();
    let item = f
        .store
        .work_start(&token, &start("race"), now)
        .unwrap()
        .item;
    let barrier = Arc::new(Barrier::new(3));
    let handles = (0..2)
        .map(|i| {
            let store = f.store.clone();
            let token = token.clone();
            let barrier = barrier.clone();
            let mutation = update(&item, &format!("op{i}"), 7 + i);
            thread::spawn(move || {
                barrier.wait();
                store.work_issue_link_set(&token, &mutation, now).is_ok()
            })
        })
        .collect::<Vec<_>>();
    barrier.wait();
    assert_eq!(
        handles
            .into_iter()
            .map(|h| h.join().unwrap())
            .filter(|ok| *ok)
            .count(),
        1
    );
}
fn rename(f: Fixture) {
    let connection = f
        .store
        .insert_connection(
            &NewConnection {
                id: new_connection_id().unwrap(),
                provider: "github".into(),
                host: "github.com".into(),
                kind: "pat".into(),
                app_id: None,
                app_slug: None,
                acts_as: Some("robot".into()),
            },
            "operator",
        )
        .unwrap();
    f.store
        .upsert_repository(&connection.id, "github.com", REPO_ID, REPO, None, None)
        .unwrap();
    let token = f.token(1, false);
    let now = unix_now();
    let mut request = start("rename");
    request.issue_choice = Some(WorkIssueChoice::Link { issue: issue(7) });
    let initial = f.store.work_start(&token, &request, now).unwrap().item;
    let destination = "acme/renamed";
    f.store
        .upsert_repository(
            &connection.id,
            "github.com",
            REPO_ID,
            destination,
            None,
            None,
        )
        .unwrap();
    f.store
        .work_repair_repository(
            &WorkRepositoryRename {
                scope: WorkRepositoryScope {
                    provider: "github".into(),
                    host: "github.com".into(),
                    repository: REPO.into(),
                    repository_id: Some(REPO_ID),
                },
                destination: destination.into(),
                apply: true,
            },
            now + 1,
        )
        .unwrap();
    let owner = f
        .store
        .upsert_user(
            &ForgeIdentity {
                provider: "github".into(),
                host: "github.com".into(),
                user_id: 1,
            },
            "user1",
        )
        .unwrap();
    let renamed = mint_bound_agent_token(
        &*f.store,
        &owner.id,
        "renamed",
        destination,
        REPO_ID,
        3600,
        false,
    )
    .unwrap()
    .0;
    let item = f
        .store
        .work_get(&renamed, &initial.id, now + 2)
        .unwrap()
        .unwrap();
    assert_eq!(item.issue_link, Some(issue(7)));
    assert_eq!(item.initial_issue_choice, initial.initial_issue_choice);
    assert_eq!(item.source, initial.source);
}
macro_rules! cases {
    ($module:ident, $factory:expr) => {
        mod $module {
            use super::*;
            #[test]
            fn choices_and_replay() {
                if let Some(f) = $factory {
                    decisions_and_replay(f);
                }
            }
            #[test]
            fn compatibility_and_authorization() {
                if let Some(f) = $factory {
                    super::compatibility_and_authorization(f);
                }
            }
            #[test]
            fn concurrent_updates() {
                if let Some(f) = $factory {
                    races(f);
                }
            }
            #[test]
            fn repository_rename() {
                if let Some(f) = $factory {
                    rename(f);
                }
            }
        }
    };
}
cases!(on_sqlite, Some(Fixture::sqlite()));
cases!(on_postgres, Fixture::postgres());
