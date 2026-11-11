//! A lead start that names a forge issue whose unfinished Work another person leads, and what the
//! Hub saw next: store cases run against SQLite always and PostgreSQL when
//! AGENTSCM_TEST_DATABASE_URL is set.
use crate::hub_pg;

use std::sync::Arc;

use agentscm::hub::HubError;
use agentscm::hub::duplicate_starts::{MAX_ROWS_PER_TOKEN, WINDOW_SECONDS};
use agentscm::hub::store::{
    self, ForgeIdentity, Store, agent_token_digest, mint_agent_token, unix_now,
};
use agentscm::hub::value_metrics::{ValueMetrics, ValueMetricsQuery};
use agentscm::hub::work::{
    CheckpointKind, ForgeWorkRef, WorkCheckpoint, WorkConflictKind, WorkItem, WorkRole, WorkStart,
};
use agentscm::hub::work_preflight::WorkPreflightRequest;
use hub_pg::PgSchema;
use postgres::NoTls;
use serde_json::{Value, json};
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

    fn sql(&self, sql: &str) {
        if self._pg.is_some() {
            postgres::Client::connect(&self.url, NoTls)
                .unwrap()
                .batch_execute(sql)
                .unwrap();
        } else {
            let path = self.url.strip_prefix("sqlite://").unwrap();
            rusqlite::Connection::open(path)
                .unwrap()
                .execute_batch(sql)
                .unwrap();
        }
    }
}

struct Agent {
    token: String,
    secret: String,
    owner: String,
}

fn agent(store: &dyn Store, user_id: i64) -> Agent {
    let identity = ForgeIdentity {
        provider: "github".into(),
        host: "github.com".into(),
        user_id,
    };
    let owner = store
        .upsert_user(&identity, &format!("user{user_id}"))
        .unwrap();
    another_token(store, &owner.id, &format!("agent{user_id}"))
}

fn another_token(store: &dyn Store, owner: &str, name: &str) -> Agent {
    let (token, secret) = mint_agent_token(store, owner, name, REPO, 3600).unwrap();
    Agent {
        token,
        secret: secret.expose().to_owned(),
        owner: owner.to_owned(),
    }
}

fn start(source: Option<&str>, work_id: Option<&str>, session: &str, role: WorkRole) -> WorkStart {
    WorkStart {
        issue_choice: None,
        work_id: work_id.map(str::to_owned),
        source: source.map(|issue| ForgeWorkRef {
            provider: "github".into(),
            host: "github.com".into(),
            repository: REPO.into(),
            issue: issue.into(),
        }),
        request_key: None,
        title: "forge work".into(),
        description: None,
        session: session.into(),
        role,
        scope: vec![format!("src/{}.rs", source.unwrap_or("joined"))],
        branch: None,
        worktree: None,
        next_action: None,
        depends_on: vec![],
        related_work: vec![],
        takeover_generation: None,
        preflight: None,
    }
}

fn lead(issue: &str, session: &str) -> WorkStart {
    start(Some(issue), None, session, WorkRole::Lead)
}

/// The start with a preflight the Hub cannot call clear, so it is refused before `start_in`.
fn not_clear(mut start: WorkStart) -> WorkStart {
    start.preflight = Some(WorkPreflightRequest {
        source_revision: format!("{:040x}", 7),
        merge_after: 0,
        merge_through: 0,
        source_relation_checked: false,
        checked_merges: Vec::new(),
    });
    start
}

fn hold(store: &dyn Store, who: &Agent, issue: &str, session: &str) -> WorkItem {
    let started = store
        .work_start(&who.token, &lead(issue, session), T0)
        .unwrap();
    assert!(started.created);
    started.item
}

fn duplicate_events(store: &dyn Store) -> Vec<(String, Value)> {
    store
        .events_after(0, 1000)
        .unwrap()
        .into_iter()
        .filter(|event| event.kind == "duplicate_start")
        .map(|event| (event.subject, serde_json::from_str(&event.data).unwrap()))
        .collect()
}

fn metrics(store: &dyn Store, token: &str, now: i64) -> ValueMetrics {
    let query = ValueMetricsQuery {
        since: Some(0),
        ..Default::default()
    };
    store.value_metrics(token, &query, now).unwrap()
}

