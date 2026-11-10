//! A person's own waiting agent session is one direct message; moments between agents are none.
use super::*;
use crate::hub::session_waiting::WaitReason;

/// The coding agent of binding `index` (0 alice, 1 bob) as the Hub authenticates it, which is
/// what a hook reports a wait under.
fn agent(f: &Fixture, index: usize) -> Principal {
    let owner = f.owner(index);
    let (_, secret, _) =
        mint_bound_agent_token(&**f.store(), &owner, "waiter", REPO, 101, 3600, false).unwrap();
    tokio::task::block_in_place(|| {
        f.store()
            .authenticate(&agent_token_digest(secret.expose()).unwrap(), unix_now())
            .unwrap()
            .unwrap()
    })
}

/// Person `index` starts waiting in session `session` at `since`.
fn wait(f: &Fixture, index: usize, session: &str, reason: WaitReason, since: i64) {
    f.state
        .onboarding
        .waiting()
        .record(
            &agent(f, index),
            session,
            session,
            "claude",
            reason,
            None,
            Some("Retry the job?"),
            since,
        )
        .unwrap();
}

fn clear(f: &Fixture, index: usize, session: &str) {
    f.state
        .onboarding
        .waiting()
        .clear(&agent(f, index).token_id, session, None, true);
}

/// One worker pass at `now`: collect the waits, then send every due message.
async fn pass(f: &Fixture, now: i64) {
    let (slack, state) = (f.slack(), f.state.clone());
    tokio::task::block_in_place(|| slack.collect_waits(&state, now)).unwrap();
    // Passes run at a synthetic `now` ahead of the real clock, which delivery reads: a message
    // queued at a pass is due at that pass, so make it due. The filler rows of the full-journal
    // test stay queued.
    slack
        .database()
        .unwrap()
        .execute(
            "UPDATE messages SET due=0 WHERE state='queued' AND id NOT LIKE 'waiting%'",
            [],
        )
        .unwrap();
    while slack.deliver(&f.state).await.unwrap() {}
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_wait_past_the_grace_is_one_message_however_many_passes_see_it() {
    let f = Fixture::new().await;
    let now = unix_now();
    wait(&f, 0, "s1", WaitReason::Question, now - 30);
    // Someone at the keyboard answers within the grace: nothing yet.
    pass(&f, now).await;
    assert!(f.sent().is_empty());
    for later in [now + 61, now + 122, now + 183] {
        pass(&f, later).await;
    }
    let sent = f.sent();
    assert_eq!(sent.len(), 1, "{sent:?}");
    assert_eq!(sent[0].0, "U1");
    assert!(
        sent[0]
            .1
            .starts_with("Your agent is waiting for you | Waiting: Your "),
        "{sent:?}"
    );
    assert!(sent[0].1.contains("Retry the job?"), "{sent:?}");
    assert_eq!(f.link("U1"), "https://hub.example.test/ui/#box-needs");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_new_wait_of_the_same_session_is_a_new_message() {
    let f = Fixture::new().await;
    let now = unix_now();
    wait(&f, 0, "s1", WaitReason::Permission, now - 120);
    // A minute on: the Hub's own worker already scanned at `now` and set the next scan a minute out.
    pass(&f, now + 61).await;
    assert_eq!(f.sent().len(), 1);
    // The session moved on and stopped again later: that is another wait.
    clear(&f, 0, "s1");
    wait(&f, 0, "s1", WaitReason::Permission, now + 100);
    pass(&f, now + 130).await;
    assert_eq!(f.sent().len(), 1, "inside the grace");
    pass(&f, now + 200).await;
    pass(&f, now + 261).await;
    assert_eq!(f.sent().len(), 2);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn only_the_waiting_agents_own_person_is_messaged() {
    let f = Fixture::new().await;
    let now = unix_now();
    wait(&f, 1, "bobs", WaitReason::Question, now - 120);
    pass(&f, now).await;
    pass(&f, now + 61).await;
    let users: Vec<_> = f.sent().into_iter().map(|(user, _)| user).collect();
    assert_eq!(users, ["U2"]);
    wait(&f, 0, "alices", WaitReason::Question, now - 120);
    pass(&f, now + 122).await;
    let users: Vec<_> = f.sent().into_iter().map(|(user, _)| user).collect();
    assert_eq!(users, ["U2", "U1"]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_full_journal_holds_a_wait_until_queued_messages_are_sent() {
    let f = Fixture::new().await;
    {
        let slack = f.slack();
        let mut db = slack.database().unwrap();
        let tx = db.transaction().unwrap();
        for n in 0..CAPACITY {
            tx.execute(
                "INSERT INTO messages(id,user,body,state,created,due) VALUES(?1,'U9','{}','queued',?2,?3)",
                params![format!("waiting{n}"), unix_now(), unix_now() + 86400],
            )
            .unwrap();
        }
        tx.commit().unwrap();
    }
    let now = unix_now();
    wait(&f, 0, "s1", WaitReason::Question, now - 120);
    pass(&f, now).await;
    assert!(f.sent().is_empty());
    // Sent rows no longer count: the next pass queues the wait, which was not lost.
    f.slack()
        .database()
        .unwrap()
        .execute(
            "UPDATE messages SET state='done' WHERE id LIKE 'waiting%'",
            [],
        )
        .unwrap();
    pass(&f, now + 61).await;
    assert_eq!(f.sent().len(), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_idle_end_of_turn_waits_five_minutes_but_a_question_keeps_the_grace() {
    let f = Fixture::new().await;
    let now = unix_now();
    let a = agent(&f, 0);
    // An idle stop counts only while its session leads unfinished Work.
    let request: crate::hub::work::WorkStart = serde_json::from_value(json!({
        "request_key": "idle-lead", "title": "Lead", "session": "idle", "scope": ["src/idle.rs"]
    }))
    .unwrap();
    tokio::task::block_in_place(|| f.store().work_start(&a.token_id, &request, now)).unwrap();
    let waiting = f.state.onboarding.waiting();
    let (idle, ask) = (WaitReason::Idle, WaitReason::Question);
    waiting
        .record(&a, "idle", "idle", "claude", idle, None, None, now - 100)
        .unwrap();
    waiting
        .record(
            &a,
            "ask",
            "ask",
            "claude",
            ask,
            None,
            Some("Retry the job?"),
            now - 100,
        )
        .unwrap();
    // Both have outlasted the Hub's grace; only the question is sent.
    pass(&f, now + 61).await;
    pass(&f, now + 122).await;
    let sent = f.sent();
    assert_eq!(sent.len(), 1, "{sent:?}");
    assert!(sent[0].1.contains("Retry the job?"), "{sent:?}");
    // The idle stop has now waited past five minutes.
    pass(&f, now + 240).await;
    assert_eq!(f.sent().len(), 2);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn hostile_text_an_agent_asks_stays_plain_text() {
    let f = Fixture::new().await;
    let now = unix_now();
    f.state
        .onboarding
        .waiting()
        .record(
            &agent(&f, 0),
            "s1",
            "s1",
            "claude",
            WaitReason::Question,
            None,
            Some("<!channel> @here <http://x|y> & more"),
            now - 120,
        )
        .unwrap();
    pass(&f, now + 61).await;
    let body = f.sent_body("U1");
    let text = body["text"].as_str().unwrap();
    assert!(!text.contains('<') && !text.contains('>'), "{text}");
    assert!(!text.contains("<!channel>") && !text.contains("<http://x|y>"));
    assert!(
        text.contains("&amp; more") || text.contains("&amp;"),
        "{text}"
    );
    // Every block is plain text, so Slack renders none of it as a mention or link.
    rendered(&body);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn moments_between_agents_send_nothing() {
    let f = Fixture::new().await;
    // Bob edits a file alice's agent holds: refused once, then repeated.
    f.contest();
    let now = unix_now();
    for n in 0..3 {
        f.run().await;
        pass(&f, now + n * 61).await;
    }
    assert!(f.sent().is_empty());
}
