//! Contested-edit outcomes and hook overhead: store cases run against SQLite always and
//! PostgreSQL when AGENTSCM_TEST_DATABASE_URL is set.
use crate::hub_pg;

use std::sync::Arc;

use agentscm::hub::hook_overhead::{ClientReport, MAX_ROWS_PER_TOKEN, RETAIN_SECONDS, Sample};
use agentscm::hub::session::{SESSION_TTL_SECONDS, SessionCall};
use agentscm::hub::store::{self, ForgeIdentity, Store, mint_agent_token};
use agentscm::hub::value_metrics::{ValueMetrics, ValueMetricsQuery};
use hub_pg::PgSchema;
use tempfile::TempDir;

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

fn agent(store: &dyn Store, user_id: i64, name: &str) -> String {
    agent_with_secret(store, user_id, name).0
}

/// The token id and the bearer secret of a fresh agent token.
fn agent_with_secret(store: &dyn Store, user_id: i64, name: &str) -> (String, String) {
    let identity = ForgeIdentity {
        provider: "github".into(),
        host: "github.com".into(),
        user_id,
    };
    let owner = store
        .upsert_user(&identity, &format!("user{user_id}"))
        .unwrap();
    let (token, secret) = mint_agent_token(store, &owner.id, name, REPO, 3600).unwrap();
    (token, secret.expose().to_owned())
}

fn call(via: &str, session: Option<&str>, paths: &[&str], deny: bool) -> SessionCall {
    SessionCall {
        session: session.map(str::to_owned),
        via: via.into(),
        task: None,
        paths: paths.iter().map(|path| (*path).to_owned()).collect(),
        deny,
        report: true,
    }
}

/// An MCP touch: never refused, told about overlaps.
fn hold(store: &dyn Store, token: &str, paths: &[&str], now: i64) {
    store
        .touch_session(token, &call("mcp", None, paths, false), now)
        .unwrap();
}

/// A hook call before an edit, which may be refused.
fn edit(store: &dyn Store, token: &str, session: &str, paths: &[&str], now: i64) -> bool {
    !store
        .touch_session(token, &call("hook:claude", Some(session), paths, true), now)
        .unwrap()
        .denied
        .is_empty()
}

fn metrics(store: &dyn Store, token: &str, now: i64) -> ValueMetrics {
    let query = ValueMetricsQuery {
        since: Some(0),
        ..Default::default()
    };
    store.value_metrics(token, &query, now).unwrap()
}

