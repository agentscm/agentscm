//! Opt-in release of the hold of Work whose merged pull request closed its source issue as
//! completed. Store cases run against SQLite always and PostgreSQL when
//! AGENTSCM_TEST_DATABASE_URL is set; the setting route runs against SQLite.
use crate::hub_pg::PgSchema;

use std::sync::Arc;

use agentscm::backend::LocalBackend;
use agentscm::hub::catchup::{
    CatchupCoverageState, CatchupMergeSource, CatchupRepositoryScope, CatchupSourceStatus,
    NewCatchupMerge,
};
use agentscm::hub::connection::{ConnectedRepository, NewConnection};
use agentscm::hub::login::{Login, LoginConfig};
use agentscm::hub::merged_holds::{
    CloseReason, IssueClosure, IssueState, OBSERVATION_KIND, merged_release_enabled,
    observation_key, release_settled, set_merged_release,
};
use agentscm::hub::planned_issues::IssueIdentity;
use agentscm::hub::store::{
    self, ForgeIdentity, Store, agent_token_digest, mint_bound_agent_token, new_connection_id,
    unix_now,
};
use agentscm::hub::work::{WorkItem, WorkStart};
use agentscm::hub::work_issues::WorkIssueLinkUpdate;
use agentscm::hub::work_lifecycle::{OutcomeValue, ReservationStatus};
use agentscm::hub::work_links::{WorkPrLinkTarget, WorkPrLinkUpdate};
use agentscm::hub::work_reconciliation::{
    DeclaredPrLink, WorkDeclarations, WorkDeclarationsUpdate,
};
use agentscm::hub::work_transition::{OperationalSelect, WorkLifecycleReader};
use agentscm::hub::{HubState, router};
use serde_json::{Value, json};
use tempfile::TempDir;

const REPO: &str = "acme/widgets";
const REPO_ID: i64 = 101;
const SESSION: &str = "lead-session";
const PULL: u64 = 41;

struct Db {
    store: Arc<dyn Store>,
    _pg: Option<PgSchema>,
    dir: Option<TempDir>,
}

impl Db {
    fn sqlite() -> Self {
        let dir = TempDir::new().unwrap();
        let url = format!("sqlite://{}", dir.path().join("hub.sqlite").display());
        Self {
            store: store::open(&url).unwrap(),
            _pg: None,
            dir: Some(dir),
        }
    }

    fn postgres() -> Option<Self> {
        let pg = PgSchema::create()?;
        Some(Self {
            store: store::open(&pg.url).unwrap(),
            _pg: Some(pg),
            dir: None,
        })
    }
}

fn scope() -> CatchupRepositoryScope {
    CatchupRepositoryScope {
        provider: "github".into(),
        host: "github.com".into(),
        repository: REPO.into(),
        repository_id: Some(REPO_ID),
    }
}

struct World {
    repo: ConnectedRepository,
    token: String,
    secret: String,
    owner: String,
    work: String,
}

fn closure(state: IssueState, reason: CloseReason, closer: Option<u64>) -> IssueClosure {
    IssueClosure {
        state,
        reason,
        closer_pull: closer,
        closer_repository_id: closer.map(|_| REPO_ID),
    }
}

fn completed_by(pull: u64) -> IssueClosure {
    closure(IssueState::Closed, CloseReason::Completed, Some(pull))
}