fn a_start_on_another_persons_issue_is_recorded_once(db: &Db) {
    let store = db.open();
    let (alice, bob) = (agent(&*store, 1), agent(&*store, 2));
    let held = hold(&*store, &alice, "7", "sa");
    let answer = store
        .work_start(&bob.token, &lead("7", "sb"), T0 + 10)
        .unwrap();
    assert_eq!(
        answer.conflict.map(|conflict| conflict.kind),
        Some(WorkConflictKind::ClaimHeld)
    );
    // The same session asking again writes neither a second row nor a second event.
    store
        .work_start(&bob.token, &lead("7", "sb"), T0 + 20)
        .unwrap();
    let events = duplicate_events(&*store);
    assert_eq!(events.len(), 1);
    let (subject, data) = &events[0];
    assert_eq!(subject, &bob.token);
    assert_eq!(
        data,
        &json!({
            "token": bob.token,
            "issue": "7",
            "source": {"provider": "github", "host": "github.com", "repository": REPO,
                "issue": "7"},
            "work": held.id,
            "holder": {"login": "user1"},
            "answer": "claim_held",
        })
    );
    assert_eq!(
        metrics(&*store, &bob.token, T0 + 30)
            .issues
            .duplicate_starts
            .observed,
        1
    );
    // The status page's activity shows it.
    let digest = agent_token_digest(&bob.secret).unwrap();
    let principal = store.authenticate(&digest, unix_now()).unwrap().unwrap();
    let activity = store.repository_events(&principal, None, 100).unwrap();
    assert!(
        activity
            .iter()
            .any(|event| event.event.kind == "duplicate_start" && event.token == bob.token)
    );
}

fn a_start_refused_by_its_preflight_is_still_recorded(db: &Db) {
    let store = db.open();
    let (alice, bob) = (agent(&*store, 1), agent(&*store, 2));
    hold(&*store, &alice, "7", "sa");
    let refused = store.work_start(&bob.token, &not_clear(lead("7", "sb")), T0 + 10);
    assert!(
        matches!(refused, Err(HubError::WorkPreflight(_))),
        "{refused:?}"
    );
    let events = duplicate_events(&*store);
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].1["answer"], "work_preflight_required");
    assert_eq!(
        metrics(&*store, &bob.token, T0 + 30)
            .issues
            .duplicate_starts
            .pending,
        1
    );
}

fn one_persons_second_agent_is_not_someone_else(db: &Db) {
    let store = db.open();
    let alice = agent(&*store, 1);
    let alice_two = another_token(&*store, &alice.owner, "agent1-b");
    hold(&*store, &alice, "7", "sa");
    let answer = store
        .work_start(&alice_two.token, &lead("7", "sa2"), T0 + 10)
        .unwrap();
    assert!(answer.conflict.is_some());
    // Refused by its preflight, the owner check still decides.
    let refused = store.work_start(&alice_two.token, &not_clear(lead("7", "sa3")), T0 + 20);
    assert!(
        matches!(refused, Err(HubError::WorkPreflight(_))),
        "{refused:?}"
    );
    assert!(duplicate_events(&*store).is_empty());
    let block = metrics(&*store, &alice.token, T0 + 30)
        .issues
        .duplicate_starts;
    assert_eq!(
        (block.availability.as_str(), block.observed),
        ("none_observed", 0)
    );
}

fn joins_takeovers_and_starts_by_work_id_write_nothing(db: &Db) {
    let store = db.open();
    let (alice, bob, carol) = (agent(&*store, 1), agent(&*store, 2), agent(&*store, 3));
    let held = hold(&*store, &alice, "7", "sa");
    let joined = store
        .work_start(
            &bob.token,
            &start(None, Some(&held.id), "sb-help", WorkRole::Helper),
            T0 + 1,
        )
        .unwrap();
    assert!(joined.joined);
    // A lead start by work_id names the Work, not the issue.
    let by_id = store
        .work_start(
            &bob.token,
            &start(None, Some(&held.id), "sb-lead", WorkRole::Lead),
            T0 + 2,
        )
        .unwrap();
    assert!(by_id.conflict.is_some());
    // Alice hands the Work over and Carol takes it by its issue.
    store
        .work_checkpoint(
            &alice.token,
            &WorkCheckpoint {
                work_id: held.id.clone(),
                operation_id: "handover".into(),
                generation: held.generation,
                session: "sa".into(),
                expected_updated_seq: Some(joined.item.updated_seq),
                kind: CheckpointKind::Handover,
                summary: "ready for takeover".into(),
                next_action: Some("next".into()),
                scope: None,
                branch: None,
                worktree: None,
                resolve_blocker: false,
                source_refs: vec![],
                depends_on: vec![],
                related_work: vec![],
            },
            T0 + 3,
        )
        .unwrap();
    // A pickup's first try without takeover_generation is refused, but the Work was offered.
    let first = store
        .work_start(&carol.token, &lead("7", "sc"), T0 + 4)
        .unwrap();
    assert!(first.conflict.is_some(), "{first:?}");
    let mut take = lead("7", "sc");
    take.takeover_generation = Some(held.generation);
    let taken = store.work_start(&carol.token, &take, T0 + 5).unwrap();
    assert!(taken.took_over, "{taken:?}");
    // Alice still owns the Work; Carol leads it, so Carol's second agent is not someone else.
    let carol_two = another_token(&*store, &carol.owner, "agent3-b");
    let again = store
        .work_start(&carol_two.token, &lead("7", "sc2"), T0 + 6)
        .unwrap();
    assert!(again.conflict.is_some(), "{again:?}");
    assert!(duplicate_events(&*store).is_empty());
    assert_eq!(
        metrics(&*store, &carol.token, T0 + 30)
            .issues
            .duplicate_starts
            .observed,
        0
    );
    // Bob's start is recorded against the lead, not the owner.
    store
        .work_start(&bob.token, &lead("7", "sb-again"), T0 + 7)
        .unwrap();
    let events = duplicate_events(&*store);
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].1["holder"]["login"], "user3");
}

