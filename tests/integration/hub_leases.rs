//! Liveness leases on Work reservations, on a deterministic clock: who renews, what lapses, and
//! what a woken or partitioned lead meets. Every case runs on SQLite and, when configured,
//! PostgreSQL. The clock heartbeat the Hub runs once a minute is driven by `tick`.
use crate::hub_pg;

use std::sync::{Arc, Barrier};
use std::thread;

use agentscm::hub::HubError;
use agentscm::hub::session::SessionCall;
use agentscm::hub::store::{self, ForgeIdentity, Store, mint_bound_agent_token, unix_now};
use agentscm::hub::work::{
    CheckpointKind, WorkCheckpoint, WorkConflictKind, WorkRole, WorkStart, WorkStartResult,
    WorkState,
};
use agentscm::hub::work_lifecycle::{
    ActivityStatus, CallPhase, IN_FLIGHT_CAP_SECONDS, LEASE_LAPSED, LEASE_SECONDS,
    MAX_OTHER_OWNER_PARTICIPANTS, RenewalCoverage,
};
use agentscm::hub::work_transition::{OperationalSelect, WorkLifecycleReader};
use hub_pg::PgSchema;
use postgres::NoTls;
use tempfile::TempDir;

const REPO: &str = "acme/widgets";
const REPOSITORY_ID: i64 = 101;
/// Long enough for every clock these cases move through.
const TTL: i64 = 3 * 86_400;

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

    /// Every column of the query's first row as text, with NULL as the empty string.
    fn row(&self, sql: &str) -> Vec<String> {
        if self._pg.is_some() {
            let rows = postgres::Client::connect(&self.url, NoTls)
                .unwrap()
                .simple_query(sql)
                .unwrap();
            rows.into_iter()
                .find_map(|message| match message {
                    postgres::SimpleQueryMessage::Row(row) => Some(
                        (0..row.len())
                            .map(|i| row.get(i).unwrap_or_default().to_owned())
                            .collect(),
                    ),
                    _ => None,
                })
                .unwrap()
        } else {
            let path = self.url.strip_prefix("sqlite://").unwrap();
            let conn = rusqlite::Connection::open(path).unwrap();
            let mut statement = conn.prepare(sql).unwrap();
            let columns = statement.column_count();
            statement
                .query_row([], |row| {
                    Ok((0..columns)
                        .map(|i| {
                            row.get::<_, Option<rusqlite::types::Value>>(i)
                                .unwrap()
                                .map(|value| match value {
                                    rusqlite::types::Value::Integer(n) => n.to_string(),
                                    rusqlite::types::Value::Text(text) => text,
                                    other => format!("{other:?}"),
                                })
                                .unwrap_or_default()
                        })
                        .collect())
                })
                .unwrap()
        }
    }

    /// The stored lease of a Work item: last renewal and expiry, empty when nothing is held.
    fn lease(&self, id: &str) -> (String, String) {
        let row = self.row(&format!(
            "SELECT reservation_renewed_at, reservation_expires_at FROM hub_work_items
             WHERE id = '{id}'"
        ));
        (row[0].clone(), row[1].clone())
    }

    fn lapse_events(&self, id: &str) -> i64 {
        self.row(&format!(
            "SELECT COUNT(*) FROM hub_events WHERE kind = 'reservation_lapsed' AND subject = '{id}'"
        ))[0]
            .parse()
            .unwrap()
    }
}

/// One owner's agent token.
struct Agent {
    token: String,
    owner: String,
}

fn agent(store: &dyn Store, user_id: i64) -> Agent {
    let owner = store
        .upsert_user(
            &ForgeIdentity {
                provider: "github".into(),
                host: "github.com".into(),
                user_id,
            },
            &format!("user{user_id}"),
        )
        .unwrap();
    another_token(store, &owner.id, user_id)
}

/// A second token of the same owner, as after a token rotation.
fn another_token(store: &dyn Store, owner: &str, n: i64) -> Agent {
    let (token, _, _) = mint_bound_agent_token(
        store,
        owner,
        &format!("agent{n}"),
        REPO,
        REPOSITORY_ID,
        TTL,
        false,
    )
    .unwrap();
    Agent {
        token,
        owner: owner.to_owned(),
    }
}

