//! The digest of stale Work and repeated collisions: store cases run against SQLite always and
//! PostgreSQL when AGENTSCM_TEST_DATABASE_URL is set; the HTTP cases run against SQLite.
use crate::hub_pg;

use std::sync::Arc;

use agentscm::backend::LocalBackend;
use agentscm::hub::catchup::{
    CatchupCoverageState, CatchupMergeSource, CatchupRepositoryScope, CatchupSourceStatus,
    NewCatchupMerge,
};
use agentscm::hub::digest::{self, DigestQuery, PAIR_COLLISIONS, QUIET_DAYS};
use agentscm::hub::digest_http;
use agentscm::hub::login::{Login, LoginConfig};
use agentscm::hub::session::SessionCall;
use agentscm::hub::store::{
    self, ForgeIdentity, Principal, Store, agent_token_digest, mint_bound_agent_token, unix_now,
};
use agentscm::hub::work::WorkStart;
use agentscm::hub::work_lifecycle::MergeStatus;
use agentscm::hub::{HubError, HubState, router};
use hub_pg::PgSchema;
use serde_json::{Value, json};
use tempfile::TempDir;

const REPO: &str = "acme/widgets";
const REPO_ID: i64 = 101;
const DAY: i64 = 86_400;
const TTL: i64 = 30 * DAY;
const CONTESTED: &str = "src/hub/contested.rs";

struct Db {
    store: Arc<dyn Store>,
    _pg: Option<PgSchema>,
    _dir: Option<TempDir>,
}

impl Db {
    fn sqlite() -> Self {
        let dir = TempDir::new().unwrap();
        let url = format!("sqlite://{}", dir.path().join("hub.sqlite").display());
        Self {
            store: store::open(&url).unwrap(),
            _pg: None,
            _dir: Some(dir),
        }
    }

    fn postgres() -> Option<Self> {
        let pg = PgSchema::create()?;
        Some(Self {
            store: store::open(&pg.url).unwrap(),
            _pg: Some(pg),
            _dir: None,
        })
    }
}

/// A person with a coding-agent token and a read-only token in `REPO`.
struct Person {
    agent: String,
    agent_secret: String,
    reader: Principal,
}

fn person(store: &dyn Store, user_id: i64, login: &str) -> Person {
    let identity = ForgeIdentity {
        provider: "github".into(),
        host: "github.com".into(),
        user_id,
    };
    let user = store.upsert_user(&identity, login).unwrap();
    let (agent, agent_secret, _) =
        mint_bound_agent_token(store, &user.id, "claude", REPO, REPO_ID, TTL, false).unwrap();
    let (_, secret, _) =
        mint_bound_agent_token(store, &user.id, "page", REPO, REPO_ID, TTL, true).unwrap();
    let reader = store
        .authenticate(&agent_token_digest(secret.expose()).unwrap(), unix_now())
        .unwrap()
        .unwrap();
    Person {
        agent,
        agent_secret: agent_secret.expose().to_owned(),
        reader,
    }
}

/// Work led by `who`, started (and last checked in) at `at`.
fn work(store: &dyn Store, who: &Person, key: &str, branch: Option<&str>, at: i64) -> String {
    let start: WorkStart = serde_json::from_value(json!({
        "request_key": key, "title": format!("Task {key}"), "session": format!("session-{key}"),
        "scope": [format!("src/work/{key}.rs")], "branch": branch,
    }))
    .unwrap();
    store.work_start(&who.agent, &start, at).unwrap().item.id
}

fn scope() -> CatchupRepositoryScope {
    CatchupRepositoryScope {
        provider: "github".into(),
        host: "github.com".into(),
        repository: REPO.into(),
        repository_id: Some(REPO_ID),
    }
}

/// A hook-style edit of `CONTESTED`, which the Hub may refuse.
fn edit(store: &dyn Store, who: &Person, session: &str) -> agentscm::hub::session::Touched {
    let call = SessionCall {
        session: Some(session.into()),
        via: "hook:claude".into(),
        task: None,
        paths: vec![CONTESTED.into()],
        deny: true,
        report: false,
    };
    store.touch_session(&who.agent, &call, unix_now()).unwrap()
}