/// A connected repository and a held Work on a branch whose pull request 41 merged, linked to
/// issue 7. `stored` is the closure observation of issue 7, if any.
fn world(db: &Db, stored: Option<IssueClosure>) -> World {
    let store = &*db.store;
    let connection = store
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
            "usr_admin",
        )
        .unwrap();
    let repo = store
        .upsert_repository(&connection.id, "github.com", REPO_ID, REPO, None, None)
        .unwrap();
    for source in [
        CatchupMergeSource::TrackedPublication,
        CatchupMergeSource::ForgePull,
    ] {
        store
            .record_catchup_source_status(
                &scope(),
                &CatchupSourceStatus {
                    source,
                    state: CatchupCoverageState::ObservedOnly,
                    checked_at: unix_now(),
                    unread_pages: 0,
                    deferred: 0,
                    window_from: None,
                },
            )
            .unwrap();
    }
    let user = store
        .upsert_user(
            &ForgeIdentity {
                provider: "github".into(),
                host: "github.com".into(),
                user_id: 1,
            },
            "merger",
        )
        .unwrap();
    let (_, secret, _) =
        mint_bound_agent_token(store, &user.id, "agent", REPO, REPO_ID, 3600, false).unwrap();
    let secret = secret.expose().to_owned();
    let token = store
        .authenticate(&agent_token_digest(&secret).unwrap(), unix_now())
        .unwrap()
        .unwrap()
        .token_id;
    store.observe_lease_clock(unix_now()).unwrap();
    let item = store
        .work_start(
            &token,
            &serde_json::from_value::<WorkStart>(json!({
                "request_key": "merged", "title": "Synthetic merged Work",
                "session": SESSION, "scope": ["src/merged.rs"], "branch": "feature/m"
            }))
            .unwrap(),
            unix_now(),
        )
        .unwrap()
        .item;
    store
        .work_issue_link_set(
            &token,
            &WorkIssueLinkUpdate {
                work_id: item.id.clone(),
                operation_id: "link-7".into(),
                generation: item.generation,
                session: SESSION.into(),
                expected_updated_seq: item.updated_seq,
                link: Some(IssueIdentity {
                    provider: "github".into(),
                    host: "github.com".into(),
                    repository_id: REPO_ID,
                    number: 7,
                }),
            },
            unix_now(),
        )
        .unwrap();
    let at = unix_now() + 5;
    store
        .record_catchup_merge(
            &scope(),
            &NewCatchupMerge {
                source: CatchupMergeSource::ForgePull,
                source_id: "forge-pr-41".into(),
                occurred_at: Some(at),
                title: None,
                branch: "feature/m".into(),
                base: "main".into(),
                head_commit: "b".repeat(40),
                merge_commit: format!("{PULL:040x}"),
                pull_number: i64::try_from(PULL).unwrap(),
                paths: vec!["src/merged.rs".into()],
                paths_complete: true,
                same_repository_head: true,
            },
            at + 1,
        )
        .unwrap();
    if let Some(stored) = stored {
        store
            .observe(
                &repo.id,
                OBSERVATION_KIND,
                &observation_key(7),
                &serde_json::to_string(&stored).unwrap(),
                1_000,
            )
            .unwrap();
    }
    World {
        repo,
        token,
        secret,
        owner: user.id,
        work: item.id,
    }
}

fn held(db: &Db, w: &World) -> (bool, WorkItem) {
    let now = unix_now() + 10;
    let item = db.store.work_get(&w.token, &w.work, now).unwrap().unwrap();
    let owner = db.store.user_by_id(&w.owner).unwrap().unwrap();
    let reader = WorkLifecycleReader::Person {
        owner: &owner,
        repository: REPO,
        repository_id: REPO_ID,
    };
    let overview = db
        .store
        .work_operational(&reader, &OperationalSelect::One(&w.work), now)
        .unwrap();
    let operational = &overview.work[0];
    assert_ne!(operational.reservation.status, ReservationStatus::Lapsed);
    assert_eq!(operational.outcome.value, OutcomeValue::None);
    (operational.reservation.basis.is_some(), item)
}

fn release(db: &Db, w: &World) -> Vec<String> {
    release_settled(&*db.store, "github", &w.repo, unix_now() + 10).unwrap()
}

fn off_by_default_keeps_the_hold(db: &Db) {
    let w = world(db, Some(completed_by(41)));
    assert!(!merged_release_enabled(&*db.store, REPO_ID));
    assert!(release(db, &w).is_empty());
    assert!(held(db, &w).0);
}

fn on_releases_a_settled_hold_once_and_never_completes(db: &Db) {
    let w = world(db, Some(completed_by(41)));
    set_merged_release(&*db.store, REPO_ID, true, &w.owner).unwrap();
    assert_eq!(release(db, &w), vec![w.work.clone()]);
    let (holds, item) = held(db, &w);
    assert!(!holds, "the hold is released");
    assert_ne!(
        format!("{:?}", item.state).to_lowercase(),
        "finished",
        "the Work stays unfinished"
    );
    let events: Vec<_> = db
        .store
        .events_after(0, 1000)
        .unwrap()
        .into_iter()
        .filter(|e| e.kind == "work_transition" && e.subject == w.work)
        .map(|e| serde_json::from_str::<Value>(&e.data).unwrap())
        .collect();
    let last = events.last().unwrap();
    assert_eq!(last["transition"], "release");
    assert_eq!(last["outcome"], Value::Null);
    // The policy is the actor: not the owner as a person, so no login is attributed.
    assert_eq!(last["actor"]["via"], "policy", "{last}");
    assert_eq!(last["actor"]["kind"], "owner");
    assert_eq!(last["actor"]["login"], Value::Null);
    assert!(
        last["reason"]
            .as_str()
            .unwrap()
            .starts_with("Released by repository policy"),
        "{last}"
    );
    // A second poll finds nothing to release and records nothing more.
    let before = db.store.latest_event_seq().unwrap();
    assert!(release(db, &w).is_empty());
    assert_eq!(db.store.latest_event_seq().unwrap(), before);
    // Turning it off again changes no stored Work.
    set_merged_release(&*db.store, REPO_ID, false, &w.owner).unwrap();
    assert!(!merged_release_enabled(&*db.store, REPO_ID));
}