fn start(key: &str, session: &str, scope: &str) -> WorkStart {
    WorkStart {
        work_id: None,
        source: None,
        request_key: Some(key.into()),
        title: key.into(),
        session: session.into(),
        role: WorkRole::Lead,
        scope: vec![scope.into()],
        branch: Some(format!("feature/{key}")),
        worktree: None,
        next_action: Some("continue".into()),
        depends_on: vec![],
        related_work: vec![],
        takeover_generation: None,
        preflight: None,
    }
}

fn joined(work_id: &str, session: &str, role: WorkRole) -> WorkStart {
    WorkStart {
        work_id: Some(work_id.into()),
        request_key: None,
        role,
        ..start("join", session, "src/**")
    }
}

fn takeover(work_id: &str, session: &str, generation: i64) -> WorkStart {
    WorkStart {
        work_id: Some(work_id.into()),
        request_key: None,
        takeover_generation: Some(generation),
        ..start("takeover", session, "src/**")
    }
}

fn admitted(store: &dyn Store, who: &Agent, start: &WorkStart, now: i64) -> WorkStartResult {
    let result = store.work_start(&who.token, start, now).unwrap();
    assert!(result.created, "{} should be admitted", start.title);
    result
}

fn assert_overlap(result: &WorkStartResult) {
    assert!(!result.created && !result.took_over, "{result:?}");
    assert_eq!(
        result.conflict.as_ref().map(|conflict| conflict.kind),
        Some(WorkConflictKind::ScopeOverlap)
    );
}

/// A hook or named MCP call of `session`, with `paths` for an edit check or none for activity.
fn touch(
    store: &dyn Store,
    who: &Agent,
    session: &str,
    paths: &[&str],
    now: i64,
) -> Result<(), HubError> {
    store
        .touch_session(
            &who.token,
            &SessionCall {
                session: Some(session.into()),
                via: "hook:claude".into(),
                task: None,
                paths: paths.iter().map(|path| (*path).to_owned()).collect(),
                deny: !paths.is_empty(),
                report: false,
            },
            now,
        )
        .map(|_| ())
}

/// The Hub's clock heartbeat once a minute from `from` through `to`.
fn tick(store: &dyn Store, from: i64, to: i64) {
    let mut at = from;
    while at < to {
        store.observe_lease_clock(at).unwrap();
        at += 60;
    }
    store.observe_lease_clock(to).unwrap();
}

fn view(
    store: &dyn Store,
    who: &Agent,
    id: &str,
    now: i64,
) -> agentscm::hub::work_lifecycle::WorkOperationalV1 {
    store
        .work_operational(
            &WorkLifecycleReader::Token(&who.token),
            &OperationalSelect::One(id),
            now,
        )
        .unwrap()
        .work
        .into_iter()
        .next()
        .unwrap()
}

fn activity(store: &dyn Store, who: &Agent, id: &str, now: i64) -> ActivityStatus {
    view(store, who, id, now).activity.status
}

/// A tool call that a hook reported for `session`.
fn tool_call(store: &dyn Store, who: &Agent, session: &str, phase: CallPhase, now: i64) {
    store
        .session_activity(
            &who.token,
            &SessionCall {
                session: Some(session.into()),
                via: "hook:claude".into(),
                task: None,
                paths: Vec::new(),
                deny: false,
                report: false,
            },
            phase,
            now,
        )
        .unwrap();
}

fn checkpoint(work_id: &str, generation: i64, session: &str, seq: i64) -> WorkCheckpoint {
    WorkCheckpoint {
        work_id: work_id.into(),
        operation_id: format!("late-{generation}-{seq}"),
        generation,
        session: session.into(),
        expected_updated_seq: Some(seq),
        kind: CheckpointKind::Progress,
        summary: "late progress".into(),
        next_action: None,
        scope: None,
        branch: None,
        worktree: None,
        resolve_blocker: false,
        source_refs: vec![],
        depends_on: vec![],
        related_work: vec![],
    }
}