fn refusal_outcomes_follow_what_the_session_did_next(db: &Db) {
    let store = db.open();
    let alice = agent(&*store, 1, "a");
    let (bob, dave, erin, frank) = (
        agent(&*store, 2, "b"),
        agent(&*store, 4, "d"),
        agent(&*store, 5, "e"),
        agent(&*store, 6, "f"),
    );
    hold(&*store, &alice, &["a.rs", "b.rs", "c.rs", "d.rs"], T0);
    for (who, file) in [
        (&bob, "a.rs"),
        (&dave, "b.rs"),
        (&erin, "c.rs"),
        (&frank, "d.rs"),
    ] {
        assert!(edit(&*store, who, "s", &[file], T0 + 1), "{file} refused");
    }
    // Bob edits again while the hold stands: pushed through.
    assert!(!edit(&*store, &bob, "s", &["a.rs"], T0 + 2));
    // Dave asks for the file himself (the Hub's own ask after the refusal does not count),
    // and Alice declines: asked, not handed off.
    store
        .ask_handoff(&dave, Some("s"), "b.rs", None, T0 + 3)
        .unwrap();
    store
        .answer_handoff(&alice, None, "b.rs", false, None, T0 + 3)
        .unwrap();
    // Alice grants erin the Hub's request: handed off.
    store
        .answer_handoff(&alice, None, "c.rs", true, None, T0 + 4)
        .unwrap();
    // Carol's MCP touch is told about the overlap and claims the file anyway: proceeded.
    let carol = agent(&*store, 3, "c");
    hold(&*store, &carol, &["d.rs"], T0 + 5);
    // George's hook reports an edit it could not be refused before (as Cursor's does): told about
    // both holds on d.rs (alice's and carol's) and went ahead. Nothing was dismissed.
    let george = agent(&*store, 7, "g");
    store
        .touch_session(
            &george,
            &call("hook:cursor", Some("g"), &["d.rs"], false),
            T0 + 6,
        )
        .unwrap();

    let now = T0 + 10;
    let live = metrics(&*store, &alice, now);
    let refusals = &live.issues.contested_edits.refusals;
    assert_eq!(refusals.observed, 4);
    assert_eq!(
        (
            refusals.pushed_through,
            refusals.asked,
            refusals.handed_off,
            refusals.pending
        ),
        (1, 1, 1, 1)
    );
    let notices = &live.issues.contested_edits.notices;
    assert_eq!((notices.observed, notices.proceeded), (3, 3));
    // Only Bob overrode a refusal; only Erin's grant resolves; Dave's declined ask does not.
    assert_eq!(live.issues.dismissed, Some(1));
    assert_eq!(live.issues.resolved, 1);
    assert_eq!(live.issues.unresolved, 2);

    // Alice's hold lapses: Frank went elsewhere, inferred and counted apart; settled rows and
    // Dave's unanswered ask keep their outcome.
    let later = metrics(&*store, &alice, T0 + 10 + 2 * SESSION_TTL_SECONDS);
    let contested = &later.issues.contested_edits;
    assert_eq!(contested.refusals.went_elsewhere, 1);
    assert_eq!(contested.refusals.pushed_through, 1);
    assert_eq!(contested.refusals.asked, 1);
    assert_eq!(contested.notices.proceeded, 3);
    assert_eq!((later.issues.resolved, later.issues.unresolved), (1, 1));
}

fn a_hold_renewed_past_its_cap_keeps_the_refusal_live_and_its_grant_settles_it(db: &Db) {
    use agentscm::hub::session::HOLD_CAP_SECONDS;
    let store = db.open();
    let (alice, bob) = (agent(&*store, 1, "a"), agent(&*store, 2, "b"));
    hold(&*store, &alice, &["a.rs"], T0);
    hold(&*store, &alice, &[], T0 + 1000);
    let refused_at = T0 + HOLD_CAP_SECONDS - 300;
    assert!(edit(&*store, &bob, "s", &["a.rs"], refused_at));
    // The hold lapses at its cap; Alice's next edit of a.rs keeps it under a new hold id.
    hold(&*store, &alice, &["a.rs"], T0 + HOLD_CAP_SECONDS + 1);
    let renewed = metrics(&*store, &alice, T0 + HOLD_CAP_SECONDS + 2);
    assert_eq!(
        renewed.issues.contested_edits.refusals.pending, 1,
        "still held by alice, not went elsewhere"
    );
    store
        .answer_handoff(&alice, None, "a.rs", true, None, T0 + HOLD_CAP_SECONDS + 3)
        .unwrap();
    let granted = metrics(&*store, &alice, T0 + HOLD_CAP_SECONDS + 4);
    assert_eq!(granted.issues.contested_edits.refusals.handed_off, 1);
    assert_eq!(granted.issues.resolved, 1);
}

fn a_session_that_waits_and_then_takes_the_file_from_its_own_tokens_session_went_elsewhere(
    db: &Db,
) {
    // One person, one token, two agent sessions: they refuse each other like any two agents.
    let store = db.open();
    let token = agent(&*store, 1, "a");
    assert!(!edit(&*store, &token, "s1", &["a.rs"], T0));
    assert!(edit(&*store, &token, "s2", &["a.rs"], T0 + 1));
    let waiting = metrics(&*store, &token, T0 + 2);
    assert_eq!(waiting.issues.contested_edits.refusals.pending, 1);
    // s1 finishes; s2's next edit takes a.rs under a hold of the same token, its own.
    store.end_session(&token, Some("s1"), T0 + 3).unwrap();
    assert!(!edit(&*store, &token, "s2", &["a.rs"], T0 + 4));
    let after = metrics(&*store, &token, T0 + 5);
    let refusals = &after.issues.contested_edits.refusals;
    assert_eq!(refusals.pushed_through, 0, "waiting was not an override");
    assert_eq!(
        refusals.went_elsewhere, 1,
        "its own hold does not keep it pending"
    );
    assert_eq!(after.issues.dismissed, Some(0));
}

