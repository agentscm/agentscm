//! Change-record storage conformance. SQLite always; PostgreSQL when configured.
#[path = "common/hub_pg.rs"]
mod hub_pg;

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use agentscm::change_record::{
    ActionFinishV1, ActionKindV1, ActionOutcomeV1, ActionStartV1, AgentHarnessV1, ApplicabilityV1,
    ArtifactMediaV1, ArtifactRefV1, AssuranceBasisV1, CaptureBatchV1, CaptureModeV1,
    CapturedEventKindV1, CapturedEventV1, CheckResultV1, ContentAvailabilityV1, CoverageV1,
    DriftV1, RecipeProvenanceV1, RetentionClassV1, SessionIdentityV1, SourceBindingV1,
    SubagentFactV1, ToolCompletionV1, ToolFactV1, ToolKindV1,
};
use agentscm::hub::store::{self, ForgeIdentity, Store, mint_agent_token, unix_now};
use agentscm::hub::work::{CheckpointKind, SourceRef, WorkCheckpoint, WorkRole, WorkStart};
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
    assert!(store.change_ingest(&token, &after_end, 410).is_err());
    let mut paused_action = action_start(&work_id, generation);
    paused_action.operation_id = "paused-new-action".into();
    paused_action.action_id = "paused-new-action".into();
    assert!(
        store
            .change_action_start(&token, &paused_action, 411)
            .is_err()
    );

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

#[test]
fn sqlite_action_capture_receipt() {
    let db = Db::sqlite();
    action_capture_receipt(&db);
    lifecycle_resume_and_replay(&db);
    informational_events_do_not_change_lifecycle_coverage(&Db::sqlite);
}

#[test]
fn postgres_action_capture_receipt() {
    if let Some(db) = Db::postgres() {
        action_capture_receipt(&db);
        lifecycle_resume_and_replay(&db);
        informational_events_do_not_change_lifecycle_coverage(&|| Db::postgres().unwrap());
    }
}