fn busy_but_silent_activity_keeps_the_lease_live(db: &Db) {
    let store = db.open();
    let (alice, bob) = (agent(&*store, 1), agent(&*store, 2));
    let now = unix_now();
    store.observe_lease_clock(now).unwrap();
    let held = admitted(&*store, &alice, &start("a", "s-a", "src/**"), now).item;
    // An hour of tool calls with no edit and no checkpoint: each pathless touch renews.
    let mut at = now;
    for step in 1..=6 {
        let next = now + step * 600;
        tick(&*store, at, next);
        at = next;
        touch(&*store, &alice, "s-a", &[], at).unwrap();
        assert_overlap(
            &store
                .work_start(&bob.token, &start(&format!("b{step}"), "s-b", "src/**"), at)
                .unwrap(),
        );
    }
    assert_eq!(
        db.lease(&held.id),
        (at.to_string(), (at + LEASE_SECONDS).to_string())
    );
    assert_eq!(
        activity(&*store, &alice, &held.id, at),
        ActivityStatus::Live
    );
}

fn another_session_of_the_same_token_neither_renews_nor_revives(db: &Db) {
    let store = db.open();
    let (alice, bob) = (agent(&*store, 1), agent(&*store, 2));
    let now = unix_now();
    store.observe_lease_clock(now).unwrap();
    let held = admitted(&*store, &alice, &start("a", "s-a", "src/**"), now).item;
    let lease = db.lease(&held.id);
    // A parallel session on the same token (#120) stays busy; the lead session does not.
    let mut at = now;
    for step in 1..=4 {
        let next = now + step * 300;
        tick(&*store, at, next);
        at = next;
        touch(&*store, &alice, "s-other", &[], at).unwrap();
    }
    assert_eq!(db.lease(&held.id), lease);
    let late = now + LEASE_SECONDS + 1;
    tick(&*store, at, late);
    touch(&*store, &alice, "s-other", &[], late).unwrap();
    assert_eq!(db.lease(&held.id), lease);
    assert_eq!(
        activity(&*store, &alice, &held.id, late),
        ActivityStatus::Lapsed
    );
    admitted(&*store, &bob, &start("b", "s-b", "src/**"), late);
}

fn a_crashed_lead_lapses_without_a_session_end(db: &Db) {
    let store = db.open();
    let (alice, bob) = (agent(&*store, 1), agent(&*store, 2));
    let now = unix_now();
    store.observe_lease_clock(now).unwrap();
    let held = admitted(&*store, &alice, &start("a", "s-a", "src/**"), now).item;
    touch(&*store, &alice, "s-a", &["src/lib.rs"], now + 30).unwrap();
    // The client dies: no SessionEnd, no pause, nothing more from s-a.
    let edge = now + LEASE_SECONDS;
    tick(&*store, now, edge - 1);
    assert_overlap(
        &store
            .work_start(&bob.token, &start("b1", "s-b", "src/**"), edge - 1)
            .unwrap(),
    );
    tick(&*store, edge - 1, edge);
    let taken = admitted(&*store, &bob, &start("b2", "s-b2", "src/**"), edge);
    assert_eq!(db.lapse_events(&held.id), 1);
    assert!(taken.conflict.is_none());
}

fn sleep_and_resume_revalidates_and_re_admits(db: &Db) {
    let store = db.open();
    let alice = agent(&*store, 1);
    let now = unix_now();
    store.observe_lease_clock(now).unwrap();
    let start_a = start("a", "s-a", "src/**");
    let held = admitted(&*store, &alice, &start_a, now).item;
    // The laptop sleeps for two hours while the Hub keeps its clock; nobody claims the scope.
    let wake = now + 7200;
    tick(&*store, now, wake);
    // Waking touches do not revive the lease.
    touch(&*store, &alice, "s-a", &[], wake).unwrap();
    assert_eq!(
        activity(&*store, &alice, &held.id, wake),
        ActivityStatus::Lapsed
    );
    // Starting the same Work again is a fresh admission, so it holds again.
    let resumed = store.work_start(&alice.token, &start_a, wake + 1).unwrap();
    assert_eq!(resumed.item.id, held.id);
    assert_eq!(
        db.lease(&held.id),
        (
            (wake + 1).to_string(),
            (wake + 1 + LEASE_SECONDS).to_string()
        )
    );
    assert_eq!(
        activity(&*store, &alice, &held.id, wake + 2),
        ActivityStatus::Live
    );
}

