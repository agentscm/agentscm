//! Intents and notices: store cases run against SQLite always and PostgreSQL
//! when AGENTSCM_TEST_DATABASE_URL is set; HTTP cases run against SQLite.
#[path = "common/hub_pg.rs"]
mod hub_pg;

use std::sync::Arc;
use std::time::{Duration, Instant};

use agentscm::backend::LocalBackend;
use agentscm::hub::intent::{Announced, MAX_INTENT_TTL_SECONDS, NewIntent};
use agentscm::hub::session::SessionCall;
use agentscm::hub::store::{
    self, ForgeIdentity, HubEvent, Store, User, agent_token_digest, mint_agent_token, unix_now,
};
use agentscm::hub::{HubError, HubState, router};
use hub_pg::PgSchema;
use serde_json::{Value, json};
use tempfile::TempDir;

const TTL: i64 = 3600;
const REPO: &str = "acme/widgets";
const T0: i64 = 1000;

struct Db {
    url: String,
    _pg: Option<PgSchema>,
    _dir: Option<TempDir>,
}

impl Db {
    fn sqlite() -> Self {
        let dir = TempDir::new().unwrap();
        Self {
            url: format!("sqlite://{}", dir.path().join("hub.sqlite").display()),
            _pg: None,
            _dir: Some(dir),
        }
    }

    fn postgres() -> Option<Self> {
        let pg = PgSchema::create()?;
        Some(Self {
            url: pg.url.clone(),
            _pg: Some(pg),
            _dir: None,
        })
    }

    fn open(&self) -> Arc<dyn Store> {
        store::open(&self.url).unwrap()
    }
}

struct Agent {
    token: String,
    owner: User,
}

fn agent_in(store: &dyn Store, repo: &str, provider: &str, user_id: i64, name: &str) -> Agent {
    let host = format!("{provider}.com");
    let identity = ForgeIdentity {
        provider: provider.into(),
        host,
        user_id,
    };
    let owner = store
        .upsert_user(&identity, &format!("user{user_id}"))
        .unwrap();
    let (token, _) = mint_agent_token(store, &owner.id, name, repo, TTL).unwrap();
    Agent { token, owner }
}

fn agent(store: &dyn Store, user_id: i64, name: &str) -> Agent {
    agent_in(store, REPO, "github", user_id, name)
}

fn intent(n: u128, paths: &[&str]) -> NewIntent {
    NewIntent {
        id: format!("int_{n:032x}"),
        task: "refactor".into(),
        branch: Some("feat".into()),
        base: Some("main".into()),
        machine: Some("laptop".into()),
        paths: paths.iter().map(|path| (*path).to_owned()).collect(),
        ttl_seconds: 600,
    }
}

fn announce(store: &dyn Store, agent: &Agent, n: u128, paths: &[&str], now: i64) -> Announced {
    store
        .announce_intent(&agent.token, &intent(n, paths), now)
        .unwrap()
}

fn notices(store: &dyn Store, agent: &Agent) -> Vec<HubEvent> {
    store.notices_after(&agent.token, 0, 1000).unwrap()
}

fn data(event: &HubEvent) -> Value {
    serde_json::from_str(&event.data).unwrap()
}

fn intent_events(store: &dyn Store) -> Vec<(String, String)> {
    let events = store.events_after(0, 1000).unwrap();
    events
        .into_iter()
        .filter(|event| event.kind.starts_with("intent_"))
        .map(|event| (event.kind, event.subject))
        .collect()
}

fn id(n: u128) -> String {
    format!("int_{n:032x}")
}

fn attributed_overlap_and_notice(db: &Db) {
    let store = db.open();
    let alice = agent_in(&*store, REPO, "github", 1, "billing-agent");
    let bob = agent_in(&*store, REPO, "gitlab", 2, "docs-agent");
    let first = announce(&*store, &alice, 1, &["src/billing/**"], T0);
    assert!(first.overlaps.is_empty());
    let second = announce(&*store, &bob, 2, &["src/billing/invoice.rs"], T0);
    assert_eq!(second.overlaps.len(), 1);
    let overlap = &second.overlaps[0];
    assert_eq!(overlap.intent.id, id(1));
    assert_eq!(overlap.intent.agent, "billing-agent");
    assert_eq!(overlap.intent.owner.login, "user1");
    assert_eq!(overlap.intent.owner.identity.provider, "github");
    assert_eq!(overlap.intent.paths, ["src/billing/**"]);
    assert_eq!(overlap.their_paths, ["src/billing/**"]);
    assert_eq!(overlap.your_paths, ["src/billing/invoice.rs"]);
    assert_eq!(second.intent.expires_at, T0 + 600);

    let got = notices(&*store, &alice);
    assert_eq!(got.len(), 1);
    assert_eq!(got[0].kind, "intent_notice");
    assert_eq!(got[0].subject, id(1));
    let notice = data(&got[0]);
    assert_eq!(notice["about"], json!(id(2)));
    assert_eq!(notice["agent"], "docs-agent");
    assert_eq!(notice["owner"]["login"], "user2");
    assert_eq!(notice["owner"]["provider"], "gitlab");
    assert_eq!(notice["owner"]["id"], json!(bob.owner.id));
    assert_eq!(notice["repository"], REPO);
    assert_eq!(notice["their_paths"], json!(["src/billing/invoice.rs"]));
    assert_eq!(notice["your_paths"], json!(["src/billing/**"]));
    assert!(notices(&*store, &bob).is_empty());
    assert!(
        store
            .notices_after(&alice.token, got[0].seq, 10)
            .unwrap()
            .is_empty()
    );
}

