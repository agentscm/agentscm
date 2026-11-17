//! Ending a session releases its intent and pauses the Work it leads in one transaction. A hook's
//! end leaves Work whose hold is live as it is. Every case runs on SQLite and, when configured,
//! PostgreSQL.
use crate::hub_pg;

use std::sync::Arc;

use agentscm::hub::HubError;
use agentscm::hub::connection::NewConnection;
use agentscm::hub::error_envelope::ErrorCode;
use agentscm::hub::handoff::Via;
use agentscm::hub::session::{SESSION_REQUIRED, SessionCall, Touched};
use agentscm::hub::session_lifecycle::{
    EndReason, LifecycleEnd, LifecycleRegister, LifecycleState, LifecycleView, Qualification,
    WorkFence,
};
use agentscm::hub::store::{
    self, ForgeIdentity, Store, mint_bound_agent_token, new_connection_id, unix_now,
};
use agentscm::hub::work::{
    CheckpointKind, WorkCheckpoint, WorkItem, WorkRole, WorkStart, WorkState,
};
use agentscm::hub::work_transition::{
    WorkTransitionActor, WorkTransitionKind, WorkTransitionRequest,
};
use hub_pg::PgSchema;
use postgres::NoTls;
use tempfile::TempDir;

const REPO: &str = "acme/widgets";
const REPOSITORY_ID: i64 = 101;
const TTL: i64 = 86_400;
const PAUSE_SUMMARY: &str = "Session ended without a reported final work outcome.";

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

    /// The first column of the query's first row as text.
    fn text(&self, sql: &str) -> String {
        if self._pg.is_some() {
            postgres::Client::connect(&self.url, NoTls)
                .unwrap()
                .simple_query(sql)
                .unwrap()
                .into_iter()
                .find_map(|message| match message {
                    postgres::SimpleQueryMessage::Row(row) => {
                        Some(row.get(0).unwrap_or_default().to_owned())
                    }
                    _ => None,
                })
                .unwrap()
        } else {
            let path = self.url.strip_prefix("sqlite://").unwrap();
            rusqlite::Connection::open(path)
                .unwrap()
                .query_row(sql, [], |row| {
                    Ok(row
                        .get::<_, Option<rusqlite::types::Value>>(0)?
                        .map(|value| match value {
                            rusqlite::types::Value::Integer(n) => n.to_string(),
                            rusqlite::types::Value::Text(text) => text,
                            other => format!("{other:?}"),
                        })
                        .unwrap_or_default())
                })
                .unwrap()
        }
    }

    /// Makes every insert into `table` that matches `condition` (over `NEW`) fail, as a crash
    /// at that statement would.
    fn fail_inserts(&self, table: &str, condition: &str) {
        if self._pg.is_some() {
            postgres::Client::connect(&self.url, NoTls)
                .unwrap()
                .batch_execute(&format!(
                    "CREATE FUNCTION fail_{table}() RETURNS trigger LANGUAGE plpgsql AS $$
                     BEGIN RAISE EXCEPTION 'synthetic storage failure'; END $$;
                     CREATE TRIGGER fail_{table} BEFORE INSERT ON {table}
                     FOR EACH ROW WHEN ({condition}) EXECUTE FUNCTION fail_{table}();"
                ))
                .unwrap();
        } else {
            let path = self.url.strip_prefix("sqlite://").unwrap();
            rusqlite::Connection::open(path)
                .unwrap()
                .execute_batch(&format!(
                    "CREATE TRIGGER fail_{table} BEFORE INSERT ON {table} WHEN {condition}
                     BEGIN SELECT RAISE(ABORT, 'synthetic storage failure'); END;"
                ))
                .unwrap();
        }
    }

    fn heal(&self, table: &str) {
        if self._pg.is_some() {
            postgres::Client::connect(&self.url, NoTls)
                .unwrap()
                .batch_execute(&format!(
                    "DROP TRIGGER fail_{table} ON {table}; DROP FUNCTION fail_{table}();"
                ))
                .unwrap();
        } else {
            let path = self.url.strip_prefix("sqlite://").unwrap();
            rusqlite::Connection::open(path)
                .unwrap()
                .execute_batch(&format!("DROP TRIGGER fail_{table};"))
                .unwrap();
        }
    }

    fn sessions(&self) -> String {
        self.text("SELECT COUNT(*) FROM hub_sessions")
    }

    fn basis(&self, work: &str) -> String {
        self.text(&format!(
            "SELECT reservation_basis FROM hub_work_items WHERE id = '{work}'"
        ))
    }
}

struct Agent {
    token: String,
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
    let (token, _, _) = mint_bound_agent_token(
        store,
        &owner.id,
        &format!("agent{user_id}"),
        REPO,
        REPOSITORY_ID,
        TTL,
        false,
    )
    .unwrap();
    Agent { token }
}

fn start(key: &str, session: &str, scope: &str) -> WorkStart {
    WorkStart {
        work_id: None,
        source: None,
        request_key: Some(key.into()),
        title: key.into(),
        description: None,
        session: session.into(),
        role: WorkRole::Lead,
        scope: vec![scope.into()],
        branch: Some(format!("feature/{key}")),
        worktree: None,
        next_action: Some("continue".into()),
        depends_on: vec![],
        related_work: vec![],
        takeover_generation: None,
        issue_choice: None,
        preflight: None,
    }
}

/// A hook call of `session` that adds `path` to its intent.
fn touch(store: &dyn Store, who: &Agent, session: &str, path: &str, now: i64) -> Touched {
    store
        .touch_session(
            &who.token,
            &SessionCall {
                session: Some(session.into()),
                via: "hook:claude".into(),
                task: None,
                paths: vec![path.into()],
                deny: false,
                report: true,
            },
            now,
        )
        .unwrap()
}

/// Work led by `session` over `dir/**`, and its session holding `dir/lib.rs`.
fn leading(store: &dyn Store, who: &Agent, session: &str, dir: &str, now: i64) -> WorkItem {
    let started = store
        .work_start(&who.token, &start(dir, session, &format!("{dir}/**")), now)
        .unwrap();
    assert!(started.created);
    let held = touch(store, who, session, &format!("{dir}/lib.rs"), now + 1);
    // The session called no start with a task, so its intent takes the title of its Work.
    assert_eq!(held.intent.unwrap().task, started.item.title);
    started.item
}

fn checkpoint(item: &WorkItem, kind: CheckpointKind, operation_id: &str) -> WorkCheckpoint {
    WorkCheckpoint {
        work_id: item.id.clone(),
        operation_id: operation_id.into(),
        generation: item.generation,
        session: item.lead_session.clone(),
        expected_updated_seq: Some(item.updated_seq),
        kind,
        summary: "synthetic".into(),
        next_action: Some("continue".into()),
        scope: None,
        branch: None,
        worktree: None,
        resolve_blocker: false,
        source_refs: vec![],
        depends_on: vec![],
        related_work: vec![],
    }
}

fn work(store: &dyn Store, who: &Agent, id: &str, now: i64) -> WorkItem {
    store.work_get(&who.token, id, now).unwrap().unwrap()
}

fn live(store: &dyn Store, now: i64) -> Vec<String> {
    store
        .live_intents(
            &crate::repository_scope::github(REPO, Some(REPOSITORY_ID)),
            now,
            10,
        )
        .unwrap()
        .into_iter()
        .map(|intent| intent.id)
        .collect()
}

fn events(store: &dyn Store) -> Vec<String> {
    store
        .events_after(0, 1000)
        .unwrap()
        .into_iter()
        .map(|event| event.kind)
        .collect()
}