fn backward_clock_skew_never_lapses_early(db: &Db) {
    let store = db.open();
    let (alice, bob) = (agent(&*store, 1), agent(&*store, 2));
    let now = unix_now();
    store.observe_lease_clock(now).unwrap();
    let held = admitted(&*store, &alice, &start("a", "s-a", "src/**"), now).item;
    // The clock jumps back an hour. The stored expiry is still ahead, so nothing lapses early;
    // the lease lasts longer by the jump instead.
    let back = now - 3600;
    tick(&*store, back, back + LEASE_SECONDS + 60);
    assert_overlap(
        &store
            .work_start(
                &bob.token,
                &start("b1", "s-b", "src/**"),
                back + LEASE_SECONDS + 60,
            )
            .unwrap(),
    );
    // Once the clock passes the stored expiry, the lease lapses as usual.
    let after = now + LEASE_SECONDS + 1;
    tick(&*store, back + LEASE_SECONDS + 60, after);
    admitted(&*store, &bob, &start("b2", "s-b2", "src/**"), after);
    assert_eq!(db.lapse_events(&held.id), 1);
}

fn a_delayed_renewal_never_revives(db: &Db) {
    let store = db.open();
    let alice = agent(&*store, 1);
    let now = unix_now();
    store.observe_lease_clock(now).unwrap();
    let early = admitted(&*store, &alice, &start("early", "s-early", "early/**"), now).item;
    let late = admitted(&*store, &alice, &start("late", "s-late", "late/**"), now).item;
    let expiry = now + LEASE_SECONDS;
    tick(&*store, now, expiry);
    // One second before expiry a renewal extends the lease; at the expiry it is too late.
    touch(&*store, &alice, "s-early", &[], expiry - 1).unwrap();
    touch(&*store, &alice, "s-late", &[], expiry).unwrap();
    assert_eq!(
        db.lease(&early.id).1,
        (expiry - 1 + LEASE_SECONDS).to_string()
    );
    assert_eq!(db.lease(&late.id).1, expiry.to_string());
    assert_eq!(
        activity(&*store, &alice, &late.id, expiry),
        ActivityStatus::Lapsed
    );
}

fn two_reclaimers_have_one_winner(db: &Db) {
    let store = db.open();
    let (alice, bob, carol) = (agent(&*store, 1), agent(&*store, 2), agent(&*store, 3));
    let now = unix_now();
    store.observe_lease_clock(now).unwrap();
    let held = admitted(&*store, &alice, &start("a", "s-a", "src/**"), now).item;
    let after = now + LEASE_SECONDS + 1;
    tick(&*store, now, after);
    let barrier = Arc::new(Barrier::new(2));
    let racers: Vec<_> = [(bob, "b"), (carol, "c")]
        .into_iter()
        .map(|(who, key)| {
            let (store, barrier) = (store.clone(), barrier.clone());
            thread::spawn(move || {
                barrier.wait();
                store
                    .work_start(
                        &who.token,
                        &start(key, &format!("s-{key}"), "src/**"),
                        after,
                    )
                    .unwrap()
            })
        })
        .collect();
    let results: Vec<WorkStartResult> = racers.into_iter().map(|r| r.join().unwrap()).collect();
    assert_eq!(results.iter().filter(|result| result.created).count(), 1);
    let loser = results.iter().find(|result| !result.created).unwrap();
    assert_overlap(loser);
    assert_eq!(db.lapse_events(&held.id), 1);
}

fn a_revoked_lead_token_stops_renewing(db: &Db) {
    let store = db.open();
    let (alice, bob) = (agent(&*store, 1), agent(&*store, 2));
    let now = unix_now();
    store.observe_lease_clock(now).unwrap();
    let held = admitted(&*store, &alice, &start("a", "s-a", "src/**"), now).item;
    assert!(store.revoke_agent_token(&alice.token).unwrap());
    tick(&*store, now, now + 600);
    assert!(matches!(
        touch(&*store, &alice, "s-a", &[], now + 600),
        Err(HubError::Unauthorized)
    ));
    let after = now + LEASE_SECONDS + 1;
    tick(&*store, now + 600, after);
    admitted(&*store, &bob, &start("b", "s-b", "src/**"), after);
    // The owner's rotated token resumes only through fresh admission, which bob now blocks.
    let rotated = another_token(&*store, &alice.owner, 11);
    assert_overlap(
        &store
            .work_start(
                &rotated.token,
                &takeover(&held.id, "s-a2", held.generation + 1),
                after + 1,
            )
            .unwrap(),
    );
}

