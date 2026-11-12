//! Recovery inspection, remedy previews and transition batches through the shared service, on a
//! deterministic clock. Every case runs on SQLite and, when configured, PostgreSQL.
use crate::hub_pg;

use std::sync::Arc;

use agentscm::hub::HubError;
use agentscm::hub::store::{self, ForgeIdentity, Store, User, mint_bound_agent_token, unix_now};
use agentscm::hub::work::{
    CheckpointKind, WorkCheckpoint, WorkItem, WorkRole, WorkStart, WorkState,
};
use agentscm::hub::work_lifecycle::{
    ActivityStatus, LEASE_SECONDS, Recipient, ReservationStatus, WorkTransitionKind,
};
use agentscm::hub::work_recovery::{
    BatchItemV1, BatchOutcome, BatchTransitionItem, FindingKind, RecoveryBucket, RecoveryCaller,
    RecoveryQuery, RefusalCode, RemedyKind, RemedyPreviewRequest, Standing, WorkRecoveryV1,
    WorkTransitionBatch, apply_batch, inspect, preview,
};
use agentscm::hub::work_transition::{
    OperationalSelect, ParticipantRef, WorkLifecycleReader, WorkTransitionActor,
    WorkTransitionRequest,
};
use hub_pg::PgSchema;
use tempfile::TempDir;

const REPO: &str = "acme/widgets";
const REPOSITORY_ID: i64 = 101;
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

    /// Runs raw SQL, to build a database state the Store can no longer reach.
    fn sql(&self, sql: &str) {
        if self._pg.is_some() {
            postgres::Client::connect(&self.url, postgres::NoTls)
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

    /// The first column of the first row of a query, as an integer or text.
    fn scalar(&self, sql: &str) -> String {
        if self._pg.is_some() {
            let row = postgres::Client::connect(&self.url, postgres::NoTls)
                .unwrap()
                .query_one(&format!("SELECT CAST(({sql}) AS TEXT)"), &[])
                .unwrap();
            row.get(0)
        } else {
            let path = self.url.strip_prefix("sqlite://").unwrap();
            rusqlite::Connection::open(path)
                .unwrap()
                .query_row(&format!("SELECT CAST(({sql}) AS TEXT)"), [], |row| {
                    row.get(0)
                })
                .unwrap()
        }
    }
}

struct Agent {
    token: String,
    user: User,
}

fn user(store: &dyn Store, id: i64) -> User {
    store
        .upsert_user(
            &ForgeIdentity {
                provider: "github".into(),
                host: "github.com".into(),
                user_id: id,
            },
            &format!("user{id}"),
        )
        .unwrap()
}

fn token(store: &dyn Store, user: &User, n: i64, read_only: bool) -> Agent {
    let (token, _, _) = mint_bound_agent_token(
        store,
        &user.id,
        &format!("agent{n}"),
        REPO,
        REPOSITORY_ID,
        TTL,
        read_only,
    )
    .unwrap();
    Agent {
        token,
        user: user.clone(),
    }
}

fn agent(store: &dyn Store, id: i64) -> Agent {
    token(store, &user(store, id), id, false)
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
        next_action: Some(format!("finish {key}")),
        depends_on: vec![],
        related_work: vec![],
        takeover_generation: None,
        preflight: None,
        issue_choice: None,
    }
}

fn caller<'a>(who: &'a Agent, session: &'a str) -> RecoveryCaller<'a> {
    RecoveryCaller::Agent {
        token: &who.token,
        session: Some(session),
    }
}

fn read(
    store: &dyn Store,
    caller: &RecoveryCaller<'_>,
    query: RecoveryQuery,
    now: i64,
) -> WorkRecoveryV1 {
    inspect(store, caller, &query, now).unwrap()
}

fn one(store: &dyn Store, caller: &RecoveryCaller<'_>, id: &str, now: i64) -> WorkRecoveryV1 {
    read(
        store,
        caller,
        RecoveryQuery {
            work_id: Some(id.into()),
            ..RecoveryQuery::default()
        },
        now,
    )
}

fn permitted(recovery: &WorkRecoveryV1, id: &str) -> Vec<RemedyKind> {
    recovery
        .work
        .iter()
        .find(|work| work.work_id == id)
        .unwrap()
        .remedies
        .iter()
        .filter(|remedy| remedy.permitted)
        .map(|remedy| remedy.remedy)
        .collect()
}