fn read(store: &dyn Store, who: &Person, now: i64) -> digest::WorkDigestV1 {
    digest::read(store, &who.reader, &DigestQuery::default(), now).unwrap()
}

fn stale_work_says_whether_its_pull_request_merged(db: &Db) {
    let store = &*db.store;
    let now = unix_now();
    let alice = person(store, 1, "alice");
    let bob = person(store, 2, "bob");
    let merged = work(store, &alice, "merged", Some("feat/merged"), now - 10 * DAY);
    let open = work(store, &alice, "open", Some("feat/open"), now - 9 * DAY);
    let loose = work(store, &alice, "loose", None, now - 8 * DAY);
    work(store, &alice, "fresh", Some("feat/fresh"), now - DAY);
    work(store, &bob, "theirs", Some("feat/theirs"), now - 10 * DAY);

    // Before the forge was read, nothing says a branch did not merge.
    let before = read(store, &alice, now);
    let listed: Vec<&str> = before
        .stale
        .items
        .iter()
        .map(|i| i.work_id.as_str())
        .collect();
    assert_eq!(listed, [merged.as_str(), open.as_str(), loose.as_str()]);
    assert!(
        before
            .stale
            .items
            .iter()
            .all(|item| item.merged == MergeStatus::Unknown),
        "{:?}",
        before.stale.items
    );

    store
        .record_catchup_merge(
            &scope(),
            &NewCatchupMerge {
                source: CatchupMergeSource::ForgePull,
                source_id: "pull:7".into(),
                occurred_at: Some(now - 5 * DAY),
                title: Some("Merged task".into()),
                branch: "feat/merged".into(),
                base: "main".into(),
                head_commit: "a".repeat(40),
                merge_commit: "b".repeat(40),
                pull_number: 7,
                paths: vec!["src/work/merged.rs".into()],
                paths_complete: true,
                same_repository_head: true,
            },
            now - 5 * DAY,
        )
        .unwrap();
    store
        .record_catchup_source_status(
            &scope(),
            &CatchupSourceStatus {
                source: CatchupMergeSource::ForgePull,
                state: CatchupCoverageState::ObservedOnly,
                checked_at: now,
                unread_pages: 0,
                deferred: 0,
                window_from: Some(now - 20 * DAY),
            },
        )
        .unwrap();

    let digest = read(store, &alice, now);
    assert_eq!(digest.stale.quiet_days, QUIET_DAYS);
    assert!(digest.stale.complete);
    assert_eq!(digest.stale.total, 3, "{:?}", digest.stale.items);
    let found: Vec<(&str, MergeStatus)> = digest
        .stale
        .items
        .iter()
        .map(|item| (item.work_id.as_str(), item.merged))
        .collect();
    assert_eq!(
        found,
        [
            (merged.as_str(), MergeStatus::Observed),
            (open.as_str(), MergeStatus::None),
            // Work without a branch is never judged by a merge.
            (loose.as_str(), MergeStatus::Unknown),
        ]
    );
    let first = &digest.stale.items[0];
    assert_eq!(first.title, "Task merged");
    assert_eq!(first.branch.as_deref(), Some("feat/merged"));
    assert_eq!(first.quiet_since, now - 10 * DAY);
    assert!(first.can_complete, "its owner may end it");
    let text = digest::render_text(&digest);
    assert!(
        text.contains("a pull request from its branch merged"),
        "{text}"
    );
    assert!(
        text.contains("no pull request from its branch has merged"),
        "{text}"
    );

    // The read wrote nothing, and another person sees only their own stale Work.
    assert_eq!(read(store, &alice, now).stale, digest.stale);
    let theirs = read(store, &bob, now);
    assert_eq!(theirs.stale.total, 1);
    assert_eq!(theirs.stale.items[0].title, "Task theirs");

    for days in [0, digest::MAX_DAYS + 1] {
        assert!(matches!(
            digest::read(store, &alice.reader, &DigestQuery { days }, now),
            Err(HubError::Invalid(_))
        ));
    }
}