fn a_partitioned_lead_reconnects_to_a_fence(db: &Db) {
    let store = db.open();
    let (alice, bob) = (agent(&*store, 1), agent(&*store, 2));
    let now = unix_now();
    store.observe_lease_clock(now).unwrap();
    let start_a = start("a", "s-a", "src/**");
    let held = admitted(&*store, &alice, &start_a, now).item;
    // The Hub cannot hear alice for longer than a lease; bob takes the scope meanwhile.
    let after = now + LEASE_SECONDS + 1;
    tick(&*store, now, after);
    admitted(&*store, &bob, &start("b", "s-b", "src/**"), after);
    // Back on the network, nothing alice sends holds the scope again without revalidation.
    let back = after + 300;
    tick(&*store, after, back);
    touch(&*store, &alice, "s-a", &[], back).unwrap();
    assert!(matches!(
        touch(&*store, &alice, "s-a", &["src/lib.rs"], back),
        Err(HubError::Invalid(_))
    ));
    assert!(matches!(
        store.work_checkpoint(
            &alice.token,
            &checkpoint(&held.id, held.generation, "s-a", held.updated_seq),
            back,
        ),
        Err(HubError::Invalid(_))
    ));
    let again = store.work_start(&alice.token, &start_a, back).unwrap();
    assert_eq!(
        (again.item.state, again.item.generation),
        (WorkState::Paused, held.generation + 1)
    );
    assert_eq!(db.lease(&held.id), (String::new(), String::new()));
    assert_overlap(
        &store
            .work_start(
                &alice.token,
                &takeover(&held.id, "s-a", held.generation + 1),
                back,
            )
            .unwrap(),
    );
}

fn helpers_reviewers_and_plain_intents_cannot_bypass_the_fence(db: &Db) {
    let store = db.open();
    let (alice, bob) = (agent(&*store, 1), agent(&*store, 2));
    let helper = another_token(&*store, &alice.owner, 12);
    let now = unix_now();
    store.observe_lease_clock(now).unwrap();
    let held = admitted(&*store, &alice, &start("a", "s-a", "src/**"), now).item;
    for (session, role) in [
        ("s-help", WorkRole::Helper),
        ("s-review", WorkRole::Reviewer),
    ] {
        let result = store
            .work_start(&helper.token, &joined(&held.id, session, role), now + 1)
            .unwrap();
        assert!(result.joined, "{role:?}");
        touch(&*store, &helper, session, &["src/lib.rs"], now + 2).unwrap();
    }
    let after = now + LEASE_SECONDS + 1;
    tick(&*store, now, after);
    admitted(&*store, &bob, &start("b", "s-b", "src/**"), after);
    // The lapse advanced the generation, so every participant from before it is fenced.
    for session in ["s-help", "s-review"] {
        assert!(
            matches!(
                touch(&*store, &helper, session, &["src/lib.rs"], after + 1),
                Err(HubError::Invalid(_))
            ),
            "{session}"
        );
    }
    // A plain announce without a session cannot take the fenced scope either.
    let intent = agentscm::hub::intent::NewIntent {
        id: format!("int_{:032x}", 7),
        task: "edit".into(),
        branch: None,
        base: None,
        machine: None,
        paths: vec!["src/lib.rs".into()],
        ttl_seconds: 600,
    };
    assert!(matches!(
        store.announce_intent(&helper.token, &intent, after + 2),
        Err(HubError::Invalid(_))
    ));
}