fn overlap_rules(db: &Db) {
    let store = db.open();
    let a = agent(&*store, 1, "a");
    let b = agent(&*store, 2, "b");
    let table = [
        ("src/a.rs", "src/a.rs", true),
        ("src/a.rs", "src/a.rs.bak", false),
        ("src/**", "srcx/a", false),
        ("src/**", "src/x/y", true),
        ("src/a/**", "src/**", true),
        ("src", "src/**", false),
        ("a/b/**", "a/bc/d", false),
    ];
    for (row, (first, second, expected)) in table.iter().enumerate() {
        // A distinct prefix per row keeps rows independent in one repository.
        let (first, second) = (format!("t{row}/{first}"), format!("t{row}/{second}"));
        let n = row as u128 * 2 + 1;
        announce(&*store, &a, n, &[&first], T0);
        let got = announce(&*store, &b, n + 1, &[&second], T0);
        assert_eq!(!got.overlaps.is_empty(), *expected, "{first} {second}");
    }
    let expected = table.iter().filter(|row| row.2).count();
    assert_eq!(notices(&*store, &a).len(), expected);
    assert!(notices(&*store, &b).is_empty());
}

fn invalid_requests_are_rejected_without_events(db: &Db) {
    let store = db.open();
    let a = agent(&*store, 1, "a");
    let control = "a\u{1}b";
    for spec in [
        "../x",
        "a/../b",
        "/etc/passwd",
        "a//b",
        "a/",
        "",
        "a/./b",
        "a\\b",
        "C:/x",
        "a/**/b",
        "*.rs",
        "**",
        "a/*",
        control,
        "a\0b",
    ] {
        let result = store.announce_intent(&a.token, &intent(1, &[spec]), T0);
        assert!(matches!(result, Err(HubError::Invalid(_))), "{spec:?}");
    }
    let many: Vec<String> = (0..257).map(|i| format!("p{i}")).collect();
    let mut cases = vec![
        NewIntent {
            paths: vec![],
            ..intent(1, &["a"])
        },
        NewIntent {
            paths: many,
            ..intent(1, &["a"])
        },
        NewIntent {
            ttl_seconds: 0,
            ..intent(1, &["a"])
        },
        NewIntent {
            ttl_seconds: MAX_INTENT_TTL_SECONDS + 1,
            ..intent(1, &["a"])
        },
        NewIntent {
            id: "int_nothex".into(),
            ..intent(1, &["a"])
        },
        NewIntent {
            task: String::new(),
            ..intent(1, &["a"])
        },
    ];
    cases.push(NewIntent {
        id: id(1).to_uppercase(),
        ..intent(1, &["a"])
    });
    for case in cases {
        let result = store.announce_intent(&a.token, &case, T0);
        assert!(matches!(result, Err(HubError::Invalid(_))), "{case:?}");
    }
    assert!(matches!(
        store.renew_intent(&a.token, &id(1), 0, T0),
        Err(HubError::Invalid(_))
    ));
    assert!(intent_events(&*store).is_empty());
    assert!(store.live_intents(REPO, T0, 10).unwrap().is_empty());
    // The maximum is accepted.
    let max = NewIntent {
        ttl_seconds: MAX_INTENT_TTL_SECONDS,
        ..intent(1, &["a"])
    };
    let got = store.announce_intent(&a.token, &max, T0).unwrap();
    assert_eq!(got.intent.expires_at, T0 + MAX_INTENT_TTL_SECONDS);
}