fn other_facts_keep_the_hold(db_with: impl Fn() -> Db) {
    let cases = [
        (
            "part of: the issue is open",
            Some(closure(IssueState::Open, CloseReason::Other, None)),
        ),
        (
            "closed as not planned",
            Some(closure(
                IssueState::Closed,
                CloseReason::NotPlanned,
                Some(41),
            )),
        ),
        ("closed by another pull request", Some(completed_by(42))),
        (
            "closed by no pull request",
            Some(closure(IssueState::Closed, CloseReason::Completed, None)),
        ),
        ("no closure observation", None),
    ];
    for (name, stored) in cases {
        let db = db_with();
        let w = world(&db, stored);
        set_merged_release(&*db.store, REPO_ID, true, &w.owner).unwrap();
        assert!(release(&db, &w).is_empty(), "{name}");
        assert!(held(&db, &w).0, "{name}: the hold is kept");
    }
}

/// The Work's lead takes it over again, so it holds once more.
fn resume(db: &Db, w: &World) {
    let now = unix_now() + 20;
    let item = db.store.work_get(&w.token, &w.work, now).unwrap().unwrap();
    db.store
        .work_start(
            &w.token,
            &serde_json::from_value::<WorkStart>(json!({
                "work_id": w.work, "title": "resume", "session": SESSION,
                "scope": ["src/merged.rs"], "takeover_generation": item.generation
            }))
            .unwrap(),
            now,
        )
        .unwrap();
    assert!(held(db, w).0, "the Work holds again");
}

fn merge_row(number: u64, branch: &str, same_repository: bool) -> NewCatchupMerge {
    let at = unix_now() + 5;
    NewCatchupMerge {
        source: CatchupMergeSource::ForgePull,
        source_id: format!("forge-pr-{number}"),
        occurred_at: Some(at),
        title: None,
        branch: branch.into(),
        base: "main".into(),
        head_commit: "b".repeat(40),
        merge_commit: format!("{number:040x}"),
        pull_number: i64::try_from(number).unwrap(),
        paths: vec!["src/x.rs".into()],
        paths_complete: true,
        same_repository_head: same_repository,
    }
}

fn a_resumed_work_is_not_released_again(db: &Db) {
    let w = world(db, Some(completed_by(41)));
    set_merged_release(&*db.store, REPO_ID, true, &w.owner).unwrap();
    assert_eq!(release(db, &w), vec![w.work.clone()]);
    assert!(!held(db, &w).0);
    resume(db, &w);
    // The decision to resume stands: later polls leave the hold alone.
    assert!(release(db, &w).is_empty());
    assert!(release(db, &w).is_empty());
    assert!(held(db, &w).0, "the resumed hold is kept");
}

fn every_pull_request_must_be_merged(db: &Db) {
    let w = world(db, Some(completed_by(41)));
    set_merged_release(&*db.store, REPO_ID, true, &w.owner).unwrap();
    // A second pull request the Work declares (v2) that is not merged.
    let item = db
        .store
        .work_get(&w.token, &w.work, unix_now())
        .unwrap()
        .unwrap();
    db.store
        .work_declarations_set(
            &w.token,
            &WorkDeclarationsUpdate {
                work_id: w.work.clone(),
                operation_id: "declare-77".into(),
                generation: item.generation,
                session: SESSION.into(),
                expected_updated_seq: item.updated_seq,
                declarations: WorkDeclarations {
                    links: vec![DeclaredPrLink {
                        id: "second".into(),
                        target: WorkPrLinkTarget {
                            repository_id: REPO_ID,
                            number: 77,
                            head_sha: "c".repeat(40),
                        },
                    }],
                    prerequisites: vec![],
                },
            },
            unix_now(),
        )
        .unwrap();
    assert!(release(db, &w).is_empty());
    assert!(held(db, &w).0, "a declared pull request is not merged");
    // Once it merges too, the Work is settled.
    db.store
        .record_catchup_merge(
            &scope(),
            &merge_row(77, "feature/second", true),
            unix_now() + 6,
        )
        .unwrap();
    assert_eq!(release(db, &w), vec![w.work.clone()]);
}

