//! Handoffs: store cases run against SQLite always and PostgreSQL when
//! AGENTSCM_TEST_DATABASE_URL is set.
use crate::hub_pg;

use std::sync::{Arc, Barrier};

use agentscm::hub::HubError;
use agentscm::hub::handoff::{Answered, Asked};
use agentscm::hub::intent::Intent;
use agentscm::hub::session::{HOLD_CAP_SECONDS, SessionCall, Touched};
use agentscm::hub::store::{
    self, ForgeIdentity, Store, agent_token_digest, mint_agent_token, unix_now,
};
use hub_pg::PgSchema;
use serde_json::{Value, json};
use tempfile::TempDir;

const REPO: &str = "acme/widgets";
const T0: i64 = 1000;
const A: &str = "src/a.rs";

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

/// User `n` is `user{n}`; the token is named `agent{n}`.
fn agent(store: &dyn Store, n: i64) -> Agent {
    let identity = ForgeIdentity {
        provider: "github".into(),
        host: "github.com".into(),
        user_id: n,
    };
    let owner = store.upsert_user(&identity, &format!("user{n}")).unwrap();
    let (token, secret) =
        mint_agent_token(store, &owner.id, &format!("agent{n}"), REPO, 3600).unwrap();
    Agent {
        token,
        secret: secret.expose().to_owned(),
    }
}

/// A hook call before an edit, which may be refused.
fn touch_call(session: &str, paths: &[&str]) -> SessionCall {
    SessionCall {
        session: Some(session.to_owned()),
        via: "hook:claude".into(),
        task: None,
        paths: paths.iter().map(|path| (*path).to_owned()).collect(),
        deny: true,
        report: true,
    }
}

fn touch(store: &dyn Store, who: &Agent, session: &str, paths: &[&str], now: i64) -> Touched {
    let call = touch_call(session, paths);
    store.touch_session(&who.token, &call, now).unwrap()
}

fn ask(store: &dyn Store, who: &Agent, session: &str, path: &str, now: i64) -> Asked {
    let session = Some(session);
    store
        .ask_handoff(&who.token, session, path, None, now)
        .unwrap()
}

fn answer(
    store: &dyn Store,
    who: &Agent,
    session: Option<&str>,
    path: &str,
    accept: bool,
    now: i64,
) -> Answered {
    store
        .answer_handoff(&who.token, session, path, accept, None, now)
        .unwrap()
}

fn events(store: &dyn Store, kind: &str) -> Vec<(String, Value)> {
    let all = store.events_after(0, 1000).unwrap();
    all.into_iter()
        .filter(|event| event.kind == kind)
        .map(|event| (event.subject, serde_json::from_str(&event.data).unwrap()))
        .collect()
}

fn live(store: &dyn Store, now: i64) -> Vec<Intent> {
    store.live_intents(REPO, now, 10).unwrap()
}

/// The live paths of the token's intent.
fn held_by(store: &dyn Store, who: &Agent, now: i64) -> Vec<String> {
    let intents = live(store, now);
    let mut mine = intents.iter().filter(|intent| intent.token == who.token);
    mine.next()
        .map_or(Vec::new(), |intent| intent.paths.clone())
}

fn touch_silent(store: &dyn Store, who: &Agent, session: &str, now: i64) -> Touched {
    let call = SessionCall {
        report: false,
        ..touch_call(session, &[])
    };
    store.touch_session(&who.token, &call, now).unwrap()
}

/// Alice's session `ha` holds `paths`; bob's session `hb` was refused `A`.
fn refused(store: &dyn Store, paths: &[&str]) -> (Agent, Agent) {
    let alice = agent(store, 1);
    let bob = agent(store, 2);
    touch(store, &alice, "ha", paths, T0);
    let denied = touch(store, &bob, "hb", &[A], T0 + 1);
    assert_eq!(denied.denied.len(), 1);
    (alice, bob)
}