fn item(
    work_id: &str,
    operation_id: &str,
    generation: i64,
    seq: i64,
    transition: WorkTransitionKind,
) -> BatchTransitionItem {
    BatchTransitionItem {
        work_id: work_id.into(),
        operation_id: operation_id.into(),
        generation,
        expected_updated_seq: seq,
        transition,
        reason: "synthetic recovery".into(),
        superseded_by: None,
        participants: Vec::new(),
    }
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

/// A harness agent retires its owner's obsolete reservation from a new session: the preview
/// shows what it frees and keeps, the batch applies it once, and a retry replays it.
fn an_owner_retires_an_obsolete_hold_and_keeps_its_follow_up(db: &Db) {
    let store = db.open();
    let alice = agent(&*store, 1);
    let now = unix_now();
    store.observe_lease_clock(now).unwrap();
    let old = store
        .work_start(&alice.token, &start("old", "s-dead", "src/old/**"), now)
        .unwrap()
        .item;
    let me = caller(&alice, "s-new");
    let page = read(&*store, &me, RecoveryQuery::default(), now + 1);
    let entry = &page.work[0];
    assert_eq!(entry.work_id, old.id);
    assert_eq!(entry.standing, Standing::Owner);
    assert_eq!(entry.bucket, RecoveryBucket::Active);
    assert!(entry.why.contains("holds its scope"), "{}", entry.why);
    let open = permitted(&page, &old.id);
    assert!(open.contains(&RemedyKind::Release), "{open:?}");
    // The other session's hold is live, so no takeover start is offered.
    assert!(!open.contains(&RemedyKind::Resume));

    let release = preview(
        &*store,
        &me,
        &RemedyPreviewRequest {
            work_id: old.id.clone(),
            remedy: RemedyKind::Release,
            session: Some("s-new".into()),
            scope: None,
            superseded_by: None,
        },
        now + 2,
    )
    .unwrap();
    assert!(release.permitted);
    assert_eq!(release.effects.state, WorkState::Paused);
    assert_eq!(release.effects.reservation, ReservationStatus::None);
    assert!(release.effects.follow_up.unfinished);
    assert_eq!(
        release.effects.follow_up.next_action.as_deref(),
        Some("finish old")
    );
    assert_eq!(release.effects.freed_paths, ["src/old/**"]);
    let apply = release.apply.clone().unwrap();
    assert_eq!(apply.tool.as_deref(), Some("agentscm_work_transition"));
    assert_eq!(apply.arguments["generation"], old.generation);

    let batch = WorkTransitionBatch {
        session: Some("s-new".into()),
        items: vec![item(
            &old.id,
            "retire-old",
            release.generation,
            release.expected_updated_seq,
            WorkTransitionKind::Release,
        )],
    };
    let actor = WorkTransitionActor::Agent {
        token: &alice.token,
    };
    let applied = apply_batch(&*store, &actor, &batch, now + 3).unwrap();
    assert_eq!(applied.items[0].outcome, BatchOutcome::Applied);
    assert_eq!((applied.applied, applied.not_applied), (1, 0));
    // The same batch again replays the receipt and changes nothing.
    let replayed = apply_batch(&*store, &actor, &batch, now + 4).unwrap();
    assert_eq!(replayed.items[0].outcome, BatchOutcome::Replayed);

    let after = one(&*store, &me, &old.id, now + 5);
    let entry = &after.work[0];
    assert_eq!(entry.bucket, RecoveryBucket::Resumable);
    assert_eq!(
        entry.operational.reservation.status,
        ReservationStatus::None
    );
    assert_eq!(entry.follow_up.next_action.as_deref(), Some("finish old"));
    assert!(entry.follow_up.unfinished);
}

/// A new session of the owner resumes Work whose hold lapsed, through the exact call the preview
/// names; the old session's start is fenced.
fn the_owner_resumes_expired_work_after_a_session_restart(db: &Db) {
    let store = db.open();
    let alice = agent(&*store, 1);
    let now = unix_now();
    store.observe_lease_clock(now).unwrap();
    let held = store
        .work_start(&alice.token, &start("a", "s-old", "src/**"), now)
        .unwrap()
        .item;
    let later = now + LEASE_SECONDS + 1;
    tick(&*store, now, later);
    // A Hub restart in between: a fresh store on the same database reads the same thing.
    let store = db.open();
    let me = caller(&alice, "s-restarted");
    let page = one(&*store, &me, &held.id, later);
    let entry = &page.work[0];
    assert_eq!(
        entry.operational.reservation.status,
        ReservationStatus::Lapsed
    );
    assert_eq!(entry.bucket, RecoveryBucket::Resumable);
    assert_eq!(entry.operational.actionable.recipient, Recipient::None);
    let resume = entry
        .remedies
        .iter()
        .find(|remedy| remedy.remedy == RemedyKind::Resume)
        .unwrap();
    assert!(resume.permitted, "{resume:?}");
    // Retain cannot revive a lapsed hold.
    let retain = entry
        .remedies
        .iter()
        .find(|remedy| remedy.remedy == RemedyKind::Retain)
        .unwrap();
    assert_eq!(
        retain.refusal.as_ref().map(|r| r.code),
        Some(RefusalCode::LeaseLapsed)
    );
    let call = resume.apply.as_ref().unwrap();
    assert_eq!(call.tool.as_deref(), Some("agentscm_start"));
    let generation = call.arguments["takeover_generation"].as_i64().unwrap();
    let resumed = store
        .work_start(
            &alice.token,
            &WorkStart {
                work_id: Some(held.id.clone()),
                request_key: None,
                takeover_generation: Some(generation),
                ..start("a", "s-restarted", "src/**")
            },
            later + 1,
        )
        .unwrap();
    assert!(resumed.took_over, "{resumed:?}");
    let entry = one(&*store, &me, &held.id, later + 2).work.remove(0);
    assert_eq!(entry.standing, Standing::Lead);
    assert_eq!(entry.bucket, RecoveryBucket::Active);
    assert_eq!(entry.operational.activity.status, ActivityStatus::Live);
}

/// Another owner's live hold is listed with its exact overlapping paths and what to do, but no
/// remedy takes it over, and a batch release of it is refused.
fn another_owners_live_conflict_is_identified_not_taken(db: &Db) {
    let store = db.open();
    let (alice, bob) = (agent(&*store, 1), agent(&*store, 2));
    let now = unix_now();
    store.observe_lease_clock(now).unwrap();
    let theirs = store
        .work_start(&bob.token, &start("b", "s-bob", "src/shared/**"), now)
        .unwrap()
        .item;
    let me = caller(&alice, "s-alice");
    let page = read(
        &*store,
        &me,
        RecoveryQuery {
            paths: vec!["src/shared/lib.rs".into()],
            ..RecoveryQuery::default()
        },
        now + 1,
    );
    assert_eq!(page.conflicts.len(), 1);
    let conflict = &page.conflicts[0];
    assert_eq!(conflict.work_id, theirs.id);
    assert!(!conflict.mine);
    assert_eq!(conflict.paths[0].your_path, "src/shared/lib.rs");
    assert_eq!(conflict.paths[0].their_path, "src/shared/**");
    assert_eq!(conflict.lease_expires_at, Some(now + LEASE_SECONDS));
    assert!(
        conflict.next.contains("Do not take it over"),
        "{}",
        conflict.next
    );
    // Alice's own page holds none of Bob's Work, and nothing for her to resume there.
    assert!(page.work.is_empty());
    assert!(page.resumable.is_empty());
    let detail = one(&*store, &me, &theirs.id, now + 1);
    assert_eq!(detail.work[0].bucket, RecoveryBucket::Other);
    assert!(permitted(&detail, &theirs.id).is_empty());
    let resume = preview(
        &*store,
        &me,
        &RemedyPreviewRequest {
            work_id: theirs.id.clone(),
            remedy: RemedyKind::Resume,
            session: Some("s-alice".into()),
            scope: None,
            superseded_by: None,
        },
        now + 2,
    )
    .unwrap();
    assert!(!resume.permitted);
    assert_eq!(resume.refusal.unwrap().code, RefusalCode::HoldLive);
    let refused = apply_batch(
        &*store,
        &WorkTransitionActor::Agent {
            token: &alice.token,
        },
        &WorkTransitionBatch {
            session: Some("s-alice".into()),
            items: vec![item(
                &theirs.id,
                "steal",
                theirs.generation,
                theirs.updated_seq,
                WorkTransitionKind::Release,
            )],
        },
        now + 3,
    )
    .unwrap();
    assert_eq!(refused.items[0].outcome, BatchOutcome::Refused);
    assert_eq!(refused.items[0].code.as_deref(), Some("unauthorized"));
    // Bob's hold is untouched.
    let held = store
        .work_operational(
            &WorkLifecycleReader::Token(&bob.token),
            &OperationalSelect::One(&theirs.id),
            now + 4,
        )
        .unwrap();
    assert_eq!(held.work[0].reservation.status, ReservationStatus::Held);
    // Once the lease lapsed, the hold blocks nobody and is no longer a conflict.
    let later = now + LEASE_SECONDS + 1;
    tick(&*store, now, later);
    let page = read(
        &*store,
        &me,
        RecoveryQuery {
            paths: vec!["src/shared/lib.rs".into()],
            ..RecoveryQuery::default()
        },
        later,
    );
    assert!(page.conflicts.is_empty());
}

/// A read-only token inspects everything a writer sees but every remedy is refused, and an
/// apply changes nothing.
fn read_only_callers_cannot_escalate(db: &Db) {
    let store = db.open();
    let alice = agent(&*store, 1);
    let reader = token(&*store, &alice.user, 9, true);
    let now = unix_now();
    store.observe_lease_clock(now).unwrap();
    let work = store
        .work_start(&alice.token, &start("a", "s-a", "src/**"), now)
        .unwrap()
        .item;
    let page = read(
        &*store,
        &caller(&reader, "s-a"),
        RecoveryQuery::default(),
        now + 1,
    );
    assert!(page.read_only);
    assert_eq!(page.work.len(), 1);
    assert!(page.work[0].remedies.iter().all(|remedy| {
        !remedy.permitted && remedy.refusal.as_ref().unwrap().code == RefusalCode::ReadOnly
    }));
    let refused = apply_batch(
        &*store,
        &WorkTransitionActor::Agent {
            token: &reader.token,
        },
        &WorkTransitionBatch {
            session: Some("s-a".into()),
            items: vec![item(
                &work.id,
                "read-only",
                work.generation,
                work.updated_seq,
                WorkTransitionKind::Abandon,
            )],
        },
        now + 2,
    )
    .unwrap();
    assert_eq!(refused.items[0].outcome, BatchOutcome::Refused);
    let after = one(&*store, &caller(&alice, "s-a"), &work.id, now + 3);
    assert_eq!(after.work[0].operational.state, WorkState::Active);
}

/// Each item of a batch has its own outcome: a raced item is refused with how to retry while the
/// others apply, and the whole batch can be sent again safely.
fn a_partly_applied_batch_explains_each_item_and_retries_safely(db: &Db) {
    let store = db.open();
    let alice = agent(&*store, 1);
    let now = unix_now();
    store.observe_lease_clock(now).unwrap();
    let a = store
        .work_start(&alice.token, &start("a", "s-a", "src/a/**"), now)
        .unwrap()
        .item;
    let b = store
        .work_start(&alice.token, &start("b", "s-b", "src/b/**"), now)
        .unwrap()
        .item;
    // B moves on after the caller read it.
    store
        .work_checkpoint(
            &alice.token,
            &WorkCheckpoint {
                work_id: b.id.clone(),
                operation_id: "b-progress".into(),
                generation: b.generation,
                session: "s-b".into(),
                expected_updated_seq: Some(b.updated_seq),
                kind: CheckpointKind::Progress,
                summary: "moved on".into(),
                next_action: None,
                scope: None,
                branch: None,
                worktree: None,
                resolve_blocker: false,
                source_refs: vec![],
                depends_on: vec![],
                related_work: vec![],
            },
            now + 1,
        )
        .unwrap();
    let batch = WorkTransitionBatch {
        session: Some("s-sweeper".into()),
        items: vec![
            item(
                &a.id,
                "a-release",
                a.generation,
                a.updated_seq,
                WorkTransitionKind::Release,
            ),
            item(
                &b.id,
                "b-release",
                b.generation,
                b.updated_seq,
                WorkTransitionKind::Release,
            ),
        ],
    };
    let actor = WorkTransitionActor::Agent {
        token: &alice.token,
    };
    let first = apply_batch(&*store, &actor, &batch, now + 2).unwrap();
    assert_eq!(first.items[0].outcome, BatchOutcome::Applied);
    assert_eq!(first.items[1].outcome, BatchOutcome::Refused);
    assert_eq!(first.items[1].code.as_deref(), Some("work_changed"));
    assert!(first.items[1].retry.contains("current generation"));
    assert_eq!((first.applied, first.not_applied), (1, 1));
    let again = apply_batch(&*store, &actor, &batch, now + 3).unwrap();
    assert_eq!(again.items[0].outcome, BatchOutcome::Replayed);
    assert_eq!(again.items[1].outcome, BatchOutcome::Refused);
    // Re-read and retry with the current fences.
    let current = one(&*store, &caller(&alice, "s-sweeper"), &b.id, now + 4);
    let view = &current.work[0].operational;
    let retried = apply_batch(
        &*store,
        &actor,
        &WorkTransitionBatch {
            session: Some("s-sweeper".into()),
            items: vec![item(
                &b.id,
                "b-release-2",
                view.generation,
                view.updated_seq,
                WorkTransitionKind::Release,
            )],
        },
        now + 5,
    )
    .unwrap();
    assert_eq!(retried.items[0].outcome, BatchOutcome::Applied);
}

/// The owner's unfinished Work pages by id; finished Work and other owners' Work stay out.
fn the_owners_unfinished_work_pages_in_order(db: &Db) {
    let store = db.open();
    let (alice, bob) = (agent(&*store, 1), agent(&*store, 2));
    let now = unix_now();
    store.observe_lease_clock(now).unwrap();
    let mut ids: Vec<String> = (0..5)
        .map(|n| {
            store
                .work_start(
                    &alice.token,
                    &start(&format!("w{n}"), &format!("s-{n}"), &format!("src/{n}/**")),
                    now,
                )
                .unwrap()
                .item
                .id
        })
        .collect();
    store
        .work_start(&bob.token, &start("bob", "s-bob", "bob/**"), now)
        .unwrap();
    // One of Alice's ends.
    let done = ids.remove(2);
    let ended = one(&*store, &caller(&alice, "s-2"), &done, now);
    let view = &ended.work[0].operational;
    let result = apply_batch(
        &*store,
        &WorkTransitionActor::Agent {
            token: &alice.token,
        },
        &WorkTransitionBatch {
            session: Some("s-2".into()),
            items: vec![item(
                &done,
                "done",
                view.generation,
                view.updated_seq,
                WorkTransitionKind::Complete,
            )],
        },
        now + 1,
    )
    .unwrap();
    assert_eq!(result.items[0].outcome, BatchOutcome::Applied);
    ids.sort();
    let me = caller(&alice, "s-pager");
    let mut seen = Vec::new();
    let mut after = None;
    loop {
        let page = read(
            &*store,
            &me,
            RecoveryQuery {
                after: after.clone(),
                limit: Some(2),
                ..RecoveryQuery::default()
            },
            now + 2,
        );
        assert!(page.work.len() <= 2);
        seen.extend(page.work.iter().map(|work| work.work_id.clone()));
        if page.complete {
            assert!(page.next_after.is_none());
            break;
        }
        after = page.next_after.clone();
        assert!(after.is_some());
    }
    assert_eq!(seen, ids);
}

/// Restart grace is the Hub's doing, not the lead's: the projection says so until the lead
/// renews, and a lapse after it names it.
fn restart_grace_is_never_reported_as_a_renewal(db: &Db) {
    let store = db.open();
    let alice = agent(&*store, 1);
    let now = unix_now();
    store.observe_lease_clock(now).unwrap();
    let held = store
        .work_start(&alice.token, &start("a", "s-a", "src/**"), now)
        .unwrap()
        .item;
    // The Hub is down for an hour, then restarts on the same database.
    let back = now + 3600;
    let store = db.open();
    store.observe_lease_clock(back).unwrap();
    let view = |store: &dyn Store, at: i64| {
        store
            .work_operational(
                &WorkLifecycleReader::Token(&alice.token),
                &OperationalSelect::One(&held.id),
                at,
            )
            .unwrap()
            .work
            .remove(0)
    };
    let graced = view(&*store, back + 1);
    assert_eq!(graced.reservation.status, ReservationStatus::Held);
    assert_eq!(graced.activity.lease_grace_at, Some(back));
    assert_eq!(graced.activity.lease_renewed_at, Some(now));
    assert!(
        graced
            .activity
            .reason
            .starts_with("The Hub, not the lead session"),
        "{}",
        graced.activity.reason
    );
    // A renewal by the lead session clears the grace mark.
    store
        .touch_session(
            &alice.token,
            &agentscm::hub::session::SessionCall {
                session: Some("s-a".into()),
                via: "hook:claude".into(),
                task: None,
                paths: Vec::new(),
                deny: false,
                report: false,
            },
            back + 120,
        )
        .unwrap();
    let renewed = view(&*store, back + 121);
    assert_eq!(renewed.activity.lease_grace_at, None);
    assert_eq!(renewed.activity.lease_renewed_at, Some(back + 120));
    assert!(
        renewed
            .activity
            .reason
            .starts_with("The lead session renewed")
    );
}

/// Paused Work whose lease ran out is reported lapsed, never held, and names nobody.
fn lapsed_or_paused_work_is_not_reported_held(db: &Db) {
    let store = db.open();
    let alice = agent(&*store, 1);
    let now = unix_now();
    store.observe_lease_clock(now).unwrap();
    let work = store
        .work_start(&alice.token, &start("a", "s-a", "src/**"), now)
        .unwrap()
        .item;
    store
        .work_checkpoint(
            &alice.token,
            &WorkCheckpoint {
                work_id: work.id.clone(),
                operation_id: "pause".into(),
                generation: work.generation,
                session: "s-a".into(),
                expected_updated_seq: Some(work.updated_seq),
                kind: CheckpointKind::Pause,
                summary: "stepping away".into(),
                next_action: None,
                scope: None,
                branch: None,
                worktree: None,
                resolve_blocker: false,
                source_refs: vec![],
                depends_on: vec![],
                related_work: vec![],
            },
            now + 1,
        )
        .unwrap();
    let me = caller(&alice, "s-b");
    let paused = one(&*store, &me, &work.id, now + 2).work.remove(0);
    // Just paused, the lease is live: it still blocks, so it is held and the owner decides.
    assert_eq!(
        paused.operational.reservation.status,
        ReservationStatus::Held
    );
    assert_eq!(paused.bucket, RecoveryBucket::NeedsYou);
    let later = now + 1 + LEASE_SECONDS;
    tick(&*store, now, later);
    let lapsed = one(&*store, &me, &work.id, later).work.remove(0);
    assert_eq!(
        lapsed.operational.reservation.status,
        ReservationStatus::Lapsed
    );
    assert_eq!(lapsed.operational.actionable.recipient, Recipient::None);
    assert_eq!(lapsed.bucket, RecoveryBucket::Resumable);
    assert!(lapsed.why.contains("lapsed"), "{}", lapsed.why);
}

/// A scoped handover frees only the paths it drops; the next lead takes the rest.
fn a_scoped_handover_frees_only_what_it_drops(db: &Db) {
    let store = db.open();
    let (alice, bob) = (agent(&*store, 1), agent(&*store, 2));
    let now = unix_now();
    store.observe_lease_clock(now).unwrap();
    let mut first = start("a", "s-a", "src/a/**");
    first.scope.push("src/b/**".into());
    let work = store.work_start(&alice.token, &first, now).unwrap().item;
    let lead = caller(&alice, "s-a");
    let handover = preview(
        &*store,
        &lead,
        &RemedyPreviewRequest {
            work_id: work.id.clone(),
            remedy: RemedyKind::Handover,
            session: Some("s-a".into()),
            scope: Some(vec!["src/a/**".into()]),
            superseded_by: None,
        },
        now + 1,
    )
    .unwrap();
    assert!(handover.permitted, "{:?}", handover.refusal);
    assert_eq!(handover.effects.kept_paths, ["src/a/**"]);
    assert_eq!(handover.effects.freed_paths, ["src/b/**"]);
    let call = handover.apply.unwrap();
    assert_eq!(call.tool.as_deref(), Some("agentscm_work_checkpoint"));
    store
        .work_checkpoint(
            &alice.token,
            &WorkCheckpoint {
                work_id: work.id.clone(),
                operation_id: "handover".into(),
                generation: call.arguments["generation"].as_i64().unwrap(),
                session: "s-a".into(),
                expected_updated_seq: call.arguments["expected_updated_seq"].as_i64(),
                kind: CheckpointKind::Handover,
                summary: "over to you".into(),
                next_action: None,
                scope: Some(vec!["src/a/**".into()]),
                branch: None,
                worktree: None,
                resolve_blocker: false,
                source_refs: vec![],
                depends_on: vec![],
                related_work: vec![],
            },
            now + 2,
        )
        .unwrap();
    // Bob's new Work over the freed paths is admitted; over the kept paths it overlaps.
    let freed = store
        .work_start(&bob.token, &start("freed", "s-bob", "src/b/**"), now + 3)
        .unwrap();
    assert!(freed.created && freed.conflict.is_none(), "{freed:?}");
    let kept = store
        .work_start(&bob.token, &start("kept", "s-bob2", "src/a/**"), now + 3)
        .unwrap();
    assert!(!kept.created);
    // A helper session cannot hand it over.
    let helper = preview(
        &*store,
        &caller(&alice, "s-other"),
        &RemedyPreviewRequest {
            work_id: work.id.clone(),
            remedy: RemedyKind::Handover,
            session: Some("s-other".into()),
            scope: None,
            superseded_by: None,
        },
        now + 4,
    )
    .unwrap();
    assert_eq!(helper.refusal.unwrap().code, RefusalCode::NotLead);
}

/// Ending Work names what it reaches: open requests that target it and Work that depends on it.
fn ending_work_names_the_requests_and_dependents_it_reaches(db: &Db) {
    let store = db.open();
    let (alice, bob) = (agent(&*store, 1), agent(&*store, 2));
    let now = unix_now();
    store.observe_lease_clock(now).unwrap();
    let base = store
        .work_start(&alice.token, &start("base", "s-a", "src/**"), now)
        .unwrap()
        .item;
    let mut dependent = start("dependent", "s-d", "docs/**");
    dependent.depends_on = vec![base.id.clone()];
    let dependent = store
        .work_start(&alice.token, &dependent, now)
        .unwrap()
        .item;
    // Bob's overlapping start queues a coordination request that targets the base Work.
    let overlap = store
        .work_start(&bob.token, &start("bob", "s-b", "src/lib.rs"), now + 1)
        .unwrap();
    let request = overlap.coordination_request.expect("a queued request").id;
    let complete = preview(
        &*store,
        &caller(&alice, "s-a"),
        &RemedyPreviewRequest {
            work_id: base.id.clone(),
            remedy: RemedyKind::Complete,
            session: Some("s-a".into()),
            scope: None,
            superseded_by: None,
        },
        now + 2,
    )
    .unwrap();
    assert!(complete.permitted);
    assert_eq!(complete.effects.state, WorkState::Finished);
    assert_eq!(
        complete.effects.dependents,
        std::slice::from_ref(&dependent.id)
    );
    assert_eq!(complete.effects.requests, [request]);
    assert!(complete.effects.affected_complete);
    // The lead completing its own Work fences nobody.
    assert!(!complete.effects.generation_advances);
}

/// A person who owns the Work, and an administrator, act through the same service with the
/// person matrix; a signed-in stranger cannot.
fn people_use_the_same_service(db: &Db) {
    let store = db.open();
    let alice = agent(&*store, 1);
    let stranger = user(&*store, 7);
    let admin = user(&*store, 8);
    let now = unix_now();
    store.observe_lease_clock(now).unwrap();
    let work = store
        .work_start(&alice.token, &start("a", "s-a", "src/**"), now)
        .unwrap()
        .item;
    let person = |user: &User, is_admin: bool| {
        let page = inspect(
            &*store,
            &RecoveryCaller::Person {
                user,
                repository: REPO,
                repository_id: REPOSITORY_ID,
                is_admin,
            },
            &RecoveryQuery {
                work_id: Some(work.id.clone()),
                ..RecoveryQuery::default()
            },
            now + 1,
        )
        .unwrap();
        permitted(&page, &work.id)
    };
    assert_eq!(
        person(&alice.user, false),
        [
            RemedyKind::Abandon,
            RemedyKind::Complete,
            RemedyKind::Release,
            RemedyKind::Retain,
            RemedyKind::Supersede
        ]
    );
    assert_eq!(
        person(&admin, true),
        [
            RemedyKind::Abandon,
            RemedyKind::Release,
            RemedyKind::Supersede
        ]
    );
    assert!(person(&stranger, false).is_empty());
    fn actor(user: &User, is_admin: bool) -> WorkTransitionActor<'_> {
        WorkTransitionActor::Person {
            user,
            repository: REPO,
            repository_id: REPOSITORY_ID,
            is_admin,
        }
    }
    let batch = WorkTransitionBatch {
        session: None,
        items: vec![item(
            &work.id,
            "person-release",
            work.generation,
            work.updated_seq,
            WorkTransitionKind::Release,
        )],
    };
    let refused = apply_batch(&*store, &actor(&stranger, false), &batch, now + 2).unwrap();
    assert_eq!(refused.items[0].code.as_deref(), Some("unauthorized"));
    let applied = apply_batch(&*store, &actor(&alice.user, false), &batch, now + 3).unwrap();
    assert_eq!(applied.items[0].outcome, BatchOutcome::Applied);
}