fn a_failed_overhead_write_keeps_only_the_totals_not_yet_written(db: &Db) {
    use agentscm::hub::hook_overhead::{flush, record};
    use agentscm::hub::store::unix_now;
    let store = db.open();
    let objects = TempDir::new().unwrap();
    let state = agentscm::hub::HubState::new(
        store.clone(),
        Arc::new(agentscm::backend::LocalBackend::open(objects.path()).unwrap()),
    );
    let (token, secret) = agent_with_secret(&*store, 1, "a");
    let sample = |session: &str, hub_calls: u64| Sample {
        session: session.into(),
        agent: "claude".into(),
        hub_calls,
        ..Default::default()
    };
    // A row at the integer limit: adding one more call to it fails in either store.
    store
        .hook_overhead(&token, REPO, &sample("big", i64::MAX as u64), unix_now())
        .unwrap();
    tokio::runtime::Runtime::new().unwrap().block_on(async {
        record(&state, Some(&secret), None, sample("a", 1));
        record(&state, Some(&secret), None, sample("big", 1));
        record(&state, Some(&secret), None, sample("c", 1));
        // "a" is written, "big" fails and waits, and "c" after it is still written; the next
        // write must not add "a" or "c" again.
        flush(&state).await;
        flush(&state).await;
    });
    let query = ValueMetricsQuery {
        since: Some(unix_now() - 3600),
        ..Default::default()
    };
    let overhead = store
        .value_metrics(&token, &query, unix_now())
        .unwrap()
        .overhead;
    for session in ["a", "c"] {
        let row = overhead
            .per_session
            .iter()
            .find(|row| row.session == session)
            .expect("written although another total failed");
        assert_eq!(row.hub_calls, 1, "a written total is not written twice");
    }
}

fn hook_overhead_adds_calls_per_session_and_prunes_old_rows(db: &Db) {
    let store = db.open();
    let token = agent(&*store, 1, "a");
    let report = ClientReport {
        id: None,
        calls: 61,
        ms_total: 250_000,
        ms_max: 5000,
        timeouts: 48,
        failed_open: 1,
    };
    let sample = |session: &str, client: Option<ClientReport>| Sample {
        session: session.into(),
        agent: "claude".into(),
        hub_calls: 1,
        injected_start: 9000,
        injected_refusal: 0,
        injected_other: 40,
        client,
    };
    store
        .hook_overhead(&token, REPO, &sample("old", None), T0)
        .unwrap();
    let now = T0 + RETAIN_SECONDS + 10;
    store
        .hook_overhead(&token, REPO, &sample("s", Some(report)), now - 2)
        .unwrap();
    store
        .hook_overhead(&token, REPO, &sample("s", Some(report)), now - 1)
        .unwrap();
    store
        .hook_overhead(&token, REPO, &sample("quiet", None), now)
        .unwrap();
    let query = ValueMetricsQuery {
        since: Some(now - 86_400),
        ..Default::default()
    };
    let overhead = store.value_metrics(&token, &query, now).unwrap().overhead;
    assert_eq!(overhead.availability, "observed");
    // The row of "old" was pruned when a newer session's row was written.
    assert_eq!(overhead.sessions, 2);
    assert_eq!(overhead.hub_calls, 3);
    assert_eq!(overhead.injected_characters.session_start, 27_000);
    assert_eq!(overhead.client.reporting_sessions, 1);
    assert_eq!(overhead.client.calls, 122);
    assert_eq!(overhead.client.latency_ms_max, 5000);
    assert_eq!(overhead.client.timed_out, 96);
    assert_eq!(overhead.sessions_without_client_report, 1);
    assert!(!overhead.partial);
    let all = ValueMetricsQuery {
        since: Some(0),
        ..Default::default()
    };
    // A window reaching past the retention is partial.
    let wide = store.value_metrics(&token, &all, T0 + 60 * 86_400);
    assert!(wide.unwrap().overhead.partial);
}