fn a_grant_moves_the_file_and_is_told_once(db: &Db) {
    let store = db.open();
    let alice = agent(&*store, 1);
    let bob = agent(&*store, 2);
    touch(&*store, &alice, "ha", &[A, "src/b.rs"], T0);
    // The refusal files the request, with the session's task as its note.
    let call = SessionCall {
        task: Some("need it now".into()),
        ..touch_call("hb", &[A])
    };
    assert_eq!(
        store
            .touch_session(&bob.token, &call, T0 + 1)
            .unwrap()
            .requested,
        [A]
    );
    let asked = ask(&*store, &bob, "hb", A, T0 + 2);
    let Asked::AlreadyAsked { holder } = asked else {
        panic!("{asked:?}");
    };
    assert_eq!(
        (holder.agent.as_str(), holder.owner.login.as_str()),
        ("agent1", "user1")
    );
    let requests = events(&*store, "handoff_request");
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].0, bob.token);
    assert_eq!(
        requests[0].1,
        json!({"request": requests[0].1["request"], "paths": [A], "note": "need it now",
            "session": "hb", "auto": true, "to": {"agent": "agent1", "login": "user1"}})
    );
    assert!(
        requests[0].1["request"]
            .as_str()
            .unwrap()
            .starts_with("hof_")
    );

    // The requester hears nothing yet; the holder hears once, and only on its own session.
    assert!(touch(&*store, &bob, "hb", &[], T0 + 3).handoffs.is_empty());
    let other = touch(&*store, &alice, "other", &[], T0 + 3);
    assert!(other.handoffs.is_empty());
    let told = touch(&*store, &alice, "ha", &[], T0 + 4);
    assert_eq!(told.handoffs.len(), 1);
    let note = &told.handoffs[0];
    assert_eq!(
        (note.kind, note.path.as_str(), note.session.as_str()),
        ("request", A, "ha")
    );
    assert_eq!(
        (
            note.agent.as_str(),
            note.login.as_str(),
            note.note.as_deref()
        ),
        ("agent2", "user2", Some("need it now"))
    );
    assert!(
        touch(&*store, &alice, "ha", &[], T0 + 5)
            .handoffs
            .is_empty()
    );

    let granted = answer(&*store, &alice, Some("ha"), A, true, T0 + 6);
    assert_eq!(
        granted,
        Answered::Granted {
            requester: ("agent2".into(), "user2".into())
        }
    );
    assert_eq!(held_by(&*store, &alice, T0 + 6), ["src/b.rs"]);
    assert_eq!(held_by(&*store, &bob, T0 + 6), [A]);
    // A second answer finds nothing.
    assert_eq!(
        answer(&*store, &alice, None, A, true, T0 + 7),
        Answered::NoRequest
    );

    // The requester's retry passes without a refusal; it hears the grant once.
    let retry = touch(&*store, &bob, "hb", &[A], T0 + 8);
    assert!(retry.denied.is_empty() && retry.added.is_empty() && retry.overlaps.is_empty());
    assert_eq!(retry.handoffs.len(), 1);
    let note = &retry.handoffs[0];
    assert_eq!(
        (note.kind, note.agent.as_str(), note.login.as_str()),
        ("grant", "agent1", "user1")
    );
    assert_eq!(note.session, "hb");
    assert!(touch(&*store, &bob, "hb", &[A], T0 + 9).handoffs.is_empty());
    // The holder is told nothing about it, and now it is the holder's file that is contested.
    let back = touch(&*store, &alice, "ha", &[A], T0 + 10);
    assert_eq!(back.denied.len(), 1);

    let grants = events(&*store, "handoff_grant");
    assert_eq!(grants.len(), 1);
    assert_eq!(grants[0].0, alice.token);
    assert_eq!(
        grants[0].1,
        json!({"request": grants[0].1["request"], "paths": [A], "session": "ha",
            "from": {"agent": "agent2", "login": "user2", "token": bob.token}, "reason": null})
    );
    assert!(events(&*store, "handoff_decline").is_empty());

    // The status page sees all three kinds.
    let digest = agent_token_digest(&bob.secret).unwrap();
    let principal = store.authenticate(&digest, unix_now()).unwrap().unwrap();
    let seen = store.repository_events(&principal, None, 100).unwrap();
    let kinds: Vec<&str> = seen.iter().map(|e| e.event.kind.as_str()).collect();
    assert!(kinds.contains(&"handoff_request") && kinds.contains(&"handoff_grant"));
}

