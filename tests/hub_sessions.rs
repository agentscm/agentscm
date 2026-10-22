//! Sessions: store cases run against SQLite always and PostgreSQL when
//! AGENTSCM_TEST_DATABASE_URL is set; the advice flow runs on both the same way.
#[path = "common/hub_pg.rs"]
mod hub_pg;

use std::sync::Arc;

use agentscm::backend::LocalBackend;
use agentscm::hub::intent::NewIntent;
use agentscm::hub::session::{
    DEFAULT_SESSION, HOLD_CAP_SECONDS, SESSION_TTL_SECONDS, SessionCall, Touched, announce_for,
    done_for, others_for, status_for,
};
use agentscm::hub::store::{
    self, ForgeIdentity, Principal, Store, agent_token_digest, mint_agent_token,
    mint_bound_agent_token, unix_now,
};
use agentscm::hub::{HubError, HubState};
use hub_pg::PgSchema;
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
    secret: String,
}

fn agent_in(store: &dyn Store, repo: &str, user_id: i64, name: &str) -> Agent {
    let identity = ForgeIdentity {
        provider: "github".into(),
        host: "github.com".into(),
        user_id,
    };
    let owner = store
        .upsert_user(&identity, &format!("user{user_id}"))
        .unwrap();
    let (token, secret) = mint_agent_token(store, &owner.id, name, repo, TTL).unwrap();
    Agent {
        token,
        secret: secret.expose().to_owned(),
    }
}

fn agent(store: &dyn Store, user_id: i64, name: &str) -> Agent {
    agent_in(store, REPO, user_id, name)
}

fn call(via: &str, session: Option<&str>, paths: &[&str]) -> SessionCall {
    SessionCall {
        session: session.map(str::to_owned),
        via: via.into(),
        task: None,
        paths: paths.iter().map(|path| (*path).to_owned()).collect(),
        deny: false,
        report: true,
    }
}

/// A hook call before an edit, which may be refused.
fn pre_edit(session: &str, paths: &[&str]) -> SessionCall {
    SessionCall {
        deny: true,
        ..call("hook:claude", Some(session), paths)
    }
}

fn hook_touch(store: &dyn Store, who: &Agent, session: &str, paths: &[&str], now: i64) -> Touched {
    store
        .touch_session(&who.token, &pre_edit(session, paths), now)
        .unwrap()
}

/// The data of the events of one kind, oldest first.
fn event_data(store: &dyn Store, kind: &str) -> Vec<serde_json::Value> {
    let events = store.events_after(0, 1000).unwrap();
    events
        .into_iter()
        .filter(|event| event.kind == kind)
        .map(|event| serde_json::from_str(&event.data).unwrap())
        .collect()
}

fn touch(store: &dyn Store, who: &Agent, paths: &[&str], now: i64) -> Touched {
    store
        .touch_session(&who.token, &call("mcp", None, paths), now)
        .unwrap()
}

fn intent_id(touched: &Touched) -> String {
    touched.intent.as_ref().unwrap().id.clone()
}

fn kinds(store: &dyn Store, prefix: &str) -> Vec<String> {
    let events = store.events_after(0, 1000).unwrap();
    events
        .into_iter()
        .filter(|event| event.kind.starts_with(prefix))
        .map(|event| event.kind)
        .collect()
}

fn paths_accumulate_in_one_intent(db: &Db) {
    let store = db.open();
    let alice = agent(&*store, 1, "a");
    let first = touch(&*store, &alice, &["src/a.rs"], T0);
    assert_eq!(first.session, DEFAULT_SESSION);
    assert_eq!(first.added, ["src/a.rs"]);
    let second = touch(&*store, &alice, &["src/b.rs", "docs/a.md"], T0 + 10);
    assert_eq!(intent_id(&first), intent_id(&second));
    assert_eq!(second.added, ["docs/a.md", "src/b.rs"]);
    let intent = second.intent.unwrap();
    assert_eq!(intent.paths, ["docs/a.md", "src/a.rs", "src/b.rs"]);
    assert_eq!(intent.task, "task unknown");
    let live = store.live_intents(REPO, T0 + 10, 10).unwrap();
    assert_eq!(live.len(), 1);
    assert_eq!(live[0].id, intent.id);
    // A task replaces the old one and is kept by later calls.
    let named = SessionCall {
        task: Some("fix billing".into()),
        ..call("mcp", None, &[])
    };
    let task = store.touch_session(&alice.token, &named, T0 + 20).unwrap();
    assert_eq!(task.intent.unwrap().task, "fix billing");
    assert_eq!(
        touch(&*store, &alice, &[], T0 + 30).intent.unwrap().task,
        "fix billing"
    );
}

fn only_new_specs_are_added_and_reported_once(db: &Db) {
    let store = db.open();
    let alice = agent(&*store, 1, "a");
    let bob = agent(&*store, 2, "b");
    touch(&*store, &alice, &["src/a.rs", "src/other.rs"], T0);
    let first = touch(&*store, &bob, &["src/a.rs", "lib/free.rs"], T0 + 1);
    assert_eq!(first.added, ["lib/free.rs", "src/a.rs"]);
    assert_eq!(first.overlaps.len(), 1);
    assert_eq!(first.overlaps[0].your_paths, ["src/a.rs"]);
    assert_eq!(first.overlaps[0].their_paths, ["src/a.rs"]);
    let again = touch(&*store, &bob, &["src/a.rs"], T0 + 2);
    assert!(again.added.is_empty() && again.overlaps.is_empty());
    // Adding a different overlapping file reports only that one.
    let more = touch(&*store, &bob, &["src/other.rs"], T0 + 3);
    assert_eq!(more.overlaps.len(), 1);
    assert_eq!(more.overlaps[0].your_paths, ["src/other.rs"]);
}