fn ending_pauses_the_work_and_releases_the_intent_together(db: &Db) {
    let store = db.open();
    let alice = agent(&*store, 1);
    let now = unix_now();
    let item = leading(&*store, &alice, "s-a", "src", now);
    let seen = events(&*store).len();
    let held = live(&*store, now + 2);
    assert_eq!(held.len(), 1);

    let end = store
        .end_session_pausing(&alice.token, Some("s-a"), "mcp", now + 10)
        .unwrap();
    assert!(end.ended && end.paused);
    let before = end.work.unwrap();
    assert_eq!(
        (before.id.as_str(), before.state),
        (item.id.as_str(), WorkState::Active)
    );

    let paused = work(&*store, &alice, &item.id, now + 10);
    assert_eq!(paused.state, WorkState::Paused);
    assert_eq!(paused.current.as_ref().unwrap().summary, PAUSE_SUMMARY);
    assert_eq!(paused.next_action.as_deref(), Some("continue"));
    assert!(live(&*store, now + 10).is_empty());
    assert_eq!(db.sessions(), "0");
    assert_eq!(
        events(&*store)[seen..],
        ["work_checkpoint", "intent_released", "session_ended"]
    );
}

fn a_failed_end_writes_nothing_and_a_retry_completes_it(db: &Db) {
    let store = db.open();
    let alice = agent(&*store, 1);
    let now = unix_now();
    let item = leading(&*store, &alice, "s-a", "src", now);
    let held = live(&*store, now + 2);
    let baseline = work(&*store, &alice, &item.id, now + 2);
    let seen = events(&*store).len();

    // A crash inside the pause, after its event and version are written, and one after both
    // the pause and the release.
    for (table, condition) in [
        ("hub_work_checkpoint_ops", "NEW.operation_id <> ''"),
        ("hub_events", "NEW.kind = 'session_ended'"),
    ] {
        db.fail_inserts(table, condition);
        let failed = store.end_session_pausing(&alice.token, Some("s-a"), "mcp", now + 10);
        db.heal(table);
        assert!(failed.is_err(), "{table}");
        // No half state: the intent is held, the session remains and the Work is untouched.
        assert_eq!(live(&*store, now + 10), held, "{table}");
        assert_eq!(db.sessions(), "1", "{table}");
        assert_eq!(
            work(&*store, &alice, &item.id, now + 10),
            baseline,
            "{table}"
        );
        assert_eq!(events(&*store).len(), seen, "{table}");
    }

    // The retry finds the same Work and completes both halves.
    let end = store
        .end_session_pausing(&alice.token, Some("s-a"), "mcp", now + 11)
        .unwrap();
    assert!(end.ended && end.paused);
    assert_eq!(
        work(&*store, &alice, &item.id, now + 11).state,
        WorkState::Paused
    );
    assert!(live(&*store, now + 11).is_empty());
    assert_eq!(
        events(&*store)[seen..],
        ["work_checkpoint", "intent_released", "session_ended"]
    );

    // A replay changes nothing.
    let paused = work(&*store, &alice, &item.id, now + 12);
    let replay = store
        .end_session_pausing(&alice.token, Some("s-a"), "mcp", now + 12)
        .unwrap();
    assert!(!replay.ended && !replay.paused && replay.work.is_none());
    assert_eq!(work(&*store, &alice, &item.id, now + 12), paused);
    assert_eq!(events(&*store).len(), seen + 3);
}

fn an_end_after_the_old_two_step_crash_finishes_the_pause(db: &Db) {
    let store = db.open();
    let alice = agent(&*store, 1);
    let now = unix_now();
    let item = leading(&*store, &alice, "s-a", "src", now);
    // The state a crash between the release and the pause used to leave: no intent, Work active.
    assert!(
        store
            .end_session(&alice.token, Some("s-a"), now + 5)
            .unwrap()
    );
    assert!(live(&*store, now + 5).is_empty());
    assert_eq!(
        work(&*store, &alice, &item.id, now + 5).state,
        WorkState::Active
    );

    let end = store
        .end_session_pausing(&alice.token, Some("s-a"), "mcp", now + 6)
        .unwrap();
    assert!(!end.ended && end.paused);
    assert_eq!(
        work(&*store, &alice, &item.id, now + 6).state,
        WorkState::Paused
    );
}

fn blocked_work_stays_blocked_and_the_intent_is_released(db: &Db) {
    let store = db.open();
    let alice = agent(&*store, 1);
    let now = unix_now();
    let item = leading(&*store, &alice, "s-a", "src", now);
    let blocked = store
        .work_checkpoint(
            &alice.token,
            &checkpoint(&item, CheckpointKind::Blocker, "block"),
            now + 2,
        )
        .unwrap()
        .item;
    assert_eq!(blocked.state, WorkState::Blocked);

    let end = store
        .end_session_pausing(&alice.token, Some("s-a"), "hook:claude", now + 10)
        .unwrap();
    assert!(end.ended && !end.paused);
    assert_eq!(end.work.unwrap().state, WorkState::Blocked);
    assert_eq!(work(&*store, &alice, &item.id, now + 10), blocked);
    assert!(live(&*store, now + 10).is_empty());
}

/// An end removes the session row and the task on it; the Work the session leads keeps its title.
fn an_edit_after_the_row_is_removed_takes_the_title_of_the_led_work(db: &Db) {
    let store = db.open();
    let alice = agent(&*store, 1);
    let now = unix_now();
    let item = leading(&*store, &alice, "s-a", "src", now);
    // Blocked Work stays eligible after the end, so the session's edits go on.
    store
        .work_checkpoint(
            &alice.token,
            &checkpoint(&item, CheckpointKind::Blocker, "block"),
            now + 2,
        )
        .unwrap();
    let end = store
        .end_session_pausing(&alice.token, Some("s-a"), "hook:claude", now + 10)
        .unwrap();
    assert!(end.ended && !end.paused);
    assert_eq!(db.sessions(), "0");

    let again = touch(&*store, &alice, "s-a", "src/more.rs", now + 11);
    assert_eq!(again.intent.unwrap().task, item.title);
    // A session that leads no Work still reads the default.
    let other = touch(&*store, &alice, "s-b", "docs/notes.md", now + 12);
    assert_eq!(other.intent.unwrap().task, "task unknown");
}

fn settled_work_is_left_as_it_is_and_the_intent_is_released(db: &Db) {
    let store = db.open();
    let alice = agent(&*store, 1);
    let now = unix_now();
    for (n, kind) in [
        CheckpointKind::Pause,
        CheckpointKind::Handover,
        CheckpointKind::Finish,
    ]
    .into_iter()
    .enumerate()
    {
        let (session, dir) = (format!("s-{n}"), format!("dir{n}"));
        let item = leading(&*store, &alice, &session, &dir, now);
        let settled = store
            .work_checkpoint(
                &alice.token,
                &checkpoint(&item, kind, &format!("settle-{n}")),
                now + 2,
            )
            .unwrap()
            .item;

        let end = store
            .end_session_pausing(&alice.token, Some(&session), "hook:claude", now + 10)
            .unwrap();
        assert!(end.ended && !end.paused && end.work.is_none(), "{kind:?}");
        assert_eq!(
            work(&*store, &alice, &item.id, now + 10),
            settled,
            "{kind:?}"
        );
    }
    assert!(live(&*store, now + 10).is_empty());
}