fn join(store: &dyn Store, who: &Agent, work_id: &str, session: &str, role: WorkRole, now: i64) {
    let mut join = start("join", session, "src/none/**");
    join.work_id = Some(work_id.into());
    join.request_key = None;
    join.role = role;
    join.scope = vec![];
    join.branch = None;
    join.next_action = None;
    store.work_start(&who.token, &join, now).unwrap();
}

fn abandon_with(
    store: &dyn Store,
    owner: &Agent,
    work: &WorkItem,
    operation: &str,
    seq: i64,
    now: i64,
) -> BatchItemV1 {
    let batch = WorkTransitionBatch {
        session: Some("s-a".into()),
        items: vec![item(
            &work.id,
            operation,
            work.generation,
            seq,
            WorkTransitionKind::Abandon,
        )],
    };
    let actor = WorkTransitionActor::Agent {
        token: &owner.token,
    };
    apply_batch(store, &actor, &batch, now)
        .unwrap()
        .items
        .remove(0)
}

/// A helper from another owner that joins and re-joins moves `updated_seq` but cannot starve the
/// owner's cleanup: joins alone do not expire the receipt.
fn a_helper_moving_updated_seq_cannot_starve_owner_cleanup(db: &Db) {
    let store = db.open();
    let alice = agent(&*store, 1);
    let bob = agent(&*store, 2);
    let now = unix_now();
    store.observe_lease_clock(now).unwrap();
    let work = store
        .work_start(&alice.token, &start("a", "s-a", "src/a/**"), now)
        .unwrap()
        .item;
    join(&*store, &bob, &work.id, "s-b", WorkRole::Helper, now + 1);
    join(&*store, &bob, &work.id, "s-b", WorkRole::Reviewer, now + 2);
    join(&*store, &bob, &work.id, "s-b", WorkRole::Helper, now + 3);
    let moved = one(&*store, &caller(&alice, "s-a"), &work.id, now + 4);
    assert!(moved.work[0].operational.updated_seq > work.updated_seq);
    let result = abandon_with(
        &*store,
        &alice,
        &work,
        "abandon-1",
        work.updated_seq,
        now + 5,
    );
    assert_eq!(result.outcome, BatchOutcome::Applied);
}