fn another_open_pull_request_on_the_branch_blocks_an_explicit_link(db: &Db) {
    let w = world(db, Some(completed_by(41)));
    set_merged_release(&*db.store, REPO_ID, true, &w.owner).unwrap();
    let item = db
        .store
        .work_get(&w.token, &w.work, unix_now())
        .unwrap()
        .unwrap();
    db.store
        .work_pr_link_set(
            &w.token,
            &WorkPrLinkUpdate {
                work_id: w.work.clone(),
                operation_id: "link-41".into(),
                generation: item.generation,
                session: SESSION.into(),
                expected_updated_seq: item.updated_seq,
                link: Some(WorkPrLinkTarget {
                    repository_id: REPO_ID,
                    number: i64::try_from(PULL).unwrap(),
                    head_sha: "b".repeat(40),
                }),
            },
            unix_now(),
        )
        .unwrap();
    let observe = |state: &str, stamp: i64| {
        let data = json!({"number": 55, "state": state, "repository_id": REPO_ID,
            "head_repository_id": REPO_ID, "head_sha": "d".repeat(40),
            "head_ref": "feature/m"})
        .to_string();
        db.store
            .observe(&w.repo.id, "pull_request", "55", &data, stamp)
            .unwrap();
    };
    observe("open", 2_000);
    assert!(release(db, &w).is_empty());
    assert!(held(db, &w).0, "a second open pull request on the branch");
    observe("closed", 3_000);
    assert_eq!(release(db, &w), vec![w.work.clone()]);
}

fn a_limited_neighbour_does_not_block_a_clean_work(db: &Db) {
    let w = world(db, Some(completed_by(41)));
    set_merged_release(&*db.store, REPO_ID, true, &w.owner).unwrap();
    // A second Work, on a branch whose merge history may be truncated: inference finds its one
    // merged pull request (60) but cannot rule out older ones.
    let other = db
        .store
        .work_start(
            &w.token,
            &serde_json::from_value::<WorkStart>(json!({
                "request_key": "limited", "title": "Limited Work", "session": "other-session",
                "scope": ["src/limited.rs"], "branch": "feature/n"
            }))
            .unwrap(),
            unix_now(),
        )
        .unwrap()
        .item;
    db.store
        .work_issue_link_set(
            &w.token,
            &WorkIssueLinkUpdate {
                work_id: other.id.clone(),
                operation_id: "link-8".into(),
                generation: other.generation,
                session: "other-session".into(),
                expected_updated_seq: other.updated_seq,
                link: Some(IssueIdentity {
                    provider: "github".into(),
                    host: "github.com".into(),
                    repository_id: REPO_ID,
                    number: 8,
                }),
            },
            unix_now(),
        )
        .unwrap();
    db.store
        .observe(
            &w.repo.id,
            OBSERVATION_KIND,
            &observation_key(8),
            &serde_json::to_string(&completed_by(60)).unwrap(),
            1_000,
        )
        .unwrap();
    for number in 100..119 {
        db.store
            .record_catchup_merge(
                &scope(),
                &merge_row(number, "feature/n", false),
                unix_now() + 6,
            )
            .unwrap();
    }
    db.store
        .record_catchup_merge(&scope(), &merge_row(60, "feature/n", true), unix_now() + 6)
        .unwrap();
    assert_eq!(release(db, &w), vec![w.work.clone()]);
    assert!(!held(db, &w).0, "the clean Work is released");
    let now = unix_now() + 10;
    let owner = db.store.user_by_id(&w.owner).unwrap().unwrap();
    let reader = WorkLifecycleReader::Person {
        owner: &owner,
        repository: REPO,
        repository_id: REPO_ID,
    };
    let limited = db
        .store
        .work_operational(&reader, &OperationalSelect::One(&other.id), now)
        .unwrap();
    assert!(
        limited.work[0].reservation.basis.is_some(),
        "the limited Work keeps its hold"
    );
}

#[test]
fn sqlite_off_by_default_keeps_the_hold() {
    off_by_default_keeps_the_hold(&Db::sqlite());
}

#[test]
fn postgres_off_by_default_keeps_the_hold() {
    let Some(db) = Db::postgres() else {
        return;
    };
    off_by_default_keeps_the_hold(&db);
}

#[test]
fn sqlite_on_releases_a_settled_hold_once_and_never_completes() {
    on_releases_a_settled_hold_once_and_never_completes(&Db::sqlite());
}

#[test]
fn postgres_on_releases_a_settled_hold_once_and_never_completes() {
    let Some(db) = Db::postgres() else {
        return;
    };
    on_releases_a_settled_hold_once_and_never_completes(&db);
}