fn a_session_with_no_work_still_releases_its_intent(db: &Db) {
    let store = db.open();
    let alice = agent(&*store, 1);
    let now = unix_now();
    touch(&*store, &alice, "s-a", "src/lib.rs", now);
    let seen = events(&*store).len();

    let end = store
        .end_session_pausing(&alice.token, Some("s-a"), "hook:claude", now + 5)
        .unwrap();
    assert!(end.ended && !end.paused && end.work.is_none());
    assert!(live(&*store, now + 5).is_empty());
    assert_eq!(
        events(&*store)[seen..],
        ["intent_released", "session_ended"]
    );
    // There is nothing left to end.
    let again = store
        .end_session_pausing(&alice.token, Some("s-a"), "hook:claude", now + 6)
        .unwrap();
    assert!(!again.ended && !again.paused && again.work.is_none());
}

fn a_retained_reservation_stays_retained(db: &Db) {
    let store = db.open();
    let alice = agent(&*store, 1);
    let now = unix_now();
    let item = leading(&*store, &alice, "s-a", "src", now);
    store
        .work_transition(
            &WorkTransitionActor::Agent {
                token: &alice.token,
            },
            &WorkTransitionRequest {
                work_id: item.id.clone(),
                operation_id: "keep".into(),
                generation: item.generation,
                expected_updated_seq: item.updated_seq,
                session: Some("s-a".into()),
                transition: WorkTransitionKind::Retain,
                reason: "keep the scope while away".into(),
                superseded_by: None,
                participants: Vec::new(),
                paths: Vec::new(),
            },
            now + 2,
        )
        .unwrap();
    assert_eq!(db.basis(&item.id), "retained");

    let end = store
        .end_session_pausing(&alice.token, Some("s-a"), "mcp", now + 10)
        .unwrap();
    assert!(end.ended && end.paused);
    assert_eq!(
        work(&*store, &alice, &item.id, now + 10).state,
        WorkState::Paused
    );
    assert_eq!(db.basis(&item.id), "retained");
}

fn an_unnamed_end_resolves_the_session_like_end_session(db: &Db) {
    let store = db.open();
    let alice = agent(&*store, 1);
    let now = unix_now();
    let first = leading(&*store, &alice, "s-a", "src", now);
    // A second hook session makes an omitted name ambiguous, and nothing is written.
    touch(&*store, &alice, "s-b", "docs/guide.md", now + 2);
    let seen = events(&*store).len();
    let Err(HubError::Rejected(rejection)) =
        store.end_session_pausing(&alice.token, None, "mcp", now + 3)
    else {
        panic!("an ambiguous session must be refused");
    };
    assert_eq!(rejection.code(), ErrorCode::SessionRequired);
    assert_eq!(rejection.message(), SESSION_REQUIRED);
    assert_eq!(events(&*store).len(), seen);
    assert_eq!(
        work(&*store, &alice, &first.id, now + 3).state,
        WorkState::Active
    );

    // With one session left, the omitted name is that session.
    assert!(
        store
            .end_session(&alice.token, Some("s-b"), now + 4)
            .unwrap()
    );
    let end = store
        .end_session_pausing(&alice.token, None, "mcp", now + 5)
        .unwrap();
    assert!(end.ended && end.paused);
    assert_eq!(
        work(&*store, &alice, &first.id, now + 5).state,
        WorkState::Paused
    );
}

fn a_token_that_can_no_longer_act_on_work_still_ends_its_session(db: &Db) {
    let store = db.open();
    let alice = agent(&*store, 1);
    let now = unix_now();
    let item = leading(&*store, &alice, "s-a", "src", now);
    // The repository is renamed after the token was minted, so Work refuses the token.
    let connection = store
        .insert_connection(
            &NewConnection {
                id: new_connection_id().unwrap(),
                provider: "github".into(),
                host: "github.com".into(),
                kind: "app".into(),
                app_id: Some(42),
                app_slug: Some("agentscm-test".into()),
                acts_as: None,
            },
            "usr_admin",
        )
        .unwrap();
    for name in [REPO, "acme/renamed"] {
        store
            .upsert_repository(
                &connection.id,
                "github.com",
                REPOSITORY_ID,
                name,
                None,
                None,
            )
            .unwrap();
    }
    assert!(matches!(
        store.work_get(&alice.token, &item.id, now + 5),
        Err(HubError::Unauthorized)
    ));

    let end = store
        .end_session_pausing(&alice.token, Some("s-a"), "hook:claude", now + 6)
        .unwrap();
    assert!(end.ended && !end.paused && end.work.is_none());
    assert!(live(&*store, now + 6).is_empty());
}

/// A hook's end may be only the gap between two turns of one session. Work whose hold is live
/// goes on and nothing is written for it; the cases above cover the pause `done` still writes.
fn a_hooks_end_leaves_work_with_a_live_hold_as_it_is(db: &Db) {
    const LEASE: i64 = 1_200;
    let store = db.open();
    let alice = agent(&*store, 1);
    let now = unix_now();
    store.observe_lease_clock(now).unwrap();
    let item = leading(&*store, &alice, "s-a", "src", now);
    let gone = leading(&*store, &alice, "s-b", "docs", now);
    let seen = events(&*store).len();
    // One session ends and never comes back.
    let end = store
        .end_session_pausing(&alice.token, Some("s-b"), "hook:claude", now + 60)
        .unwrap();
    assert!(end.ended && !end.paused);
    let renewed = || {
        db.text(&format!(
            "SELECT reservation_renewed_at FROM hub_work_items WHERE id = '{}'",
            item.id
        ))
    };

    // Two turns of a client that ends its session after each one.
    for turn in [100, 300] {
        let end = store
            .end_session_pausing(&alice.token, Some("s-a"), "hook:claude", now + turn)
            .unwrap();
        assert!(end.ended && !end.paused);
        assert_eq!(end.work.unwrap().id, item.id);
        // The session and its intent are gone; the Work is the same version, still active.
        assert_eq!(db.sessions(), "0");
        assert!(live(&*store, now + turn).is_empty());
        let after = work(&*store, &alice, &item.id, now + turn);
        assert_eq!(
            (after.state, after.updated_seq),
            (WorkState::Active, item.updated_seq)
        );
        // Its lease runs from the end.
        assert_eq!(renewed(), (now + turn).to_string());
        // The session comes back and edits on under the same Work.
        let back = touch(&*store, &alice, "s-a", "src/lib.rs", now + turn + 50);
        assert_eq!(back.intent.unwrap().task, item.title);
    }
    assert!(
        !events(&*store)[seen..]
            .iter()
            .any(|kind| kind.starts_with("work_")),
        "{:?}",
        &events(&*store)[seen..]
    );

    // Once the hold has lapsed no lease bounds the wait, so the end pauses as it always did.
    let lapsed = now + 2 * LEASE;
    let mut at = now;
    while at < lapsed {
        at = (at + LEASE / 2).min(lapsed);
        store.observe_lease_clock(at).unwrap();
    }
    let end = store
        .end_session_pausing(&alice.token, Some("s-a"), "hook:claude", lapsed)
        .unwrap();
    assert!(end.ended && end.paused);
    let paused = work(&*store, &alice, &item.id, lapsed);
    assert_eq!(paused.state, WorkState::Paused);
    assert_eq!(paused.current.as_ref().unwrap().summary, PAUSE_SUMMARY);

    // The Work of the session that never came back is still active, as that of a session that
    // was killed: the Work admitted over its files pauses it.
    assert_eq!(
        work(&*store, &alice, &gone.id, lapsed).state,
        WorkState::Active
    );
    let next = store
        .work_start(&alice.token, &start("docs-next", "s-c", "docs/**"), lapsed)
        .unwrap();
    assert!(next.created && next.conflict.is_none());
    assert_eq!(
        work(&*store, &alice, &gone.id, lapsed).state,
        WorkState::Paused
    );
}