/// A real change after the observed seq still refuses, even with joins around it.
fn a_non_join_change_after_the_observed_seq_still_refuses(db: &Db) {
    let store = db.open();
    let alice = agent(&*store, 1);
    let bob = agent(&*store, 2);
    let now = unix_now();
    store.observe_lease_clock(now).unwrap();
    let work = store
        .work_start(&alice.token, &start("a", "s-a", "src/a/**"), now)
        .unwrap()
        .item;
    join(&*store, &bob, &work.id, "s-b", WorkRole::Helper, now + 1);
    let current = one(&*store, &caller(&alice, "s-a"), &work.id, now + 2);
    store
        .work_checkpoint(
            &alice.token,
            &WorkCheckpoint {
                work_id: work.id.clone(),
                operation_id: "progress".into(),
                generation: work.generation,
                session: "s-a".into(),
                expected_updated_seq: Some(current.work[0].operational.updated_seq),
                kind: CheckpointKind::Progress,
                summary: "moved on".into(),
                next_action: None,
                scope: None,
                branch: None,
                worktree: None,
                resolve_blocker: false,
                source_refs: vec![],
                depends_on: vec![],
                related_work: vec![],
            },
            now + 3,
        )
        .unwrap();
    join(&*store, &bob, &work.id, "s-b", WorkRole::Reviewer, now + 4);
    let result = abandon_with(
        &*store,
        &alice,
        &work,
        "abandon-1",
        work.updated_seq,
        now + 5,
    );
    assert_eq!(result.outcome, BatchOutcome::Refused);
    assert_eq!(result.code.as_deref(), Some("work_changed"));
}