fn heartbeat_renews_the_expiry(db: &Db) {
    let store = db.open();
    let alice = agent(&*store, 1, "a");
    let first = touch(&*store, &alice, &["a.rs"], T0);
    assert_eq!(first.intent.unwrap().expires_at, T0 + SESSION_TTL_SECONDS);
    let beat = touch(&*store, &alice, &[], T0 + 600);
    assert!(beat.added.is_empty());
    assert_eq!(
        beat.intent.unwrap().expires_at,
        T0 + 600 + SESSION_TTL_SECONDS
    );
    let later = T0 + 600 + SESSION_TTL_SECONDS - 1;
    assert_eq!(store.live_intents(REPO, later, 10).unwrap().len(), 1);
    // A session without paths has no intent and does not list.
    let bob = agent(&*store, 2, "b");
    assert!(touch(&*store, &bob, &[], T0).intent.is_none());
    assert_eq!(store.live_intents(REPO, T0, 10).unwrap().len(), 1);
}

fn expired_intent_is_not_revived(db: &Db) {
    let store = db.open();
    let alice = agent(&*store, 1, "a");
    let bob = agent(&*store, 2, "b");
    let held = NewIntent {
        id: format!("int_{:032x}", 1),
        task: "other".into(),
        branch: None,
        base: None,
        machine: None,
        paths: vec!["old.rs".into()],
        ttl_seconds: 3600,
    };
    store.announce_intent(&bob.token, &held, T0).unwrap();
    let first = touch(&*store, &alice, &["old.rs"], T0);
    assert_eq!(first.overlaps.len(), 1);
    let late = T0 + SESSION_TTL_SECONDS + 5;
    assert!(store.live_intents(REPO, late, 10).unwrap().len() == 1);
    // The old paths are not carried over: the call starts a fresh intent and is warned again.
    let fresh = touch(&*store, &alice, &["old.rs", "new.rs"], late);
    assert_ne!(intent_id(&first), intent_id(&fresh));
    assert_eq!(fresh.intent.as_ref().unwrap().paths, ["new.rs", "old.rs"]);
    assert_eq!(fresh.added, ["new.rs", "old.rs"]);
    assert_eq!(fresh.overlaps.len(), 1);
    assert_eq!(store.live_intents(REPO, late, 10).unwrap().len(), 2);
    // A heartbeat on an expired session creates nothing.
    let much_later = late + SESSION_TTL_SECONDS + 5;
    let beat = touch(&*store, &alice, &[], much_later);
    assert!(beat.intent.is_none() && beat.added.is_empty());
    assert_eq!(store.live_intents(REPO, much_later, 10).unwrap().len(), 1);
}

fn done_releases_and_a_later_touch_starts_afresh(db: &Db) {
    let store = db.open();
    let alice = agent(&*store, 1, "a");
    let first = touch(&*store, &alice, &["a.rs"], T0);
    assert!(store.end_session(&alice.token, None, T0 + 1).unwrap());
    assert!(store.live_intents(REPO, T0 + 1, 10).unwrap().is_empty());
    assert!(!store.end_session(&alice.token, None, T0 + 2).unwrap());
    let second = touch(&*store, &alice, &["a.rs"], T0 + 3);
    assert_ne!(intent_id(&first), intent_id(&second));
    assert_eq!(second.added, ["a.rs"]);
    assert_eq!(
        kinds(&*store, "session_"),
        ["session_started", "session_ended", "session_started"]
    );
    // An intent released through the intent API also restarts the session.
    store
        .release_intent(&alice.token, &intent_id(&second), T0 + 4)
        .unwrap();
    let third = touch(&*store, &alice, &["b.rs"], T0 + 5);
    assert_ne!(intent_id(&second), intent_id(&third));
    assert_eq!(third.intent.unwrap().paths, ["b.rs"]);
}

fn the_default_session_joins_a_recent_hook_session(db: &Db) {
    let store = db.open();
    let alice = agent(&*store, 1, "a");
    let mcp_only = touch(&*store, &alice, &["a.rs"], T0);
    assert_eq!(mcp_only.session, DEFAULT_SESSION);
    let hook = store
        .touch_session(
            &alice.token,
            &call("hook:claude-code", Some("s-1"), &["b.rs"]),
            T0 + 10,
        )
        .unwrap();
    assert_eq!(hook.session, "s-1");
    let joined = touch(&*store, &alice, &["c.rs"], T0 + 20);
    assert_eq!(joined.session, "s-1");
    assert_eq!(joined.via, ["hook:claude-code", "mcp"]);
    assert_eq!(intent_id(&joined), intent_id(&hook));
    // Past the TTL the hook session is no longer recent.
    let late = T0 + 20 + SESSION_TTL_SECONDS + 1;
    assert_eq!(touch(&*store, &alice, &[], late).session, DEFAULT_SESSION);
    assert!(store.end_session(&alice.token, Some("s-1"), late).unwrap());
    assert!(!store.end_session(&alice.token, Some("s-1"), late).unwrap());
}

fn an_ambiguous_default_stays_default(db: &Db) {
    let store = db.open();
    let alice = agent(&*store, 1, "a");
    let hook = |session: &str, now| {
        let call = call("hook:claude-code", Some(session), &["a.rs"]);
        store.touch_session(&alice.token, &call, now).unwrap()
    };
    hook("s-1", T0);
    assert_eq!(touch(&*store, &alice, &[], T0 + 1).session, "s-1");
    hook("s-2", T0 + 2);
    assert_eq!(touch(&*store, &alice, &[], T0 + 3).session, DEFAULT_SESSION);
    // Neither hook session is ended by a done that names none.
    assert!(store.end_session(&alice.token, None, T0 + 4).unwrap());
    assert!(
        store
            .end_session(&alice.token, Some("s-1"), T0 + 5)
            .unwrap()
    );
    assert!(
        store
            .end_session(&alice.token, Some("s-2"), T0 + 5)
            .unwrap()
    );
}