fn a_running_shell_call_stays_live_up_to_the_cap(db: &Db) {
    let store = db.open();
    let (alice, bob) = (agent(&*store, 1), agent(&*store, 2));
    let now = unix_now();
    store.observe_lease_clock(now).unwrap();
    let held = admitted(&*store, &alice, &start("a", "s-a", "src/**"), now).item;
    // A 50-minute test run in one shell call: nothing else reaches the Hub meanwhile.
    tool_call(&*store, &alice, "s-a", CallPhase::Start, now + 60);
    assert_eq!(
        db.lease(&held.id).1,
        (now + 60 + IN_FLIGHT_CAP_SECONDS).to_string()
    );
    let busy = now + 3000;
    tick(&*store, now, busy);
    assert_overlap(
        &store
            .work_start(&bob.token, &start("b1", "s-b", "src/**"), busy)
            .unwrap(),
    );
    // Two calls overlap; the lease falls back to the idle lease only when the last one ends.
    tool_call(&*store, &alice, "s-a", CallPhase::Start, busy);
    tool_call(&*store, &alice, "s-a", CallPhase::End, busy + 10);
    assert_eq!(
        db.lease(&held.id).1,
        (busy + IN_FLIGHT_CAP_SECONDS).to_string()
    );
    tool_call(&*store, &alice, "s-a", CallPhase::End, busy + 20);
    assert_eq!(
        db.lease(&held.id).1,
        (busy + 20 + LEASE_SECONDS).to_string()
    );
    // A call whose end never arrives (the client crashed) holds at most the cap.
    tool_call(&*store, &alice, "s-a", CallPhase::Start, busy + 30);
    let capped = busy + 30 + IN_FLIGHT_CAP_SECONDS;
    tick(&*store, busy, capped - 1);
    assert_overlap(
        &store
            .work_start(&bob.token, &start("b2", "s-b", "src/**"), capped - 1)
            .unwrap(),
    );
    tick(&*store, capped - 1, capped);
    admitted(&*store, &bob, &start("b3", "s-b3", "src/**"), capped);
}

fn coverage_follows_what_the_hub_saw_of_the_lead_session(db: &Db) {
    let store = db.open();
    let alice = agent(&*store, 1);
    let now = unix_now();
    store.observe_lease_clock(now).unwrap();
    let held = admitted(&*store, &alice, &start("a", "s-a", "src/**"), now).item;
    let coverage = |at| view(&*store, &alice, &held.id, at).activity.coverage;
    assert_eq!(coverage(now), RenewalCoverage::Unknown);
    store
        .touch_session(
            &alice.token,
            &SessionCall {
                session: Some("s-a".into()),
                via: "mcp".into(),
                task: None,
                paths: Vec::new(),
                deny: false,
                report: false,
            },
            now + 1,
        )
        .unwrap();
    assert_eq!(coverage(now + 1), RenewalCoverage::McpOnly);
    touch(&*store, &alice, "s-a", &["src/lib.rs"], now + 2).unwrap();
    assert_eq!(coverage(now + 2), RenewalCoverage::Edits);
    let incomplete = view(&*store, &alice, &held.id, now + 2);
    assert!(
        incomplete
            .completeness
            .unknown
            .contains(&"activity_coverage".to_owned())
    );
    assert!(incomplete.activity.reason.contains("agentscm client setup"));
    tool_call(&*store, &alice, "s-a", CallPhase::Tick, now + 3);
    let covered = view(&*store, &alice, &held.id, now + 3);
    assert_eq!(covered.activity.coverage, RenewalCoverage::ToolCalls);
    assert!(
        !covered
            .completeness
            .unknown
            .contains(&"activity_coverage".to_owned())
    );
}

fn a_stale_report_alone_never_opens_takeover(db: &Db) {
    let store = db.open();
    let (alice, bob) = (agent(&*store, 1), agent(&*store, 2));
    let now = unix_now();
    store.observe_lease_clock(now).unwrap();
    let held = admitted(&*store, &alice, &start("a", "s-a", "src/**"), now).item;
    // Alice keeps working without a checkpoint: her claim goes stale, her lease stays live.
    let mut at = now;
    for step in 1..=4 {
        let next = now + step * 300;
        tick(&*store, at, next);
        at = next;
        touch(&*store, &alice, "s-a", &[], at).unwrap();
    }
    let attempt = store
        .work_start(&bob.token, &takeover(&held.id, "s-b", held.generation), at)
        .unwrap();
    assert!(!attempt.took_over);
    assert_eq!(
        attempt.conflict.map(|conflict| conflict.kind),
        Some(WorkConflictKind::ClaimHeld)
    );
    // Once the lease lapsed, the explicit takeover is open.
    let lapsed = at + LEASE_SECONDS + 1;
    tick(&*store, at, lapsed);
    assert!(
        store
            .work_start(
                &bob.token,
                &takeover(&held.id, "s-b", held.generation),
                lapsed
            )
            .unwrap()
            .took_over
    );
}