#[test]
fn sqlite_a_resumed_work_is_not_released_again() {
    a_resumed_work_is_not_released_again(&Db::sqlite());
}

#[test]
fn postgres_a_resumed_work_is_not_released_again() {
    let Some(db) = Db::postgres() else {
        return;
    };
    a_resumed_work_is_not_released_again(&db);
}

#[test]
fn sqlite_every_pull_request_must_be_merged() {
    every_pull_request_must_be_merged(&Db::sqlite());
}

#[test]
fn postgres_every_pull_request_must_be_merged() {
    let Some(db) = Db::postgres() else {
        return;
    };
    every_pull_request_must_be_merged(&db);
}

#[test]
fn sqlite_another_open_pull_request_on_the_branch_blocks_an_explicit_link() {
    another_open_pull_request_on_the_branch_blocks_an_explicit_link(&Db::sqlite());
}

#[test]
fn postgres_another_open_pull_request_on_the_branch_blocks_an_explicit_link() {
    let Some(db) = Db::postgres() else {
        return;
    };
    another_open_pull_request_on_the_branch_blocks_an_explicit_link(&db);
}

#[test]
fn sqlite_a_limited_neighbour_does_not_block_a_clean_work() {
    a_limited_neighbour_does_not_block_a_clean_work(&Db::sqlite());
}

#[test]
fn postgres_a_limited_neighbour_does_not_block_a_clean_work() {
    let Some(db) = Db::postgres() else {
        return;
    };
    a_limited_neighbour_does_not_block_a_clean_work(&db);
}

#[test]
fn sqlite_other_facts_keep_the_hold() {
    other_facts_keep_the_hold(Db::sqlite);
}

#[test]
fn postgres_other_facts_keep_the_hold() {
    if Db::postgres().is_none() {
        return;
    }
    // One schema per case.
    other_facts_keep_the_hold(|| Db::postgres().unwrap());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn only_an_admin_switches_the_release_on_and_it_reads_off_by_default() {
    use reqwest::Method;
    const SETTINGS: &str = "/v1/hub/standup/merged-release/settings";
    let db = Db::sqlite();
    let alice = world(&db, None);
    let admin_user = db
        .store
        .upsert_user(
            &ForgeIdentity {
                provider: "github".into(),
                host: "github.com".into(),
                user_id: 900,
            },
            "root",
        )
        .unwrap();
    let (_, admin, _) = mint_bound_agent_token(
        &*db.store,
        &admin_user.id,
        "claude",
        REPO,
        REPO_ID,
        3600,
        false,
    )
    .unwrap();
    let admin = admin.expose().to_owned();
    let login = LoginConfig::from_lookup("", |name| {
        (name == "AGENTSCM_ADMINS").then(|| "github:#900".to_owned())
    })
    .unwrap();
    let objects = LocalBackend::open(db.dir.as_ref().unwrap().path().join("objects")).unwrap();
    let state =
        HubState::new(db.store.clone(), Arc::new(objects)).with_login(Login::new(login).unwrap());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, router(state)).await });
    let call = |method: Method, secret: String, body: Option<Value>| {
        let base = base.clone();
        async move {
            let mut request = reqwest::Client::new()
                .request(method, format!("{base}{SETTINGS}"))
                .bearer_auth(secret);
            if let Some(body) = body {
                request = request
                    .header("content-type", "application/json")
                    .body(body.to_string());
            }
            let response = request.send().await.unwrap();
            let status = response.status().as_u16();
            let bytes = response.bytes().await.unwrap();
            (
                status,
                serde_json::from_slice::<Value>(&bytes).unwrap_or(Value::Null),
            )
        }
    };
    let (status, view) = call(Method::GET, alice.secret.clone(), None).await;
    assert_eq!(status, 200, "{view}");
    assert_eq!(view, json!({"repository": REPO, "enabled": false}));
    let on = Some(json!({"enabled": true}));
    let (status, refusal) = call(Method::PUT, alice.secret.clone(), on.clone()).await;
    assert!((400..500).contains(&status), "{status}: {refusal}");
    assert!(!merged_release_enabled(&*db.store, REPO_ID));
    let (status, view) = call(Method::PUT, admin.clone(), on).await;
    assert_eq!(status, 200, "{view}");
    assert_eq!(view["enabled"], true);
    assert!(merged_release_enabled(&*db.store, REPO_ID));
    let (_, view) = call(Method::GET, alice.secret.clone(), None).await;
    assert_eq!(view["enabled"], true);
    let (status, view) = call(Method::PUT, admin, Some(json!({"enabled": false}))).await;
    assert_eq!(status, 200, "{view}");
    assert_eq!(view["enabled"], false);
}