fn outcomes_follow_what_the_asking_person_did_next(db: &Db) {
    let store = db.open();
    let (alice, bob) = (agent(&*store, 1), agent(&*store, 2));
    let seven = hold(&*store, &alice, "7", "sa7");
    let eight = hold(&*store, &alice, "8", "sa8");
    let asked = T0 + 10;
    for issue in ["7", "8"] {
        let answer = store
            .work_start(&bob.token, &lead(issue, "sb"), asked)
            .unwrap();
        assert!(answer.conflict.is_some());
    }
    // Bob joins the Work on #7 as a helper: proceeded. Nothing is seen for #8.
    store
        .work_start(
            &bob.token,
            &start(None, Some(&seven.id), "sb-help", WorkRole::Helper),
            asked + 10,
        )
        .unwrap();

    let early = metrics(&*store, &bob.token, asked + 3600);
    let issues = serde_json::to_value(&early.issues).unwrap();
    assert_eq!(
        issues["duplicate_starts"],
        json!({"availability": "observed", "observed": 2, "proceeded": 1, "stood_down": 0,
            "pending": 1, "unknown": 0, "partial": false})
    );
    // The block adds nothing to the issue totals.
    assert_eq!(
        (
            early.issues.resolved,
            early.issues.unresolved,
            early.issues.dismissed
        ),
        (0, 0, None)
    );
    assert!(
        early
            .coverage
            .gaps
            .iter()
            .any(|gap| gap.starts_with("Duplicate starts are recorded only"))
    );

    let late = metrics(&*store, &bob.token, asked + WINDOW_SECONDS + 1);
    let block = &late.issues.duplicate_starts;
    assert_eq!(
        (block.proceeded, block.stood_down, block.pending),
        (1, 1, 0)
    );
    assert_eq!((late.issues.resolved, late.issues.unresolved), (0, 0));
    assert!(
        late.coverage
            .gaps
            .iter()
            .any(|gap| gap.starts_with("Stood down"))
    );

    // The holder's Work record is gone: unknown.
    db.sql(&format!(
        "UPDATE hub_duplicate_starts SET holder_work = 'wrk_gone' WHERE holder_work = '{}'",
        eight.id
    ));
    let gone = metrics(&*store, &bob.token, asked + WINDOW_SECONDS + 2);
    let block = &gone.issues.duplicate_starts;
    assert_eq!(
        (block.proceeded, block.unknown, block.stood_down),
        (1, 1, 0)
    );
}

fn a_token_at_its_cap_keeps_its_newest_rows_and_reads_partial(db: &Db) {
    let store = db.open();
    let (alice, bob) = (agent(&*store, 1), agent(&*store, 2));
    hold(&*store, &alice, "7", "sa");
    let rows: Vec<String> = (0..MAX_ROWS_PER_TOKEN)
        .map(|n| {
            format!(
                "INSERT INTO hub_duplicate_starts
                     (repository, token, session, owner, holder_work, holder_owner, observed_at)
                 VALUES ('{REPO}', '{}', 's{n}', '{}', 'wrk_old{n}', '{}', {});",
                bob.token,
                bob.owner,
                alice.owner,
                T0 + n
            )
        })
        .collect();
    db.sql(&rows.concat());
    store
        .work_start(&bob.token, &lead("7", "sb"), T0 + 2 * MAX_ROWS_PER_TOKEN)
        .unwrap();
    let block = metrics(&*store, &bob.token, T0 + 3 * MAX_ROWS_PER_TOKEN)
        .issues
        .duplicate_starts;
    // The oldest row made room for the new one.
    assert_eq!(block.observed as i64, MAX_ROWS_PER_TOKEN);
    assert_eq!(block.pending, 1);
    assert!(block.partial, "a token at its cap makes the counts partial");
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
    a_start_on_another_persons_issue_is_recorded_once,
    a_start_refused_by_its_preflight_is_still_recorded,
    one_persons_second_agent_is_not_someone_else,
    joins_takeovers_and_starts_by_work_id_write_nothing,
    outcomes_follow_what_the_asking_person_did_next,
    a_token_at_its_cap_keeps_its_newest_rows_and_reads_partial,
);
