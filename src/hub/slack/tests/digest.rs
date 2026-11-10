//! The weekly digest through Slack: who is sent it, what it holds, and what its buttons may do.
use super::*;
use crate::hub::catchup::{CatchupMergeSource, CatchupRepositoryScope, NewCatchupMerge};
use crate::hub::digest_http;
use crate::hub::slack::digest::{COMPLETE, RELEASE, STOP};
use crate::hub::work::{WorkItem, WorkState};

const DAY: i64 = 86_400;
const REPO_ID: i64 = 101;

/// Work led by `token`'s agent that last checked in `days` ago.
fn stale(f: &Fixture, token: &str, title: &str, branch: Option<&str>, days: i64) -> WorkItem {
    let key: String = title
        .chars()
        .filter(char::is_ascii_alphanumeric)
        .collect::<String>()
        .to_ascii_lowercase();
    let start: WorkStart = serde_json::from_value(json!({
        "request_key": key, "title": title, "session": format!("session-{key}"),
        "scope": [format!("src/{key}.rs")], "branch": branch,
    }))
    .unwrap();
    tokio::task::block_in_place(|| {
        f.store()
            .work_start(token, &start, unix_now() - days * DAY)
            .unwrap()
            .item
    })
}

fn merged(f: &Fixture, branch: &str) {
    let at = unix_now() - DAY;
    let merge = NewCatchupMerge {
        source: CatchupMergeSource::ForgePull,
        source_id: "pull:7".into(),
        occurred_at: Some(at),
        title: Some("Synthetic merge".into()),
        branch: branch.into(),
        base: "main".into(),
        head_commit: "a".repeat(40),
        merge_commit: "b".repeat(40),
        pull_number: 7,
        paths: Vec::new(),
        paths_complete: true,
        same_repository_head: true,
    };
    let scope = CatchupRepositoryScope {
        provider: "github".into(),
        host: "github.com".into(),
        repository: REPO.into(),
        repository_id: Some(REPO_ID),
    };
    tokio::task::block_in_place(|| f.store().record_catchup_merge(&scope, &merge, at).unwrap());
}

fn switch_on(f: &Fixture, repository_id: i64) {
    tokio::task::block_in_place(|| {
        digest_http::set_repository(&**f.store(), repository_id, true, "usr_admin").unwrap()
    });
}

/// Lets the next pass look for due digests instead of waiting out the scan interval.
fn rescan(f: &Fixture) {
    f.slack()
        .database()
        .unwrap()
        .execute("DELETE FROM digest_scan", [])
        .unwrap();
}

/// Worker passes until both bindings' jobs are handled: a pass ends at the first job it drops.
async fn drain(f: &Fixture) {
    for _ in 0..3 {
        f.run().await;
    }
}

async fn pass(f: &Fixture) {
    rescan(f);
    drain(f).await;
}

/// The digests sent so far, as (Slack user, message).
fn digests(f: &Fixture) -> Vec<(String, Value)> {
    f.calls
        .lock()
        .unwrap()
        .iter()
        .filter(|(path, body)| {
            path == "/api/chat.postMessage"
                && body["blocks"][0]["text"]["text"] == "Your weekly digest"
        })
        .map(|(_, body)| (body["channel"].as_str().unwrap().to_owned(), body.clone()))
        .collect()
}

/// Every text of a message's blocks, joined by ` | `; all of it must be plain text.
fn text_of(blocks: &Value) -> String {
    let mut parts = Vec::new();
    for block in blocks.as_array().unwrap() {
        let texts: Vec<&Value> = match block["type"].as_str().unwrap() {
            "header" | "section" => vec![&block["text"]],
            "context" => block["elements"].as_array().unwrap().iter().collect(),
            _ => Vec::new(),
        };
        for text in texts {
            assert_eq!(text["type"], "plain_text", "{block}");
            assert_eq!(text["emoji"], false, "{block}");
            parts.push(text["text"].as_str().unwrap().to_owned());
        }
    }
    parts.join(" | ")
}

/// The buttons under the row of the Work titled `title`, as action ids.
fn row<'a>(blocks: &'a Value, title: &str) -> Vec<&'a Value> {
    let blocks = blocks.as_array().unwrap();
    let at = blocks
        .iter()
        .position(|block| {
            block["text"]["text"]
                .as_str()
                .is_some_and(|text| text.starts_with(title))
        })
        .unwrap_or_else(|| panic!("no row for {title}"));
    blocks
        .get(at + 1)
        .filter(|block| block["type"] == "actions")
        .and_then(|block| block["elements"].as_array())
        .map(|elements| elements.iter().collect())
        .unwrap_or_default()
}