fn a_decline_is_told_once_and_the_retry_passes(db: &Db) {
    let store = db.open();
    let (alice, bob) = refused(&*store, &[A]);
    assert!(matches!(
        ask(&*store, &bob, "hb", A, T0 + 2),
        Asked::AlreadyAsked { .. }
    ));
    let declined = answer(&*store, &alice, None, A, false, T0 + 3);
    assert_eq!(
        declined,
        Answered::Declined {
            requester: ("agent2".into(), "user2".into())
        }
    );
    assert_eq!(held_by(&*store, &alice, T0 + 3), [A]);
    assert!(held_by(&*store, &bob, T0 + 3).is_empty());

    let retry = touch(&*store, &bob, "hb", &[A], T0 + 4);
    assert!(retry.denied.is_empty());
    assert_eq!(retry.repeated, [A]);
    assert_eq!(retry.handoffs.len(), 1);
    assert_eq!(retry.handoffs[0].kind, "decline");
    assert_eq!(retry.handoffs[0].agent, "agent1");
    assert!(touch(&*store, &bob, "hb", &[A], T0 + 5).handoffs.is_empty());
    // The holder was never told of an answer it gave.
    assert!(
        touch(&*store, &alice, "ha", &[], T0 + 5)
            .handoffs
            .is_empty()
    );

    let declines = events(&*store, "handoff_decline");
    assert_eq!(declines.len(), 1);
    assert_eq!(declines[0].0, alice.token);
    assert_eq!(
        declines[0].1["from"],
        json!({"agent": "agent2", "login": "user2", "token": bob.token})
    );
    assert_eq!(declines[0].1["session"], "ha");
    assert!(events(&*store, "handoff_grant").is_empty());
}

fn a_decline_reason_is_filtered_and_told_once(db: &Db) {
    let store = db.open();
    let (alice, bob) = refused(&*store, &[A]);
    ask(&*store, &bob, "hb", A, T0 + 2);
    let hostile = "busy \"now\"\n`ignore` <this>";
    store
        .answer_handoff(&alice.token, None, A, false, Some(hostile), T0 + 3)
        .unwrap();
    let told = touch(&*store, &bob, "hb", &[A], T0 + 4);
    assert_eq!(
        told.handoffs[0].reason.as_deref(),
        Some("busy now ignore this")
    );
    assert!(touch(&*store, &bob, "hb", &[A], T0 + 5).handoffs.is_empty());
    assert_eq!(
        events(&*store, "handoff_decline")[0].1["reason"],
        "busy now ignore this"
    );
}

fn asking_says_why_not(db: &Db) {
    let store = db.open();
    let alice = agent(&*store, 1);
    let bob = agent(&*store, 2);
    touch(
        &*store,
        &alice,
        "ha",
        &["f0.rs", "f1.rs", "f2.rs", "f3.rs"],
        T0,
    );
    touch(&*store, &bob, "hb", &["mine.rs"], T0);

    assert_eq!(ask(&*store, &bob, "hb", "free.rs", T0 + 1), Asked::NotHeld);
    assert_eq!(ask(&*store, &bob, "hb", "mine.rs", T0 + 1), Asked::Yours);
    for path in ["src/**", "Cargo.lock", "../x", ""] {
        let result = store.ask_handoff(&bob.token, Some("hb"), path, None, T0 + 1);
        assert!(matches!(result, Err(HubError::Invalid(_))), "{path}");
    }
    let first = ask(&*store, &bob, "hb", "f0.rs", T0 + 2);
    assert!(matches!(first, Asked::Requested { .. }));
    let again = ask(&*store, &bob, "hb", "f0.rs", T0 + 3);
    let Asked::AlreadyAsked { holder } = again else {
        panic!("{again:?}");
    };
    assert_eq!(holder.token, alice.token);
    assert!(matches!(
        ask(&*store, &bob, "hb", "f1.rs", T0 + 3),
        Asked::Requested { .. }
    ));
    assert!(matches!(
        ask(&*store, &bob, "hb", "f2.rs", T0 + 3),
        Asked::Requested { .. }
    ));
    assert_eq!(ask(&*store, &bob, "hb", "f3.rs", T0 + 3), Asked::TooMany);
    // Another session of the same token has its own allowance.
    touch(&*store, &bob, "hc", &[], T0 + 3);
    assert!(matches!(
        ask(&*store, &bob, "hc", "f3.rs", T0 + 3),
        Asked::Requested { .. }
    ));
    // An answer frees a slot.
    answer(&*store, &alice, None, "f0.rs", false, T0 + 4);
    assert!(matches!(
        ask(&*store, &bob, "hb", "f3.rs", T0 + 5),
        Asked::Requested { .. }
    ));
    assert_eq!(events(&*store, "handoff_request").len(), 5);
    assert!(matches!(
        store.ask_handoff("tok_nope", None, A, None, T0),
        Err(HubError::Unauthorized)
    ));
}