fn a_lapsed_lead_is_told_when_who_and_how_to_resume(db: &Db) {
    let store = db.open();
    let (alice, bob) = (agent(&*store, 1), agent(&*store, 2));
    let now = unix_now();
    store.observe_lease_clock(now).unwrap();
    let held = admitted(&*store, &alice, &start("a", "s-a", "src/**"), now).item;
    let after = now + LEASE_SECONDS + 1;
    tick(&*store, now, after);
    let taken = admitted(&*store, &bob, &start("b", "s-b", "src/**"), after).item;
    let Err(HubError::Invalid(message)) = touch(&*store, &alice, "s-a", &["src/lib.rs"], after + 1)
    else {
        panic!("a lapsed lead's edit must be refused");
    };
    assert!(message.starts_with(LEASE_LAPSED), "{message}");
    for part in [
        format!("lapsed at Unix {}", now + LEASE_SECONDS),
        format!("{} (agent agent2) holds the scope now", taken.id),
        "Branch feature/a may hold unpushed work".to_owned(),
        format!(
            "agentscm_start work_id {} takeover_generation {}",
            held.id,
            held.generation + 1
        ),
    ] {
        assert!(message.contains(&part), "{part}: {message}");
    }
}

fn other_owners_fill_at_most_their_share_of_participants(db: &Db) {
    let store = db.open();
    let alice = agent(&*store, 1);
    let now = unix_now();
    store.observe_lease_clock(now).unwrap();
    let held = admitted(&*store, &alice, &start("a", "s-a", "src/**"), now).item;
    for n in 0..MAX_OTHER_OWNER_PARTICIPANTS {
        let other = agent(&*store, 100 + n as i64);
        let joined_now = store
            .work_start(
                &other.token,
                &joined(&held.id, &format!("s-{n}"), WorkRole::Reviewer),
                now + 1,
            )
            .unwrap();
        assert!(joined_now.joined);
    }
    let one_more = agent(&*store, 999);
    assert!(matches!(
        store.work_start(
            &one_more.token,
            &joined(&held.id, "s-more", WorkRole::Helper),
            now + 2,
        ),
        Err(HubError::Invalid(message)) if message.contains("participants from other owners")
    ));
    // The owner's own sessions are not bound by other owners' share.
    let own = another_token(&*store, &alice.owner, 11);
    assert!(
        store
            .work_start(
                &own.token,
                &joined(&held.id, "s-own", WorkRole::Helper),
                now + 3
            )
            .unwrap()
            .joined
    );
}

fn the_owner_recovers_from_a_dead_lead_token_while_others_wait(db: &Db) {
    let store = db.open();
    let (alice, bob) = (agent(&*store, 1), agent(&*store, 2));
    let now = unix_now();
    store.observe_lease_clock(now).unwrap();
    let held = admitted(&*store, &alice, &start("a", "s-a", "src/**"), now).item;
    assert!(store.revoke_agent_token(&alice.token).unwrap());
    // The lease is still live, so another owner waits for it to lapse.
    let refused = store
        .work_start(
            &bob.token,
            &takeover(&held.id, "s-b", held.generation),
            now + 60,
        )
        .unwrap();
    assert_eq!(
        refused.conflict.map(|conflict| conflict.kind),
        Some(WorkConflictKind::ClaimHeld)
    );
    // The owner's rotated token takes its Work back now: the revoked token can never renew.
    let rotated = another_token(&*store, &alice.owner, 11);
    let recovered = store
        .work_start(
            &rotated.token,
            &takeover(&held.id, "s-a2", held.generation),
            now + 61,
        )
        .unwrap();
    assert!(recovered.took_over, "{recovered:?}");
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
    busy_but_silent_activity_keeps_the_lease_live,
    another_session_of_the_same_token_neither_renews_nor_revives,
    a_crashed_lead_lapses_without_a_session_end,
    sleep_and_resume_revalidates_and_re_admits,
    backward_clock_skew_never_lapses_early,
    a_delayed_renewal_never_revives,
    two_reclaimers_have_one_winner,
    a_revoked_lead_token_stops_renewing,
    a_partitioned_lead_reconnects_to_a_fence,
    helpers_reviewers_and_plain_intents_cannot_bypass_the_fence,
    a_running_shell_call_stays_live_up_to_the_cap,
    coverage_follows_what_the_hub_saw_of_the_lead_session,
    a_stale_report_alone_never_opens_takeover,
    a_lapsed_lead_is_told_when_who_and_how_to_resume,
    other_owners_fill_at_most_their_share_of_participants,
    the_owner_recovers_from_a_dead_lead_token_while_others_wait,
);