fn observed_exit_failure_rolls_back_work_intent_and_terminal_fence(db: &Db) {
    let store = db.open();
    let alice = agent(&*store, 1);
    let now = unix_now();
    let item = leading(&*store, &alice, "s-a", "src", now);
    let view = store
        .lifecycle_register(
            &alice.token,
            &registration("s-a", "one", None, now + 1),
            now + 1,
        )
        .unwrap();
    let request = exit_request(&view, &item, now + 10);
    db.fail_inserts("hub_events", "NEW.kind = 'session_ended'");
    assert!(
        store
            .lifecycle_end(&alice.token, &request, now + 10)
            .is_err()
    );
    assert_eq!(work(&*store, &alice, &item.id, now + 11), item);
    assert_eq!(
        store
            .lifecycle_get(&alice.token, "s-a", now + 11)
            .unwrap()
            .unwrap()
            .state,
        LifecycleState::Live
    );
    assert_eq!(db.sessions(), "1");
    assert_eq!(live(&*store, now + 11).len(), 1);
    db.heal("hub_events");
    assert!(
        store
            .lifecycle_end(&alice.token, &request, now + 12)
            .unwrap()
            .ended
    );
}

fn lifecycle_mutation_rejects_readonly_and_revoked_tokens(db: &Db) {
    let store = db.open();
    let alice = agent(&*store, 1);
    let now = unix_now();
    db.text(&format!(
        "UPDATE hub_agent_tokens SET read_only=TRUE WHERE id='{}' RETURNING id",
        alice.token
    ));
    assert!(
        store
            .lifecycle_register(
                &alice.token,
                &registration("s-a", "one", None, now + 1),
                now + 1
            )
            .is_err()
    );
    db.text(&format!(
        "UPDATE hub_agent_tokens SET read_only=FALSE WHERE id='{}' RETURNING id",
        alice.token
    ));
    let view = store
        .lifecycle_register(
            &alice.token,
            &registration("s-a", "one", None, now + 2),
            now + 2,
        )
        .unwrap();
    db.text(&format!(
        "UPDATE hub_agent_tokens SET revoked=TRUE WHERE id='{}' RETURNING id",
        alice.token
    ));
    let request = LifecycleEnd {
        session: "s-a".into(),
        incarnation: view.incarnation,
        operation_id: "end".into(),
        observed_at: now + 3,
        reason: EndReason::ObservedExit,
        work: vec![],
    };
    assert!(
        store
            .lifecycle_end(&alice.token, &request, now + 3)
            .is_err()
    );
    assert_eq!(
        db.text("SELECT state FROM hub_session_incarnations WHERE session='s-a'"),
        "live"
    );
}

fn registration(
    session: &str,
    nonce: &str,
    expected: Option<String>,
    at: i64,
) -> LifecycleRegister {
    LifecycleRegister {
        session: session.into(),
        start_nonce: nonce.into(),
        operation_id: format!("register-{nonce}"),
        observed_at: at,
        expected_incarnation: expected,
        qualified: true,
    }
}

fn exit_request(view: &LifecycleView, item: &WorkItem, at: i64) -> LifecycleEnd {
    LifecycleEnd {
        session: view.session.clone(),
        incarnation: view.incarnation.clone(),
        operation_id: format!("exit-{}", view.start_nonce),
        observed_at: at,
        reason: EndReason::ObservedExit,
        work: vec![WorkFence {
            work_id: item.id.clone(),
            generation: item.generation,
            updated_seq: item.updated_seq,
        }],
    }
}

fn transport_touch(
    store: &dyn Store,
    token: &str,
    incarnation: Option<&str>,
    at: i64,
) -> agentscm::hub::Result<Touched> {
    store.touch_session_named_with_transport(
        token,
        &SessionCall {
            session: Some("s-a".into()),
            via: "hook:claude".into(),
            task: None,
            paths: vec![],
            deny: false,
            report: true,
        },
        true,
        incarnation,
        at,
    )
}

fn observed_exit_pauses_once_and_replay_does_not_renew(db: &Db) {
    let store = db.open();
    let alice = agent(&*store, 1);
    let now = unix_now();
    let item = leading(&*store, &alice, "s-a", "src", now);
    let view = store
        .lifecycle_register(
            &alice.token,
            &registration("s-a", "one", None, now + 1),
            now + 1,
        )
        .unwrap();
    let request = exit_request(&view, &item, now + 10);
    let result = store
        .lifecycle_end(&alice.token, &request, now + 10)
        .unwrap();
    assert!(result.ended);
    assert_eq!(result.paused_work, std::slice::from_ref(&item.id));
    assert_eq!(result.lifecycle.state, LifecycleState::Terminal);
    assert_eq!(result.lifecycle.observed_at, now + 10);
    assert_eq!(
        work(&*store, &alice, &item.id, now + 11).state,
        WorkState::Paused
    );
    assert!(live(&*store, now + 11).is_empty());
    assert_eq!(db.sessions(), "0");
    let before = events(&*store);
    let expires = db.text(&format!(
        "SELECT reservation_expires_at FROM hub_work_items WHERE id='{}'",
        item.id
    ));
    assert!(expires.parse::<i64>().unwrap() <= now + 10);
    assert_eq!(
        store
            .lifecycle_end(&alice.token, &request, now + 100)
            .unwrap(),
        result
    );
    assert_eq!(events(&*store), before);
    assert_eq!(
        db.text(&format!(
            "SELECT reservation_expires_at FROM hub_work_items WHERE id='{}'",
            item.id
        )),
        expires
    );
    assert!(!before.iter().any(|kind| kind == "work_completed"));
}

fn observed_exit_preserves_blocked_report_and_retained_authority(db: &Db) {
    let store = db.open();
    let alice = agent(&*store, 1);
    let now = unix_now();
    let item = leading(&*store, &alice, "s-a", "src", now);
    let blocked = store
        .work_checkpoint(
            &alice.token,
            &checkpoint(&item, CheckpointKind::Blocker, "blocked"),
            now + 1,
        )
        .unwrap()
        .item;
    let view = store
        .lifecycle_register(
            &alice.token,
            &registration("s-a", "one", None, now + 2),
            now + 2,
        )
        .unwrap();
    let result = store
        .lifecycle_end(
            &alice.token,
            &exit_request(&view, &blocked, now + 10),
            now + 10,
        )
        .unwrap();
    assert!(result.paused_work.is_empty());
    assert_eq!(work(&*store, &alice, &item.id, now + 11), blocked);
    assert!(live(&*store, now + 11).is_empty());
    assert!(
        db.text(&format!(
            "SELECT reservation_expires_at FROM hub_work_items WHERE id='{}'",
            item.id
        ))
        .parse::<i64>()
        .unwrap()
            <= now + 10
    );

    let other = leading(&*store, &alice, "retained", "docs", now + 20);
    let retained = store
        .work_transition(
            &WorkTransitionActor::Agent {
                token: &alice.token,
            },
            &WorkTransitionRequest {
                work_id: other.id.clone(),
                operation_id: "retain".into(),
                generation: other.generation,
                expected_updated_seq: other.updated_seq,
                session: Some("retained".into()),
                transition: WorkTransitionKind::Retain,
                reason: "keep deliberately".into(),
                superseded_by: None,
                participants: vec![],
                paths: Vec::new(),
            },
            now + 21,
        )
        .unwrap()
        .item;
    let expires = db.text(&format!(
        "SELECT reservation_expires_at FROM hub_work_items WHERE id='{}'",
        other.id
    ));
    let view = store
        .lifecycle_register(
            &alice.token,
            &registration("retained", "two", None, now + 22),
            now + 22,
        )
        .unwrap();
    store
        .lifecycle_end(
            &alice.token,
            &exit_request(&view, &retained, now + 23),
            now + 23,
        )
        .unwrap();
    assert_eq!(db.basis(&other.id), "retained");
    assert_eq!(
        db.text(&format!(
            "SELECT reservation_expires_at FROM hub_work_items WHERE id='{}'",
            other.id
        )),
        expires
    );
}