fn a_request_lapses_after_the_hold_cap(db: &Db) {
    let store = db.open();
    let (alice, bob) = refused(&*store, &[A]);
    assert!(matches!(
        ask(&*store, &bob, "hb", A, T0 + 2),
        Asked::AlreadyAsked { .. }
    ));
    // Still open just before the cap; the holder's own intent ends at it.
    let at = T0 + 1 + HOLD_CAP_SECONDS;
    assert!(touch(&*store, &alice, "ha", &[], at - 1).handoffs.len() == 1);
    assert_eq!(
        answer(&*store, &alice, None, A, true, at),
        Answered::NoRequest
    );
    assert!(touch(&*store, &alice, "ha", &[], at).handoffs.is_empty());
    assert!(touch(&*store, &bob, "hb", &[], at).handoffs.is_empty());
    assert!(held_by(&*store, &bob, at).is_empty());

    // A lapsed request does not count against the session's three.
    let carol = agent(&*store, 3);
    touch(
        &*store,
        &carol,
        "hz",
        &["z0.rs", "z1.rs", "z2.rs", "z3.rs"],
        at,
    );
    for path in ["z0.rs", "z1.rs", "z2.rs"] {
        assert!(matches!(
            ask(&*store, &bob, "hb", path, at),
            Asked::Requested { .. }
        ));
    }
    let later = at + HOLD_CAP_SECONDS;
    touch(&*store, &carol, "hz", &["z3.rs"], later);
    assert!(matches!(
        ask(&*store, &bob, "hb", "z3.rs", later),
        Asked::Requested { .. }
    ));
}

fn only_the_holder_can_answer(db: &Db) {
    let store = db.open();
    let (alice, bob) = refused(&*store, &[A]);
    let carol = agent(&*store, 3);
    assert!(matches!(
        ask(&*store, &bob, "hb", A, T0 + 2),
        Asked::AlreadyAsked { .. }
    ));
    assert_eq!(
        answer(&*store, &carol, None, A, true, T0 + 3),
        Answered::NoRequest
    );
    assert_eq!(
        answer(&*store, &bob, None, A, true, T0 + 3),
        Answered::NoRequest
    );
    assert_eq!(
        answer(&*store, &alice, Some("other"), A, true, T0 + 3),
        Answered::NoRequest
    );
    assert_eq!(
        answer(&*store, &alice, None, "src/zzz.rs", true, T0 + 3),
        Answered::NoRequest
    );
    let bad = store.answer_handoff(&alice.token, None, "src/**", true, None, T0 + 3);
    assert!(matches!(bad, Err(HubError::Invalid(_))));
    assert_eq!(held_by(&*store, &alice, T0 + 3), [A]);
    assert!(held_by(&*store, &bob, T0 + 3).is_empty());
    // The holder lets go first: the answer says so and the request is closed.
    store.end_session(&alice.token, Some("ha"), T0 + 4).unwrap();
    assert_eq!(
        answer(&*store, &alice, Some("ha"), A, true, T0 + 5),
        Answered::NotHeld
    );
    assert_eq!(
        answer(&*store, &alice, None, A, true, T0 + 6),
        Answered::NoRequest
    );
    assert_eq!(ask(&*store, &bob, "hb", A, T0 + 6), Asked::NotHeld);
}