/// An `expected_updated_seq` that is not a recorded seq of this Work cannot skip over changes.
fn an_unrecorded_expected_seq_is_not_accepted_over_joins(db: &Db) {
    let store = db.open();
    let alice = agent(&*store, 1);
    let bob = agent(&*store, 2);
    let now = unix_now();
    store.observe_lease_clock(now).unwrap();
    let work = store
        .work_start(&alice.token, &start("a", "s-a", "src/a/**"), now)
        .unwrap()
        .item;
    let other = store
        .work_start(&alice.token, &start("b", "s-b", "src/b/**"), now)
        .unwrap()
        .item;
    join(&*store, &bob, &work.id, "s-c", WorkRole::Helper, now + 1);
    for (n, seq) in [(1, 1), (2, other.updated_seq), (3, work.updated_seq + 1000)] {
        let op = format!("abandon-{n}");
        let result = abandon_with(&*store, &alice, &work, &op, seq, now + 2);
        assert_eq!(result.outcome, BatchOutcome::Refused, "seq {seq}");
        assert_eq!(result.code.as_deref(), Some("work_changed"), "seq {seq}");
    }
    let result = abandon_with(
        &*store,
        &alice,
        &work,
        "abandon-ok",
        work.updated_seq,
        now + 3,
    );
    assert_eq!(result.outcome, BatchOutcome::Applied);
}

fn current(store: &dyn Store, who: &Agent, id: &str, now: i64) -> WorkItem {
    store.work_get(&who.token, id, now).unwrap().unwrap()
}

fn prune_request(
    work: &WorkItem,
    operation: &str,
    session: Option<&str>,
    targets: &[(&str, &str)],
) -> WorkTransitionRequest {
    WorkTransitionRequest {
        work_id: work.id.clone(),
        operation_id: operation.into(),
        generation: work.generation,
        expected_updated_seq: work.updated_seq,
        session: session.map(str::to_owned),
        transition: WorkTransitionKind::PruneParticipants,
        reason: "synthetic participant repair".into(),
        superseded_by: None,
        participants: targets
            .iter()
            .map(|(token, session)| ParticipantRef {
                token: (*token).into(),
                session: (*session).into(),
            })
            .collect(),
    }
}

fn as_agent(who: &Agent) -> WorkTransitionActor<'_> {
    WorkTransitionActor::Agent { token: &who.token }
}

fn join_many(store: &dyn Store, who: &Agent, work: &str, count: usize, now: i64) {
    for n in 0..count {
        join(
            store,
            who,
            work,
            &format!("h-{n}"),
            WorkRole::Helper,
            now + n as i64,
        );
    }
}

fn finish(store: &dyn Store, who: &Agent, work: &WorkItem, session: &str, now: i64) {
    store
        .work_checkpoint(
            &who.token,
            &WorkCheckpoint {
                work_id: work.id.clone(),
                operation_id: "finish".into(),
                generation: work.generation,
                session: session.into(),
                expected_updated_seq: Some(work.updated_seq),
                kind: CheckpointKind::Finish,
                summary: "done".into(),
                next_action: None,
                scope: None,
                branch: None,
                worktree: None,
                resolve_blocker: false,
                source_refs: vec![],
                depends_on: vec![],
                related_work: vec![],
            },
            now,
        )
        .unwrap();
}

fn takeover(
    store: &dyn Store,
    who: &Agent,
    work: &WorkItem,
    session: &str,
    now: i64,
) -> Result<agentscm::hub::work::WorkStartResult, HubError> {
    store.work_start(
        &who.token,
        &WorkStart {
            work_id: Some(work.id.clone()),
            request_key: None,
            takeover_generation: Some(work.generation),
            ..start("a", session, "src/a/**")
        },
        now,
    )
}

fn assert_invalid(result: Result<impl std::fmt::Debug, HubError>, text: &str) {
    match result {
        Err(HubError::Invalid(message)) => assert!(message.contains(text), "{message}"),
        other => panic!("expected a refusal containing {text:?}, got {other:?}"),
    }
}

/// A Work at the 100 participant cap refuses a resume and a reopen from a new session; its owner
/// prunes helpers with the current fences and both then succeed, without moving the lease.
fn a_full_participant_list_is_repairable_and_resumable(db: &Db) {
    let store = db.open();
    let alice = agent(&*store, 1);
    let now = unix_now();
    store.observe_lease_clock(now).unwrap();
    let work = store
        .work_start(&alice.token, &start("a", "s-lead", "src/a/**"), now)
        .unwrap()
        .item;
    join_many(&*store, &alice, &work.id, 99, now + 1);
    let later = now + LEASE_SECONDS + 1;
    tick(&*store, now, later);
    let full = current(&*store, &alice, &work.id, later);
    assert_eq!(full.participants.len(), 100);
    assert_invalid(
        takeover(&*store, &alice, &full, "s-new", later + 1),
        "maximum of 100 participants",
    );
    let before = db.scalar(&format!(
        "SELECT COALESCE(reservation_basis, '') || ':' || reservation_expires_at FROM hub_work_items WHERE id = '{}'",
        work.id
    ));
    let pruned = store
        .work_transition(
            &as_agent(&alice),
            &prune_request(
                &full,
                "prune-1",
                Some("s-new"),
                &[(&alice.token, "h-0"), (&alice.token, "h-1")],
            ),
            later + 2,
        )
        .unwrap();
    assert!(!pruned.receipt.unwrap().replayed);
    assert_eq!(pruned.item.participants.len(), 98);
    assert_eq!(pruned.item.generation, full.generation);
    assert_eq!(pruned.item.state, full.state);
    assert_eq!(pruned.item.lead_session, "s-lead");
    assert_eq!(
        db.scalar(&format!(
            "SELECT COUNT(*) FROM hub_work_participants WHERE work = '{}'",
            work.id
        )),
        "98"
    );
    let after = db.scalar(&format!(
        "SELECT COALESCE(reservation_basis, '') || ':' || reservation_expires_at FROM hub_work_items WHERE id = '{}'",
        work.id
    ));
    assert_eq!(before, after, "a prune leaves the reservation lease alone");
    let resumed = takeover(&*store, &alice, &pruned.item, "s-new", later + 3).unwrap();
    assert!(resumed.took_over);

    // Finished Work at the cap: reopen is refused, prune is allowed, reopen then succeeds.
    let done = store
        .work_start(&alice.token, &start("b", "s-b-lead", "src/b/**"), now)
        .unwrap()
        .item;
    join_many(&*store, &alice, &done.id, 99, later + 10);
    finish(
        &*store,
        &alice,
        &current(&*store, &alice, &done.id, later + 200),
        "s-b-lead",
        later + 201,
    );
    let finished = current(&*store, &alice, &done.id, later + 202);
    assert_eq!(finished.state, WorkState::Finished);
    let reopen = |work: &WorkItem, operation: &str| WorkTransitionRequest {
        transition: WorkTransitionKind::Reopen,
        ..prune_request(work, operation, Some("s-re"), &[])
    };
    assert_invalid(
        store.work_transition(&as_agent(&alice), &reopen(&finished, "re-1"), later + 203),
        "maximum of 100 participants",
    );
    let pruned = store
        .work_transition(
            &as_agent(&alice),
            &prune_request(&finished, "prune-2", Some("s-re"), &[(&alice.token, "h-5")]),
            later + 204,
        )
        .unwrap();
    assert_eq!(pruned.item.state, WorkState::Finished);
    assert_eq!(
        db.scalar(&format!(
            "SELECT COALESCE(terminal_outcome, '') FROM hub_work_items WHERE id = '{}'",
            done.id
        )),
        "completed"
    );
    let reopened = store
        .work_transition(
            &as_agent(&alice),
            &reopen(&pruned.item, "re-2"),
            later + 205,
        )
        .unwrap();
    assert_eq!(reopened.item.state, WorkState::Active);
    assert_eq!(reopened.item.lead_session, "s-re");
}