fn a_first_hook_adopts_the_recent_mcp_default(db: &Db) {
    let store = db.open();
    let alice = agent(&*store, 1, "a");
    let started = touch(&*store, &alice, &["a.rs"], T0);
    let hook = store
        .touch_session(
            &alice.token,
            &call("hook:claude-code", Some("s-1"), &["b.rs"]),
            T0 + 10,
        )
        .unwrap();
    assert_eq!(hook.session, "s-1");
    assert_eq!(hook.via, ["hook:claude-code", "mcp"]);
    assert_eq!(intent_id(&hook), intent_id(&started));
    assert_eq!(hook.intent.unwrap().paths, ["a.rs", "b.rs"]);
    assert_eq!(hook.added, ["b.rs"]);
    assert_eq!(
        kinds(&*store, "session_"),
        ["session_started", "session_connected"]
    );
    // The default session was renamed, not copied: nothing is left to end under it.
    assert!(
        !store
            .end_session(&alice.token, Some(DEFAULT_SESSION), T0 + 11)
            .unwrap()
    );

    // An mcp default that is no longer recent is left alone.
    let bob = agent(&*store, 2, "b");
    let old = touch(&*store, &bob, &["a.rs"], T0);
    let late = T0 + SESSION_TTL_SECONDS + 1;
    let hook = store
        .touch_session(
            &bob.token,
            &call("hook:claude-code", Some("s-9"), &["b.rs"]),
            late,
        )
        .unwrap();
    assert_ne!(intent_id(&hook), intent_id(&old));
    assert_eq!(hook.via, ["hook:claude-code"]);
}

fn via_accumulates_and_is_listed(db: &Db) {
    let store = db.open();
    let alice = agent(&*store, 1, "a");
    let bob = agent(&*store, 2, "b");
    let rest = NewIntent {
        id: format!("int_{:032x}", 9),
        task: "rest".into(),
        branch: None,
        base: None,
        machine: None,
        paths: vec!["z.rs".into()],
        ttl_seconds: 600,
    };
    store.announce_intent(&bob.token, &rest, T0).unwrap();
    let hook = call("hook:codex", Some("s"), &["a.rs"]);
    store.touch_session(&alice.token, &hook, T0).unwrap();
    let both = store
        .touch_session(&alice.token, &call("mcp", Some("s"), &[]), T0 + 1)
        .unwrap();
    assert_eq!(both.via, ["hook:codex", "mcp"]);
    let live = store.live_intents(REPO, T0 + 2, 10).unwrap();
    let via = |id: &str| live.iter().find(|i| i.id == id).unwrap().via.clone();
    assert_eq!(via(&intent_id(&both)), ["hook:codex", "mcp"]);
    assert!(via(&rest.id).is_empty());
    assert_eq!(
        kinds(&*store, "session_"),
        ["session_started", "session_connected"]
    );
    // Repeating a mode adds no event.
    store.touch_session(&alice.token, &hook, T0 + 3).unwrap();
    assert_eq!(kinds(&*store, "session_").len(), 2);
}

fn notices_piggyback_once(db: &Db) {
    let store = db.open();
    let alice = agent(&*store, 1, "a");
    let bob = agent(&*store, 2, "b");
    touch(&*store, &alice, &["src/a.rs"], T0);
    assert!(touch(&*store, &alice, &[], T0 + 1).notices.is_empty());
    touch(&*store, &bob, &["src/a.rs"], T0 + 2);
    let got = touch(&*store, &alice, &[], T0 + 3);
    assert_eq!(got.notices.len(), 1);
    assert_eq!(got.notices[0].kind, "intent_notice");
    assert!(touch(&*store, &alice, &[], T0 + 4).notices.is_empty());
    // Bob, who started later, gets none.
    assert!(touch(&*store, &bob, &[], T0 + 5).notices.is_empty());
}

fn inactive_tokens_are_refused(db: &Db) {
    let store = db.open();
    let revoked = agent(&*store, 1, "revoked");
    let expired = agent(&*store, 2, "expired");
    touch(&*store, &revoked, &["a.rs"], T0);
    store.revoke_agent_token(&revoked.token).unwrap();
    let before = store.events_after(0, 1000).unwrap().len();
    let later = unix_now() + TTL + 10;
    for (who, now) in [(&revoked, T0 + 1), (&expired, later)] {
        let result = store.touch_session(&who.token, &call("mcp", None, &["b.rs"]), now);
        assert!(matches!(result, Err(HubError::Unauthorized)));
        let result = store.end_session(&who.token, None, now);
        assert!(matches!(result, Err(HubError::Unauthorized)));
    }
    assert_eq!(store.events_after(0, 1000).unwrap().len(), before);
}

fn the_path_cap_keeps_the_call_paths(db: &Db) {
    let store = db.open();
    let alice = agent(&*store, 1, "a");
    let many: Vec<String> = (0..50).map(|i| format!("old/{i:03}.rs")).collect();
    let many: Vec<&str> = many.iter().map(String::as_str).collect();
    let first = touch(&*store, &alice, &many, T0);
    assert_eq!(first.intent.unwrap().paths.len(), 50);
    let next = touch(&*store, &alice, &["new/a.rs", "new/b.rs"], T0 + 1);
    assert_eq!(next.added, ["new/a.rs", "new/b.rs"]);
    let paths = next.intent.unwrap().paths;
    assert_eq!(paths.len(), 50);
    assert!(paths.contains(&"new/a.rs".to_owned()) && paths.contains(&"new/b.rs".to_owned()));
}

fn sessions_hold_files_and_never_the_defaults(db: &Db) {
    let store = db.open();
    let alice = agent(&*store, 1, "a");
    let bob = agent(&*store, 2, "b");
    let result = store.touch_session(&alice.token, &call("mcp", None, &["src/**"]), T0);
    assert!(matches!(result, Err(HubError::Invalid(_))));
    assert!(kinds(&*store, "session_").is_empty());
    // Lock files, generated files and vendored directories are dropped from the call.
    let dropped = [
        "Cargo.lock",
        "web/node_modules/x.js",
        "dist/b.js",
        "x.min.js",
    ];
    let first = touch(&*store, &alice, &[&dropped[..], &["src/a.rs"]].concat(), T0);
    assert_eq!(first.added, ["src/a.rs"]);
    assert_eq!(first.intent.unwrap().paths, ["src/a.rs"]);
    // Nobody is refused or warned about them either.
    let mut held = vec!["Cargo.lock", "web/node_modules/x.js"];
    held.push("src/a.rs");
    touch(&*store, &alice, &held, T0 + 1);
    let edit = hook_touch(&*store, &bob, "s", &dropped, T0 + 2);
    assert!(edit.denied.is_empty() && edit.added.is_empty() && edit.intent.is_none());
}

