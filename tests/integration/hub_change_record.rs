//! Change-record storage conformance. SQLite always; PostgreSQL when configured.
use crate::hub_pg;

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use agentscm::change_record::{
    ActionFinishV1, ActionKindV1, ActionOutcomeV1, ActionStartV1, AgentHarnessV1, ApplicabilityV1,
    ArtifactMediaV1, ArtifactRefV1, AssuranceBasisV1, CaptureBatchV1, CaptureModeV1,
    CapturedEventKindV1, CapturedEventV1, CheckResultV1, ContentAvailabilityV1, CoverageV1,
    DriftV1, MAX_PROJECTED_SESSION_SETUPS, RecipeProvenanceV1, RetentionClassV1, SessionIdentityV1,
    SessionSetupV1, SetupObservationV1, SourceBindingV1, SubagentFactV1, ToolCompletionV1,
    ToolFactV1, ToolKindV1,
};
use agentscm::hub::change_record::SharedBodyState;
use agentscm::hub::store::{self, ForgeIdentity, Store, mint_agent_token, unix_now};
use agentscm::hub::work::{
    CheckpointKind, SourceRef, WorkCheckpoint, WorkItem, WorkRole, WorkStart,
};
use hub_pg::PgSchema;
use tempfile::TempDir;

const REPO: &str = "acme/widgets";
static NEXT_SETUP: AtomicUsize = AtomicUsize::new(1);

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

fn setup(db: &Db) -> (Arc<dyn Store>, String, String, i64) {
    let setup_id = NEXT_SETUP.fetch_add(1, Ordering::Relaxed);
    let store = db.open();
    let user = store
        .upsert_user(
            &ForgeIdentity {
                provider: "github".into(),
                host: "github.com".into(),
                user_id: 42,
            },
            "builder",
        )
        .unwrap();
    let (token, _) = mint_agent_token(&*store, &user.id, "codex", REPO, 3600).unwrap();
    let now = unix_now();
    let work = store
        .work_start(
            &token,
            &WorkStart {
                work_id: None,
                source: None,
                request_key: Some(format!("change-record-test-{setup_id}")),
                title: "capture a source-bound check".into(),
                session: "session-1".into(),
                role: WorkRole::Lead,
                scope: vec!["src/**".into()],
                branch: Some("feature/change-record".into()),
                worktree: Some("worktree-1".into()),
                next_action: Some("inspect the receipt".into()),
                depends_on: vec![],
                related_work: vec![],
                takeover_generation: None,
                preflight: None,
            },
            now,
        )
        .unwrap()
        .item;
    (store, token, work.id, work.generation)
}

fn session() -> SessionIdentityV1 {
    SessionIdentityV1 {
        harness: AgentHarnessV1::Codex,
        adapter_version: "1.0.0".into(),
        session_id: "session-1".into(),
        parent_session_id: None,
        capture_mode: CaptureModeV1::LocalConversation,
    }
}

fn event_batch(work_id: &str, generation: i64, operation: &str, sequence: u64) -> CaptureBatchV1 {
    CaptureBatchV1 {
        schema_version: 1,
        operation_id: operation.into(),
        work_id: work_id.into(),
        generation,
        session: session(),
        events: vec![CapturedEventV1 {
            event_id: format!("event-{sequence}"),
            sequence,
            observed_at: 100 + sequence as i64,
            kind: if sequence == 2 {
                CapturedEventKindV1::SessionEnded
            } else {
                CapturedEventKindV1::SessionStarted
            },
            tool: None,
            artifact: None,
            setup: None,
            gap: None,
            boundary: None,
            subagent: None,
        }],
    }
}

fn event_batch_kind(
    work_id: &str,
    generation: i64,
    operation: &str,
    sequence: u64,
    kind: CapturedEventKindV1,
) -> CaptureBatchV1 {
    let mut batch = event_batch(work_id, generation, operation, sequence);
    batch.events[0].event_id = format!("event-{generation}-{sequence}");
    batch.events[0].kind = kind;
    batch
}

fn source() -> SourceBindingV1 {
    SourceBindingV1::Git {
        repository: REPO.into(),
        worktree_id: "worktree-1".into(),
        head: "1".repeat(40),
        tree: "2".repeat(40),
        base: "3".repeat(40),
        dirty: false,
        untracked_count: 0,
        omissions: vec![],
        captured_snapshot: None,
        snapshot_coverage: CoverageV1::Complete,
    }
}

fn action_start(work_id: &str, generation: i64) -> ActionStartV1 {
    ActionStartV1 {
        schema_version: 1,
        operation_id: "start-1".into(),
        work_id: work_id.into(),
        generation,
        session_id: "session-1".into(),
        action_id: "action-1".into(),
        source: source(),
        kind: ActionKindV1::Verification,
        recipe_provenance: RecipeProvenanceV1::ConfiguredExactMatch,
        recipe_digest: "4".repeat(64),
        setup_digest: "5".repeat(64),
        observed_order: 2,
        observation_scope_id: Some("scope-1".into()),
        started_at: 200,
    }
}

fn action_finish(work_id: &str, generation: i64) -> ActionFinishV1 {
    ActionFinishV1 {
        schema_version: 1,
        operation_id: "finish-1".into(),
        work_id: work_id.into(),
        generation,
        action_id: "action-1".into(),
        outcome: ActionOutcomeV1::Succeeded,
        result: CheckResultV1::Passed,
        assurance: AssuranceBasisV1::CallerAsserted,
        coverage: CoverageV1::Partial,
        finished_at: 220,
        drift: DriftV1::NoneObserved,
        output_artifacts: vec![ArtifactRefV1 {
            digest: "6".repeat(64),
            size_bytes: 20,
            media: ArtifactMediaV1::Text,
            retention: RetentionClassV1::WorkLocal,
            availability: ContentAvailabilityV1::Local,
        }],
    }
}