fn server_clock_expiry_and_renew(db: &Db) {
    let store = db.open();
    let a = agent(&*store, 1, "a");
    let b = agent(&*store, 2, "b");
    let short = NewIntent {
        ttl_seconds: 60,
        ..intent(1, &["x/f.rs"])
    };
    store.announce_intent(&a.token, &short, T0).unwrap();
    let probe = |n: u128, now: i64| {
        let probe = NewIntent {
            ttl_seconds: 1,
            ..intent(n, &["x/f.rs"])
        };
        store
            .announce_intent(&b.token, &probe, now)
            .unwrap()
            .overlaps
    };
    assert_eq!(probe(2, T0 + 59).len(), 1);
    assert!(probe(3, T0 + 60).is_empty());
    assert!(
        store
            .renew_intent(&a.token, &id(1), 100, T0 + 61)
            .unwrap()
            .is_none()
    );
    assert!(store.live_intents(REPO, T0 + 61, 10).unwrap().is_empty());

    let second = NewIntent {
        ttl_seconds: 60,
        ..intent(4, &["y/f.rs"])
    };
    store.announce_intent(&a.token, &second, T0).unwrap();
    let renewed = store
        .renew_intent(&a.token, &id(4), 100, T0 + 30)
        .unwrap()
        .unwrap();
    assert_eq!(renewed.expires_at, T0 + 130);
    let probe = NewIntent {
        ttl_seconds: 1,
        ..intent(5, &["y/f.rs"])
    };
    let seen = store.announce_intent(&b.token, &probe, T0 + 100).unwrap();
    assert_eq!(seen.overlaps.len(), 1);
    assert_eq!(seen.overlaps[0].intent.expires_at, T0 + 130);
    let live = store.live_intents(REPO, T0 + 100, 10).unwrap();
    assert!(live.iter().any(|intent| intent.id == id(4)));
    assert!(store.live_intents(REPO, T0 + 130, 10).unwrap().is_empty());
}

fn release_stops_notices(db: &Db) {
    let store = db.open();
    let a = agent(&*store, 1, "a");
    let b = agent(&*store, 2, "b");
    announce(&*store, &a, 1, &["r/**"], T0);
    // Another token can neither renew nor release it.
    assert!(
        store
            .renew_intent(&b.token, &id(1), 60, T0 + 1)
            .unwrap()
            .is_none()
    );
    assert!(!store.release_intent(&b.token, &id(1), T0 + 1).unwrap());
    assert_eq!(store.live_intents(REPO, T0 + 1, 10).unwrap().len(), 1);
    assert!(store.release_intent(&a.token, &id(1), T0 + 10).unwrap());
    assert!(!store.release_intent(&a.token, &id(1), T0 + 11).unwrap());
    assert!(!store.release_intent(&a.token, &id(99), T0 + 11).unwrap());
    assert!(
        store
            .renew_intent(&a.token, &id(1), 60, T0 + 12)
            .unwrap()
            .is_none()
    );
    assert!(store.live_intents(REPO, T0 + 12, 10).unwrap().is_empty());
    let got = announce(&*store, &b, 2, &["r/f"], T0 + 20);
    assert!(got.overlaps.is_empty());
    assert!(notices(&*store, &a).is_empty());
    let result = store.announce_intent(&a.token, &intent(1, &["r/**"]), T0 + 30);
    assert!(matches!(result, Err(HubError::Invalid(_))));
}

fn repository_scope(db: &Db) {
    let store = db.open();
    let a = agent(&*store, 1, "a");
    let x = agent_in(&*store, "acme/other", "github", 2, "x");
    let c = agent(&*store, 3, "c");
    announce(&*store, &a, 1, &["a/**"], T0);
    let got = announce(&*store, &x, 2, &["a/f"], T0);
    assert!(got.overlaps.is_empty());
    assert!(notices(&*store, &a).is_empty());
    assert!(notices(&*store, &x).is_empty());
    let widgets = store.live_intents(REPO, T0, 10).unwrap();
    assert_eq!(widgets.iter().map(|i| &i.id).collect::<Vec<_>>(), [&id(1)]);
    let other = store.live_intents("acme/other", T0, 10).unwrap();
    assert_eq!(other.iter().map(|i| &i.id).collect::<Vec<_>>(), [&id(2)]);
    // An id held by another token is refused, in the same repository or another.
    for taker in [&x, &c] {
        let result = store.announce_intent(&taker.token, &intent(1, &["z"]), T0);
        assert!(matches!(result, Err(HubError::Invalid(_))));
    }
    assert_eq!(store.live_intents(REPO, T0, 10).unwrap().len(), 1);
}

fn idempotent_and_incremental(db: &Db) {
    let store = db.open();
    let a = agent(&*store, 1, "a");
    let b = agent(&*store, 2, "b");
    let c = agent(&*store, 3, "c");
    announce(&*store, &a, 1, &["s/**"], T0);
    announce(&*store, &c, 3, &["t/**"], T0);
    let first = announce(&*store, &b, 2, &["s/f"], T0);
    let events = store.events_after(0, 1000).unwrap().len();
    let again = announce(&*store, &b, 2, &["s/f"], T0);
    assert_eq!(again, first);
    assert_eq!(store.events_after(0, 1000).unwrap().len(), events);
    assert_eq!(
        (notices(&*store, &a).len(), notices(&*store, &c).len()),
        (1, 0)
    );

    // Adding a path notifies only the intent that overlaps the added path.
    let wider = announce(&*store, &b, 2, &["s/f", "t/g"], T0);
    assert_eq!(wider.overlaps.len(), 2);
    assert_eq!(
        (notices(&*store, &a).len(), notices(&*store, &c).len()),
        (1, 1)
    );
    let got = notices(&*store, &c);
    assert_eq!(data(&got[0])["their_paths"], json!(["t/g"]));
    // Dropping a path notifies nobody.
    announce(&*store, &b, 2, &["s/f"], T0);
    assert_eq!(
        (notices(&*store, &a).len(), notices(&*store, &c).len()),
        (1, 1)
    );
}