fn legacy_calls_make_mixed_sticky_and_terminal_cannot_be_resurrected(db: &Db) {
    let store = db.open();
    let alice = agent(&*store, 1);
    let now = unix_now();
    let item = leading(&*store, &alice, "s-a", "src", now);
    let reg = registration("s-a", "one", None, now + 1);
    let view = store
        .lifecycle_register(&alice.token, &reg, now + 1)
        .unwrap();
    transport_touch(&*store, &alice.token, Some(&view.incarnation), now + 2).unwrap();
    assert_eq!(
        store
            .lifecycle_get(&alice.token, "s-a", now + 2)
            .unwrap()
            .unwrap()
            .qualification,
        Qualification::Qualified
    );
    transport_touch(&*store, &alice.token, None, now + 3).unwrap();
    let mut upgrade = reg.clone();
    upgrade.operation_id = "qualification-two".into();
    assert_eq!(
        store
            .lifecycle_register(&alice.token, &upgrade, now + 4)
            .unwrap()
            .qualification,
        Qualification::Mixed
    );
    let mut request = exit_request(&view, &item, now + 5);
    assert!(
        store
            .lifecycle_end(&alice.token, &request, now + 5)
            .is_err()
    );
    assert_eq!(db.sessions(), "1");
    request.reason = EndReason::ExplicitEnd;
    store
        .lifecycle_end(&alice.token, &request, now + 6)
        .unwrap();
    assert!(transport_touch(&*store, &alice.token, None, now + 7).is_err());
    assert!(
        store
            .lifecycle_register(&alice.token, &reg, now + 7)
            .is_err()
    );
    let resume = registration("s-a", "two", Some(view.incarnation.clone()), now + 8);
    let next = store
        .lifecycle_register(&alice.token, &resume, now + 8)
        .unwrap();
    assert_eq!(
        store
            .lifecycle_register(&alice.token, &resume, now + 50)
            .unwrap(),
        next
    );
    assert_ne!(view.incarnation, next.incarnation);
    assert!(transport_touch(&*store, &alice.token, Some(&view.incarnation), now + 9).is_err());
    transport_touch(&*store, &alice.token, Some(&next.incarnation), now + 10).unwrap();
    let before = events(&*store);
    for rejected in [
        store
            .ask_handoff_with_transport(
                &alice.token,
                Some("s-a"),
                "src/lib.rs",
                None,
                (Via::Agent, Some(&view.incarnation)),
                now + 11,
            )
            .err(),
        store
            .answer_handoff_with_transport(
                &alice.token,
                Some("s-a"),
                "src/lib.rs",
                (true, None),
                (Via::Agent, Some(&view.incarnation)),
                now + 11,
            )
            .err(),
    ] {
        let Some(HubError::Rejected(rejection)) = rejected else {
            panic!("stale handoffs must fail the incarnation fence");
        };
        assert_eq!(rejection.code(), ErrorCode::InvalidOrChanged);
    }
    assert_eq!(events(&*store), before);
    assert_eq!(
        store
            .lifecycle_get(&alice.token, "s-a", now + 11)
            .unwrap()
            .unwrap()
            .qualification,
        Qualification::Qualified
    );
}

fn registration_and_qualification_replay_do_not_touch_activity(db: &Db) {
    let store = db.open();
    let alice = agent(&*store, 1);
    let now = unix_now();
    let item = leading(&*store, &alice, "s-a", "src", now);
    let activity = db.text("SELECT updated_at FROM hub_sessions WHERE session='s-a'");
    let expires = db.text(&format!(
        "SELECT reservation_expires_at FROM hub_work_items WHERE id='{}'",
        item.id
    ));
    let mut reg = registration("s-a", "one", None, now + 1);
    reg.qualified = false;
    let unknown = store
        .lifecycle_register(&alice.token, &reg, now + 1)
        .unwrap();
    assert_eq!(unknown.qualification, Qualification::Unknown);
    let mut reused = reg.clone();
    reused.start_nonce = "different-start".into();
    reused.expected_incarnation = Some(unknown.incarnation.clone());
    assert!(
        store
            .lifecycle_register(&alice.token, &reused, now + 2)
            .is_err()
    );
    reg.qualified = true;
    assert!(
        store
            .lifecycle_register(&alice.token, &reg, now + 2)
            .is_err()
    );
    reg.operation_id = "second-event".into();
    assert_eq!(
        store
            .lifecycle_register(&alice.token, &reg, now + 100)
            .unwrap()
            .qualification,
        Qualification::Qualified
    );
    assert_eq!(
        store
            .lifecycle_register(&alice.token, &reg, now + 200)
            .unwrap()
            .observed_at,
        now + 1
    );
    assert_eq!(
        db.text("SELECT updated_at FROM hub_sessions WHERE session='s-a'"),
        activity
    );
    assert_eq!(
        db.text(&format!(
            "SELECT reservation_expires_at FROM hub_work_items WHERE id='{}'",
            item.id
        )),
        expires
    );
}

fn present_transport_selects_latest_native_in_an_ambiguous_domain(db: &Db) {
    let store = db.open();
    let alice = agent(&*store, 1);
    let now = unix_now();
    leading(&*store, &alice, "default", "src", now);
    let mut canonical = registration("default", "canonical", None, now + 1);
    canonical.qualified = false;
    store
        .lifecycle_register(&alice.token, &canonical, now + 1)
        .unwrap();
    touch(&*store, &alice, "s-a", "src/lib.rs", now + 2);
    let first = store
        .lifecycle_register(
            &alice.token,
            &registration("s-a", "one", None, now + 3),
            now + 3,
        )
        .unwrap();
    assert_eq!(
        store
            .lifecycle_get(&alice.token, "s-a", now + 4)
            .unwrap()
            .unwrap(),
        first,
        "an exact native state lookup must not become ambiguous with its canonical domain",
    );
    assert!(
        store
            .lifecycle_domain_get(&alice.token, "default", now + 4)
            .is_err()
    );
    assert_eq!(
        store
            .lifecycle_check(&alice.token, "default", Some(&first.incarnation), now + 4)
            .unwrap()
            .unwrap()
            .session,
        "s-a"
    );
    assert!(
        store
            .lifecycle_check(&alice.token, "default", None, now + 4)
            .is_err()
    );
    let next = store
        .lifecycle_register(
            &alice.token,
            &registration("s-a", "two", Some(first.incarnation.clone()), now + 5),
            now + 5,
        )
        .unwrap();
    assert!(
        store
            .lifecycle_check(&alice.token, "default", Some(&first.incarnation), now + 6)
            .is_err()
    );
    assert_eq!(
        store
            .lifecycle_check(&alice.token, "default", Some(&next.incarnation), now + 6)
            .unwrap()
            .unwrap()
            .incarnation,
        next.incarnation
    );
    assert_eq!(
        store
            .lifecycle_get(&alice.token, "s-a", now + 6)
            .unwrap()
            .unwrap(),
        next
    );
    assert!(
        store
            .lifecycle_domain_get(&alice.token, "default", now + 6)
            .is_err()
    );
    let end = LifecycleEnd {
        session: "s-a".into(),
        incarnation: next.incarnation.clone(),
        operation_id: "native-state-end".into(),
        observed_at: now + 7,
        reason: EndReason::ExplicitEnd,
        work: store
            .lifecycle_fences(&alice.token, "s-a", now + 7)
            .unwrap(),
    };
    let ended = store.lifecycle_end(&alice.token, &end, now + 7).unwrap();
    assert_eq!(
        store.lifecycle_end(&alice.token, &end, now + 8).unwrap(),
        ended
    );
    assert_eq!(
        store
            .lifecycle_get(&alice.token, "s-a", now + 8)
            .unwrap()
            .unwrap()
            .state,
        LifecycleState::Terminal
    );
    assert!(
        store
            .lifecycle_domain_get(&alice.token, "default", now + 8)
            .is_err()
    );
}

