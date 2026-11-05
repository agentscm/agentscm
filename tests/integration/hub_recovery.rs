//! Recovery inspection, remedy previews and transition batches through the shared service, on a
//! deterministic clock. Every case runs on SQLite and, when configured, PostgreSQL.
use crate::hub_pg;

use std::sync::Arc;

use agentscm::hub::store::{self, ForgeIdentity, Store, User, mint_bound_agent_token, unix_now};
use agentscm::hub::work::{CheckpointKind, WorkCheckpoint, WorkRole, WorkStart, WorkState};
use agentscm::hub::work_lifecycle::{
    ActivityStatus, LEASE_SECONDS, Recipient, ReservationStatus, WorkTransitionKind,
};
use agentscm::hub::work_recovery::{
    BatchOutcome, BatchTransitionItem, RecoveryBucket, RecoveryCaller, RecoveryQuery, RefusalCode,
    RemedyKind, RemedyPreviewRequest, Standing, WorkRecoveryV1, WorkTransitionBatch, apply_batch,
    inspect, preview,
};
use agentscm::hub::work_transition::{OperationalSelect, WorkLifecycleReader, WorkTransitionActor};
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
);