fn a_holder_that_dropped_the_file_cannot_grant_it(db: &Db) {
    let store = db.open();
    let (alice, bob) = refused(&*store, &[A, "src/b.rs"]);
    assert!(matches!(
        ask(&*store, &bob, "hb", A, T0 + 2),
        Asked::AlreadyAsked { .. }
    ));
    // The holder's intent loses the file another way: a grant to someone else.
    let carol = agent(&*store, 3);
    assert_eq!(touch(&*store, &carol, "hc", &[A], T0 + 3).denied.len(), 1);
    assert!(matches!(
        ask(&*store, &carol, "hc", A, T0 + 4),
        Asked::AlreadyAsked { .. }
    ));
    assert!(matches!(
        answer(&*store, &alice, None, A, true, T0 + 5),
        Answered::Granted { .. }
    ));
    // Alice no longer holds A, so carol's request cannot be granted by her.
    assert_eq!(
        answer(&*store, &alice, None, A, true, T0 + 6),
        Answered::NotHeld
    );
    assert_eq!(
        answer(&*store, &alice, None, A, true, T0 + 7),
        Answered::NoRequest
    );
    assert_eq!(held_by(&*store, &bob, T0 + 6), [A]);
    assert_eq!(held_by(&*store, &alice, T0 + 6), ["src/b.rs"]);
}

fn granting_the_last_path_releases_the_intent(db: &Db) {
    let store = db.open();
    let (alice, bob) = refused(&*store, &[A]);
    assert!(matches!(
        ask(&*store, &bob, "hb", A, T0 + 2),
        Asked::AlreadyAsked { .. }
    ));
    let released_before = events(&*store, "intent_released").len();
    answer(&*store, &alice, None, A, true, T0 + 3);
    assert_eq!(
        events(&*store, "intent_released").len(),
        released_before + 1
    );
    let holders = live(&*store, T0 + 3);
    assert_eq!(holders.len(), 1);
    assert_eq!(holders[0].token, bob.token);
    assert_eq!(holders[0].paths, [A]);
    // Alice's next touch starts a fresh intent; the file is bob's now.
    let next = touch(&*store, &alice, "ha", &["src/c.rs"], T0 + 4);
    assert_eq!(next.intent.unwrap().paths, ["src/c.rs"]);
}

fn a_refused_touch_keeps_the_deliveries(db: &Db) {
    let store = db.open();
    let (alice, bob) = refused(&*store, &[A]);
    let carol = agent(&*store, 3);
    touch(&*store, &carol, "hc", &["src/x.rs", "src/y.rs"], T0 + 1);
    assert!(matches!(
        ask(&*store, &bob, "hb", A, T0 + 2),
        Asked::AlreadyAsked { .. }
    ));

    // The holder's refused touch tells nothing; the next one does.
    let blocked = touch(&*store, &alice, "ha", &["src/x.rs"], T0 + 3);
    assert_eq!(blocked.denied.len(), 1);
    assert!(blocked.handoffs.is_empty());
    assert_eq!(touch(&*store, &alice, "ha", &[], T0 + 4).handoffs.len(), 1);

    // So does the requester's.
    answer(&*store, &alice, None, A, false, T0 + 5);
    let blocked = touch(&*store, &bob, "hb", &["src/y.rs"], T0 + 6);
    assert_eq!(blocked.denied.len(), 1);
    assert!(blocked.handoffs.is_empty());
    let next = touch(&*store, &bob, "hb", &[], T0 + 7);
    assert_eq!(next.handoffs.len(), 1);
    assert_eq!(next.handoffs[0].kind, "decline");
}