fn events_are_appended_once(db: &Db) {
    let store = db.open();
    let a = agent(&*store, 1, "a");
    let b = agent(&*store, 2, "b");
    announce(&*store, &a, 1, &["e/**"], T0);
    announce(&*store, &b, 2, &["e/f"], T0);
    store
        .renew_intent(&a.token, &id(1), 900, T0 + 5)
        .unwrap()
        .unwrap();
    // The same announce with a new lifetime only moves the expiry.
    let same = NewIntent {
        ttl_seconds: 700,
        ..intent(2, &["e/f"])
    };
    store.announce_intent(&b.token, &same, T0 + 6).unwrap();
    assert!(store.release_intent(&a.token, &id(1), T0 + 7).unwrap());
    let kinds: Vec<(&str, String)> = vec![
        ("intent_announced", id(1)),
        ("intent_announced", id(2)),
        ("intent_notice", id(1)),
        ("intent_renewed", id(1)),
        ("intent_renewed", id(2)),
        ("intent_released", id(1)),
    ];
    let got = intent_events(&*store);
    let got: Vec<(&str, String)> = got.iter().map(|(k, s)| (k.as_str(), s.clone())).collect();
    assert_eq!(got, kinds);
    for event in store.events_after(0, 1000).unwrap() {
        assert!(event.id.starts_with("evt_"), "{}", event.id);
        let data = data(&event);
        let text = event.data.to_lowercase();
        assert!(
            !text.contains("digest") && !text.contains("secret"),
            "{text}"
        );
        if event.kind == "intent_announced" {
            assert_eq!(data["repository"], REPO);
            assert!(data["paths"].is_array());
        }
    }
}

fn inactive_tokens_are_refused(db: &Db) {
    let store = db.open();
    let revoked = agent(&*store, 1, "revoked");
    let expired = agent(&*store, 2, "expired");
    let disabled = agent(&*store, 3, "disabled");
    for (n, who) in [(1, &revoked), (2, &expired), (3, &disabled)] {
        announce(&*store, who, n, &[&format!("u{n}/f")], T0);
    }
    let before = intent_events(&*store);
    store.revoke_agent_token(&revoked.token).unwrap();
    store.set_user_disabled(&disabled.owner.id, true).unwrap();
    let later = unix_now() + TTL + 10;
    for (n, who, now) in [
        (1, &revoked, T0 + 1),
        (2, &expired, later),
        (3, &disabled, T0 + 1),
    ] {
        let result = store.announce_intent(&who.token, &intent(n + 10, &["v"]), now);
        assert!(matches!(result, Err(HubError::Unauthorized)), "{n}");
        let result = store.renew_intent(&who.token, &id(n), 60, now);
        assert!(matches!(result, Err(HubError::Unauthorized)), "{n}");
        let result = store.release_intent(&who.token, &id(n), now);
        assert!(matches!(result, Err(HubError::Unauthorized)), "{n}");
    }
    assert_eq!(intent_events(&*store), before);
}

const REPEATS: u128 = 5;