fn action_capture_receipt(db: &Db) {
    let (store, token, work_id, generation) = setup(db);
    let decision_seq = store
        .work_get(&token, &work_id, 289)
        .unwrap()
        .unwrap()
        .updated_seq;
    store
        .work_checkpoint(
            &token,
            &WorkCheckpoint {
                work_id: work_id.clone(),
                operation_id: "decision-1".into(),
                generation,
                session: "session-1".into(),
                expected_updated_seq: Some(decision_seq),
                kind: CheckpointKind::Decision,
                summary: "Use the retained source-bound receipt for handoff.".into(),
                next_action: None,
                scope: None,
                branch: None,
                worktree: None,
                resolve_blocker: false,
                source_refs: vec![SourceRef::RepositoryPath {
                    path: "src/synthetic.rs".into(),
                    line: Some(1),
                }],
                depends_on: vec![],
                related_work: vec![],
            },
            290,
        )
        .unwrap();
    let late = event_batch(&work_id, generation, "events-2", 2);
    assert_eq!(store.change_ingest(&token, &late, 300).unwrap().accepted, 1);
    let partial = store.change_record(&token, &work_id, 301).unwrap().unwrap();
    assert_eq!(partial.sessions.coverage, CoverageV1::Partial);

    let early = event_batch(&work_id, generation, "events-1", 1);
    assert_eq!(
        store.change_ingest(&token, &early, 302).unwrap().accepted,
        1
    );
    let replay = store.change_ingest(&token, &early, 303).unwrap();
    assert!(replay.replayed);

    let start = action_start(&work_id, generation);
    store.change_action_start(&token, &start, 304).unwrap();
    assert!(
        store
            .change_action_start(&token, &start, 305)
            .unwrap()
            .replayed
    );
    let finish = action_finish(&work_id, generation);
    store.change_action_finish(&token, &finish, 306).unwrap();

    let summary = store.change_record(&token, &work_id, 307).unwrap().unwrap();
    assert_eq!(summary.sessions.coverage, CoverageV1::Complete);
    assert_eq!(summary.completeness.source, CoverageV1::Complete);
    let action = summary.latest_action.as_ref().unwrap();
    assert_eq!(action.result, CheckResultV1::Passed);
    assert_eq!(action.applicability, ApplicabilityV1::Unknown);
    assert_eq!(action.assurance, AssuranceBasisV1::CallerAsserted);
    assert_eq!(action.evidence_availability, ContentAvailabilityV1::Local);
    assert_eq!(
        summary.decision.status,
        agentscm::change_record::DecisionStatusV1::Proposed
    );
    assert_eq!(
        summary.decision.author_kind,
        Some(agentscm::change_record::DecisionAuthorKindV1::Agent)
    );
    assert_eq!(summary.decision.source_refs.len(), 1);
    assert_eq!(summary.intent.title, "capture a source-bound check");
    assert!(summary.subject.is_some());

    let mut elevated = action_finish(&work_id, generation);
    elevated.operation_id = "finish-elevated".into();
    elevated.assurance = AssuranceBasisV1::IndependentVerification;
    assert!(store.change_action_finish(&token, &elevated, 308).is_err());

    let mut later = action_start(&work_id, generation);
    later.operation_id = "start-2".into();
    later.action_id = "action-2".into();
    // Fair replay may deliver a causally older retained start after the newer one. A backward
    // caller clock cannot displace durable order from the same local observation scope.
    later.started_at = 100;
    later.observed_order = 1;
    store.change_action_start(&token, &later, 309).unwrap();
    let reordered = store.change_record(&token, &work_id, 310).unwrap().unwrap();
    assert_eq!(reordered.latest_action.unwrap().action_id, "action-1");

    store
        .work_start(
            &token,
            &WorkStart {
                work_id: Some(work_id.clone()),
                source: None,
                request_key: None,
                title: "join from an independent capture session".into(),
                session: "session-2".into(),
                role: WorkRole::Helper,
                scope: vec![],
                branch: None,
                worktree: None,
                next_action: None,
                depends_on: vec![],
                related_work: vec![],
                takeover_generation: None,
                preflight: None,
            },
            311,
        )
        .unwrap();
    let mut independent_session = action_start(&work_id, generation);
    independent_session.operation_id = "start-3".into();
    independent_session.action_id = "action-3".into();
    independent_session.session_id = "session-2".into();
    independent_session.observed_order = 1;
    // The same private spool can serve two canonical sessions. Its counters are comparable only
    // within one (observation scope, session) pair, never across those sessions.
    independent_session.observation_scope_id = Some("scope-1".into());
    independent_session.started_at = 500;
    store
        .change_action_start(&token, &independent_session, 312)
        .unwrap();
    let cross_session = store.change_record(&token, &work_id, 313).unwrap().unwrap();
    // Receipt order chooses across session representatives, so the later session-2 receipt wins
    // even though session-1 previously retained a larger local counter.
    assert_eq!(cross_session.latest_action.unwrap().action_id, "action-3");

    let mut local_later = action_start(&work_id, generation);
    local_later.operation_id = "start-4".into();
    local_later.action_id = "action-4".into();
    local_later.observed_order = 3;
    local_later.started_at = 50;
    store
        .change_action_start(&token, &local_later, 314)
        .unwrap();
    let mixed_scopes = store.change_record(&token, &work_id, 315).unwrap().unwrap();
    // A later session-1 counter advances only that session's representative; its newer receipt
    // then wins across the two incomparable session representatives.
    assert_eq!(mixed_scopes.latest_action.unwrap().action_id, "action-4");

    let mut no_scope = action_start(&work_id, generation);
    no_scope.operation_id = "start-5".into();
    no_scope.action_id = "action-5".into();
    no_scope.observed_order = 0;
    no_scope.observation_scope_id = None;
    no_scope.started_at = 600;
    store.change_action_start(&token, &no_scope, 316).unwrap();
    let missing_scope = store.change_record(&token, &work_id, 317).unwrap().unwrap();
    assert_eq!(missing_scope.latest_action.unwrap().action_id, "action-5");

    let finish_seq = store
        .work_get(&token, &work_id, 317)
        .unwrap()
        .unwrap()
        .updated_seq;
    store
        .work_checkpoint(
            &token,
            &WorkCheckpoint {
                work_id: work_id.clone(),
                operation_id: "finish-work".into(),
                generation,
                session: "session-1".into(),
                expected_updated_seq: Some(finish_seq),
                kind: CheckpointKind::Finish,
                summary: "Handoff fixture complete.".into(),
                next_action: None,
                scope: None,
                branch: None,
                worktree: None,
                resolve_blocker: false,
                source_refs: vec![],
                depends_on: vec![],
                related_work: vec![],
            },
            311,
        )
        .unwrap();
    assert!(store.change_ingest(&token, &early, 312).unwrap().replayed);
    assert!(
        store
            .change_action_start(&token, &start, 313)
            .unwrap()
            .replayed
    );
    assert!(
        store
            .change_action_finish(&token, &finish, 314)
            .unwrap()
            .replayed
    );
    let mut stale_new = event_batch(&work_id, generation, "events-after-finish", 3);
    stale_new.events[0].kind = CapturedEventKindV1::Gap;
    stale_new.events[0].gap = Some(agentscm::change_record::GapFactV1 {
        first_missing: 3,
        last_missing: 3,
        reason: agentscm::change_record::GapReasonV1::CaptureInterrupted,
    });
    assert!(store.change_ingest(&token, &stale_new, 315).is_err());
}