fn native_incarnation_fences_canonical_default_alias_and_ends_its_work(db: &Db) {
    let store = db.open();
    let alice = agent(&*store, 1);
    let now = unix_now();
    let item = leading(&*store, &alice, "default", "src", now);
    touch(&*store, &alice, "s-a", "src/lib.rs", now + 1);
    assert_eq!(
        db.text("SELECT canonical_session FROM hub_session_aliases WHERE observed_session='s-a'"),
        "default"
    );
    let view = store
        .lifecycle_register(
            &alice.token,
            &registration("s-a", "one", None, now + 2),
            now + 2,
        )
        .unwrap();
    assert!(
        store
            .lifecycle_get(&alice.token, "default", now + 2)
            .unwrap()
            .is_none()
    );
    assert_eq!(
        store
            .lifecycle_domain_get(&alice.token, "default", now + 2)
            .unwrap()
            .unwrap(),
        view
    );
    assert_eq!(
        store
            .lifecycle_check(&alice.token, "default", Some(&view.incarnation), now + 2)
            .unwrap()
            .unwrap()
            .session,
        "s-a"
    );
    let updated = store
        .work_checkpoint_with_transport(
            &alice.token,
            &checkpoint(&item, CheckpointKind::Progress, "alias-progress"),
            Some(&view.incarnation),
            now + 3,
        )
        .unwrap()
        .item;
    assert_eq!(
        store
            .lifecycle_fences(&alice.token, "s-a", now + 3)
            .unwrap(),
        [WorkFence {
            work_id: updated.id.clone(),
            generation: updated.generation,
            updated_seq: updated.updated_seq
        }]
    );
    assert!(
        store
            .lifecycle_end(
                &alice.token,
                &exit_request(&view, &updated, now + 10),
                now + 10
            )
            .unwrap()
            .ended
    );
    assert_eq!(
        work(&*store, &alice, &item.id, now + 10).state,
        WorkState::Paused
    );
    assert!(live(&*store, now + 10).is_empty());
    let next = store
        .lifecycle_register(
            &alice.token,
            &registration("s-a", "two", Some(view.incarnation.clone()), now + 11),
            now + 11,
        )
        .unwrap();
    assert!(
        store
            .lifecycle_check(&alice.token, "default", Some(&view.incarnation), now + 12)
            .is_err()
    );
    assert_eq!(
        store
            .lifecycle_check(&alice.token, "default", Some(&next.incarnation), now + 12)
            .unwrap()
            .unwrap()
            .qualification,
        Qualification::Qualified
    );
    store
        .lifecycle_check(&alice.token, "default", None, now + 13)
        .unwrap();
    assert_eq!(
        store
            .lifecycle_get(&alice.token, "s-a", now + 13)
            .unwrap()
            .unwrap()
            .qualification,
        Qualification::Mixed
    );
}

fn observed_exit_fences_changed_work_and_foreign_tokens(db: &Db) {
    let store = db.open();
    let alice = agent(&*store, 1);
    let bob = agent(&*store, 2);
    let now = unix_now();
    let item = leading(&*store, &alice, "s-a", "src", now);
    let view = store
        .lifecycle_register(
            &alice.token,
            &registration("s-a", "one", None, now + 1),
            now + 1,
        )
        .unwrap();
    let request = exit_request(&view, &item, now + 10);
    assert!(store.lifecycle_end(&bob.token, &request, now + 10).is_err());
    let updated = store
        .work_checkpoint_with_transport(
            &alice.token,
            &checkpoint(&item, CheckpointKind::Progress, "new-progress"),
            Some(&view.incarnation),
            now + 2,
        )
        .unwrap()
        .item;
    assert!(
        store
            .lifecycle_end(&alice.token, &request, now + 10)
            .is_err()
    );
    let mut wrong_generation = exit_request(&view, &updated, now + 10);
    wrong_generation.work[0].generation += 1;
    assert!(
        store
            .lifecycle_end(&alice.token, &wrong_generation, now + 10)
            .is_err()
    );
    assert_eq!(
        store
            .lifecycle_get(&alice.token, "s-a", now + 10)
            .unwrap()
            .unwrap()
            .state,
        LifecycleState::Live
    );
    assert_eq!(
        work(&*store, &alice, &item.id, now + 10).state,
        WorkState::Active
    );
    assert_eq!(db.sessions(), "1");
    store
        .lifecycle_end(
            &alice.token,
            &exit_request(&view, &updated, now + 10),
            now + 10,
        )
        .unwrap();
    let next = store
        .lifecycle_register(
            &alice.token,
            &registration("s-a", "two", Some(view.incarnation.clone()), now + 11),
            now + 11,
        )
        .unwrap();
    let paused = work(&*store, &alice, &item.id, now + 11);
    assert!(
        store
            .work_start_named_with_transport(
                &alice.token,
                &start("stale-start", "s-a", "elsewhere/**"),
                true,
                Some(&view.incarnation),
                now + 12
            )
            .is_err()
    );
    assert!(
        store
            .work_checkpoint_with_transport(
                &alice.token,
                &checkpoint(&paused, CheckpointKind::Progress, "stale-checkpoint"),
                Some(&view.incarnation),
                now + 12
            )
            .is_err()
    );
    assert_eq!(work(&*store, &alice, &item.id, now + 12), paused);
    assert_eq!(
        store
            .lifecycle_get(&alice.token, "s-a", now + 12)
            .unwrap()
            .unwrap()
            .incarnation,
        next.incarnation
    );
}