fn ids(buttons: &[&Value]) -> Vec<String> {
    buttons
        .iter()
        .map(|button| button["action_id"].as_str().unwrap().to_owned())
        .collect()
}

fn button<'a>(blocks: &'a Value, title: &str, action_id: &str) -> &'a Value {
    row(blocks, title)
        .into_iter()
        .find(|button| button["action_id"] == action_id)
        .unwrap_or_else(|| panic!("no {action_id} button for {title}"))
}

fn has_stop(blocks: &Value) -> bool {
    blocks.as_array().unwrap().iter().any(|block| {
        block["elements"]
            .as_array()
            .is_some_and(|elements| elements.iter().any(|e| e["action_id"] == STOP))
    })
}

/// The `count`th call of `method` the fake Slack saw; a click's follow-up runs after the click
/// is acknowledged.
async fn nth(f: &Fixture, method: &str, count: usize) -> Value {
    for _ in 0..100 {
        let found = f
            .calls
            .lock()
            .unwrap()
            .iter()
            .filter(|(path, _)| path == method)
            .nth(count - 1)
            .map(|(_, body)| body.clone());
        if let Some(found) = found {
            return found;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("no {method} number {count} was sent");
}

fn digest_jobs(f: &Fixture, user: &str) -> i64 {
    f.slack()
        .database()
        .unwrap()
        .query_row(
            "SELECT count(*) FROM status_jobs WHERE kind='digest' AND user=?1",
            [user],
            |r| r.get(0),
        )
        .unwrap()
}

fn work(f: &Fixture, id: &str) -> WorkItem {
    tokio::task::block_in_place(|| {
        f.store()
            .work_get(&f.alice, id, unix_now())
            .unwrap()
            .unwrap()
    })
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_digest_is_off_by_default_weekly_and_only_for_bound_people_with_something_in_it() {
    let f = Fixture::new().await;
    stale(
        &f,
        &f.alice,
        "<!channel> Ship the exporter",
        Some("feat/exporter"),
        6,
    );
    stale(&f, &f.alice, "Tidy the parser", None, 5);
    merged(&f, "feat/exporter");
    // A person of another repository that has the digest on, and no Slack binding.
    let identity = ForgeIdentity {
        provider: "github".into(),
        host: "github.com".into(),
        user_id: 3,
    };
    let start: WorkStart = serde_json::from_value(
        json!({"request_key": "other", "title": "Elsewhere", "session": "s", "scope": ["a.rs"]}),
    )
    .unwrap();
    tokio::task::block_in_place(|| {
        let store = &**f.store();
        let carol = store.upsert_user(&identity, "carol").unwrap();
        let (other, _, _) =
            mint_bound_agent_token(store, &carol.id, "claude", "acme/other", 202, 3600, false)
                .unwrap();
        store
            .work_start(&other, &start, unix_now() - 9 * DAY)
            .unwrap();
    });
    switch_on(&f, 202);

    pass(&f).await;
    assert!(
        digests(&f).is_empty(),
        "off until the repository switches it on"
    );
    assert_eq!(digest_jobs(&f, "U1"), 0);

    switch_on(&f, REPO_ID);
    pass(&f).await;
    let sent = digests(&f);
    // Bob is bound too, but his digest would say nothing, so none is sent.
    assert_eq!(sent.len(), 1, "{sent:?}");
    let (user, body) = &sent[0];
    assert_eq!(
        user, "U1",
        "the bound person's direct message, never a channel"
    );
    let text = text_of(&body["blocks"]);
    assert!(text.contains("acme/widgets, "), "{text}");
    assert!(text.contains("quiet for 3 days or more: 2"), "{text}");
    assert!(
        text.contains("<!channel> Ship the exporter\nactive, branch feat/exporter; quiet 6 days; a pull request from its branch merged"),
        "{text}"
    );
    assert!(
        text.contains("Tidy the parser\nactive, no branch; quiet 5 days; whether a pull request from its branch merged is unknown"),
        "{text}"
    );
    assert!(text.contains("Collisions are counted from"), "{text}");
    assert!(!text.contains("Elsewhere"), "{text}");
    let summary = body["text"].as_str().unwrap();
    assert_eq!(
        summary,
        "Your weekly digest for acme/widgets: 2 stale tasks, 0 paths with collisions"
    );
    assert_eq!(
        ids(&row(&body["blocks"], "<!channel> Ship the exporter")),
        [COMPLETE, RELEASE, OPEN]
    );
    let open = button(&body["blocks"], "Tidy the parser", OPEN);
    assert!(
        open["url"]
            .as_str()
            .unwrap()
            .starts_with("https://hub.example.test/ui/#work/wrk_"),
        "{open}"
    );
    assert!(has_stop(&body["blocks"]));

    // Once a week: another pass in the same week sends nothing, the next week queues one more.
    pass(&f).await;
    assert_eq!(digests(&f).len(), 1);
    assert_eq!(digest_jobs(&f, "U1"), 1);
    let next_week = unix_now() + 7 * DAY;
    tokio::task::block_in_place(|| f.slack().queue_digests(&f.state, next_week)).unwrap();
    assert_eq!(digest_jobs(&f, "U1"), 2);
    assert_eq!(
        digest_jobs(&f, "U2"),
        2,
        "queued each week, dropped when it says nothing"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_person_who_switched_it_off_gets_none_and_the_stop_button_switches_it_off() {
    let f = Fixture::new().await;
    stale(&f, &f.alice, "Ship the exporter", None, 6);
    stale(&f, &f.bob, "Rework the importer", None, 6);
    let (alice, bob) = (f.owner(0), f.owner(1));
    switch_on(&f, REPO_ID);
    // Bob's digest is queued, then he switches his off: it is dropped when it comes up to send.
    rescan(&f);
    tokio::task::block_in_place(|| f.slack().queue_digests(&f.state, unix_now())).unwrap();
    assert_eq!((digest_jobs(&f, "U1"), digest_jobs(&f, "U2")), (1, 1));
    tokio::task::block_in_place(|| {
        digest_http::set_opted_out(&**f.store(), &bob, REPO_ID, true).unwrap();
    });
    drain(&f).await;
    let sent = digests(&f);
    assert_eq!(sent.len(), 1, "{sent:?}");
    assert_eq!(sent[0].0, "U1");
    assert!(text_of(&sent[0].1["blocks"]).contains("Ship the exporter"));

    // Alice stops hers from the message: the button goes, the task's own buttons stay.
    let body = &sent[0].1;
    assert_eq!(
        f.click_value("U1", STOP, &Value::Null, body).await,
        StatusCode::OK
    );
    let update = nth(&f, "/api/chat.update", 1).await;
    let outcome = update["text"].as_str().unwrap();
    assert!(
        outcome.starts_with("Weekly digests for acme/widgets are off for you."),
        "{outcome}"
    );
    assert!(!has_stop(&update["blocks"]));
    assert_eq!(
        ids(&row(&update["blocks"], "Ship the exporter")),
        [COMPLETE, RELEASE, OPEN]
    );
    assert!(text_of(&update["blocks"]).ends_with(outcome));
    let settings = tokio::task::block_in_place(|| {
        digest_http::settings(&**f.store(), REPO, Some(REPO_ID), &alice).unwrap()
    });
    assert!(settings.enabled && settings.opted_out && !settings.delivering);

    // Neither is queued a digest the next week.
    let next_week = unix_now() + 7 * DAY;
    tokio::task::block_in_place(|| f.slack().queue_digests(&f.state, next_week)).unwrap();
    assert_eq!((digest_jobs(&f, "U1"), digest_jobs(&f, "U2")), (1, 1));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn buttons_change_only_the_clicking_persons_own_work_as_that_person() {
    let f = Fixture::new().await;
    let exporter = stale(
        &f,
        &f.alice,
        "Ship <the> exporter",
        Some("feat/exporter"),
        6,
    );
    let parser = stale(&f, &f.alice, "Tidy the parser", None, 5);
    merged(&f, "feat/exporter");
    switch_on(&f, REPO_ID);
    pass(&f).await;
    let sent = digests(&f);
    assert_eq!(sent.len(), 1, "{sent:?}");
    let body = &sent[0].1;
    let complete = button(&body["blocks"], "Ship <the> exporter", COMPLETE)["value"].clone();

    // Bob presses a button that carries alice's Work: refused where he clicked, nothing changes.
    assert_eq!(
        f.click_value("U2", COMPLETE, &complete, body).await,
        StatusCode::OK
    );
    let notice = nth(&f, "/api/chat.postEphemeral", 1).await;
    assert_eq!(notice["user"], "U2");
    assert!(
        notice["text"].as_str().unwrap().contains("is not yours"),
        "{notice}"
    );
    // So is someone with no binding at all.
    assert_eq!(
        f.click_value("U9", COMPLETE, &complete, body).await,
        StatusCode::OK
    );
    let notice = nth(&f, "/api/chat.postEphemeral", 2).await;
    assert!(
        notice["text"]
            .as_str()
            .unwrap()
            .contains("Your Slack link to the Hub has ended"),
        "{notice}"
    );
    let untouched = work(&f, &exporter.id);
    assert_eq!(
        (untouched.state, untouched.updated_seq),
        (WorkState::Active, exporter.updated_seq)
    );

    // Alice completes it: the outcome goes under the message and only that row loses its buttons.
    assert_eq!(
        f.click_value("U1", COMPLETE, &complete, body).await,
        StatusCode::OK
    );
    let update = nth(&f, "/api/chat.update", 1).await;
    assert_eq!(
        update["text"],
        "Marked 'Ship &lt;the&gt; exporter' complete."
    );
    assert_eq!(ids(&row(&update["blocks"], "Ship <the> exporter")), [OPEN]);
    assert_eq!(
        ids(&row(&update["blocks"], "Tidy the parser")),
        [COMPLETE, RELEASE, OPEN]
    );
    assert!(has_stop(&update["blocks"]));
    assert!(text_of(&update["blocks"]).contains("Release frees the files"));
    assert_eq!(work(&f, &exporter.id).state, WorkState::Finished);
    let events = tokio::task::block_in_place(|| f.store().events_after(0, 1000).unwrap());
    let transition: Value = events
        .iter()
        .rev()
        .find(|event| event.kind == "work_transition" && event.subject == exporter.id)
        .map(|event| serde_json::from_str(&event.data).unwrap())
        .unwrap();
    assert_eq!(transition["transition"], "complete");
    assert_eq!(transition["actor"]["kind"], "owner");
    assert_eq!(transition["actor"]["via"], "person");
    assert_eq!(transition["actor"]["login"], "alice");
    assert_eq!(
        transition["reason"],
        "Marked complete by its owner from the weekly digest in Slack."
    );

    // The same button again finds the Work changed since the digest and changes nothing.
    assert_eq!(
        f.click_value("U1", COMPLETE, &complete, body).await,
        StatusCode::OK
    );
    let update = nth(&f, "/api/chat.update", 2).await;
    assert!(
        update["text"]
            .as_str()
            .unwrap()
            .starts_with("Nothing changed."),
        "{update}"
    );

    // Release frees the other task's files and leaves it unfinished.
    let release = button(&body["blocks"], "Tidy the parser", RELEASE)["value"].clone();
    assert_eq!(
        f.click_value("U1", RELEASE, &release, body).await,
        StatusCode::OK
    );
    let update = nth(&f, "/api/chat.update", 3).await;
    assert!(
        update["text"]
            .as_str()
            .unwrap()
            .starts_with("Released 'Tidy the parser'."),
        "{update}"
    );
    let released = work(&f, &parser.id);
    assert_eq!(
        (released.state, released.generation),
        (WorkState::Paused, parser.generation + 1)
    );

    // A malformed button value is a protocol error, and an unknown Work is nobody's.
    let payload = json!({"w": "not-an-id", "g": 1, "u": 1}).to_string();
    f.click_value("U1", COMPLETE, &json!(payload), body).await;
    nth(&f, "/api/chat.postEphemeral", 3).await;
    let missing = json!({"w": format!("wrk_{}", "0".repeat(32)), "g": 1, "u": 1}).to_string();
    f.click_value("U1", COMPLETE, &json!(missing), body).await;
    let notice = nth(&f, "/api/chat.postEphemeral", 4).await;
    assert!(
        notice["text"].as_str().unwrap().contains("is not yours"),
        "{notice}"
    );
    assert_eq!(
        f.click_value("U1", COMPLETE, &json!("{"), body).await,
        StatusCode::OK
    );
    let notice = nth(&f, "/api/chat.postEphemeral", 5).await;
    assert!(
        notice["text"].as_str().unwrap().contains("did not accept"),
        "{notice}"
    );
}