fn the_first_contested_edit_is_refused_and_a_repeat_passes(db: &Db) {
    let store = db.open();
    let alice = agent(&*store, 1, "a");
    let bob = agent(&*store, 2, "b");
    touch(&*store, &alice, &["src/a.rs"], T0);

    let refused = hook_touch(&*store, &bob, "s", &["src/a.rs", "src/b.rs"], T0 + 1);
    assert_eq!(refused.denied.len(), 1);
    assert_eq!(refused.denied[0].your_paths, ["src/a.rs"]);
    assert_eq!(refused.denied[0].their_paths, ["src/a.rs"]);
    assert_eq!(refused.denied[0].intent.token, alice.token);
    // Nothing is held for a refused call, not even the file nobody else holds.
    assert!(refused.added.is_empty() && refused.intent.is_none() && refused.overlaps.is_empty());
    assert_eq!(store.live_intents(REPO, T0 + 1, 10).unwrap().len(), 1);
    // The holder is not told: bob never started on the file.
    assert!(touch(&*store, &alice, &[], T0 + 2).notices.is_empty());
    let denied = event_data(&*store, "edit_denied");
    assert_eq!(denied.len(), 1);
    assert_eq!(denied[0]["paths"], serde_json::json!(["src/a.rs"]));
    assert_eq!(denied[0]["holders"].as_array().unwrap().len(), 1);
    assert_eq!(denied[0]["session"], "s");

    // The same edit again goes through: held, and no overlap is reported a second time.
    let again = hook_touch(&*store, &bob, "s", &["src/a.rs", "src/b.rs"], T0 + 3);
    assert!(again.denied.is_empty() && again.overlaps.is_empty());
    assert_eq!(again.repeated, ["src/a.rs"]);
    assert_eq!(again.added, ["src/a.rs", "src/b.rs"]);
    assert_eq!(again.intent.unwrap().paths, ["src/a.rs", "src/b.rs"]);
    assert_eq!(event_data(&*store, "edit_repeated").len(), 1);
    assert_eq!(touch(&*store, &alice, &[], T0 + 4).notices.len(), 1);
    // It left the refused set: nothing is repeated any more.
    let third = hook_touch(&*store, &bob, "s", &["src/a.rs"], T0 + 5);
    assert!(third.repeated.is_empty() && third.added.is_empty());

    // A refused call still heartbeats the intent the session holds.
    touch(&*store, &alice, &["src/c.rs"], T0 + 6);
    let beat = hook_touch(&*store, &bob, "s", &["src/c.rs"], T0 + 600);
    assert_eq!(beat.denied.len(), 1);
    assert_eq!(
        beat.intent.unwrap().expires_at,
        T0 + 600 + SESSION_TTL_SECONDS
    );
    // Calls that cannot be refused (MCP) are warned, as before.
    let carol = agent(&*store, 3, "c");
    let warned = touch(&*store, &carol, &["src/c.rs"], T0 + 601);
    assert!(warned.denied.is_empty() && warned.overlaps.len() == 1);
}

fn a_refused_file_stays_acknowledged_after_its_hold_lapses(db: &Db) {
    let store = db.open();
    let alice = agent(&*store, 1, "a");
    let bob = agent(&*store, 2, "b");
    touch(&*store, &alice, &["a.rs"], T0);
    assert_eq!(
        hook_touch(&*store, &bob, "s", &["a.rs"], T0 + 1)
            .denied
            .len(),
        1
    );
    assert_eq!(
        hook_touch(&*store, &bob, "s", &["a.rs"], T0 + 2).repeated,
        ["a.rs"]
    );
    // Bob goes idle and his hold lapses while alice keeps hers.
    touch(&*store, &alice, &[], T0 + 1000);
    let back = hook_touch(&*store, &bob, "s", &["a.rs"], T0 + 1500);
    assert!(back.denied.is_empty(), "refused twice");
    assert_eq!(back.added, ["a.rs"]);
    assert_eq!(event_data(&*store, "edit_denied").len(), 1);
}

fn every_touch_keeps_last_active_fresh(db: &Db) {
    let store = db.open();
    let alice = agent(&*store, 1, "a");
    touch(&*store, &alice, &["a.rs"], T0);
    // Past minute 20 the hold cap pins the expiry, so nothing else would be written.
    for minute in 1..=5 {
        touch(&*store, &alice, &[], T0 + minute * 300);
    }
    let live = store.live_intents(REPO, T0 + 1500, 10).unwrap();
    assert_eq!(live[0].updated_at, T0 + 1500);
}

fn only_session_intents_refuse(db: &Db) {
    let store = db.open();
    let alice = agent(&*store, 1, "a");
    let bob = agent(&*store, 2, "b");
    let rest = NewIntent {
        id: format!("int_{:032x}", 4),
        task: "rest".into(),
        branch: None,
        base: None,
        machine: None,
        paths: vec!["a.rs".into()],
        ttl_seconds: 600,
    };
    store.announce_intent(&alice.token, &rest, T0).unwrap();
    let edit = hook_touch(&*store, &bob, "s", &["a.rs"], T0 + 1);
    assert!(edit.denied.is_empty() && edit.overlaps.len() == 1);
}

fn no_notice_to_ones_own_other_connection(db: &Db) {
    let store = db.open();
    let alice = agent(&*store, 1, "a");
    let bob = agent(&*store, 2, "b");
    hook_touch(&*store, &alice, "s", &["a.rs"], T0);
    store
        .touch_session(&alice.token, &call("mcp", Some("m"), &["a.rs"]), T0 + 1)
        .unwrap();
    assert!(
        hook_touch(&*store, &alice, "s", &[], T0 + 2)
            .notices
            .is_empty()
    );
    // Another token's announce still notifies.
    store
        .touch_session(&bob.token, &call("mcp", None, &["a.rs"]), T0 + 3)
        .unwrap();
    assert_eq!(
        hook_touch(&*store, &alice, "s", &[], T0 + 4).notices.len(),
        1
    );
}