async fn protocol_http(
    client: &reqwest::Client,
    base: &str,
    secret: &str,
    path: &str,
    body: Option<serde_json::Value>,
    metadata: Option<(&str, &str)>,
) -> (u16, serde_json::Value) {
    let mut request = if let Some(body) = body {
        client
            .post(format!("{base}{path}"))
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .body(serde_json::to_vec(&body).unwrap())
    } else {
        client.get(format!("{base}{path}"))
    }
    .bearer_auth(secret);
    if let Some((session, incarnation)) = metadata {
        request = request
            .header("x-agentscm-session", session)
            .header("x-agentscm-incarnation", incarnation);
    }
    let reply = request.send().await.unwrap();
    let status = reply.status().as_u16();
    let bytes = reply.bytes().await.unwrap();
    (status, serde_json::from_slice(&bytes).unwrap())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn native_http_protocol_fences_surviving_transport_and_exposes_work_arrays() {
    use agentscm::backend::LocalBackend;
    use agentscm::hub::{HubState, router};
    use serde_json::json;
    let db = Db::sqlite();
    let store = db.open();
    let owner = store
        .upsert_user(
            &ForgeIdentity {
                provider: "github".into(),
                host: "github.com".into(),
                user_id: 1,
            },
            "alice",
        )
        .unwrap();
    let (token, secret, _) = mint_bound_agent_token(
        &*store,
        &owner.id,
        "claude",
        REPO,
        REPOSITORY_ID,
        TTL,
        false,
    )
    .unwrap();
    let alice = Agent { token };
    let objects = tempfile::tempdir().unwrap();
    let app = router(HubState::new(
        store.clone(),
        Arc::new(LocalBackend::open(objects.path()).unwrap()),
    ));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    let client = reqwest::Client::new();
    let secret = secret.expose();
    let (status, identity) =
        protocol_http(&client, &base, secret, "/v1/hub/whoami", None, None).await;
    assert_eq!(status, 200);
    assert_eq!(identity["cooperation_protocol"], 1);
    let now = unix_now();
    let item = leading(&*store, &alice, "s-a", "src", now);
    let (status, registered) = protocol_http(
        &client,
        &base,
        secret,
        "/v1/hub/cooperation/register",
        Some(serde_json::to_value(registration("s-a", "one", None, now + 1)).unwrap()),
        None,
    )
    .await;
    assert_eq!(status, 200, "{registered}");
    let view: LifecycleView = serde_json::from_value(registered).unwrap();
    let rpc = json!({"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"agentscm_start","arguments":{"session":"s-a","task":"stale request","paths":["src/lib.rs"]}}});
    let (_, mismatch) = protocol_http(
        &client,
        &base,
        secret,
        "/mcp",
        Some(rpc.clone()),
        Some(("other-session", &view.incarnation)),
    )
    .await;
    assert_eq!(mismatch["result"]["isError"], true, "{mismatch}");
    assert_eq!(
        store
            .lifecycle_get(&alice.token, "s-a", now + 1)
            .unwrap()
            .unwrap()
            .qualification,
        Qualification::Qualified
    );
    let (status, ended) = protocol_http(
        &client,
        &base,
        secret,
        "/v1/hub/hook/claude",
        Some(json!({"hook_event_name":"SessionEnd","session_id":"s-a"})),
        Some(("s-a", &view.incarnation)),
    )
    .await;
    assert_eq!(status, 200, "{ended}");
    let (status, state) = protocol_http(
        &client,
        &base,
        secret,
        "/v1/hub/cooperation/state",
        Some(json!({"session":"s-a"})),
        None,
    )
    .await;
    assert_eq!(status, 200);
    let fences: Vec<WorkFence> = serde_json::from_value(state["work"].clone()).unwrap();
    assert_eq!(fences.len(), 1);
    assert_eq!(fences[0].work_id, item.id);
    let mut end = exit_request(&view, &work(&*store, &alice, &item.id, now + 2), now + 2);
    end.reason = EndReason::ExplicitEnd;
    end.work = fences;
    let (status, terminal) = protocol_http(
        &client,
        &base,
        secret,
        "/v1/hub/cooperation/end",
        Some(serde_json::to_value(end).unwrap()),
        None,
    )
    .await;
    assert_eq!(status, 200, "{terminal}");
    assert_eq!(terminal["lifecycle"]["state"], "terminal");
    let (_, legacy) = protocol_http(&client, &base, secret, "/mcp", Some(rpc.clone()), None).await;
    assert_eq!(legacy["result"]["isError"], true, "{legacy}");
    let (status, next) = protocol_http(
        &client,
        &base,
        secret,
        "/v1/hub/cooperation/register",
        Some(
            serde_json::to_value(registration(
                "s-a",
                "two",
                Some(view.incarnation.clone()),
                now + 3,
            ))
            .unwrap(),
        ),
        None,
    )
    .await;
    assert_eq!(status, 200, "{next}");
    let next: LifecycleView = serde_json::from_value(next).unwrap();
    let before = work(&*store, &alice, &item.id, now + 3);
    let (_, stale) = protocol_http(
        &client,
        &base,
        secret,
        "/mcp",
        Some(rpc),
        Some(("s-a", &view.incarnation)),
    )
    .await;
    assert_eq!(stale["result"]["isError"], true, "{stale}");
    assert_eq!(work(&*store, &alice, &item.id, now + 3), before);
    assert_eq!(
        store
            .lifecycle_get(&alice.token, "s-a", now + 3)
            .unwrap()
            .unwrap()
            .incarnation,
        next.incarnation
    );
    server.abort();
}

fn batch_and_overlap_consent_keep_the_native_fence(db: &Db) {
    use agentscm::hub::work_overlap::WorkOverlapConsentRequest;
    use agentscm::hub::work_recovery::{
        BatchOutcome, BatchTransitionItem, WorkTransitionBatch, apply_batch_with_transport,
    };
    let store = db.open();
    let alice = agent(&*store, 1);
    let bob = agent(&*store, 2);
    let now = unix_now();
    let item = leading(&*store, &alice, "s-a", "src", now);
    let proposal = store
        .work_start(&bob.token, &start("requester", "s-b", "src/**"), now + 1)
        .unwrap()
        .coordination_request
        .unwrap();
    let first = store
        .lifecycle_register(
            &alice.token,
            &registration("s-a", "one", None, now + 2),
            now + 2,
        )
        .unwrap();
    let consent = WorkOverlapConsentRequest {
        request_id: proposal.id,
        target_work_id: item.id.clone(),
        operation_id: "native-consent".into(),
        session: "s-a".into(),
        expected_updated_seq: proposal.updated_seq,
        target_generation: item.generation,
        target_updated_seq: item.updated_seq,
        paths: vec!["src/lib.rs".into()],
    };
    store
        .work_overlap_consent_with_transport(
            &alice.token,
            &consent,
            Some(&first.incarnation),
            now + 3,
        )
        .unwrap();
    let batch = WorkTransitionBatch {
        session: Some("s-a".into()),
        items: vec![BatchTransitionItem {
            work_id: item.id.clone(),
            operation_id: "native-retain".into(),
            generation: item.generation,
            expected_updated_seq: item.updated_seq,
            transition: WorkTransitionKind::Retain,
            reason: "keep deliberately".into(),
            superseded_by: None,
            participants: vec![],
            paths: Vec::new(),
        }],
    };
    let actor = WorkTransitionActor::Agent {
        token: &alice.token,
    };
    let result =
        apply_batch_with_transport(&*store, &actor, &batch, Some(&first.incarnation), now + 4)
            .unwrap();
    assert_eq!(result.items[0].outcome, BatchOutcome::Applied);
    assert_eq!(
        store
            .lifecycle_get(&alice.token, "s-a", now + 4)
            .unwrap()
            .unwrap()
            .qualification,
        Qualification::Qualified
    );
    store
        .lifecycle_register(
            &alice.token,
            &registration("s-a", "two", Some(first.incarnation.clone()), now + 5),
            now + 5,
        )
        .unwrap();
    let before = work(&*store, &alice, &item.id, now + 5);
    let result =
        apply_batch_with_transport(&*store, &actor, &batch, Some(&first.incarnation), now + 6)
            .unwrap();
    assert_eq!(result.items[0].outcome, BatchOutcome::Refused);
    assert_eq!(result.items[0].code.as_deref(), Some("refused"));
    let Err(HubError::Rejected(rejection)) = store.work_overlap_consent_with_transport(
        &alice.token,
        &consent,
        Some(&first.incarnation),
        now + 6,
    ) else {
        panic!("old consent must fail incarnation fence");
    };
    assert_eq!(rejection.code(), ErrorCode::InvalidOrChanged);
    assert_eq!(work(&*store, &alice, &item.id, now + 6), before);
    let owner = store.user_by_id(&item.owner).unwrap().unwrap();
    let policy = WorkTransitionActor::Policy {
        owner: &owner,
        repository: REPO,
        repository_id: REPOSITORY_ID,
    };
    let complete = WorkTransitionRequest {
        work_id: item.id.clone(),
        operation_id: "native-policy-complete".into(),
        generation: before.generation,
        expected_updated_seq: before.updated_seq,
        session: None,
        transition: WorkTransitionKind::Complete,
        reason: "provider confirmed completion".into(),
        superseded_by: None,
        participants: vec![],
        paths: Vec::new(),
    };
    let Err(HubError::Rejected(rejection)) = store.work_transition_with_guard(
        &policy,
        &complete,
        Some((&alice.token, Some("s-a"), Some(&first.incarnation))),
        now + 7,
    ) else {
        panic!("native-triggered policy completion must reject the old incarnation");
    };
    assert_eq!(rejection.code(), ErrorCode::InvalidOrChanged);
    assert_eq!(work(&*store, &alice, &item.id, now + 7), before);
    assert!(
        !store
            .merge_prompt_claimed(&item.id, "hub_complete")
            .unwrap()
    );
    let current = store
        .lifecycle_get(&alice.token, "s-a", now + 7)
        .unwrap()
        .unwrap();
    let completed = store
        .work_transition_with_guard(
            &policy,
            &complete,
            Some((&alice.token, Some("s-a"), Some(&current.incarnation))),
            now + 8,
        )
        .unwrap();
    assert_eq!(completed.item.state, WorkState::Finished);
    assert!(
        store
            .merge_prompt_claimed(&item.id, "hub_complete")
            .unwrap()
    );
    let replay = store
        .work_transition_with_guard(
            &policy,
            &complete,
            Some((&alice.token, Some("s-a"), Some(&current.incarnation))),
            now + 9,
        )
        .unwrap();
    assert!(replay.receipt.unwrap().replayed);
}

fn legacy_end_sentinel_refuses_a_lifecycle_registered_after_entry(db: &Db) {
    use agentscm::hub::session_lifecycle::EXPECT_NO_LIFECYCLE;
    let store = db.open();
    let alice = agent(&*store, 1);
    let now = unix_now();
    assert!(
        store
            .lifecycle_check(&alice.token, "legacy", Some(EXPECT_NO_LIFECYCLE), now)
            .unwrap()
            .is_none()
    );
    let first = leading(&*store, &alice, "legacy", "legacy-src", now);
    let paused = store
        .work_checkpoint_with_transport(
            &alice.token,
            &checkpoint(&first, CheckpointKind::Pause, "old-end"),
            Some(EXPECT_NO_LIFECYCLE),
            now + 2,
        )
        .unwrap()
        .item;
    assert_eq!(paused.state, WorkState::Paused);
    let active = leading(&*store, &alice, "s-a", "src", now + 3);
    let current = store
        .lifecycle_register(
            &alice.token,
            &registration("s-a", "new-start", None, now + 5),
            now + 5,
        )
        .unwrap();
    let before_work = work(&*store, &alice, &active.id, now + 6);
    let before = events(&*store);
    assert!(
        store
            .work_checkpoint_with_transport(
                &alice.token,
                &checkpoint(&active, CheckpointKind::Pause, "late-legacy-end"),
                Some(EXPECT_NO_LIFECYCLE),
                now + 6
            )
            .is_err()
    );
    assert_eq!(events(&*store), before);
    assert_eq!(work(&*store, &alice, &active.id, now + 6), before_work);
    assert_eq!(
        store
            .lifecycle_get(&alice.token, "s-a", now + 6)
            .unwrap()
            .unwrap(),
        current
    );
    let end = LifecycleEnd {
        session: "s-a".into(),
        incarnation: current.incarnation,
        operation_id: "sentinel-terminal".into(),
        observed_at: now + 7,
        reason: EndReason::ExplicitEnd,
        work: store
            .lifecycle_fences(&alice.token, "s-a", now + 7)
            .unwrap(),
    };
    store.lifecycle_end(&alice.token, &end, now + 7).unwrap();
    assert!(
        store
            .lifecycle_check(&alice.token, "s-a", Some(EXPECT_NO_LIFECYCLE), now + 8)
            .is_err(),
        "terminal registration also excludes a legacy entry fence"
    );
}

fn native_end_includes_more_than_five_old_paused_work_items(db: &Db) {
    let store = db.open();
    let alice = agent(&*store, 1);
    let now = unix_now();
    let mut paused = Vec::new();
    for n in 0..6 {
        let item = store
            .work_start(
                &alice.token,
                &start(&format!("old-{n}"), "s-a", &format!("old-{n}/**")),
                now,
            )
            .unwrap()
            .item;
        paused.push(
            store
                .work_checkpoint(
                    &alice.token,
                    &checkpoint(&item, CheckpointKind::Pause, &format!("pause-{n}")),
                    now + 1,
                )
                .unwrap()
                .item,
        );
    }
    let active = leading(&*store, &alice, "s-a", "src", now + 2);
    let current = store
        .lifecycle_register(
            &alice.token,
            &registration("s-a", "many-paused", None, now + 4),
            now + 4,
        )
        .unwrap();
    let fences = store
        .lifecycle_fences(&alice.token, "s-a", now + 5)
        .unwrap();
    assert_eq!(fences.len(), 7);
    let end = LifecycleEnd {
        session: "s-a".into(),
        incarnation: current.incarnation,
        operation_id: "many-paused-end".into(),
        observed_at: now + 5,
        reason: EndReason::ExplicitEnd,
        work: fences,
    };
    let result = store.lifecycle_end(&alice.token, &end, now + 5).unwrap();
    assert_eq!(result.paused_work, vec![active.id]);
    for item in paused {
        assert_eq!(
            work(&*store, &alice, &item.id, now + 6),
            item,
            "historical paused Work stays unchanged"
        );
    }
    assert_eq!(result.lifecycle.state, LifecycleState::Terminal);
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
    legacy_end_sentinel_refuses_a_lifecycle_registered_after_entry,
    native_end_includes_more_than_five_old_paused_work_items,
    batch_and_overlap_consent_keep_the_native_fence,
    ending_pauses_the_work_and_releases_the_intent_together,
    a_failed_end_writes_nothing_and_a_retry_completes_it,
    an_end_after_the_old_two_step_crash_finishes_the_pause,
    blocked_work_stays_blocked_and_the_intent_is_released,
    an_edit_after_the_row_is_removed_takes_the_title_of_the_led_work,
    settled_work_is_left_as_it_is_and_the_intent_is_released,
    a_session_with_no_work_still_releases_its_intent,
    a_retained_reservation_stays_retained,
    an_unnamed_end_resolves_the_session_like_end_session,
    a_token_that_can_no_longer_act_on_work_still_ends_its_session,
    a_hooks_end_leaves_work_with_a_live_hold_as_it_is,
    observed_exit_pauses_once_and_replay_does_not_renew,
    observed_exit_preserves_blocked_report_and_retained_authority,
    legacy_calls_make_mixed_sticky_and_terminal_cannot_be_resurrected,
    registration_and_qualification_replay_do_not_touch_activity,
    observed_exit_fences_changed_work_and_foreign_tokens,
    observed_exit_failure_rolls_back_work_intent_and_terminal_fence,
    lifecycle_mutation_rejects_readonly_and_revoked_tokens,
    native_incarnation_fences_canonical_default_alias_and_ends_its_work,
    present_transport_selects_latest_native_in_an_ambiguous_domain,
);
