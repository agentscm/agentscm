//! Ending a session releases its intent and pauses the Work it leads in one transaction. Every
//! case runs on SQLite and, when configured, PostgreSQL.
use crate::hub_pg;

use std::sync::Arc;

use agentscm::hub::HubError;
use agentscm::hub::connection::NewConnection;
use agentscm::hub::session::{SESSION_REQUIRED, SessionCall, Touched};
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
        .live_intents(REPO, now, 10)
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
        .end_session_pausing(&alice.token, Some("s-a"), "hook:claude", now + 10)
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
        let failed = store.end_session_pausing(&alice.token, Some("s-a"), "hook:claude", now + 10);
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
        .end_session_pausing(&alice.token, Some("s-a"), "hook:claude", now + 11)
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
        .end_session_pausing(&alice.token, Some("s-a"), "hook:claude", now + 12)
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
        .end_session_pausing(&alice.token, Some("s-a"), "hook:claude", now + 6)
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
    let Err(HubError::Invalid(message)) =
        store.end_session_pausing(&alice.token, None, "mcp", now + 3)
    else {
        panic!("an ambiguous session must be refused");
    };
    assert_eq!(message, SESSION_REQUIRED);
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
);