fn collisions_count_only_retained_history(db: &Db) {
    let store = &*db.store;
    let alice = person(store, 1, "alice");
    let bob = person(store, 2, "bob");
    let carol = person(store, 3, "carol");
    assert!(edit(store, &alice, "a").denied.is_empty());
    for session in ["b1", "b2", "b3"] {
        assert!(!edit(store, &bob, session).denied.is_empty(), "{session}");
    }
    let now = unix_now();
    let retained_from = store.events_after(0, 1).unwrap()[0].at;

    // The Hub's first event is inside the period: what came before is unknown, not zero.
    let digest = read(store, &alice, now);
    let collisions = &digest.collisions;
    assert!(!collisions.complete);
    assert_eq!(collisions.known_from, Some(retained_from));
    assert_eq!(collisions.limits, ["event_history_starts_in_period"]);
    assert_eq!((collisions.refused, collisions.repeated), (3, 0));
    assert_eq!(collisions.unattributed, 0);
    let hot = &collisions.hot_paths[0];
    assert_eq!((hot.path.as_str(), hot.refused), ("src/hub", 3));
    assert_eq!(hot.gap_samples, 3);
    assert!(
        hot.typical_gap_seconds
            .is_some_and(|gap| (0..60).contains(&gap))
    );
    assert_eq!(collisions.pairs.len(), 1, "{:?}", collisions.pairs);
    let pair = &collisions.pairs[0];
    assert_eq!(pair.owners, ["alice".to_owned(), "bob".to_owned()]);
    assert_eq!(
        (pair.path.as_str(), pair.collisions),
        (CONTESTED, PAIR_COLLISIONS)
    );
    assert!(pair.includes_you);
    assert!(!read(store, &carol, now).collisions.pairs[0].includes_you);
    let text = digest::render_text(&digest);
    assert!(text.contains("unknown, not zero"), "{text}");
    assert!(
        text.contains("alice and bob: src/hub/contested.rs, 3 times"),
        "{text}"
    );

    // A later period that the retained history covers whole, with nothing in it, is a known zero.
    let later = read(store, &carol, now + 10 * DAY);
    assert!(later.collisions.complete && later.collisions.limits.is_empty());
    assert_eq!(later.collisions.known_from, Some(later.since));
    assert_eq!(
        (later.collisions.refused, later.collisions.repeated),
        (0, 0)
    );
    assert!(later.is_empty(), "{later:?}");
    let text = digest::render_text(&later);
    assert!(
        text.contains("No refused or repeated edits are recorded."),
        "{text}"
    );
    assert!(!text.contains("unknown"), "{text}");
}

fn delivery_switches_are_per_repository_and_per_person(db: &Db) {
    let store = &*db.store;
    let alice = person(store, 1, "alice");
    let view = |who: &Person| {
        digest_http::settings(store, REPO, who.reader.repository_id, &who.reader.owner.id).unwrap()
    };
    assert!(
        !view(&alice).enabled && !view(&alice).delivering,
        "off by default"
    );
    digest_http::set_repository(store, REPO_ID, true, &alice.reader.owner.id).unwrap();
    assert!(view(&alice).delivering);
    digest_http::set_opted_out(store, &alice.reader.owner.id, REPO_ID, true).unwrap();
    let mine = view(&alice);
    assert!(mine.enabled && mine.opted_out && !mine.delivering);
    assert!(view(&person(store, 2, "bob")).delivering, "per person");
}

#[test]
fn sqlite_stale_work_says_whether_its_pull_request_merged() {
    stale_work_says_whether_its_pull_request_merged(&Db::sqlite());
}

#[test]
fn postgres_stale_work_says_whether_its_pull_request_merged() {
    let Some(db) = Db::postgres() else {
        return;
    };
    stale_work_says_whether_its_pull_request_merged(&db);
}

#[test]
fn sqlite_collisions_count_only_retained_history() {
    collisions_count_only_retained_history(&Db::sqlite());
}

#[test]
fn postgres_collisions_count_only_retained_history() {
    let Some(db) = Db::postgres() else {
        return;
    };
    collisions_count_only_retained_history(&db);
}

#[test]
fn sqlite_delivery_switches_are_per_repository_and_per_person() {
    delivery_switches_are_per_repository_and_per_person(&Db::sqlite());
}