fn concurrent_grants_and_touches_leave_one_owner(db: &Db) {
    let store = db.open();
    let (alice, bob) = refused(&*store, &[A, "src/b.rs"]);
    assert!(matches!(
        ask(&*store, &bob, "hb", A, T0 + 2),
        Asked::AlreadyAsked { .. }
    ));
    let barrier = Arc::new(Barrier::new(3));
    let grants: Vec<_> = (0..2)
        .map(|_| {
            let (store, barrier, token) = (store.clone(), barrier.clone(), alice.token.clone());
            std::thread::spawn(move || {
                barrier.wait();
                store.answer_handoff(&token, Some("ha"), A, true, None, T0 + 3)
            })
        })
        .collect();
    let touching = {
        let (store, barrier, token) = (store.clone(), barrier.clone(), bob.token.clone());
        std::thread::spawn(move || {
            barrier.wait();
            store.touch_session(&token, &touch_call("hb", &[A]), T0 + 3)
        })
    };
    let results: Vec<Answered> = grants
        .into_iter()
        .map(|grant| grant.join().unwrap().unwrap())
        .collect();
    let touched = touching.join().unwrap().unwrap();
    let won = results
        .iter()
        .filter(|result| matches!(result, Answered::Granted { .. }))
        .count();
    let none = results
        .iter()
        .filter(|r| **r == Answered::NoRequest)
        .count();
    assert_eq!((won, none), (1, 1), "{results:?}");
    assert_eq!(held_by(&*store, &alice, T0 + 4), ["src/b.rs"]);
    assert_eq!(held_by(&*store, &bob, T0 + 4), [A]);
    assert_eq!(events(&*store, "handoff_grant").len(), 1);
    // The requester's hook retry still passes, and it was told of the grant exactly once.
    let told = [T0 + 4, T0 + 5].map(|now| touch(&*store, &bob, "hb", &[A], now));
    assert!(told.iter().all(|t| t.denied.is_empty()));
    let heard: usize =
        told.iter().map(|t| t.handoffs.len()).sum::<usize>() + touched.handoffs.len();
    assert_eq!(heard, 1);
}

fn asking_needs_a_session_and_a_free_queue(db: &Db) {
    let store = db.open();
    let alice = agent(&*store, 1);
    touch(&*store, &alice, "ha", &[A], T0);
    let bob = agent(&*store, 2);
    let unknown = store.ask_handoff(&bob.token, Some("hb"), A, None, T0 + 1);
    assert!(matches!(unknown, Err(HubError::Invalid(_))), "{unknown:?}");
    assert!(events(&*store, "handoff_request").is_empty());
    // Three requests from anyone to one holder for one file are the cap.
    for (n, session) in [(2, "hb"), (3, "hc"), (4, "hd")] {
        let who = agent(&*store, n);
        touch(&*store, &who, session, &[], T0 + 1);
        assert!(matches!(
            ask(&*store, &who, session, A, T0 + 2),
            Asked::Requested { .. }
        ));
    }
    let erin = agent(&*store, 5);
    touch(&*store, &erin, "he", &[], T0 + 1);
    assert_eq!(ask(&*store, &erin, "he", A, T0 + 3), Asked::TooMany);
    assert_eq!(events(&*store, "handoff_request").len(), 3);
}

fn a_request_whose_requester_ended_lapses(db: &Db) {
    let store = db.open();
    let (alice, bob) = refused(&*store, &[A]);
    assert!(matches!(
        ask(&*store, &bob, "hb", A, T0 + 2),
        Asked::AlreadyAsked { .. }
    ));
    store.end_session(&bob.token, Some("hb"), T0 + 3).unwrap();
    assert_eq!(
        answer(&*store, &alice, None, A, true, T0 + 4),
        Answered::NoRequest
    );
    assert_eq!(held_by(&*store, &alice, T0 + 4), [A]);
    assert!(held_by(&*store, &bob, T0 + 4).is_empty());
    assert!(events(&*store, "handoff_grant").is_empty());
    // The request is closed, not left to be granted later.
    touch(&*store, &bob, "hb", &[], T0 + 5);
    assert_eq!(
        answer(&*store, &alice, None, A, true, T0 + 6),
        Answered::NoRequest
    );
    assert!(held_by(&*store, &bob, T0 + 6).is_empty());
}

fn handoffs_wait_for_a_reply_that_can_show_them(db: &Db) {
    let store = db.open();
    let (alice, bob) = refused(&*store, &[A]);
    assert!(matches!(
        ask(&*store, &bob, "hb", A, T0 + 2),
        Asked::AlreadyAsked { .. }
    ));
    assert!(
        touch_silent(&*store, &alice, "ha", T0 + 3)
            .handoffs
            .is_empty()
    );
    assert_eq!(touch(&*store, &alice, "ha", &[], T0 + 4).handoffs.len(), 1);
    answer(&*store, &alice, None, A, true, T0 + 5);
    assert!(
        touch_silent(&*store, &bob, "hb", T0 + 6)
            .handoffs
            .is_empty()
    );
    let told = touch(&*store, &bob, "hb", &[], T0 + 7);
    assert_eq!(told.handoffs.len(), 1);
    assert_eq!(told.handoffs[0].kind, "grant");
}