fn directories_and_other_sessions_files_are_never_refused(db: &Db) {
    let store = db.open();
    let alice = agent(&*store, 1, "a");
    let bob = agent(&*store, 2, "b");
    let rest = NewIntent {
        id: format!("int_{:032x}", 3),
        task: "wide".into(),
        branch: None,
        base: None,
        machine: None,
        paths: vec!["src/**".into(), "lib/b.rs".into()],
        ttl_seconds: 600,
    };
    store.announce_intent(&alice.token, &rest, T0).unwrap();
    touch(&*store, &alice, &["lib/b.rs"], T0);
    // A directory spec overlapping the file is advice, never a refusal; the exact file is.
    let edit = hook_touch(&*store, &bob, "s", &["src/a.rs"], T0 + 1);
    assert!(edit.denied.is_empty() && edit.overlaps.len() == 1);
    let exact = hook_touch(&*store, &bob, "s", &["lib/b.rs"], T0 + 2);
    assert_eq!(exact.denied.len(), 1);
}

fn refusals_are_rate_limited(db: &Db) {
    let store = db.open();
    let alice = agent(&*store, 1, "a");
    let bob = agent(&*store, 2, "b");
    let files = ["f1.rs", "f2.rs", "f3.rs", "f4.rs", "f5.rs"];
    touch(&*store, &alice, &files, T0);
    for (i, file) in files[..3].iter().enumerate() {
        let refused = hook_touch(&*store, &bob, "s", &[file], T0 + 1 + i as i64);
        assert_eq!(refused.denied.len(), 1, "{file}");
    }
    // The fourth in ten minutes passes, with the normal warning.
    let limited = hook_touch(&*store, &bob, "s", &["f4.rs"], T0 + 10);
    assert!(limited.denied.is_empty() && limited.rate_limited);
    assert_eq!(limited.added, ["f4.rs"]);
    assert_eq!(limited.overlaps.len(), 1);
    assert_eq!(event_data(&*store, "edit_denied").len(), 3);
    // Later the window has slid past the first refusals.
    let later = hook_touch(&*store, &bob, "s", &["f5.rs"], T0 + 603);
    assert_eq!(later.denied.len(), 1);
    assert!(!later.rate_limited);
}

fn the_same_token_is_not_refused_by_its_mcp_only_session(db: &Db) {
    let store = db.open();
    let alice = agent(&*store, 1, "a");
    // MCP calls carry no session id, so a named MCP session may be the hook session's agent.
    store
        .touch_session(&alice.token, &call("mcp", Some("m"), &["x.rs"]), T0)
        .unwrap();
    let same = hook_touch(&*store, &alice, "s-1", &["x.rs"], T0 + 1);
    assert!(same.denied.is_empty() && same.overlaps.is_empty());
    assert_eq!(same.added, ["x.rs"]);
    // Two hook sessions of one token are two agents.
    let other = hook_touch(&*store, &alice, "s-2", &["x.rs"], T0 + 2);
    assert_eq!(other.denied.len(), 1);
    // And the MCP-only session is not warned about the hook sessions either.
    let mcp = store
        .touch_session(&alice.token, &call("mcp", Some("m"), &["y.rs"]), T0 + 3)
        .unwrap();
    assert!(mcp.overlaps.is_empty());
    let hook = hook_touch(&*store, &alice, "s-1", &["y.rs"], T0 + 4);
    assert!(hook.denied.is_empty() && hook.overlaps.is_empty());
}

fn a_hold_ends_at_the_cap_or_when_idle(db: &Db) {
    let store = db.open();
    let alice = agent(&*store, 1, "a");
    let first = touch(&*store, &alice, &["a.rs", "b.rs"], T0);
    // Heartbeats renew the hold, but never past 30 minutes from its start.
    let mid = touch(&*store, &alice, &[], T0 + 300);
    assert_eq!(
        mid.intent.unwrap().expires_at,
        T0 + 300 + SESSION_TTL_SECONDS
    );
    let near = touch(&*store, &alice, &[], T0 + 1000);
    assert_eq!(near.intent.unwrap().expires_at, T0 + HOLD_CAP_SECONDS);
    assert!(kinds(&*store, "hold_").is_empty());
    let fresh = touch(&*store, &alice, &["c.rs"], T0 + HOLD_CAP_SECONDS);
    assert_ne!(intent_id(&first), intent_id(&fresh));
    assert_eq!(fresh.intent.as_ref().unwrap().paths, ["c.rs"]);
    let expired = event_data(&*store, "hold_expired");
    assert_eq!(expired.len(), 1);
    assert_eq!(expired[0]["cause"], "cap");
    assert_eq!(expired[0]["intent"], intent_id(&first));
    assert_eq!(expired[0]["session"], DEFAULT_SESSION);

    // Idle: no call for longer than the lifetime.
    let late = T0 + HOLD_CAP_SECONDS + SESSION_TTL_SECONDS + 5;
    let idle = touch(&*store, &alice, &["d.rs"], late);
    assert_ne!(intent_id(&fresh), intent_id(&idle));
    assert_eq!(event_data(&*store, "hold_expired")[1]["cause"], "idle");
    // A released hold is not an expiry.
    store.end_session(&alice.token, None, late + 1).unwrap();
    touch(&*store, &alice, &["e.rs"], late + 2);
    assert_eq!(kinds(&*store, "hold_").len(), 2);
}

fn other_repositories_stay_apart(db: &Db) {
    let store = db.open();
    let alice = agent(&*store, 1, "a");
    let carol = agent_in(&*store, "acme/other", 3, "c");
    touch(&*store, &alice, &["src/a.rs"], T0);
    let theirs = touch(&*store, &carol, &["src/a.rs"], T0 + 1);
    assert!(theirs.overlaps.is_empty());
    assert_eq!(
        store.live_intents("acme/other", T0 + 1, 10).unwrap().len(),
        1
    );
    assert_eq!(store.live_intents(REPO, T0 + 1, 10).unwrap().len(), 1);
    // Sessions are per token: the same name does not collide.
    let same = |who: &Agent| {
        let named = call("mcp", Some("same"), &["x.rs"]);
        store.touch_session(&who.token, &named, T0 + 2).unwrap()
    };
    assert_ne!(intent_id(&same(&alice)), intent_id(&same(&carol)));
}