/// Work joined before the cap on other-owner participants can hold more than 25 of them, which
/// refuses another owner's takeover. The owner prunes enough of them and the takeover succeeds.
fn legacy_over_share_work_repairs(db: &Db) {
    let store = db.open();
    let (alice, bob, carol) = (agent(&*store, 1), agent(&*store, 2), agent(&*store, 3));
    let now = unix_now();
    store.observe_lease_clock(now).unwrap();
    let work = store
        .work_start(&alice.token, &start("a", "s-lead", "src/a/**"), now)
        .unwrap()
        .item;
    // A snapshot from before the cap: 30 sessions of one other owner.
    let snapshot = db.scalar(&format!(
        "SELECT snapshot FROM hub_work_items WHERE id = '{}'",
        work.id
    ));
    let mut value: serde_json::Value = serde_json::from_str(&snapshot).unwrap();
    for n in 0..30 {
        value["participants"]
            .as_array_mut()
            .unwrap()
            .push(serde_json::json!({
                "owner": bob.user.id, "login": bob.user.login, "token": bob.token,
                "agent": "agent2", "session": format!("legacy-{n}"), "role": "helper"
            }));
        db.sql(&format!(
            "INSERT INTO hub_work_participants
                (work, token, owner, agent, session, role, generation, created_at, updated_at)
             VALUES ('{}', '{}', '{}', 'agent2', 'legacy-{n}', 'helper', {}, {now}, {now})",
            work.id, bob.token, bob.user.id, work.generation
        ));
    }
    db.sql(&format!(
        "UPDATE hub_work_items SET snapshot = '{}' WHERE id = '{}'",
        value.to_string().replace('\'', "''"),
        work.id
    ));
    let later = now + LEASE_SECONDS + 1;
    tick(&*store, now, later);
    let over = current(&*store, &alice, &work.id, later);
    assert_eq!(over.participants.len(), 31);
    assert_invalid(
        takeover(&*store, &carol, &over, "s-carol", later + 1),
        "participants from other owners",
    );
    // Five removals leave 25 other-owner participants, which still fills the cap.
    let first: Vec<_> = (0..5).map(|n| format!("legacy-{n}")).collect();
    let targets: Vec<_> = first
        .iter()
        .map(|s| (bob.token.as_str(), s.as_str()))
        .collect();
    let pruned = store
        .work_transition(
            &as_agent(&alice),
            &prune_request(&over, "prune-1", Some("s-new"), &targets),
            later + 2,
        )
        .unwrap();
    assert_invalid(
        takeover(&*store, &carol, &pruned.item, "s-carol", later + 3),
        "participants from other owners",
    );
    let more: Vec<_> = (5..8).map(|n| format!("legacy-{n}")).collect();
    let targets: Vec<_> = more
        .iter()
        .map(|s| (bob.token.as_str(), s.as_str()))
        .collect();
    let pruned = store
        .work_transition(
            &as_agent(&alice),
            &prune_request(&pruned.item, "prune-2", Some("s-new"), &targets),
            later + 4,
        )
        .unwrap();
    let resumed = takeover(&*store, &carol, &pruned.item, "s-carol", later + 5).unwrap();
    assert!(resumed.took_over);
}

/// Who may prune, what is refused whole, retries, the audit record and the pruned session's claim.
fn only_the_owner_or_an_admin_prunes_and_nothing_is_partly_pruned(db: &Db) {
    let store = db.open();
    let (alice, bob) = (agent(&*store, 1), agent(&*store, 2));
    let admin = user(&*store, 8);
    let now = unix_now();
    store.observe_lease_clock(now).unwrap();
    let work = store
        .work_start(&alice.token, &start("a", "s-lead", "src/a/**"), now)
        .unwrap()
        .item;
    join(&*store, &alice, &work.id, "s-h1", WorkRole::Helper, now + 1);
    join(&*store, &alice, &work.id, "s-h2", WorkRole::Helper, now + 2);
    join(&*store, &bob, &work.id, "s-b", WorkRole::Helper, now + 3);
    store
        .touch_session(
            &alice.token,
            &agentscm::hub::session::SessionCall {
                session: Some("s-h1".into()),
                via: "mcp".into(),
                task: Some("edit".into()),
                paths: vec!["src/a/x.rs".into()],
                deny: false,
                report: false,
            },
            now + 4,
        )
        .unwrap();
    let intents = "SELECT COUNT(*) FROM hub_intents WHERE released_at IS NULL";
    let live = db.scalar(intents);
    let item = current(&*store, &alice, &work.id, now + 5);
    let seq = store.latest_event_seq().unwrap();
    let a = alice.token.as_str();
    let apply = |actor: &WorkTransitionActor<'_>, request: &WorkTransitionRequest| {
        store.work_transition(actor, request, now + 6)
    };
    // Another owner, the lead's own session, and a Hub-level admin without standing as an agent.
    assert!(matches!(
        apply(
            &as_agent(&bob),
            &prune_request(&item, "o1", Some("s-b"), &[(a, "s-h1")])
        ),
        Err(HubError::Unauthorized)
    ));
    assert_invalid(
        apply(
            &as_agent(&alice),
            &prune_request(&item, "o2", Some("s-lead"), &[(a, "s-h1")]),
        ),
        "may not request",
    );
    // The lead, an absent participant, and a listed pair whose other half is valid: no removal.
    for (op, targets) in [
        ("o3", vec![(a, "s-lead")]),
        ("o4", vec![(a, "s-gone")]),
        ("o5", vec![(a, "s-h1"), (a, "s-lead")]),
        ("o6", vec![(a, "s-h1"), (a, "s-gone")]),
        ("o7", vec![(bob.token.as_str(), "s-h1")]),
    ] {
        assert_invalid(
            apply(
                &as_agent(&alice),
                &prune_request(&item, op, Some("s-new"), &targets),
            ),
            "current non-lead participant",
        );
    }
    // Shape errors are refused before anything is read.
    assert_invalid(
        apply(
            &as_agent(&alice),
            &prune_request(&item, "o8", Some("s-new"), &[]),
        ),
        "requires participants",
    );
    assert_invalid(
        apply(
            &as_agent(&alice),
            &prune_request(&item, "o9", Some("s-new"), &[(a, "s-h1"), (a, "s-h1")]),
        ),
        "must not repeat",
    );
    let mut stray = prune_request(&item, "o10", Some("s-lead"), &[(a, "s-h1")]);
    stray.transition = WorkTransitionKind::Release;
    assert_invalid(
        apply(&as_agent(&alice), &stray),
        "only valid for prune_participants",
    );
    assert_eq!(
        current(&*store, &alice, &work.id, now + 7)
            .participants
            .len(),
        4
    );
    assert_eq!(store.latest_event_seq().unwrap(), seq);

    let request = prune_request(&item, "prune", Some("s-new"), &[(a, "s-h1")]);
    let applied = apply(&as_agent(&alice), &request).unwrap();
    assert!(!applied.receipt.as_ref().unwrap().replayed);
    assert_eq!(applied.item.participants.len(), 3);
    // The pruned session's file intent on this Work is released with it.
    assert_eq!(live, "1");
    assert_eq!(db.scalar(intents), "0");
    // An exact retry replays; a different list under the same operation id is refused.
    let replay = apply(&as_agent(&alice), &request).unwrap();
    assert!(replay.receipt.unwrap().replayed);
    assert_eq!(replay.item.participants.len(), 3);
    assert_invalid(
        apply(
            &as_agent(&alice),
            &prune_request(&item, "prune", Some("s-new"), &[(a, "s-h2")]),
        ),
        "operation_id was already used",
    );
    // A person administrator prunes another owner's helper with no session.
    let item = current(&*store, &alice, &work.id, now + 8);
    let by_admin = WorkTransitionActor::Person {
        user: &admin,
        repository: REPO,
        repository_id: REPOSITORY_ID,
        is_admin: true,
    };
    let done = apply(
        &by_admin,
        &prune_request(&item, "admin", None, &[(bob.token.as_str(), "s-b")]),
    )
    .unwrap();
    assert_eq!(done.item.participants.len(), 2);

    let events: Vec<_> = store
        .events_after(0, 1000)
        .unwrap()
        .into_iter()
        .filter(|event| event.kind == "work_transition")
        .collect();
    assert_eq!(events.len(), 2);
    for event in &events {
        assert!(!event.data.contains(&alice.token), "{}", event.data);
        assert!(!event.data.contains(&bob.token), "{}", event.data);
    }
    let first: serde_json::Value = serde_json::from_str(&events[0].data).unwrap();
    assert_eq!(first["transition"], "prune_participants");
    assert_eq!(first["removed_participants"][0]["session"], "s-h1");
    assert_eq!(first["removed_participants"][0]["agent"], "agent1");
    assert_eq!(first["removed_participants"][0]["owner"], alice.user.id);
    assert_eq!(first["actor"]["kind"], "owner");
    let second: serde_json::Value = serde_json::from_str(&events[1].data).unwrap();
    assert_eq!(second["actor"]["kind"], "admin");
    assert_eq!(second["removed_participants"][0]["owner"], bob.user.id);
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