fn lifecycle_resume_and_replay(db: &Db) {
    let (store, token, work_id, generation) = setup(db);
    let owner = store
        .upsert_user(
            &ForgeIdentity {
                provider: "github".into(),
                host: "github.com".into(),
                user_id: 42,
            },
            "builder",
        )
        .unwrap();
    let (helper, _) = mint_agent_token(&*store, &owner.id, "helper", REPO, 3600).unwrap();
    store
        .work_start(
            &helper,
            &WorkStart {
                work_id: Some(work_id.clone()),
                source: None,
                request_key: None,
                title: "join capture fixture".into(),
                session: "helper-session".into(),
                role: WorkRole::Helper,
                scope: vec![],
                branch: None,
                worktree: None,
                next_action: None,
                depends_on: vec![],
                related_work: vec![],
                takeover_generation: None,
                preflight: None,
            },
            399,
        )
        .unwrap();
    assert_eq!(
        store
            .capture_work_for_session(&helper, "helper-session", 399)
            .unwrap()
            .unwrap()
            .id,
        work_id
    );
    let end = event_batch_kind(
        &work_id,
        generation,
        "lifecycle-end",
        3,
        CapturedEventKindV1::SessionEnded,
    );
    let middle = event_batch_kind(
        &work_id,
        generation,
        "lifecycle-middle",
        2,
        CapturedEventKindV1::AssistantMessage,
    );
    let start = event_batch_kind(
        &work_id,
        generation,
        "lifecycle-start",
        1,
        CapturedEventKindV1::SessionStarted,
    );
    store.change_ingest(&token, &end, 400).unwrap();
    store.change_ingest(&token, &middle, 401).unwrap();
    assert_eq!(
        store
            .change_record(&token, &work_id, 402)
            .unwrap()
            .unwrap()
            .sessions
            .coverage,
        CoverageV1::Partial
    );
    store.change_ingest(&token, &start, 403).unwrap();
    assert_eq!(
        store
            .change_record(&token, &work_id, 404)
            .unwrap()
            .unwrap()
            .sessions
            .coverage,
        CoverageV1::Complete
    );
    let action = action_start(&work_id, generation);
    let finish = action_finish(&work_id, generation);
    store.change_action_start(&token, &action, 405).unwrap();
    store.change_action_finish(&token, &finish, 406).unwrap();
    let mut invalid_end = event_batch_kind(
        &work_id,
        generation,
        "helper-end-first",
        1,
        CapturedEventKindV1::SessionEnded,
    );
    invalid_end.session.session_id = "helper-session".into();
    let mut invalid_start = event_batch_kind(
        &work_id,
        generation,
        "helper-start-second",
        2,
        CapturedEventKindV1::SessionStarted,
    );
    invalid_start.session.session_id = "helper-session".into();
    store.change_ingest(&helper, &invalid_end, 406).unwrap();
    store.change_ingest(&helper, &invalid_start, 406).unwrap();
    let malformed = store.change_record(&token, &work_id, 406).unwrap().unwrap();
    assert_eq!(malformed.sessions.count, 2);
    assert_eq!(malformed.sessions.coverage, CoverageV1::Partial);

    let updated_seq = store
        .work_get(&token, &work_id, 407)
        .unwrap()
        .unwrap()
        .updated_seq;
    store
        .work_checkpoint(
            &token,
            &WorkCheckpoint {
                work_id: work_id.clone(),
                operation_id: "pause-capture".into(),
                generation,
                session: "session-1".into(),
                expected_updated_seq: Some(updated_seq),
                kind: CheckpointKind::Pause,
                summary: "pause the synthetic capture".into(),
                next_action: None,
                scope: None,
                branch: None,
                worktree: None,
                resolve_blocker: false,
                source_refs: vec![],
                depends_on: vec![],
                related_work: vec![],
            },
            408,
        )
        .unwrap();
    assert!(store.change_ingest(&token, &start, 409).unwrap().replayed);
    assert!(
        store
            .change_action_start(&token, &action, 409)
            .unwrap()
            .replayed
    );
    assert!(
        store
            .change_action_finish(&token, &finish, 409)
            .unwrap()
            .replayed
    );
    let after_end = event_batch_kind(
        &work_id,
        generation,
        "paused-new-event",
        4,
        CapturedEventKindV1::AssistantMessage,
    );
    // A paused Work still accepts capture: session end pauses it and a resumed session keeps going.
    assert!(
        !store
            .change_ingest(&token, &after_end, 410)
            .unwrap()
            .replayed
    );
    let mut paused_action = action_start(&work_id, generation);
    paused_action.operation_id = "paused-new-action".into();
    paused_action.action_id = "paused-new-action".into();
    store
        .change_action_start(&token, &paused_action, 411)
        .unwrap();

    let resumed = store
        .work_start(
            &token,
            &WorkStart {
                work_id: Some(work_id.clone()),
                source: None,
                request_key: None,
                title: "resume synthetic capture".into(),
                session: "session-1".into(),
                role: WorkRole::Lead,
                scope: vec!["src/**".into()],
                branch: None,
                worktree: None,
                next_action: None,
                depends_on: vec![],
                related_work: vec![],
                takeover_generation: Some(generation),
                preflight: None,
            },
            412,
        )
        .unwrap()
        .item;
    assert_eq!(resumed.generation, generation + 1);
    let resumed_summary = store.change_record(&token, &work_id, 413).unwrap().unwrap();
    assert_eq!(resumed_summary.sessions.coverage, CoverageV1::Unknown);
    assert!(resumed_summary.latest_action.is_none());
    assert!(resumed_summary.subject.is_none());
    assert_eq!(resumed_summary.completeness.result, CoverageV1::Unknown);

    let resumed_end = event_batch_kind(
        &work_id,
        resumed.generation,
        "resumed-end",
        3,
        CapturedEventKindV1::SessionEnded,
    );
    let resumed_middle = event_batch_kind(
        &work_id,
        resumed.generation,
        "resumed-middle",
        2,
        CapturedEventKindV1::AssistantMessage,
    );
    let resumed_start = event_batch_kind(
        &work_id,
        resumed.generation,
        "resumed-start",
        1,
        CapturedEventKindV1::SessionStarted,
    );
    store.change_ingest(&token, &resumed_end, 414).unwrap();
    store.change_ingest(&token, &resumed_middle, 415).unwrap();
    store.change_ingest(&token, &resumed_start, 416).unwrap();
    assert_eq!(
        store
            .change_record(&token, &work_id, 417)
            .unwrap()
            .unwrap()
            .sessions
            .coverage,
        CoverageV1::Complete
    );
    let after_terminal = event_batch_kind(
        &work_id,
        resumed.generation,
        "resumed-after-end",
        4,
        CapturedEventKindV1::AssistantMessage,
    );
    store.change_ingest(&token, &after_terminal, 418).unwrap();
    assert_eq!(
        store
            .change_record(&token, &work_id, 419)
            .unwrap()
            .unwrap()
            .sessions
            .coverage,
        CoverageV1::Partial
    );
}