fn pending_requests_follow_the_adopted_session(db: &Db) {
    let store = db.open();
    let alice = agent(&*store, 1);
    let started = SessionCall {
        session: None,
        via: "mcp".into(),
        task: None,
        paths: vec![A.into()],
        deny: false,
        report: true,
    };
    let started = store.touch_session(&alice.token, &started, T0).unwrap();
    assert_eq!(started.session, "default");
    let bob = agent(&*store, 2);
    touch(&*store, &bob, "hb", &[], T0 + 1);
    assert!(matches!(
        ask(&*store, &bob, "hb", A, T0 + 2),
        Asked::Requested { .. }
    ));
    // The holder's first hook call renames its session; the request goes with it.
    let hooked = touch(&*store, &alice, "ha", &[], T0 + 3);
    assert_eq!(hooked.handoffs.len(), 1);
    assert_eq!(
        (hooked.handoffs[0].kind, hooked.handoffs[0].session.as_str()),
        ("request", "ha")
    );
    let granted = answer(&*store, &alice, Some("ha"), A, true, T0 + 4);
    assert!(matches!(granted, Answered::Granted { .. }));
}

fn the_same_agent_is_not_a_holder_to_ask(db: &Db) {
    let store = db.open();
    let alice = agent(&*store, 1);
    let started = SessionCall {
        session: Some("m1".into()),
        via: "mcp".into(),
        task: None,
        paths: vec![A.into()],
        deny: false,
        report: true,
    };
    store.touch_session(&alice.token, &started, T0).unwrap();
    touch(&*store, &alice, "ha", &[], T0 + 1);
    assert_eq!(ask(&*store, &alice, "ha", A, T0 + 2), Asked::NotHeld);
}

fn an_omitted_answer_session_resolves_like_the_other_tools(db: &Db) {
    let store = db.open();
    let (alice, bob) = refused(&*store, &[A]);
    assert!(matches!(
        ask(&*store, &bob, "hb", A, T0 + 2),
        Asked::AlreadyAsked { .. }
    ));
    // Two hook sessions of one token: an omitted session is the default one, not any of them.
    touch(&*store, &alice, "hz", &[], T0 + 3);
    assert_eq!(
        answer(&*store, &alice, None, A, true, T0 + 4),
        Answered::NoRequest
    );
    assert!(matches!(
        answer(&*store, &alice, Some("ha"), A, true, T0 + 4),
        Answered::Granted { .. }
    ));
}

fn an_announced_file_can_still_be_asked_for(db: &Db) {
    let store = db.open();
    let alice = agent(&*store, 1);
    let bob = agent(&*store, 2);
    touch(&*store, &alice, "ha", &[A], T0);
    // Bob announced the file through MCP (agentscm_start with paths), so both hold it.
    let announce = SessionCall {
        via: "mcp".into(),
        deny: false,
        ..touch_call("hb", &[A])
    };
    store.touch_session(&bob.token, &announce, T0 + 1).unwrap();
    assert!(matches!(
        ask(&*store, &bob, "hb", A, T0 + 2),
        Asked::Requested { .. }
    ));
}

fn a_renewed_holder_intent_can_still_grant(db: &Db) {
    let store = db.open();
    let alice = agent(&*store, 1);
    let bob = agent(&*store, 2);
    touch(&*store, &alice, "ha", &[A], T0);
    touch(&*store, &bob, "hb", &[], T0 + 1);
    // Heartbeats keep alice's hold live until its cap.
    for at in [T0 + 1000, T0 + 1499] {
        touch(&*store, &alice, "ha", &[A], at);
    }
    assert!(matches!(
        ask(&*store, &bob, "hb", A, T0 + 1500),
        Asked::Requested { .. }
    ));
    // Alice's hold cap ends her intent; her next edit holds the file again in a fresh one.
    let later = T0 + HOLD_CAP_SECONDS + 50;
    touch(&*store, &alice, "ha", &[A], later);
    assert!(matches!(
        answer(&*store, &alice, Some("ha"), A, true, later + 10),
        Answered::Granted { .. }
    ));
    assert_eq!(held_by(&*store, &bob, later + 11), [A]);
}