/// The fastest of `REPEATS` timed announces over `n` held intents of 50 paths each: a 50-path
/// announce overlapping five of them, and a broad one overlapping all of them. Load only adds
/// time, so the minimum is the least noisy estimate of the work.
fn announce_costs(store: &dyn Store, n: u128) -> (Duration, Duration) {
    let a = agent(store, n as i64, &format!("a{n}"));
    let root = format!("app{n}");
    let base = n * 100;
    let long = NewIntent {
        ttl_seconds: TTL,
        ..intent(0, &["x"])
    };
    for i in 0..n {
        let mut paths: Vec<String> = (0..45).map(|j| format!("{root}/svc{i}/f{j}.rs")).collect();
        paths.extend((0..5).map(|j| format!("{root}/svc{i}/d{j}/**")));
        let new = NewIntent {
            id: id(base + i),
            paths,
            ..long.clone()
        };
        store.announce_intent(&a.token, &new, T0).unwrap();
    }
    let mut few = Duration::MAX;
    for r in 0..REPEATS {
        let mut paths: Vec<String> = (5 * r + 1..=5 * r + 5)
            .flat_map(|k| {
                [
                    format!("{root}/svc{k}/f0.rs"),
                    format!("{root}/svc{k}/d0/y.rs"),
                ]
            })
            .collect();
        paths.extend((0..40).map(|j| format!("elsewhere{n}/r{r}/n{j}.rs")));
        assert_eq!(paths.len(), 50);
        let new = NewIntent {
            id: id(base + n + r),
            paths,
            ..long.clone()
        };
        let started = Instant::now();
        let announced = store.announce_intent(&a.token, &new, T0).unwrap();
        few = few.min(started.elapsed());
        assert_eq!(announced.overlaps.len(), 5);
        assert!(announced.overlaps.iter().all(|o| o.their_paths.len() == 2));
    }
    let mut broad = Duration::MAX;
    for r in 0..REPEATS {
        let new = NewIntent {
            id: id(base + n + REPEATS + r),
            paths: vec![format!("{root}/**")],
            ..long.clone()
        };
        let started = Instant::now();
        let announced = store.announce_intent(&a.token, &new, T0).unwrap();
        broad = broad.min(started.elapsed());
        // The setup intents, the 50-path ones and the earlier broad ones.
        assert_eq!(announced.overlaps.len() as u128, n + REPEATS + r);
    }
    // Every broad announce notifies the holder of each overlapped intent.
    assert!(notices(store, &a).len() as u128 >= n.min(1000));
    (few, broad)
}

/// Announce must stay at most linear in held intents. Comparing 10N with N in one run cancels
/// host speed and load: linear work grows about 10x and quadratic 100x, so 30x (their
/// geometric midpoint) separates them without a wall-clock budget.
fn scale(db: &Db) {
    let store = db.open();
    let (few_small, broad_small) = announce_costs(&*store, 100);
    let (few_large, broad_large) = announce_costs(&*store, 1000);
    for (name, small, large) in [
        ("50-path", few_small, few_large),
        ("broad", broad_small, broad_large),
    ] {
        let ratio = large.as_secs_f64() / small.as_secs_f64();
        eprintln!(
            "{name} announce: {small:?} over 100 intents, {large:?} over 1,000 ({ratio:.1}x)"
        );
        assert!(
            ratio < 30.0,
            "{name} announce grew {ratio:.1}x for 10x intents"
        );
    }
}

macro_rules! conformance {
    ($($case:ident),* $(,)?) => {
        mod on_sqlite {
            $(#[test] fn $case() { super::$case(&super::Db::sqlite()); })*
        }
        mod on_postgres {
            $(#[test] fn $case() {
                if let Some(db) = super::Db::postgres() { super::$case(&db); }
            })*
        }
    };
}

/// Keys that sort between `a/b/` and `a/b0` only in byte order; Postgres needs COLLATE "C".
fn byte_order_matching(db: &Db) {
    let store = db.open();
    let alice = agent(&*store, 1, "a");
    let bob = agent(&*store, 2, "b");
    let keys = ["a/B/x", "a/b-x", "a/b.rs", "a/b_c", "a/b0", "a/b/c"];
    for (n, key) in keys.iter().enumerate() {
        announce(&*store, &alice, 10 + n as u128, &[key], T0);
    }
    let overlapping = |n: u128, path: &str| -> Vec<String> {
        announce(&*store, &bob, n, &[path], T0)
            .overlaps
            .into_iter()
            .map(|overlap| overlap.intent.id)
            .collect()
    };
    assert_eq!(overlapping(20, "a/b/**"), [id(15)]);
    assert_eq!(overlapping(21, "a/B/**"), [id(10)]);
    assert_eq!(overlapping(22, "a/b0"), [id(14)]);
}

fn reannounce_after_expiry_notices_again(db: &Db) {
    let store = db.open();
    let alice = agent(&*store, 1, "a");
    let bob = agent(&*store, 2, "b");
    announce(&*store, &alice, 1, &["src/a.rs"], T0);
    announce(&*store, &bob, 2, &["src/a.rs"], T0);
    assert_eq!(notices(&*store, &alice).len(), 1);
    store
        .renew_intent(&alice.token, &id(1), 600, T0 + 500)
        .unwrap()
        .unwrap();
    // Bob's intent expired at T0 + 600, so coming back is new work again.
    announce(&*store, &bob, 2, &["src/a.rs"], T0 + 700);
    assert_eq!(notices(&*store, &alice).len(), 2);
    let mut retask = intent(2, &["src/a.rs"]);
    retask.task = "another task".into();
    store
        .announce_intent(&bob.token, &retask, T0 + 701)
        .unwrap();
    assert_eq!(
        notices(&*store, &alice).len(),
        2,
        "a task-only change adds no path"
    );
}