fn checkpoint(
    work_id: &str,
    generation: i64,
    seq: i64,
    op: &str,
    kind: CheckpointKind,
) -> WorkCheckpoint {
    WorkCheckpoint {
        work_id: work_id.into(),
        operation_id: op.into(),
        generation,
        session: "session-1".into(),
        expected_updated_seq: Some(seq),
        kind,
        summary: "synthetic transition".into(),
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

fn updated_seq(store: &dyn Store, token: &str, work_id: &str) -> i64 {
    store
        .work_get(token, work_id, 500)
        .unwrap()
        .unwrap()
        .updated_seq
}

fn capture_survives_pause_and_stops_at_end_of_claim(fresh: &dyn Fn() -> Db) {
    let capturable = |store: &dyn Store, token: &str, work_id: &str, generation: i64, tag: &str| {
        let target = store
            .capture_work_for_session(token, "session-1", 501)
            .unwrap();
        let batch = event_batch_kind(
            work_id,
            generation,
            &format!("{tag}-event"),
            1,
            CapturedEventKindV1::AssistantMessage,
        );
        let mut start = action_start(work_id, generation);
        start.operation_id = format!("{tag}-start");
        start.action_id = format!("{tag}-action");
        let mut finish = action_finish(work_id, generation);
        finish.operation_id = format!("{tag}-finish");
        finish.action_id = format!("{tag}-action");
        (
            target.map(|item| (item.id, item.generation)),
            store.change_ingest(token, &batch, 502).is_ok(),
            store.change_action_start(token, &start, 503).is_ok(),
            store.change_action_finish(token, &finish, 504).is_ok(),
        )
    };

    // Paused and Blocked keep capturing without changing the Work state.
    for (kind, tag) in [
        (CheckpointKind::Pause, "paused"),
        (CheckpointKind::Blocker, "blocked"),
    ] {
        let db = fresh();
        let (store, token, work_id, generation) = setup(&db);
        let seq = updated_seq(&*store, &token, &work_id);
        store
            .work_checkpoint(
                &token,
                &checkpoint(&work_id, generation, seq, tag, kind),
                500,
            )
            .unwrap();
        let after_transition = updated_seq(&*store, &token, &work_id);
        assert_eq!(
            capturable(&*store, &token, &work_id, generation, tag),
            (Some((work_id.clone(), generation)), true, true, true)
        );
        assert_eq!(updated_seq(&*store, &token, &work_id), after_transition);
    }

    // Finish and Handover end the claim.
    for (kind, tag) in [
        (CheckpointKind::Finish, "finished"),
        (CheckpointKind::Handover, "handover"),
    ] {
        let db = fresh();
        let (store, token, work_id, generation) = setup(&db);
        // An action started while the Work was current, so finish is refused only by state.
        let mut open = action_start(&work_id, generation);
        open.operation_id = format!("{tag}-open-start");
        open.action_id = format!("{tag}-open");
        store.change_action_start(&token, &open, 499).unwrap();
        let seq = updated_seq(&*store, &token, &work_id);
        store
            .work_checkpoint(
                &token,
                &checkpoint(&work_id, generation, seq, tag, kind),
                500,
            )
            .unwrap();
        let mut finish = action_finish(&work_id, generation);
        finish.operation_id = format!("{tag}-open-finish");
        finish.action_id = format!("{tag}-open");
        assert!(store.change_action_finish(&token, &finish, 504).is_err());
        assert_eq!(
            capturable(&*store, &token, &work_id, generation, tag),
            (None, false, false, false)
        );
    }

    // A takeover bumps the generation and fences the old session, even from Paused.
    let db = fresh();
    let (store, token, work_id, generation) = setup(&db);
    let seq = updated_seq(&*store, &token, &work_id);
    store
        .work_checkpoint(
            &token,
            &checkpoint(
                &work_id,
                generation,
                seq,
                "pause-before-takeover",
                CheckpointKind::Pause,
            ),
            500,
        )
        .unwrap();
    let taken = store
        .work_start(
            &token,
            &WorkStart {
                work_id: Some(work_id.clone()),
                source: None,
                request_key: None,
                title: "takeover".into(),
                session: "session-2".into(),
                role: WorkRole::Lead,
                scope: vec![],
                branch: None,
                worktree: None,
                next_action: None,
                depends_on: vec![],
                related_work: vec![],
                takeover_generation: Some(generation),
                preflight: None,
            },
            500,
        )
        .unwrap()
        .item;
    assert_eq!(taken.generation, generation + 1);
    assert_eq!(
        capturable(&*store, &token, &work_id, generation, "fenced"),
        (None, false, false, false)
    );
}

fn informational_events_do_not_change_lifecycle_coverage(fresh: &dyn Fn() -> Db) {
    // Each case gets its own database: a session participates in one Work at a time.
    let ingest = |kinds: &[(CapturedEventKindV1, Option<&str>)]| {
        let db = fresh();
        let (store, token, work_id, generation) = setup(&db);
        let mut batch = event_batch(&work_id, generation, "informational", 1);
        batch.events = kinds
            .iter()
            .enumerate()
            .map(|(index, (kind, boundary))| {
                let sequence = index as u64 + 1;
                let mut event = event_batch_kind(&work_id, generation, "x", sequence, *kind)
                    .events
                    .remove(0);
                event.boundary = boundary.map(str::to_owned);
                match kind {
                    CapturedEventKindV1::SubagentStarted | CapturedEventKindV1::SubagentStopped => {
                        event.subagent = Some(SubagentFactV1 {
                            agent_id: "agent-1".into(),
                            agent_type: Some("Explore".into()),
                        });
                    }
                    CapturedEventKindV1::ToolFinished => {
                        event.tool = Some(ToolFactV1 {
                            kind: ToolKindV1::Other,
                            request_id: Some("toolu-1".into()),
                            name: Some("mcp__server__tool".into()),
                            completion: Some(ToolCompletionV1::Returned),
                        });
                    }
                    _ => {}
                }
                event
            })
            .collect();
        store.change_ingest(&token, &batch, 400).unwrap();
        store
            .change_record(&token, &work_id, 401)
            .unwrap()
            .unwrap()
            .sessions
            .coverage
    };
    use CapturedEventKindV1::*;
    // Informational kinds and a compaction restart sit inside one valid lifecycle.
    assert_eq!(
        ingest(&[
            (SessionStarted, Some("startup")),
            (SubagentStarted, None),
            (ToolFinished, None),
            (SubagentStopped, None),
            (TurnEnded, None),
            (Compaction, None),
            (SessionStarted, Some("compact")),
            (TurnEnded, None),
            (SessionEnded, Some("other")),
        ]),
        CoverageV1::Complete
    );
    // They never complete a session that was not observed ending, or one with a second origin.
    assert_ne!(
        ingest(&[(SessionStarted, Some("startup")), (TurnEnded, None)]),
        CoverageV1::Complete
    );
    assert_ne!(
        ingest(&[
            (SessionStarted, Some("startup")),
            (SessionStarted, Some("resume")),
            (SessionEnded, None),
        ]),
        CoverageV1::Complete
    );
}

fn setup_observation(
    model: Option<&str>,
    permission_mode: Option<&str>,
    effort: Option<&str>,
) -> SetupObservationV1 {
    SetupObservationV1 {
        provider: Some("anthropic".into()),
        model: model.map(str::to_owned),
        permission_mode: permission_mode.map(str::to_owned),
        effort: effort.map(str::to_owned),
        artifact: ArtifactRefV1 {
            digest: "7".repeat(64),
            size_bytes: 10,
            media: ArtifactMediaV1::Json,
            retention: RetentionClassV1::WorkLocal,
            availability: ContentAvailabilityV1::Local,
        },
        unknown_components: vec![],
    }
}

fn join_session(store: &dyn Store, token: &str, work_id: &str, session: &str) {
    let joined = store
        .work_start(
            token,
            &WorkStart {
                work_id: Some(work_id.into()),
                source: None,
                request_key: None,
                title: "capture a source-bound check".into(),
                session: session.into(),
                role: WorkRole::Helper,
                scope: vec![],
                branch: None,
                worktree: None,
                next_action: None,
                depends_on: vec![],
                related_work: vec![],
                takeover_generation: None,
                preflight: None,
            },
            unix_now(),
        )
        .unwrap();
    assert!(joined.joined);
}

fn setup_batch(
    work_id: &str,
    generation: i64,
    session_id: &str,
    parent: Option<&str>,
    observations: Vec<Option<SetupObservationV1>>,
) -> CaptureBatchV1 {
    let mut batch = event_batch(work_id, generation, &format!("setup-{session_id}"), 1);
    batch.session.session_id = session_id.into();
    batch.session.parent_session_id = parent.map(str::to_owned);
    batch.events = observations
        .into_iter()
        .enumerate()
        .map(|(index, setup)| {
            let mut event = event_batch_kind(
                work_id,
                generation,
                "x",
                index as u64 + 1,
                CapturedEventKindV1::TurnEnded,
            )
            .events
            .remove(0);
            event.event_id = format!("{session_id}-event-{index}");
            event.setup = setup;
            event
        })
        .collect();
    batch
}

fn session_setups_project_latest_per_session(db: &Db) {
    let (store, token, work_id, generation) = setup(db);
    assert!(
        store
            .change_record(&token, &work_id, 500)
            .unwrap()
            .unwrap()
            .session_setups
            .is_empty()
    );
    // The later setup observation replaces the earlier one; a later event without setup does not.
    let first = setup_batch(
        &work_id,
        generation,
        "session-1",
        None,
        vec![
            Some(setup_observation(Some("model-a"), Some("default"), None)),
            Some(setup_observation(
                Some("model-b"),
                Some("plan"),
                Some("high"),
            )),
            None,
        ],
    );
    store.change_ingest(&token, &first, 501).unwrap();
    // A session that never reported setup is absent; absent fields stay None, not guessed.
    for id in ["session-0", "session-2"] {
        join_session(&*store, &token, &work_id, id);
    }
    let silent = setup_batch(&work_id, generation, "session-0", None, vec![None]);
    store.change_ingest(&token, &silent, 502).unwrap();
    let child = setup_batch(
        &work_id,
        generation,
        "session-2",
        Some("session-1"),
        vec![Some(setup_observation(None, None, None))],
    );
    store.change_ingest(&token, &child, 503).unwrap();
    let record = store.change_record(&token, &work_id, 504).unwrap().unwrap();
    assert_eq!(record.session_setups_omitted, 0);
    assert_eq!(
        record.session_setups,
        vec![
            SessionSetupV1 {
                session_id: "session-1".into(),
                parent_session_id: None,
                model: Some("model-b".into()),
                permission_mode: Some("plan".into()),
                effort: Some("high".into()),
            },
            SessionSetupV1 {
                session_id: "session-2".into(),
                parent_session_id: Some("session-1".into()),
                model: None,
                permission_mode: None,
                effort: None,
            },
        ]
    );
}

fn session_setups_are_bounded_and_ordered(db: &Db) {
    let (store, token, work_id, generation) = setup(db);
    let total = MAX_PROJECTED_SESSION_SETUPS + 3;
    // Ingest out of id order to prove the ordering comes from the projection.
    for index in (0..total).rev() {
        let id = format!("s-{index:02}");
        join_session(&*store, &token, &work_id, &id);
        let batch = setup_batch(
            &work_id,
            generation,
            &id,
            None,
            vec![Some(setup_observation(Some("model-a"), None, None))],
        );
        store.change_ingest(&token, &batch, 600).unwrap();
    }
    let record = store.change_record(&token, &work_id, 601).unwrap().unwrap();
    let ids: Vec<_> = record
        .session_setups
        .iter()
        .map(|setup| setup.session_id.clone())
        .collect();
    let expected: Vec<_> = (0..MAX_PROJECTED_SESSION_SETUPS)
        .map(|index| format!("s-{index:02}"))
        .collect();
    assert_eq!(ids, expected);
    assert_eq!(record.session_setups_omitted, 3);
}

#[test]
fn sqlite_session_setups() {
    session_setups_project_latest_per_session(&Db::sqlite());
    session_setups_are_bounded_and_ordered(&Db::sqlite());
}

#[test]
fn postgres_session_setups() {
    if Db::postgres().is_some() {
        session_setups_project_latest_per_session(&Db::postgres().unwrap());
        session_setups_are_bounded_and_ordered(&Db::postgres().unwrap());
    }
}

fn pause_work(store: &dyn Store, token: &str, work: &WorkItem, session: &str, tag: &str) {
    let seq = updated_seq(store, token, &work.id);
    let mut pause = checkpoint(&work.id, work.generation, seq, tag, CheckpointKind::Pause);
    pause.session = session.into();
    store.work_checkpoint(token, &pause, 500).unwrap();
}

fn capture_target_prefers_active_then_lead(fresh: &dyn Fn() -> Db) {
    let db = fresh();
    let (store, token, led, generation) = setup(&db);
    let owner = store
        .upsert_user(
            &ForgeIdentity {
                provider: "github".into(),
                host: "github.com".into(),
                user_id: 42,
            },
            "builder",
        )
        .unwrap();
    let (other, _) = mint_agent_token(&*store, &owner.id, "other", REPO, 3600).unwrap();
    let lead_other = |session: &str| {
        store
            .work_start(
                &other,
                &WorkStart {
                    work_id: None,
                    source: None,
                    request_key: Some(format!("ambiguity-{session}")),
                    title: format!("ambiguity fixture {session}"),
                    session: session.into(),
                    role: WorkRole::Lead,
                    scope: vec![],
                    branch: None,
                    worktree: None,
                    next_action: None,
                    depends_on: vec![],
                    related_work: vec![],
                    takeover_generation: None,
                    preflight: None,
                },
                unix_now(),
            )
            .unwrap()
            .item
    };
    let (x, y) = (lead_other("lead-x"), lead_other("lead-y"));
    join_session(&*store, &token, &x.id, "session-1");
    join_session(&*store, &token, &y.id, "session-1");
    let target = || {
        store
            .capture_work_for_session(&token, "session-1", 501)
            .map(|item| item.map(|item| item.id))
    };
    // Active and led by the session beats two active Works it only helps.
    assert_eq!(target().unwrap(), Some(led.clone()));

    // Paused loses to active; two equal helper rows stay ambiguous.
    let led_item = store.work_get(&token, &led, 500).unwrap().unwrap();
    assert_eq!(led_item.generation, generation);
    pause_work(&*store, &token, &led_item, "session-1", "amb-led");
    assert!(target().is_err());
    pause_work(&*store, &other, &x, "lead-x", "amb-x");
    assert_eq!(target().unwrap(), Some(y.id.clone()));

    // With everything paused the session's lead role decides.
    pause_work(&*store, &other, &y, "lead-y", "amb-y");
    assert_eq!(target().unwrap(), Some(led));
}

#[test]
fn sqlite_action_capture_receipt() {
    let db = Db::sqlite();
    action_capture_receipt(&db);
    lifecycle_resume_and_replay(&db);
    informational_events_do_not_change_lifecycle_coverage(&Db::sqlite);
    capture_survives_pause_and_stops_at_end_of_claim(&Db::sqlite);
    capture_target_prefers_active_then_lead(&Db::sqlite);
}

#[test]
fn postgres_action_capture_receipt() {
    if let Some(db) = Db::postgres() {
        action_capture_receipt(&db);
        lifecycle_resume_and_replay(&db);
        informational_events_do_not_change_lifecycle_coverage(&|| Db::postgres().unwrap());
        capture_survives_pause_and_stops_at_end_of_claim(&|| Db::postgres().unwrap());
        capture_target_prefers_active_then_lead(&|| Db::postgres().unwrap());
    }
}

fn shared_batch(
    work_id: &str,
    generation: i64,
    operation: &str,
    sequence: u64,
    body: &[u8],
    size: u64,
) -> CaptureBatchV1 {
    let mut batch = event_batch(work_id, generation, operation, sequence);
    batch.session.capture_mode = CaptureModeV1::SharedConversation;
    batch.events[0].event_id = format!("shared-event-{operation}");
    batch.events[0].artifact = Some(ArtifactRefV1 {
        digest: sha256_hex(body),
        size_bytes: size,
        media: ArtifactMediaV1::Json,
        retention: RetentionClassV1::SharedApproved,
        availability: ContentAvailabilityV1::Local,
    });
    batch
}

fn sha256_hex(bytes: &[u8]) -> String {
    agentscm::store::digest(bytes)
}

fn shared_body_lifecycle(db: &Db) {
    let (store, token, work_id, generation) = setup(db);
    let stranger = store
        .upsert_user(
            &ForgeIdentity {
                provider: "github".into(),
                host: "github.com".into(),
                user_id: 43,
            },
            "stranger",
        )
        .unwrap();
    let (stranger_token, _) = mint_agent_token(&*store, &stranger.id, "other", REPO, 3600).unwrap();
    let now = unix_now();
    let (first, second, third) = (
        b"{\"a\":1}".as_slice(),
        b"{\"b\":2}".as_slice(),
        b"{\"c\":3}".as_slice(),
    );
    let digest = sha256_hex(first);

    let receipt = store
        .change_ingest(
            &token,
            &shared_batch(&work_id, generation, "one", 1, first, 7),
            now,
        )
        .unwrap();
    assert_eq!(receipt.accepted, 1);
    let access = store
        .shared_body_lookup(&token, &work_id, &digest, now)
        .unwrap()
        .unwrap();
    let owner = access.caller.clone();
    let row = access.own().unwrap();
    assert_eq!(row.state, SharedBodyState::Declared);
    assert_eq!((row.size_bytes, row.sequence), (7, 1));
    assert_eq!(
        (row.declared_at, row.author.as_str()),
        (now, owner.as_str())
    );
    // Another human sees the declaration exists but has none of their own.
    let other = store
        .shared_body_lookup(&stranger_token, &work_id, &digest, now)
        .unwrap()
        .unwrap();
    assert_eq!(other.caller, stranger.id);
    assert!(other.own().is_none() && other.accessible(false, now).is_none());
    assert_eq!(other.rows.len(), 1);
    // An undeclared digest has no rows, which is not the same as an unknown Work.
    assert!(
        store
            .shared_body_lookup(&token, &work_id, &sha256_hex(b"nothing"), now)
            .unwrap()
            .unwrap()
            .rows
            .is_empty()
    );
    assert!(
        store
            .shared_body_lookup(&token, "work_unknown", &digest, now)
            .unwrap()
            .is_none()
    );

    // A replay and a second event naming the same body keep the first declaration.
    let replay = store
        .change_ingest(
            &token,
            &shared_batch(&work_id, generation, "one", 1, first, 7),
            now + 1,
        )
        .unwrap();
    assert!(replay.replayed);
    store
        .change_ingest(
            &token,
            &shared_batch(&work_id, generation, "two", 2, first, 7),
            now + 2,
        )
        .unwrap();
    let unchanged = store
        .shared_body_lookup(&token, &work_id, &digest, now)
        .unwrap()
        .unwrap()
        .own()
        .unwrap()
        .clone();
    assert_eq!(
        (unchanged.event_id.as_str(), unchanged.declared_at),
        ("shared-event-one", now)
    );

    // The same digest declared with another size is refused and records nothing.
    let conflict = store.change_ingest(
        &token,
        &shared_batch(&work_id, generation, "three", 3, first, 8),
        now + 3,
    );
    assert!(matches!(conflict, Err(agentscm::hub::HubError::Invalid(_))));
    // The refused batch rolled back whole, so its event id and sequence are still free.
    let retried = store
        .change_ingest(
            &token,
            &shared_batch(&work_id, generation, "three", 3, first, 7),
            now + 3,
        )
        .unwrap();
    assert_eq!(retried.accepted, 1);

    assert_eq!(
        store
            .shared_body_mark_available("work_unknown", &digest, &owner, now, 100)
            .unwrap(),
        None
    );
    assert_eq!(
        store
            .shared_body_mark_available(&work_id, &digest, &owner, now + 10, 100)
            .unwrap(),
        Some(SharedBodyState::Available)
    );
    // A second upload does not extend the retention.
    store
        .shared_body_mark_available(&work_id, &digest, &owner, now + 50, 100)
        .unwrap();
    let row = store
        .shared_body_lookup(&token, &work_id, &digest, now)
        .unwrap()
        .unwrap()
        .own()
        .unwrap()
        .clone();
    assert_eq!(
        (row.uploaded_at, row.expires_at),
        (Some(now + 10), Some(now + 110))
    );
    assert_eq!(row.state_at(now + 109), SharedBodyState::Available);
    assert_eq!(row.state_at(now + 110), SharedBodyState::Expired);

    assert!(
        store
            .shared_bodies_expire(now + 109, 10, 1_000_000_000)
            .unwrap()
            .is_empty()
    );
    assert!(
        store
            .shared_bodies_pending_physical_delete(now + 999, 10)
            .unwrap()
            .is_empty()
    );
    let expired = store
        .shared_bodies_expire(now + 110, 10, 1_000_000_000)
        .unwrap();
    assert_eq!(expired.len(), 1);
    assert_eq!(
        (expired[0].digest.as_str(), expired[0].state),
        (digest.as_str(), SharedBodyState::Expired)
    );
    assert!(
        store
            .shared_bodies_expire(now + 999, 10, 1_000_000_000)
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        store
            .shared_body_mark_available(&work_id, &digest, &owner, now + 120, 100)
            .unwrap(),
        Some(SharedBodyState::Expired)
    );
    assert_eq!(
        store
            .shared_bodies_pending_physical_delete(now + 999, 10)
            .unwrap()
            .len(),
        1
    );
    store
        .shared_body_mark_physically_deleted(&work_id, &digest, &owner, now + 130)
        .unwrap();
    assert!(
        store
            .shared_bodies_pending_physical_delete(now + 999, 10)
            .unwrap()
            .is_empty()
    );

    // A tombstone replaces expiry, keeps the first physical-removal time, and is final.
    let tombstoned = store
        .shared_body_tombstone(&work_id, &digest, Some(&owner), &stranger.id, now + 140)
        .unwrap();
    assert_eq!(
        tombstoned.iter().map(|row| row.state).collect::<Vec<_>>(),
        [SharedBodyState::Deleted]
    );
    let again = store
        .shared_body_tombstone(&work_id, &digest, None, "usr_later", now + 150)
        .unwrap();
    assert_eq!(again[0].deleted_by.as_deref(), Some(stranger.id.as_str()));
    assert!(
        store
            .shared_body_tombstone(&work_id, &sha256_hex(b"x"), None, "usr_x", now)
            .unwrap()
            .is_empty()
    );
    let row = store
        .shared_body_lookup(&token, &work_id, &digest, now)
        .unwrap()
        .unwrap()
        .own()
        .unwrap()
        .clone();
    assert_eq!(row.state, SharedBodyState::Deleted);
    assert_eq!(
        (row.deleted_at, row.deleted_by.as_deref()),
        (Some(now + 140), Some(stranger.id.as_str()))
    );
    assert_eq!(row.physical_deleted_at, Some(now + 130));
    assert_eq!(
        store
            .shared_body_mark_available(&work_id, &digest, &owner, now + 160, 100)
            .unwrap(),
        Some(SharedBodyState::Deleted)
    );

    // Sweeps are bounded: three due bodies need two passes of two.
    for (index, body) in [first, second, third].into_iter().enumerate().skip(1) {
        let operation = format!("more-{index}");
        store
            .change_ingest(
                &token,
                &shared_batch(&work_id, generation, &operation, 10 + index as u64, body, 7),
                now + 200,
            )
            .unwrap();
        store
            .shared_body_mark_available(&work_id, &sha256_hex(body), &owner, now + 200, 10)
            .unwrap();
    }
    assert_eq!(
        store
            .shared_bodies_expire(now + 300, 1, 1_000_000_000)
            .unwrap()
            .len(),
        1
    );
    assert_eq!(
        store
            .shared_bodies_expire(now + 300, 5, 1_000_000_000)
            .unwrap()
            .len(),
        1
    );
    assert_eq!(
        store
            .shared_bodies_pending_physical_delete(now + 999, 1)
            .unwrap()
            .len(),
        1
    );
    assert_eq!(
        store
            .shared_bodies_pending_physical_delete(now + 999, 5)
            .unwrap()
            .len(),
        2
    );
}

#[test]
fn sqlite_shared_body_lifecycle() {
    shared_body_lifecycle(&Db::sqlite());
}

#[test]
fn postgres_shared_body_lifecycle() {
    if let Some(db) = Db::postgres() {
        shared_body_lifecycle(&db);
    }
}

fn shared_conversation_listing(db: &Db) {
    let (store, token, work_id, generation) = setup(db);
    let stranger = store
        .upsert_user(
            &ForgeIdentity {
                provider: "github".into(),
                host: "github.com".into(),
                user_id: 43,
            },
            "stranger",
        )
        .unwrap();
    let (stranger_token, _) = mint_agent_token(&*store, &stranger.id, "other", REPO, 3600).unwrap();
    for session in ["B-session", "a-session"] {
        join_session(&*store, &token, &work_id, session);
    }
    let now = unix_now();
    let bodies: Vec<Vec<u8>> = (0..4)
        .map(|n| format!("{{\"n\":{n}}}").into_bytes())
        .collect();
    // Declared out of order, in sessions that sort differently bytewise and by locale.
    for (index, (session, sequence)) in [
        ("session-1", 1),
        ("a-session", 2),
        ("B-session", 1),
        ("a-session", 1),
    ]
    .into_iter()
    .enumerate()
    {
        let mut batch = shared_batch(
            &work_id,
            generation,
            &format!("list-{index}"),
            sequence,
            &bodies[index],
            bodies[index].len() as u64,
        );
        batch.session.session_id = session.into();
        batch.events[0].event_id = format!("{session}-{sequence}");
        batch.events[0].kind = CapturedEventKindV1::AssistantMessage;
        batch.events[0].observed_at = 500 + index as i64;
        store.change_ingest(&token, &batch, now).unwrap();
    }

    let page = store
        .shared_conversation_page(&token, &work_id, None, false, now)
        .unwrap()
        .unwrap();
    let order: Vec<(&str, u64)> = page
        .iter()
        .map(|entry| (entry.session_id.as_str(), entry.sequence))
        .collect();
    assert_eq!(
        order,
        [
            ("B-session", 1),
            ("a-session", 1),
            ("a-session", 2),
            ("session-1", 1)
        ]
    );
    let first = &page[0];
    assert_eq!(first.digest, sha256_hex(&bodies[2]));
    assert_eq!(first.event_id, "B-session-1");
    assert_eq!(
        (
            first.kind,
            first.observed_at,
            first.size_bytes,
            first.expires_at
        ),
        (CapturedEventKindV1::AssistantMessage, 502, 7, None)
    );
    assert!(
        page.iter()
            .all(|entry| entry.availability == ContentAvailabilityV1::Local)
    );

    // A cursor continues strictly after its entry, for a position that no longer needs to exist.
    for (index, entry) in page.iter().enumerate() {
        let rest = store
            .shared_conversation_page(&token, &work_id, Some(&entry.cursor()), false, now)
            .unwrap()
            .unwrap();
        assert_eq!(rest, page[index + 1..], "{index}");
    }

    // Availability follows the row, and an available body is expired from its deadline.
    let digest = sha256_hex(&bodies[2]);
    let owner = store
        .shared_body_lookup(&token, &work_id, &digest, now)
        .unwrap()
        .unwrap()
        .caller;
    store
        .shared_body_mark_available(&work_id, &digest, &owner, now, 100)
        .unwrap();
    store
        .shared_body_tombstone(&work_id, &sha256_hex(&bodies[1]), None, &stranger.id, now)
        .unwrap();
    let states = |at: i64| -> Vec<ContentAvailabilityV1> {
        store
            .shared_conversation_page(&token, &work_id, None, false, at)
            .unwrap()
            .unwrap()
            .iter()
            .map(|entry| entry.availability)
            .collect()
    };
    use ContentAvailabilityV1::{Deleted, Expired, Local, Shared};
    assert_eq!(states(now + 99), [Shared, Local, Deleted, Local]);
    assert_eq!(states(now + 100), [Expired, Local, Deleted, Local]);
    assert_eq!(
        store
            .shared_conversation_page(&token, &work_id, None, false, now)
            .unwrap()
            .unwrap()[0]
            .expires_at,
        Some(now + 100)
    );

    // Another human sees none of these unless the caller is an admin; an unknown Work is
    // absent; bad cursors fail.
    let other = |all_authors: bool| {
        store
            .shared_conversation_page(&stranger_token, &work_id, None, all_authors, now)
            .unwrap()
            .unwrap()
    };
    assert!(other(false).is_empty());
    assert_eq!(
        other(true)
            .iter()
            .map(|entry| entry.cursor())
            .collect::<Vec<_>>(),
        page.iter().map(|entry| entry.cursor()).collect::<Vec<_>>()
    );
    assert!(
        store
            .shared_conversation_page(&token, "work_unknown", None, false, now)
            .unwrap()
            .is_none()
    );
    assert!(
        store
            .shared_conversation_page(&token, &work_id, Some("garbage"), false, now)
            .is_err()
    );
}

#[test]
fn sqlite_shared_conversation_listing() {
    shared_conversation_listing(&Db::sqlite());
}

#[test]
fn postgres_shared_conversation_listing() {
    if let Some(db) = Db::postgres() {
        shared_conversation_listing(&db);
    }
}

fn shared_bodies_are_rows_per_author(db: &Db) {
    let (store, token, work_id, generation) = setup(db);
    let helper = store
        .upsert_user(
            &ForgeIdentity {
                provider: "github".into(),
                host: "github.com".into(),
                user_id: 44,
            },
            "helper",
        )
        .unwrap();
    let (helper_token, _) = mint_agent_token(&*store, &helper.id, "other", REPO, 3600).unwrap();
    join_session(&*store, &helper_token, &work_id, "helper-session");
    let now = unix_now();
    let body = b"{\"same\":1}".as_slice();
    let digest = sha256_hex(body);
    store
        .change_ingest(
            &token,
            &shared_batch(&work_id, generation, "own", 1, body, 10),
            now,
        )
        .unwrap();
    // The helper declares the identical body from their own session without colliding.
    // A different size from another author's row is not an error and reveals nothing.
    let mut theirs = shared_batch(&work_id, generation, "theirs", 1, body, 11);
    theirs.session.session_id = "helper-session".into();
    store.change_ingest(&helper_token, &theirs, now).unwrap();
    // Only the same author's own earlier size must agree.
    let mut wrong = shared_batch(&work_id, generation, "wrong", 2, body, 12);
    wrong.session.session_id = "helper-session".into();
    assert!(matches!(
        store.change_ingest(&helper_token, &wrong, now),
        Err(agentscm::hub::HubError::Invalid(_))
    ));

    let access = store
        .shared_body_lookup(&helper_token, &work_id, &digest, now)
        .unwrap()
        .unwrap();
    assert_eq!(access.rows.len(), 2);
    assert_eq!(access.own().unwrap().author, helper.id);
    let owner = access
        .rows
        .iter()
        .find(|row| row.author != helper.id)
        .unwrap()
        .author
        .clone();
    let own = |who: &str, at: &[String]| {
        store
            .shared_bodies_own(who, &work_id, at, now)
            .unwrap()
            .unwrap()
    };
    let asked = [digest.clone(), sha256_hex(b"nothing")];
    assert_eq!(own(&helper_token, &asked).len(), 1);
    assert_eq!(own(&helper_token, &asked)[0].author, helper.id);
    assert_eq!(own(&token, &asked)[0].author, owner);
    assert!(
        store
            .shared_bodies_own(&token, "work_unknown", &asked, now)
            .unwrap()
            .is_none()
    );

    // The listing is the caller's own entries, or every author's for an admin.
    let listed = |who: &str, all: bool| {
        store
            .shared_conversation_page(who, &work_id, None, all, now)
            .unwrap()
            .unwrap()
            .len()
    };
    assert_eq!(
        (listed(&token, false), listed(&helper_token, false)),
        (1, 1)
    );
    assert_eq!(listed(&token, true), 2);

    // The object is needed only while a declaration is available; a declared one writes it later.
    assert!(!store.shared_body_object_in_use(&work_id, &digest).unwrap());
    store
        .shared_body_mark_available(&work_id, &digest, &owner, now, 100)
        .unwrap();
    assert!(store.shared_body_object_in_use(&work_id, &digest).unwrap());
    let deleted = store
        .shared_body_tombstone(&work_id, &digest, Some(&helper.id), &helper.id, now)
        .unwrap();
    assert_eq!(deleted.len(), 1);
    assert!(store.shared_body_object_in_use(&work_id, &digest).unwrap());
    store
        .shared_body_tombstone(&work_id, &digest, Some(&owner), &owner, now)
        .unwrap();
    assert!(!store.shared_body_object_in_use(&work_id, &digest).unwrap());

    // A failed removal backs off, is visible again at its retry time, and bookkeeping is per row.
    let pending = |at: i64| store.shared_bodies_pending_physical_delete(at, 10).unwrap();
    assert_eq!(pending(now).len(), 2);
    store
        .shared_body_delete_failed(&work_id, &digest, &owner, now + 50)
        .unwrap();
    assert_eq!(pending(now + 49).len(), 1);
    assert_eq!(pending(now + 50).len(), 2);
    assert_eq!(
        pending(now + 50)
            .iter()
            .map(|row| row.delete_attempts)
            .max(),
        Some(1)
    );
    store
        .shared_body_mark_physically_deleted(&work_id, &digest, &owner, now + 60)
        .unwrap();
    assert_eq!(pending(now + 99).len(), 1);
    // A failure after a recorded removal makes the row pending again.
    store
        .shared_body_delete_failed(&work_id, &digest, &owner, now + 70)
        .unwrap();
    assert_eq!(pending(now + 70).len(), 2);
}

#[test]
fn sqlite_shared_bodies_are_rows_per_author() {
    shared_bodies_are_rows_per_author(&Db::sqlite());
}

#[test]
fn postgres_shared_bodies_are_rows_per_author() {
    if let Some(db) = Db::postgres() {
        shared_bodies_are_rows_per_author(&db);
    }
}

fn shared_bodies_demote_and_expire_stale_declarations(db: &Db) {
    let (store, token, work_id, generation) = setup(db);
    let now = unix_now();
    let (first, second) = (b"{\"a\":1}".as_slice(), b"{\"b\":2}".as_slice());
    let (one, two) = (sha256_hex(first), sha256_hex(second));
    store
        .change_ingest(
            &token,
            &shared_batch(&work_id, generation, "one", 1, first, 7),
            now,
        )
        .unwrap();
    store
        .change_ingest(
            &token,
            &shared_batch(&work_id, generation, "two", 2, second, 7),
            now,
        )
        .unwrap();
    let access = store
        .shared_body_lookup(&token, &work_id, &one, now)
        .unwrap()
        .unwrap();
    let owner = access.caller;
    let state = |digest: &str| {
        store
            .shared_body_lookup(&token, &work_id, digest, now)
            .unwrap()
            .unwrap()
            .own()
            .unwrap()
            .clone()
    };

    store
        .shared_body_mark_available(&work_id, &one, &owner, now, 100)
        .unwrap();
    // Only an available row is demoted, and only the named author's when one is given.
    assert_eq!(
        store
            .shared_body_demote_available(&work_id, &one, Some("nobody"))
            .unwrap(),
        0
    );
    assert_eq!(
        store
            .shared_body_demote_available(&work_id, &two, None)
            .unwrap(),
        0
    );
    assert_eq!(
        store
            .shared_body_demote_available(&work_id, &one, Some(&owner))
            .unwrap(),
        1
    );
    assert_eq!(state(&one).state, SharedBodyState::Declared);
    // A demoted row uploads again like a fresh declaration.
    store
        .shared_body_mark_available(&work_id, &one, &owner, now + 10, 100)
        .unwrap();
    assert_eq!(state(&one).expires_at, Some(now + 110));
    // A row demoted after its upload keeps that upload's deadline rather than the declaration's.
    store
        .shared_body_demote_available(&work_id, &one, Some(&owner))
        .unwrap();

    // At the retention period after its declaration a never-uploaded row expires without an
    // object of its own; a demoted row waits for its own deadline.
    assert!(
        store
            .shared_bodies_expire(now + 99, 10, 100)
            .unwrap()
            .is_empty()
    );
    let expired = store.shared_bodies_expire(now + 100, 10, 100).unwrap();
    assert_eq!(expired.len(), 1);
    assert_eq!(expired[0].digest, two);
    assert_eq!(state(&two).state, SharedBodyState::Expired);
    assert_eq!(state(&two).expires_at, Some(now + 100));
    assert_eq!(state(&one).state, SharedBodyState::Declared);
    assert_eq!(
        store
            .shared_bodies_expire(now + 110, 10, 100)
            .unwrap()
            .len(),
        1
    );
    assert_eq!(
        store
            .shared_bodies_pending_physical_delete(now + 110, 10)
            .unwrap()
            .len(),
        2
    );
}

#[test]
fn sqlite_shared_bodies_demote_and_expire_stale_declarations() {
    shared_bodies_demote_and_expire_stale_declarations(&Db::sqlite());
}

#[test]
fn postgres_shared_bodies_demote_and_expire_stale_declarations() {
    if let Some(db) = Db::postgres() {
        shared_bodies_demote_and_expire_stale_declarations(&db);
    }
}