fn activity_is_scoped_newest_first_and_limited(db: &Db) {
    let store = db.open();
    let alice = agent(&*store, 1, "a");
    let bob = agent(&*store, 2, "b");
    let carol = agent_in(&*store, "acme/other", 3, "c");
    let me = principal(&*store, &alice);
    let held = intent_id(&touch(&*store, &alice, &["src/a.rs"], T0));
    touch(&*store, &carol, &["src/a.rs"], T0 + 1);
    hook_touch(&*store, &bob, "s", &["src/a.rs"], T0 + 2);
    touch(&*store, &bob, &["src/a.rs"], T0 + 3);

    let events = store.repository_events(&me, 100).unwrap();
    let seqs: Vec<i64> = events.iter().map(|e| e.event.seq).collect();
    assert!(seqs.windows(2).all(|pair| pair[0] > pair[1]), "{seqs:?}");
    assert!(events.iter().all(|e| e.token != carol.token));
    let has = |kind: &str, token: &str| {
        events
            .iter()
            .any(|e| e.event.kind == kind && e.token == token)
    };
    assert!(has("session_started", &bob.token) && has("edit_denied", &bob.token));
    // Intent events are attributed to the intent's token, a notice to its recipient.
    assert!(has("intent_announced", &alice.token) && has("intent_notice", &alice.token));
    let notice = events
        .iter()
        .find(|e| e.event.kind == "intent_notice")
        .unwrap();
    assert_eq!(notice.event.subject, held);
    assert_eq!(
        (notice.agent.as_str(), notice.owner.as_str()),
        ("a", "user1")
    );
    // No token creation or other kinds.
    let shown = [
        "session_started",
        "session_connected",
        "session_ended",
        "edit_denied",
        "edit_repeated",
        "hold_expired",
        "intent_announced",
        "intent_released",
        "intent_notice",
        "pr_notice",
        "handoff_request",
    ];
    assert!(
        events
            .iter()
            .all(|e| shown.contains(&e.event.kind.as_str()))
    );
    let two = store.repository_events(&me, 2).unwrap();
    assert_eq!(two, events[..2]);
    let theirs = principal(&*store, &carol);
    assert!(!store.repository_events(&theirs, 100).unwrap().is_empty());

    // Sessions of this repository only, newest first; the edge of the lifetime is included.
    let live = store.live_sessions(&me, T0 + 3, 10).unwrap();
    let tokens: Vec<&str> = live.iter().map(|s| s.token.as_str()).collect();
    assert_eq!(tokens, [&bob.token, &alice.token]);
    let mine = &live[1];
    assert_eq!(mine.intent.as_deref(), Some(held.as_str()));
    assert_eq!((mine.agent.as_str(), mine.owner.as_str()), ("a", "user1"));
    assert_eq!(mine.via, r#"["mcp"]"#);
    // A stable handle that is neither the session id nor the same for the two agents.
    assert_eq!(mine.key.len(), 16);
    assert!(mine.key.bytes().all(|b| b.is_ascii_hexdigit()) && !mine.key.contains(DEFAULT_SESSION));
    assert_ne!(mine.key, live[0].key);
    let again = store.live_sessions(&me, T0 + 3, 10).unwrap();
    assert_eq!(again[1].key, mine.key);
    assert_eq!((mine.created_at, mine.updated_at), (T0, T0));
    assert_eq!(store.live_sessions(&me, T0 + 3, 1).unwrap().len(), 1);
    let at_edge = store
        .live_sessions(&me, T0 + SESSION_TTL_SECONDS, 10)
        .unwrap();
    assert_eq!(at_edge.len(), 2);
    let after = store
        .live_sessions(&me, T0 + SESSION_TTL_SECONDS + 1, 10)
        .unwrap();
    assert!(after.iter().all(|s| s.token == bob.token) && after.len() == 1);
}

fn activity_scope_follows_the_forge_repository_and_host(db: &Db) {
    let store = db.open();
    let user = |provider: &str, host: &str, id: i64| {
        let identity = ForgeIdentity {
            provider: provider.into(),
            host: host.into(),
            user_id: id,
        };
        store.upsert_user(&identity, &format!("user{id}")).unwrap()
    };
    let mint = |owner: &str, name: &str, repository_id: i64, ttl: i64| {
        let (id, secret, _) =
            mint_bound_agent_token(&*store, owner, name, REPO, repository_id, ttl, false).unwrap();
        Agent {
            token: id,
            secret: secret.expose().to_owned(),
        }
    };
    let (alice, bob, carol) = (
        user("github", "github.com", 1),
        user("github", "github.com", 2),
        user("gitlab", "gitlab.com", 3),
    );
    // The same name, as the forge now knows it: one repository id, a recreated one, another host.
    let (a, b, c) = (
        mint(&alice.id, "a", 10, TTL),
        mint(&bob.id, "b", 11, TTL),
        mint(&carol.id, "c", 10, TTL),
    );
    for (who, path) in [(&a, "a.rs"), (&b, "b.rs"), (&c, "c.rs")] {
        touch(&*store, who, &[path], T0);
    }
    for (who, others) in [(&a, [&b, &c]), (&b, [&a, &c]), (&c, [&a, &b])] {
        let me = principal(&*store, who);
        let sessions = store.live_sessions(&me, T0, 10).unwrap();
        assert_eq!(sessions.len(), 1, "{}", who.token);
        assert_eq!(sessions[0].token, who.token);
        let events = store.repository_events(&me, 100).unwrap();
        assert!(!events.is_empty());
        for other in others {
            assert!(events.iter().all(|e| e.token != other.token));
        }
        // Intent events are scoped the same way, not by repository name alone.
        let intents = events
            .iter()
            .filter(|e| e.event.subject.starts_with("int_"));
        assert!(intents.count() >= 1);
    }
}

fn only_active_tokens_have_live_sessions(db: &Db) {
    let store = db.open();
    let alice = agent(&*store, 1, "a");
    let bob = agent(&*store, 2, "b");
    let carol = agent(&*store, 3, "c");
    let (short, expires_at) = {
        let identity = ForgeIdentity {
            provider: "github".into(),
            host: "github.com".into(),
            user_id: 4,
        };
        let owner = store.upsert_user(&identity, "user4").unwrap();
        let (token, _) = mint_agent_token(&*store, &owner.id, "short", REPO, 60).unwrap();
        let expires_at = store
            .list_agent_tokens(&owner.id)
            .unwrap()
            .into_iter()
            .find(|entry| entry.id == token)
            .unwrap()
            .expires_at;
        (
            Agent {
                token,
                secret: String::new(),
            },
            expires_at,
        )
    };
    // Use the recorded expiry rather than racing a one-second wall-clock token on slow runners.
    let now = expires_at - 1;
    for who in [&alice, &bob, &carol, &short] {
        touch(&*store, who, &["x.rs"], now);
    }
    let me = principal(&*store, &alice);
    let tokens = |at: i64| {
        let mut found: Vec<String> = store
            .live_sessions(&me, at, 10)
            .unwrap()
            .into_iter()
            .map(|s| s.token)
            .collect();
        found.sort();
        found
    };
    let mut all = vec![
        alice.token.clone(),
        bob.token.clone(),
        carol.token.clone(),
        short.token.clone(),
    ];
    all.sort();
    assert_eq!(tokens(now), all);
    // An expired token, a revoked token and a disabled owner drop out.
    all.retain(|token| *token != short.token);
    assert_eq!(tokens(expires_at), all);
    assert!(store.revoke_agent_token(&bob.token).unwrap());
    all.retain(|token| *token != bob.token);
    assert_eq!(tokens(expires_at), all);
    store
        .set_user_disabled(&principal(&*store, &carol).owner.id, true)
        .unwrap();
    all.retain(|token| *token != carol.token);
    assert_eq!(tokens(expires_at), all);
}

fn invalid_calls_are_rejected_without_events(db: &Db) {
    let store = db.open();
    let alice = agent(&*store, 1, "a");
    let long = "s".repeat(129);
    let many: Vec<String> = (0..51).map(|i| format!("p{i}")).collect();
    let many: Vec<&str> = many.iter().map(String::as_str).collect();
    let calls = [
        call("curl", None, &["a"]),
        call("hook:Bad", None, &["a"]),
        call("mcp", Some(&long), &["a"]),
        call("mcp", Some("a\nb"), &["a"]),
        call("mcp", None, &["../a"]),
        call("mcp", None, &many),
        SessionCall {
            task: Some("t\n".into()),
            ..call("mcp", None, &["a"])
        },
    ];
    for bad in calls {
        let result = store.touch_session(&alice.token, &bad, T0);
        assert!(matches!(result, Err(HubError::Invalid(_))), "{bad:?}");
    }
    assert!(matches!(
        store.end_session(&alice.token, Some(&long), T0),
        Err(HubError::Invalid(_))
    ));
    assert!(
        store
            .events_after(0, 10)
            .unwrap()
            .iter()
            .all(|e| !e.kind.starts_with("session_"))
    );
    assert!(store.live_intents(REPO, T0, 10).unwrap().is_empty());
}

fn a_file_held_through_mcp_is_still_refused_on_the_first_hook_edit(db: &Db) {
    let store = db.open();
    let alice = agent(&*store, 1, "a");
    let bob = agent(&*store, 2, "b");
    let carol = agent(&*store, 3, "c");
    hook_touch(&*store, &alice, "s", &["src/a.rs"], T0);
    // The hook joins first, then the agent's MCP start names alice's file: held, with the note.
    hook_touch(&*store, &bob, "s", &["src/b.rs"], T0 + 1);
    let claim = store
        .touch_session(&bob.token, &call("mcp", Some("s"), &["src/a.rs"]), T0 + 2)
        .unwrap();
    assert_eq!(claim.added, ["src/a.rs"]);
    assert_eq!(claim.overlaps.len(), 1);
    // The first hook edit of it is still refused once; the repeat passes.
    let edit = hook_touch(&*store, &bob, "s", &["src/a.rs"], T0 + 3);
    assert_eq!(edit.denied.len(), 1);
    let again = hook_touch(&*store, &bob, "s", &["src/a.rs"], T0 + 4);
    assert!(again.denied.is_empty());
    assert_eq!(again.repeated, ["src/a.rs"]);
    assert!(
        hook_touch(&*store, &bob, "s", &["src/a.rs"], T0 + 5)
            .repeated
            .is_empty()
    );

    // MCP before the first hook: the hook adopts the MCP session, and is still refused once.
    touch(&*store, &carol, &["src/a.rs"], T0 + 6);
    let adopted = hook_touch(&*store, &carol, "t", &["src/a.rs"], T0 + 7);
    // One overlap per holder: alice and bob both hold it by now.
    assert_eq!(adopted.denied.len(), 2);
    assert!(adopted.denied.iter().all(|o| o.your_paths == ["src/a.rs"]));

    // A file a refusing edit checked while nobody else held it is never refused later.
    let dave = agent(&*store, 4, "d");
    hook_touch(&*store, &alice, "s", &["src/z.rs"], T0 + 8);
    let late = hook_touch(&*store, &dave, "u", &["src/z.rs"], T0 + 9);
    assert_eq!(late.denied.len(), 1);
    hook_touch(&*store, &dave, "u", &["src/z.rs"], T0 + 10);
    assert!(
        hook_touch(&*store, &alice, "s", &["src/z.rs"], T0 + 11)
            .denied
            .is_empty()
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
    paths_accumulate_in_one_intent,
    only_new_specs_are_added_and_reported_once,
    heartbeat_renews_the_expiry,
    expired_intent_is_not_revived,
    done_releases_and_a_later_touch_starts_afresh,
    the_default_session_joins_a_recent_hook_session,
    an_ambiguous_default_stays_default,
    a_first_hook_adopts_the_recent_mcp_default,
    via_accumulates_and_is_listed,
    notices_piggyback_once,
    inactive_tokens_are_refused,
    the_path_cap_keeps_the_call_paths,
    sessions_hold_files_and_never_the_defaults,
    the_first_contested_edit_is_refused_and_a_repeat_passes,
    a_file_held_through_mcp_is_still_refused_on_the_first_hook_edit,
    a_refused_file_stays_acknowledged_after_its_hold_lapses,
    every_touch_keeps_last_active_fresh,
    only_session_intents_refuse,
    no_notice_to_ones_own_other_connection,
    directories_and_other_sessions_files_are_never_refused,
    refusals_are_rate_limited,
    the_same_token_is_not_refused_by_its_mcp_only_session,
    a_hold_ends_at_the_cap_or_when_idle,
    other_repositories_stay_apart,
    activity_is_scoped_newest_first_and_limited,
    activity_scope_follows_the_forge_repository_and_host,
    only_active_tokens_have_live_sessions,
    invalid_calls_are_rejected_without_events,
);

fn principal(store: &dyn Store, who: &Agent) -> Principal {
    let digest = agent_token_digest(&who.secret).unwrap();
    store.authenticate(&digest, unix_now()).unwrap().unwrap()
}

/// The async entry points MCP and the hooks call, end to end.
async fn advice_flow(state: &HubState, [alice, bob, carol]: [Principal; 3]) {
    let hook = |session: &str, paths: &[&str]| call("hook:claude-code", Some(session), paths);

    let clean = announce_for(state, &alice, hook("s", &["src/a.rs"]))
        .await
        .unwrap();
    assert_eq!((clean.text.as_str(), clean.stop), ("", false));

    // The same exact file: STOP, once.
    let stop = announce_for(state, &bob, call("mcp", None, &["src/a.rs"]))
        .await
        .unwrap();
    assert!(stop.stop);
    assert!(
        stop.text
            .starts_with("AgentSCM: another agent is already working on src/a.rs.")
    );
    assert!(
        stop.text.contains("user1's agent (alice-agent)"),
        "{}",
        stop.text
    );
    let repeat = announce_for(state, &bob, call("mcp", None, &["src/a.rs"]))
        .await
        .unwrap();
    assert_eq!((repeat.text.as_str(), repeat.stop), ("", false));

    // Alice learns of bob at her next call: FYI only.
    let beat = announce_for(state, &alice, hook("s", &[])).await.unwrap();
    assert!(!beat.stop);
    assert!(
        beat.text
            .starts_with("AgentSCM: other work overlaps src/a.rs."),
        "{}",
        beat.text
    );
    assert!(
        beat.text
            .contains("user2's agent (bob-agent) started working on src/a.rs")
    );
    let quiet = announce_for(state, &alice, hook("s", &[])).await.unwrap();
    assert_eq!(quiet.text, "");

    // Sessions hold files; a directory spec (from the intent API) is only ever an overlap.
    let dir_spec = announce_for(state, &carol, call("mcp", None, &["src/**"])).await;
    assert!(matches!(dir_spec, Err(HubError::Invalid(_))));
    let wide = NewIntent {
        id: format!("int_{:032x}", 7),
        task: "wide".into(),
        branch: None,
        base: None,
        machine: None,
        paths: vec!["src/**".into()],
        ttl_seconds: 3600,
    };
    // The blocking Postgres client must not run on the async runtime.
    let (store, token) = (state.store.clone(), carol.token_id.clone());
    tokio::task::spawn_blocking(move || store.announce_intent(&token, &wide, unix_now()))
        .await
        .unwrap()
        .unwrap();
    let inside = announce_for(state, &bob, call("mcp", None, &["src/c/d.rs"]))
        .await
        .unwrap();
    assert!(
        !inside.stop && inside.text.contains("src/c/d.rs") && inside.text.contains("src/**"),
        "{}",
        inside.text
    );

    // A hook call before an edit gets the refusal reason, not advice, and holds nothing.
    let refused = announce_for(
        state,
        &carol,
        SessionCall {
            deny: true,
            ..call("hook:codex", Some("c"), &["src/a.rs"])
        },
    )
    .await
    .unwrap();
    let reason = refused.deny.unwrap();
    assert!(
        reason.starts_with("AgentSCM: this edit to src/a.rs was not applied"),
        "{reason}"
    );
    assert!(
        reason.contains("user1's agent alice-agent, last active"),
        "{reason}"
    );
    assert!(refused.text.is_empty() && !refused.stop);
    assert!(refused.touched.intent.is_none());

    // Status: others and pull requests, without bob's own intent.
    let own = bob_intent(state, &bob).await;
    let others = others_for(state, &bob, Some(&own)).await.unwrap();
    assert!(
        others.starts_with("AgentSCM: 2 other agents working in acme/widgets"),
        "{others}"
    );
    let status = status_for(state, &bob, call("mcp", None, &[]))
        .await
        .unwrap();
    assert!(
        status.contains("2 other agents") && status.ends_with("AgentSCM: no open pull requests.")
    );

    // Done releases; a second done finds nothing.
    assert!(done_for(state, &alice, Some("s".into())).await.unwrap());
    assert!(!done_for(state, &alice, Some("s".into())).await.unwrap());
    let after = others_for(state, &bob, Some(&own)).await.unwrap();
    assert!(
        after.starts_with("AgentSCM: 1 other agent working"),
        "{after}"
    );
    let empty = others_for(state, &alice, None).await.unwrap();
    assert!(empty.starts_with("AgentSCM: 2 other agents"), "{empty}");
}

async fn bob_intent(state: &HubState, bob: &Principal) -> String {
    let beat = announce_for(state, bob, call("mcp", None, &[]))
        .await
        .unwrap();
    beat.touched.intent.unwrap().id
}

fn run_advice_flow(db: &Db) {
    let store = db.open();
    let dir = TempDir::new().unwrap();
    let objects = LocalBackend::open(dir.path().join("objects")).unwrap();
    let state = HubState::new(store.clone(), Arc::new(objects));
    // The Postgres client cannot be dropped inside a runtime, so all stores outlive it.
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    let agents = [(1, "alice-agent"), (2, "bob-agent"), (3, "carol-agent")]
        .map(|(id, name)| principal(&*store, &agent(&*store, id, name)));
    runtime.block_on(advice_flow(&state, agents));
}

#[test]
fn advice_flow_on_sqlite() {
    run_advice_flow(&Db::sqlite());
}

#[test]
fn advice_flow_on_postgres() {
    if let Some(db) = Db::postgres() {
        run_advice_flow(&db);
    }
}