fn person<'a>(user: &'a User, is_admin: bool) -> RecoveryCaller<'a> {
    RecoveryCaller::Person {
        user,
        repository: REPO,
        repository_id: REPOSITORY_ID,
        is_admin,
    }
}

fn all_owners() -> RecoveryQuery {
    RecoveryQuery {
        all_owners: true,
        ..RecoveryQuery::default()
    }
}

fn finding_kinds(recovery: &WorkRecoveryV1, id: &str) -> Vec<FindingKind> {
    recovery
        .work
        .iter()
        .find(|work| work.work_id == id)
        .unwrap()
        .findings
        .iter()
        .map(|finding| finding.kind)
        .collect()
}

/// A Hub administrator pages every owner's Work through the person route's service; nobody else
/// may, and an owner or an agent still reads only their own.
fn an_admin_inventories_every_owner_and_owners_see_their_own(db: &Db) {
    let store = db.open();
    let (alice, bob) = (agent(&*store, 1), agent(&*store, 2));
    let (stranger, admin) = (user(&*store, 7), user(&*store, 8));
    let now = unix_now();
    store.observe_lease_clock(now).unwrap();
    let a = store
        .work_start(&alice.token, &start("a", "s-a", "src/a/**"), now)
        .unwrap()
        .item;
    let b = store
        .work_start(&bob.token, &start("b", "s-b", "src/b/**"), now)
        .unwrap()
        .item;

    let seen = inspect(&*store, &person(&admin, true), &all_owners(), now + 1).unwrap();
    let ids: Vec<_> = seen.work.iter().map(|work| work.work_id.as_str()).collect();
    assert_eq!(ids.len(), 2, "{ids:?}");
    assert!(ids.contains(&a.id.as_str()) && ids.contains(&b.id.as_str()));
    assert!(
        seen.work
            .iter()
            .all(|work| work.standing == Standing::Admin)
    );
    // Admin standing keeps the transitions an administrator already had.
    assert_eq!(
        permitted(&seen, &a.id),
        [
            RemedyKind::Abandon,
            RemedyKind::Release,
            RemedyKind::Supersede
        ]
    );
    // Without all_owners an administrator reads their own Work, which is none.
    let own = read(
        &*store,
        &person(&admin, true),
        RecoveryQuery::default(),
        now + 1,
    );
    assert!(own.work.is_empty());

    for refused in [
        inspect(&*store, &person(&stranger, false), &all_owners(), now + 1),
        inspect(&*store, &person(&alice.user, false), &all_owners(), now + 1),
        inspect(&*store, &caller(&alice, "s-a"), &all_owners(), now + 1),
        inspect(
            &*store,
            &person(&admin, true),
            &RecoveryQuery {
                work_id: Some(a.id.clone()),
                all_owners: true,
                ..RecoveryQuery::default()
            },
            now + 1,
        ),
    ] {
        match refused {
            Err(HubError::Invalid(message)) => assert!(
                message.contains("Hub administrator") || message.contains("work_id"),
                "{message}"
            ),
            other => panic!("expected a refusal, got {other:?}"),
        }
    }

    let mine = read(
        &*store,
        &caller(&alice, "s-a"),
        RecoveryQuery::default(),
        now + 1,
    );
    assert_eq!(mine.work.len(), 1);
    assert_eq!(mine.work[0].work_id, a.id);
    assert_eq!(mine.work[0].standing, Standing::Lead);
    let theirs = read(
        &*store,
        &person(&alice.user, false),
        RecoveryQuery::default(),
        now + 1,
    );
    assert_eq!(theirs.work.len(), 1);
}

/// A Work left alone for a long time gets findings, but none proposes `complete`, and the
/// remedies it lists are the ones it had without them.
fn inventory_never_proposes_complete_from_age(db: &Db) {
    let store = db.open();
    let (alice, bob) = (agent(&*store, 1), agent(&*store, 2));
    let admin = user(&*store, 8);
    let now = unix_now();
    store.observe_lease_clock(now).unwrap();
    let work = store
        .work_start(&alice.token, &start("a", "s-a", "src/a/**"), now)
        .unwrap()
        .item;
    let later = now + 2 * 86_400;
    tick(&*store, now, now + LEASE_SECONDS + 1);
    store.observe_lease_clock(later).unwrap();

    let mine = read(
        &*store,
        &caller(&alice, "s-new"),
        RecoveryQuery::default(),
        later,
    );
    let kinds = finding_kinds(&mine, &work.id);
    assert!(kinds.contains(&FindingKind::LapsedHold), "{kinds:?}");
    assert!(kinds.contains(&FindingKind::AbandonedSession), "{kinds:?}");
    let entry = &mine.work[0];
    let lapsed = entry
        .findings
        .iter()
        .find(|finding| finding.kind == FindingKind::LapsedHold)
        .unwrap();
    assert_eq!(lapsed.proposed, Some(RemedyKind::Release));
    assert!(
        lapsed.evidence.contains("lapsed at Unix"),
        "{}",
        lapsed.evidence
    );
    let abandoned = entry
        .findings
        .iter()
        .find(|finding| finding.kind == FindingKind::AbandonedSession)
        .unwrap();
    assert_eq!(abandoned.proposed, Some(RemedyKind::Resume));
    // Other standing is told, never proposed, a takeover of a stale claim.
    let theirs = read(
        &*store,
        &caller(&bob, "s-b"),
        RecoveryQuery {
            work_id: Some(work.id.clone()),
            ..RecoveryQuery::default()
        },
        later,
    );
    assert!(
        theirs.work[0]
            .findings
            .iter()
            .all(|finding| finding.proposed.is_none())
    );
    let everyone = inspect(&*store, &person(&admin, true), &all_owners(), later).unwrap();
    for recovery in [&mine, &theirs, &everyone] {
        for work in &recovery.work {
            assert!(
                work.findings
                    .iter()
                    .all(|finding| finding.proposed != Some(RemedyKind::Complete)),
                "{:?}",
                work.findings
            );
        }
    }
    // The remedies are the same with or without the inventory's findings: complete stays refused
    // to the admin and is never added.
    assert_eq!(
        permitted(&everyone, &work.id),
        [
            RemedyKind::Abandon,
            RemedyKind::Release,
            RemedyKind::Supersede
        ]
    );
}