/// Two hub processes on one database: racing overlapping announces see each other.
fn racing_announces_see_each_other(db: &Db) {
    let (first, second) = (db.open(), db.open());
    let alice = agent(&*first, 1, "a");
    let bob = agent(&*first, 2, "b");
    for round in 0..20_u128 {
        let path = format!("src/{round}.rs");
        let barrier = std::sync::Barrier::new(2);
        let seen: usize = std::thread::scope(|scope| {
            let a = scope.spawn(|| {
                barrier.wait();
                announce(&*first, &alice, 100 + round, &[&path], T0)
                    .overlaps
                    .len()
            });
            let b = scope.spawn(|| {
                barrier.wait();
                announce(&*second, &bob, 200 + round, &[&path], T0)
                    .overlaps
                    .len()
            });
            a.join().unwrap() + b.join().unwrap()
        });
        assert_eq!(seen, 1, "round {round}");
    }
}

conformance!(
    attributed_overlap_and_notice,
    overlap_rules,
    invalid_requests_are_rejected_without_events,
    server_clock_expiry_and_renew,
    release_stops_notices,
    repository_scope,
    idempotent_and_incremental,
    events_are_appended_once,
    inactive_tokens_are_refused,
    scale,
    byte_order_matching,
    reannounce_after_expiry_notices_again,
    racing_announces_see_each_other,
);

struct Hub {
    base: String,
    store: Arc<dyn Store>,
    client: reqwest::Client,
    _dir: TempDir,
}

impl Hub {
    async fn start() -> Self {
        let dir = TempDir::new().unwrap();
        let url = format!("sqlite://{}", dir.path().join("hub.sqlite").display());
        let store = store::open(&url).unwrap();
        let objects = LocalBackend::open(dir.path().join("objects")).unwrap();
        let app = router(HubState::new(store.clone(), Arc::new(objects)));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await });
        Self {
            base: format!("http://{address}"),
            store,
            client: reqwest::Client::new(),
            _dir: dir,
        }
    }

    /// A bearer secret for a fresh user's token named `name`.
    fn secret(&self, user_id: i64, name: &str) -> String {
        let identity = ForgeIdentity {
            provider: "github".into(),
            host: "github.com".into(),
            user_id,
        };
        let user = self
            .store
            .upsert_user(&identity, &format!("user{user_id}"))
            .unwrap();
        let (_, secret) = mint_agent_token(&*self.store, &user.id, name, REPO, TTL).unwrap();
        secret.expose().to_owned()
    }

    async fn call(
        &self,
        method: reqwest::Method,
        path: &str,
        secret: Option<&str>,
        body: Option<&str>,
    ) -> (u16, Value) {
        let mut request = self
            .client
            .request(method, format!("{}{path}", self.base))
            .header("content-type", "application/json");
        if let Some(secret) = secret {
            request = request.header("authorization", format!("Bearer {secret}"));
        }
        if let Some(body) = body {
            request = request.body(body.to_owned());
        }
        let response = request.send().await.unwrap();
        let status = response.status().as_u16();
        (
            status,
            serde_json::from_str(&response.text().await.unwrap()).unwrap(),
        )
    }
}

fn announce_body(n: u128, paths: &[&str]) -> String {
    json!({"intent_id": id(n), "task": "t", "paths": paths, "ttl_s": 600}).to_string()
}