/// A real hook call through the Hub's router reaches value metrics, with the client's report.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_hook_call_and_its_client_report_reach_value_metrics() {
    use agentscm::hub::hook_overhead::REPORT_HEADER;
    use agentscm::hub::store::unix_now;
    let dir = TempDir::new().unwrap();
    let store = store::open(&format!(
        "sqlite://{}",
        dir.path().join("hub.sqlite").display()
    ))
    .unwrap();
    let objects = agentscm::backend::LocalBackend::open(dir.path().join("objects")).unwrap();
    let state = agentscm::hub::HubState::new(store.clone(), Arc::new(objects));
    let app = agentscm::hub::router(state.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await });
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
    let (token, secret) = mint_agent_token(&*store, &owner.id, "a", REPO, 3600).unwrap();
    // The client sends one report twice, as it does when the first carrying call timed out on
    // its side after the Hub received it: the Hub counts it once.
    for _ in 0..2 {
        let reply = reqwest::Client::new()
            .post(format!("http://{address}/v1/hub/hook/claude"))
            .bearer_auth(secret.expose())
            .header(
                REPORT_HEADER,
                r#"{"id":42,"calls":3,"ms_total":9000,"ms_max":5000,"timeouts":1,"failed_open":1}"#,
            )
            .header("content-type", "application/json")
            .body(r#"{"hook_event_name":"SessionStart","session_id":"s1"}"#)
            .send()
            .await
            .unwrap();
        assert_eq!(reply.status(), 200);
    }
    // Totals are written in the background, at most every 30 seconds: write them now.
    let query = ValueMetricsQuery {
        since: Some(unix_now() - 3600),
        ..Default::default()
    };
    let mut overhead = None;
    for _ in 0..50 {
        agentscm::hub::hook_overhead::flush(&state).await;
        let read = store.value_metrics(&token, &query, unix_now()).unwrap();
        if read.overhead.hub_calls >= 2 {
            overhead = Some(read.overhead);
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    let overhead = overhead.expect("the hook call was recorded");
    assert_eq!(overhead.hub_calls, 2);
    assert!(overhead.injected_characters.session_start > 0);
    assert_eq!(
        (
            overhead.client.calls,
            overhead.client.timed_out,
            overhead.client.failed_open
        ),
        (3, 1, 1)
    );
    assert_eq!(overhead.sessions_without_client_report, 0);
}

fn one_tokens_sessions_cannot_push_out_another_tokens_rows(db: &Db) {
    let store = db.open();
    let (busy, quiet) = (agent(&*store, 1, "a"), agent(&*store, 2, "b"));
    let sample = |session: String| Sample {
        session,
        agent: "claude".into(),
        hub_calls: 1,
        ..Default::default()
    };
    store
        .hook_overhead(&quiet, REPO, &sample("quiet".into()), T0)
        .unwrap();
    for n in 0..=MAX_ROWS_PER_TOKEN {
        store
            .hook_overhead(&busy, REPO, &sample(format!("s{n}")), T0 + 1 + n)
            .unwrap();
    }
    let query = ValueMetricsQuery {
        since: Some(0),
        ..Default::default()
    };
    let overhead = store
        .value_metrics(&quiet, &query, T0 + 2 + MAX_ROWS_PER_TOKEN)
        .unwrap()
        .overhead;
    // The busy token keeps its newest rows; the quiet token's row is still there.
    assert_eq!(overhead.sessions as i64, MAX_ROWS_PER_TOKEN + 1);
    assert!(
        overhead.partial,
        "a token at its cap makes the counts partial"
    );
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

conformance!(
    refusal_outcomes_follow_what_the_session_did_next,
    hook_overhead_adds_calls_per_session_and_prunes_old_rows,
    one_tokens_sessions_cannot_push_out_another_tokens_rows,
    a_hold_renewed_past_its_cap_keeps_the_refusal_live_and_its_grant_settles_it,
    a_failed_overhead_write_keeps_only_the_totals_not_yet_written,
    a_session_that_waits_and_then_takes_the_file_from_its_own_tokens_session_went_elsewhere,
);