/// Reading an inventory with findings writes nothing.
fn read_only_inventory_changes_nothing(db: &Db) {
    let store = db.open();
    let alice = agent(&*store, 1);
    let admin = user(&*store, 8);
    let now = unix_now();
    store.observe_lease_clock(now).unwrap();
    let work = store
        .work_start(&alice.token, &start("a", "s-a", "src/a/**"), now)
        .unwrap()
        .item;
    let later = now + LEASE_SECONDS + 1;
    tick(&*store, now, later);
    let state = || {
        (
            db.scalar("SELECT COUNT(*) FROM hub_events"),
            db.scalar(&format!(
                "SELECT updated_seq FROM hub_work_items WHERE id = '{}'",
                work.id
            )),
            db.scalar("SELECT COUNT(*) FROM hub_work_participants"),
        )
    };
    let before = state();
    let page = inspect(&*store, &person(&admin, true), &all_owners(), later).unwrap();
    assert!(!page.work[0].findings.is_empty());
    let mine = read(
        &*store,
        &caller(&alice, "s-new"),
        RecoveryQuery::default(),
        later,
    );
    assert!(!mine.work[0].findings.is_empty());
    assert_eq!(state(), before);
    assert_eq!(
        current(&*store, &alice, &work.id, later).updated_seq,
        work.updated_seq
    );
}

/// A full participant list is reported, the prune the inventory proposes previews and applies
/// as returned, and a new session can then take the Work over.
fn the_inventory_proposes_a_prune_that_frees_a_full_list(db: &Db) {
    let store = db.open();
    let alice = agent(&*store, 1);
    let now = unix_now();
    store.observe_lease_clock(now).unwrap();
    let work = store
        .work_start(&alice.token, &start("a", "s-lead", "src/a/**"), now)
        .unwrap()
        .item;
    join_many(&*store, &alice, &work.id, 99, now + 1);
    let later = now + LEASE_SECONDS + 1;
    tick(&*store, now, later);
    let me = caller(&alice, "s-new");
    let page = one(&*store, &me, &work.id, later);
    let cap = page.work[0]
        .findings
        .iter()
        .find(|finding| finding.kind == FindingKind::ParticipantCap)
        .expect("a full list is reported");
    assert_eq!(cap.proposed, Some(RemedyKind::PruneParticipants));
    assert!(cap.complete);
    assert!(
        cap.evidence.contains("100 participants"),
        "{}",
        cap.evidence
    );
    assert!(permitted(&page, &work.id).contains(&RemedyKind::PruneParticipants));

    let previewed = preview(
        &*store,
        &me,
        &RemedyPreviewRequest {
            work_id: work.id.clone(),
            remedy: RemedyKind::PruneParticipants,
            session: Some("s-new".into()),
            scope: None,
            superseded_by: None,
        },
        later + 1,
    )
    .unwrap();
    assert!(previewed.permitted, "{:?}", previewed.refusal);
    assert_eq!(previewed.effects.fenced.len(), 1);
    assert_eq!(
        previewed.effects.reservation,
        previewed.operational.reservation.status
    );
    let json = serde_json::to_string(&previewed.effects).unwrap();
    assert!(
        !json.contains(&alice.token) && !json.contains("h-0"),
        "{json}"
    );
    let apply = previewed.apply.clone().unwrap();
    assert_eq!(apply.tool.as_deref(), Some("agentscm_work_transition"));
    assert_eq!(apply.arguments["transition"], "prune_participants");
    assert_eq!(apply.arguments["participants"].as_array().unwrap().len(), 1);
    // The call carries a digest, never a native session id.
    assert!(
        !apply.arguments.to_string().contains("h-"),
        "{}",
        apply.arguments
    );

    let mut request: WorkTransitionRequest = serde_json::from_value(serde_json::json!({
        "work_id": apply.arguments["work_id"],
        "operation_id": "prune-from-preview",
        "generation": apply.arguments["generation"],
        "expected_updated_seq": apply.arguments["expected_updated_seq"],
        "session": apply.arguments["session"],
        "transition": apply.arguments["transition"],
        "reason": "synthetic participant repair",
        "participants": apply.arguments["participants"],
    }))
    .unwrap();
    let pruned = store
        .work_transition(&as_agent(&alice), &request, later + 2)
        .unwrap();
    assert_eq!(pruned.item.participants.len(), 99);
    // The same call again replays.
    request.operation_id = "prune-from-preview".into();
    assert!(
        store
            .work_transition(&as_agent(&alice), &request, later + 3)
            .unwrap()
            .receipt
            .unwrap()
            .replayed
    );
    // With room left, the finding and the prune are gone.
    let after = one(&*store, &me, &work.id, later + 4);
    assert!(!finding_kinds(&after, &work.id).contains(&FindingKind::ParticipantCap));
    assert!(!permitted(&after, &work.id).contains(&RemedyKind::PruneParticipants));
    let resumed = takeover(&*store, &alice, &pruned.item, "s-new", later + 5).unwrap();
    assert!(resumed.took_over);
}

/// Another owner's agent can read a Work but is offered no transition and no prune on it.
fn other_owners_items_are_refused_not_taken(db: &Db) {
    let store = db.open();
    let (alice, bob) = (agent(&*store, 1), agent(&*store, 2));
    let now = unix_now();
    store.observe_lease_clock(now).unwrap();
    let work = store
        .work_start(&alice.token, &start("a", "s-lead", "src/a/**"), now)
        .unwrap()
        .item;
    join_many(&*store, &alice, &work.id, 99, now + 1);
    let later = now + LEASE_SECONDS + 1;
    tick(&*store, now, later);
    let theirs = read(
        &*store,
        &caller(&bob, "s-b"),
        RecoveryQuery {
            work_id: Some(work.id.clone()),
            ..RecoveryQuery::default()
        },
        later,
    );
    let entry = &theirs.work[0];
    assert_eq!(entry.standing, Standing::Other);
    assert!(
        entry
            .remedies
            .iter()
            .all(|remedy| !remedy.permitted || remedy.remedy == RemedyKind::Resume)
    );
    let prune = entry
        .remedies
        .iter()
        .find(|remedy| remedy.remedy == RemedyKind::PruneParticipants)
        .unwrap();
    assert!(!prune.permitted);
    assert_eq!(
        prune.refusal.as_ref().unwrap().code,
        RefusalCode::NotAuthorized
    );
    assert!(prune.apply.is_none());
    assert!(
        entry
            .findings
            .iter()
            .all(|finding| finding.proposed.is_none())
    );
}

conformance!(
    an_owner_retires_an_obsolete_hold_and_keeps_its_follow_up,
    the_owner_resumes_expired_work_after_a_session_restart,
    another_owners_live_conflict_is_identified_not_taken,
    read_only_callers_cannot_escalate,
    a_partly_applied_batch_explains_each_item_and_retries_safely,
    the_owners_unfinished_work_pages_in_order,
    restart_grace_is_never_reported_as_a_renewal,
    lapsed_or_paused_work_is_not_reported_held,
    a_scoped_handover_frees_only_what_it_drops,
    ending_work_names_the_requests_and_dependents_it_reaches,
    people_use_the_same_service,
    a_helper_moving_updated_seq_cannot_starve_owner_cleanup,
    a_non_join_change_after_the_observed_seq_still_refuses,
    an_unrecorded_expected_seq_is_not_accepted_over_joins,
    a_full_participant_list_is_repairable_and_resumable,
    legacy_over_share_work_repairs,
    only_the_owner_or_an_admin_prunes_and_nothing_is_partly_pruned,
    an_admin_inventories_every_owner_and_owners_see_their_own,
    inventory_never_proposes_complete_from_age,
    read_only_inventory_changes_nothing,
    the_inventory_proposes_a_prune_that_frees_a_full_list,
    other_owners_items_are_refused_not_taken,
);