const INTENTS: &str = "/v1/hub/repos/acme%2Fwidgets/intents";
const NOTICES: &str = "/v1/hub/repos/acme%2Fwidgets/notices";
const ACTIVITY: &str = "/v1/hub/repos/acme%2Fwidgets/activity";

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn long_poll_delivers_a_notice_and_overlap_is_not_a_denial() {
    let hub = Hub::start().await;
    let alice = hub.secret(1, "billing-agent");
    let bob = hub.secret(2, "docs-agent");
    let post = reqwest::Method::POST;
    let (status, _) = hub
        .call(
            post.clone(),
            INTENTS,
            Some(&alice),
            Some(&announce_body(1, &["src/**"])),
        )
        .await;
    assert_eq!(status, 200);

    let url = format!("{NOTICES}?wait=20");
    let started = Instant::now();
    let waiting = {
        let (hub_base, client, alice) = (hub.base.clone(), hub.client.clone(), alice.clone());
        tokio::spawn(async move {
            let response = client
                .get(format!("{hub_base}{url}"))
                .header("authorization", format!("Bearer {alice}"))
                .send()
                .await
                .unwrap();
            serde_json::from_str::<Value>(&response.text().await.unwrap()).unwrap()
        })
    };
    tokio::time::sleep(Duration::from_millis(300)).await;
    let (status, body) = hub
        .call(
            post,
            INTENTS,
            Some(&bob),
            Some(&announce_body(2, &["src/a.rs"])),
        )
        .await;
    assert_eq!(status, 200);
    let overlap = &body["overlaps"][0];
    assert_eq!(overlap["intent"]["id"], json!(id(1)));
    assert_eq!(overlap["intent"]["agent"]["name"], "billing-agent");
    assert_eq!(overlap["intent"]["owner"]["login"], "user1");
    assert_eq!(overlap["their_paths"], json!(["src/**"]));
    assert_eq!(overlap["your_paths"], json!(["src/a.rs"]));
    assert_eq!(body["intent"]["agent"]["name"], "docs-agent");

    let reply = waiting.await.unwrap();
    assert!(
        // Half the 20 s wait: the announce woke the poll rather than its deadline ending it.
        started.elapsed() < Duration::from_secs(10),
        "{:?}",
        started.elapsed()
    );
    let notices = reply["notices"].as_array().unwrap();
    assert_eq!(notices.len(), 1);
    assert_eq!(notices[0]["intent"], json!(id(1)));
    assert_eq!(notices[0]["notice"]["agent"], "docs-agent");
    assert_eq!(notices[0]["notice"]["about"], json!(id(2)));
    assert_eq!(reply["cursor"], notices[0]["seq"]);

    // Nothing newer: an immediate poll is empty and keeps the cursor.
    let after = reply["cursor"].as_i64().unwrap();
    let (status, empty) = hub
        .call(
            reqwest::Method::GET,
            &format!("{NOTICES}?after={after}"),
            Some(&alice),
            None,
        )
        .await;
    assert_eq!(status, 200);
    assert_eq!(empty, json!({"notices": [], "cursor": after}));
    // A wait with nothing to deliver ends at its deadline.
    let started = Instant::now();
    let url = format!("{NOTICES}?after={after}&wait=1");
    let (status, empty) = hub
        .call(reqwest::Method::GET, &url, Some(&alice), None)
        .await;
    assert_eq!(
        (status, empty["notices"].as_array().unwrap().len()),
        (200, 0)
    );
    assert!(started.elapsed() >= Duration::from_millis(900));
    // The other holder has no notices.
    let (_, none) = hub
        .call(reqwest::Method::GET, NOTICES, Some(&bob), None)
        .await;
    assert!(none["notices"].as_array().unwrap().is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn requests_are_authenticated_scoped_and_validated() {
    let hub = Hub::start().await;
    let alice = hub.secret(1, "a");
    let (get, post, delete) = (
        reqwest::Method::GET,
        reqwest::Method::POST,
        reqwest::Method::DELETE,
    );
    let valid = announce_body(1, &["a/b.rs"]);
    let (status, body) = hub.call(post.clone(), INTENTS, None, Some(&valid)).await;
    assert_eq!((status, body), (401, json!({"error": "unauthorized"})));
    assert_eq!(hub.call(get.clone(), NOTICES, None, None).await.0, 401);
    assert_eq!(
        hub.call(get.clone(), INTENTS, Some("bogus"), None).await.0,
        401
    );

    // Other repositories, and anything that is not one encoded segment, are 404.
    for path in [
        "/v1/hub/repos/acme%2Fother/intents",
        "/v1/hub/repos/acme%2Fother/notices",
        "/v1/hub/repos/acme/intents",
        "/v1/hub/repos/acme/widgets/intents",
        "/v1/hub/repos/acme%2F..%2Fwidgets/intents",
    ] {
        let (status, body) = hub.call(get.clone(), path, Some(&alice), None).await;
        assert_eq!(
            (status, body),
            (404, json!({"error": "not_found"})),
            "{path}"
        );
    }
    let other = "/v1/hub/repos/acme%2Fother/intents";
    assert_eq!(
        hub.call(post.clone(), other, Some(&alice), Some(&valid))
            .await
            .0,
        404
    );

    let bad_bodies = [
        json!({"intent_id": id(1), "task": "t", "paths": ["a"], "ttl_s": 60, "extra": 1}),
        json!({"intent_id": id(1), "task": "t", "paths": ["a/**/b"], "ttl_s": 60}),
        json!({"intent_id": id(1), "task": "t", "paths": [], "ttl_s": 60}),
        json!({"intent_id": id(1), "task": "t", "paths": ["a"], "ttl_s": 0}),
        json!({"intent_id": "nope", "task": "t", "paths": ["a"], "ttl_s": 60}),
        json!({"task": "t", "paths": ["a"], "ttl_s": 60}),
    ];
    for body in bad_bodies {
        let (status, reply) = hub
            .call(post.clone(), INTENTS, Some(&alice), Some(&body.to_string()))
            .await;
        assert_eq!(
            (status, reply),
            (400, json!({"error": "invalid_request"})),
            "{body}"
        );
    }
    let (status, _) = hub
        .call(post.clone(), INTENTS, Some(&alice), Some("not json"))
        .await;
    assert_eq!(status, 400);
    for query in ["wait=abc", "wait=26", "after=-1", "after=x", "foo=1"] {
        let url = format!("{NOTICES}?{query}");
        let (status, reply) = hub.call(get.clone(), &url, Some(&alice), None).await;
        assert_eq!(
            (status, reply),
            (400, json!({"error": "invalid_request"})),
            "{query}"
        );
    }
    assert!(
        hub.store
            .live_intents(REPO, unix_now(), 10)
            .unwrap()
            .is_empty()
    );

    let (status, body) = hub
        .call(post.clone(), INTENTS, Some(&alice), Some(&valid))
        .await;
    assert_eq!(status, 200);
    assert_eq!(body["overlaps"], json!([]));
    let (_, listed) = hub.call(get.clone(), INTENTS, Some(&alice), None).await;
    assert_eq!(listed["truncated"], false);
    assert_eq!(listed["intents"][0]["paths"], json!(["a/b.rs"]));

    let renew = format!("{INTENTS}/{}/renew", id(1));
    let (status, body) = hub
        .call(post.clone(), &renew, Some(&alice), Some(r#"{"ttl_s":900}"#))
        .await;
    assert_eq!(status, 200);
    assert!(body["intent"]["expires_at"].as_i64().unwrap() >= unix_now() + 890);
    let unknown = format!("{INTENTS}/{}/renew", id(9));
    let (status, _) = hub
        .call(
            post.clone(),
            &unknown,
            Some(&alice),
            Some(r#"{"ttl_s":900}"#),
        )
        .await;
    assert_eq!(status, 404);
    let (status, _) = hub
        .call(post.clone(), &renew, Some(&alice), Some(r#"{"ttl_s":0}"#))
        .await;
    assert_eq!(status, 400);

    let release = format!("{INTENTS}/{}", id(1));
    let (status, body) = hub.call(delete.clone(), &release, Some(&alice), None).await;
    assert_eq!((status, body), (200, json!({"released": true})));
    let (status, body) = hub.call(delete.clone(), &release, Some(&alice), None).await;
    assert_eq!((status, body), (200, json!({"released": false})));
    let gone = format!("{INTENTS}/{}", id(9));
    let (_, body) = hub.call(delete, &gone, Some(&alice), None).await;
    assert_eq!(body, json!({"released": false}));
    let (_, listed) = hub.call(get, INTENTS, Some(&alice), None).await;
    assert_eq!(listed["intents"], json!([]));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn activity_is_authenticated_scoped_and_shaped() {
    let hub = Hub::start().await;
    let alice = hub.secret(1, "a");
    let get = reqwest::Method::GET;
    assert_eq!(
        hub.call(get.clone(), ACTIVITY, None, None).await,
        (401, json!({"error": "unauthorized"}))
    );
    let other = "/v1/hub/repos/acme%2Fother/activity";
    assert_eq!(
        hub.call(get.clone(), other, Some(&alice), None).await,
        (404, json!({"error": "not_found"}))
    );
    for query in ["limit=0", "limit=101", "limit=-1", "limit=x", "foo=1"] {
        let url = format!("{ACTIVITY}?{query}");
        let (status, reply) = hub.call(get.clone(), &url, Some(&alice), None).await;
        assert_eq!(
            (status, reply),
            (400, json!({"error": "invalid_request"})),
            "{query}"
        );
    }
    let (status, empty) = hub.call(get.clone(), ACTIVITY, Some(&alice), None).await;
    assert_eq!(
        (status, empty),
        (200, json!({"sessions": [], "events": []}))
    );

    let digest = agent_token_digest(&alice).unwrap();
    let token = hub
        .store
        .authenticate(&digest, unix_now())
        .unwrap()
        .unwrap();
    let call = SessionCall {
        session: Some("secret-session-name".into()),
        via: "mcp".into(),
        task: Some("refactor".into()),
        paths: vec!["src/a.rs".into()],
        deny: false,
        report: true,
    };
    hub.store
        .touch_session(&token.token_id, &call, unix_now())
        .unwrap();
    let url = format!("{ACTIVITY}?limit=1");
    let (status, body) = hub.call(get, &url, Some(&alice), None).await;
    assert_eq!(status, 200);
    let sessions = body["sessions"].as_array().unwrap();
    assert_eq!(sessions.len(), 1);
    let session = &sessions[0];
    assert_eq!(
        session["agent"],
        json!({"token": token.token_id, "name": "a"})
    );
    assert_eq!(session["owner"], json!({"login": "user1"}));
    assert_eq!(session["via"], json!(["mcp"]));
    assert_eq!(session["task"], "refactor");
    assert!(session["intent"].as_str().unwrap().starts_with("int_"));
    assert!(!body.to_string().contains("secret-session-name"));
    let events = body["events"].as_array().unwrap();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0]["agent"], session["agent"]);
    assert!(events[0]["data"].is_object() && events[0]["seq"].is_i64());
}