fn a_refusal_asks_the_holder_once(db: &Db) {
    let store = db.open();
    let alice = agent(&*store, 1);
    let bob = agent(&*store, 2);
    touch(&*store, &alice, "ha", &[A], T0);
    let denied = touch(&*store, &bob, "hb", &[A], T0 + 1);
    assert_eq!(
        (denied.denied.len(), &denied.requested[..]),
        (1, [A.into()].as_slice())
    );
    // No task, so no note; the event says the hub asked.
    let requests = events(&*store, "handoff_request");
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].1["auto"], true);
    assert_eq!(requests[0].1["note"], Value::Null);
    let told = touch(&*store, &alice, "ha", &[], T0 + 2);
    assert_eq!(told.handoffs.len(), 1);
    assert_eq!(told.handoffs[0].note, None);
    // The agent's own ask finds the request there, and a repeat of the edit files nothing.
    assert!(matches!(
        ask(&*store, &bob, "hb", A, T0 + 3),
        Asked::AlreadyAsked { .. }
    ));
    let repeat = touch(&*store, &bob, "hb", &[A], T0 + 4);
    assert!(repeat.denied.is_empty() && repeat.requested.is_empty());
    assert_eq!(events(&*store, "handoff_request").len(), 1);
    // A grant moves the file and the retry passes.
    answer(&*store, &alice, Some("ha"), A, true, T0 + 5);
    assert_eq!(held_by(&*store, &bob, T0 + 5), [A]);
}

fn a_refused_call_asks_for_three_files_at_most(db: &Db) {
    let store = db.open();
    let alice = agent(&*store, 1);
    let bob = agent(&*store, 2);
    let files = ["f0.rs", "f1.rs", "f2.rs", "f3.rs", "f4.rs"];
    touch(&*store, &alice, "ha", &files, T0);
    let denied = touch(&*store, &bob, "hb", &files, T0 + 1);
    assert_eq!(denied.denied.len(), 1);
    assert_eq!(denied.requested, ["f0.rs", "f1.rs", "f2.rs"]);
    assert_eq!(events(&*store, "handoff_request").len(), 3);
    assert_eq!(touch(&*store, &alice, "ha", &[], T0 + 2).handoffs.len(), 3);
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
    a_grant_moves_the_file_and_is_told_once,
    a_decline_is_told_once_and_the_retry_passes,
    a_decline_reason_is_filtered_and_told_once,
    asking_says_why_not,
    a_request_lapses_after_the_hold_cap,
    only_the_holder_can_answer,
    a_holder_that_dropped_the_file_cannot_grant_it,
    granting_the_last_path_releases_the_intent,
    a_refused_touch_keeps_the_deliveries,
    concurrent_grants_and_touches_leave_one_owner,
    asking_needs_a_session_and_a_free_queue,
    a_request_whose_requester_ended_lapses,
    handoffs_wait_for_a_reply_that_can_show_them,
    pending_requests_follow_the_adopted_session,
    the_same_agent_is_not_a_holder_to_ask,
    an_omitted_answer_session_resolves_like_the_other_tools,
    an_announced_file_can_still_be_asked_for,
    a_renewed_holder_intent_can_still_grant,
    a_refusal_asks_the_holder_once,
    a_refused_call_asks_for_three_files_at_most,
);

/// The request is best effort: when filing it fails, the touch is still refused, not failed.
#[test]
fn a_failed_request_does_not_break_the_refusal() {
    let db = Db::sqlite();
    let store = db.open();
    let alice = agent(&*store, 1);
    let bob = agent(&*store, 2);
    touch(&*store, &alice, "ha", &[A], T0);
    let path = db.url.strip_prefix("sqlite://").unwrap();
    rusqlite::Connection::open(path)
        .unwrap()
        .execute_batch("ALTER TABLE hub_handoffs RENAME TO hub_handoffs_gone")
        .unwrap();
    let denied = touch(&*store, &bob, "hb", &[A], T0 + 1);
    assert_eq!(denied.denied.len(), 1);
    assert!(denied.requested.is_empty());
    // The session and the refusal were stored: the repeat passes.
    let quiet = SessionCall {
        report: false,
        ..touch_call("hb", &[A])
    };
    let repeat = store.touch_session(&bob.token, &quiet, T0 + 2).unwrap();
    assert!(repeat.denied.is_empty());
}