#[test]
fn postgres_delivery_switches_are_per_repository_and_per_person() {
    let Some(db) = Db::postgres() else {
        return;
    };
    delivery_switches_are_per_repository_and_per_person(&db);
}

/// A Hub without a Slack app, where user 900 is a Hub admin.
async fn serve(db: &Db) -> String {
    let login = LoginConfig::from_lookup("", |name| {
        (name == "AGENTSCM_ADMINS").then(|| "github:#900".to_owned())
    })
    .unwrap();
    let objects = LocalBackend::open(db._dir.as_ref().unwrap().path().join("objects")).unwrap();
    let state =
        HubState::new(db.store.clone(), Arc::new(objects)).with_login(Login::new(login).unwrap());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, router(state)).await });
    base
}

async fn call(
    base: &str,
    method: reqwest::Method,
    path: &str,
    secret: &str,
    body: Option<Value>,
) -> (u16, Value) {
    let mut request = reqwest::Client::new()
        .request(method, format!("{base}{path}"))
        .bearer_auth(secret);
    if let Some(body) = body {
        request = request
            .header("content-type", "application/json")
            .body(body.to_string());
    }
    let response = request.send().await.unwrap();
    let status = response.status().as_u16();
    let bytes = response.bytes().await.unwrap();
    (status, serde_json::from_slice(&bytes).unwrap_or(Value::Null))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_route_reads_without_slack_and_only_an_admin_switches_delivery_on() {
    use reqwest::Method;
    const DIGEST: &str = "/v1/hub/standup/digest";
    const SETTINGS: &str = "/v1/hub/standup/digest/settings";
    let db = Db::sqlite();
    let alice = person(&*db.store, 1, "alice");
    let admin = person(&*db.store, 900, "root");
    work(&*db.store, &alice, "old", None, unix_now() - 5 * DAY);
    let base = serve(&db).await;
    let alice = alice.agent_secret.as_str();

    let (status, digest) = call(&base, Method::GET, DIGEST, alice, None).await;
    assert_eq!(status, 200, "{digest}");
    assert_eq!(digest["schema_version"], 1);
    assert_eq!(digest["repository"], REPO);
    assert_eq!(digest["days"], 7);
    assert_eq!(digest["stale"]["items"][0]["title"], "Task old");
    assert_eq!(digest["stale"]["items"][0]["merged"], "unknown");
    assert_eq!(digest["collisions"]["complete"], false);
    for query in ["?days=0", "?days=32", "?weeks=1"] {
        let path = format!("{DIGEST}{query}");
        assert_eq!(
            call(&base, Method::GET, &path, alice, None).await.0,
            400,
            "{query}"
        );
    }

    let (status, settings) = call(&base, Method::GET, SETTINGS, alice, None).await;
    assert_eq!(status, 200, "{settings}");
    assert_eq!(
        settings,
        json!({"repository": REPO, "enabled": false, "opted_out": false, "delivering": false})
    );
    // Without a forge role the Hub can confirm, a person who is not a Hub admin is refused.
    let on = Some(json!({"enabled": true}));
    let (status, refusal) = call(&base, Method::PUT, SETTINGS, alice, on.clone()).await;
    assert!((400..500).contains(&status), "{status}: {refusal}");
    let (_, settings) = call(&base, Method::GET, SETTINGS, alice, None).await;
    assert_eq!(settings["enabled"], false);

    let (status, settings) = call(&base, Method::PUT, SETTINGS, &admin.agent_secret, on).await;
    assert_eq!(status, 200, "{settings}");
    assert_eq!(settings["enabled"], true);
    let (_, settings) = call(&base, Method::GET, SETTINGS, alice, None).await;
    assert_eq!(settings["delivering"], true);
    let out = Some(json!({"opted_out": true}));
    let (status, settings) = call(&base, Method::PUT, &format!("{SETTINGS}/me"), alice, out).await;
    assert_eq!(status, 200, "{settings}");
    assert_eq!(
        settings,
        json!({"repository": REPO, "enabled": true, "opted_out": true, "delivering": false})
    );
    let unknown = Some(json!({"opted_out": true, "enabled": true}));
    let path = format!("{SETTINGS}/me");
    assert_eq!(call(&base, Method::PUT, &path, alice, unknown).await.0, 400);
}
