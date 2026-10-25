#![cfg(unix)]

use std::fs::{self, OpenOptions};
use std::io::{BufReader, Cursor, Read, Write};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::Path;
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Barrier};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use agentscm::change_capture::claude_code::{
    ActionCaptureConfig, ActionRuleV1, CaptureRequest, HookCaptureOutcome, NativeActionOutcome,
    NativeResultSourceV1, capture_hook, capture_native_hook, capture_provider_usage,
    deferred_native_hook, import_jsonl,
};
use agentscm::change_capture::spool::ProviderHistoryCursorV1;
use agentscm::change_capture::{
    AppendOutcome, ConfiguredRecipeRunRequest, ConfiguredRecipeV1, DeferredHookV1,
    DeferredReasonV1, GitSourceRequest, LocalSpool, MIN_ACTION_CAPTURE_BYTES,
    MIN_ACTION_CAPTURE_RECORDS, NamespaceBindOutcome, ObservedCommandRequest,
    RecipeReconcileOutcome, SpoolClassificationV1, SpoolLimits, SpoolNamespaceV1,
    capture_git_source, export_spool_archive, inspect_spool, reconcile_configured_recipe,
    run_configured_recipe, run_configured_recipe_with_cancellation, run_observed_command,
};
use agentscm::change_record::{
    ActionFinishV1, ActionKindV1, ActionOutcomeV1, ActionStartV1, AgentHarnessV1, ArtifactMediaV1,
    AssuranceBasisV1, CaptureBatchV1, CaptureEnvelopeV1, CaptureModeV1, CapturedEventKindV1,
    CapturedEventV1, CheckResultV1, ContentAvailabilityV1, CoverageV1, DriftV1, GapReasonV1,
    RecipeProvenanceV1, RetentionClassV1, SCHEMA_VERSION_V1, SessionIdentityV1, SourceBindingV1,
    ToolCompletionV1,
};
use sha2::{Digest, Sha256};
use tempfile::TempDir;

use agentscm::execution::Cancellation;
use agentscm::hub::insights::{
    AttemptOutcome, AttemptPhase, UsageBasis, UsageMeasurementScope, UsageRecord,
};

fn capture_request(mode: CaptureModeV1) -> CaptureRequest {
    CaptureRequest {
        operation_prefix: "capture:fixture".into(),
        work_id: "wrk_fixture".into(),
        generation: 1,
        observed_session_id: "session_fixture".into(),
        canonical_session_id: "session_fixture".into(),
        parent_session_id: None,
        capture_mode: mode,
        live_capture_mode: mode,
    }
}

#[test]
fn spool_v4_initialization_is_atomic_quota_counted_and_safe_for_cooperative_openers() {
    let temp = TempDir::new().unwrap();
    let too_small = temp.path().join("too-small");
    assert!(matches!(
        LocalSpool::open_with_limits(
            &too_small,
            SpoolLimits {
                max_records: 5,
                max_total_bytes: 4096,
                max_artifact_bytes: 4096,
            },
        ),
        Err(agentscm::change_capture::CaptureError::Limit(_))
    ));
    assert!(!too_small.exists());

    let raced = temp.path().join("raced");
    let barrier = Arc::new(Barrier::new(3));
    let open = || {
        let barrier = barrier.clone();
        let raced = raced.clone();
        thread::spawn(move || {
            barrier.wait();
            LocalSpool::open_with_limits(
                raced,
                SpoolLimits {
                    max_records: 6,
                    max_total_bytes: 4096,
                    max_artifact_bytes: 4096,
                },
            )
        })
    };
    let first = open();
    let second = open();
    barrier.wait();
    first.join().unwrap().unwrap();
    second.join().unwrap().unwrap();
    for name in [
        "spool.lock",
        "managed-capture.lock",
        "capture-spool-v4.json",
        "compaction-v1.json",
        "order-v1.json",
        "observation-scope-v1.json",
    ] {
        assert!(raced.join(name).is_file(), "missing fixed record {name}");
    }
    let lowered = LocalSpool::open_with_limits(
        &raced,
        SpoolLimits {
            max_records: 5,
            max_total_bytes: 4096,
            max_artifact_bytes: 4096,
        },
    )
    .unwrap();
    assert!(matches!(
        lowered.retain_artifact(
            b"fixed metadata already exceeds the lowered write cap",
            ArtifactMediaV1::Binary,
            RetentionClassV1::WorkLocal,
        ),
        Err(agentscm::change_capture::CaptureError::Limit(_))
    ));
}

#[test]
fn incomplete_or_legacy_nonempty_spool_refuses_without_migration() {
    let temp = TempDir::new().unwrap();
    let root = temp.path().join("partial");
    fs::create_dir(&root).unwrap();
    fs::set_permissions(&root, fs::Permissions::from_mode(0o700)).unwrap();
    let lock = root.join("spool.lock");
    fs::write(&lock, []).unwrap();
    fs::set_permissions(&lock, fs::Permissions::from_mode(0o600)).unwrap();
    fs::write(root.join("managed-occurrence-v2.json"), br#"{"version":2}"#).unwrap();
    fs::write(
        root.join("legacy-observed-native-start.json"),
        b"opaque legacy fact",
    )
    .unwrap();
    let snapshot = || {
        let mut values: Vec<_> = fs::read_dir(&root)
            .unwrap()
            .map(|entry| {
                let path = entry.unwrap().path();
                (
                    path.file_name().unwrap().to_owned(),
                    fs::read(&path).unwrap(),
                )
            })
            .collect();
        values.sort_by(|left, right| left.0.cmp(&right.0));
        values
    };
    let before = snapshot();
    assert!(matches!(
        LocalSpool::open(&root),
        Err(agentscm::change_capture::CaptureError::Conflict(_))
    ));
    assert_eq!(snapshot(), before);
    assert!(!root.join("capture-spool-v4.json").exists());
    assert!(!root.join("managed-capture.lock").exists());
}

#[test]
fn v4_identity_expected_open_allows_offline_move_and_refuses_replacement_before_mutation() {
    let temp = TempDir::new().unwrap();
    let original = temp.path().join("original");
    let moved = temp.path().join("moved");
    let spool = LocalSpool::open(&original).unwrap();
    let identity = spool.identity();
    drop(spool);
    fs::rename(&original, &moved).unwrap();
    let reopened =
        LocalSpool::open_with_limits_expected(&moved, SpoolLimits::default(), identity).unwrap();
    assert_eq!(reopened.identity(), identity);
    drop(reopened);

    let displaced = temp.path().join("displaced");
    fs::rename(&moved, &displaced).unwrap();
    let replacement = LocalSpool::open(&moved).unwrap();
    assert_ne!(replacement.identity(), identity);
    drop(replacement);
    let mut before = retained_bytes(&moved);
    before.sort();
    assert!(matches!(
        LocalSpool::open_with_limits_expected(&moved, SpoolLimits::default(), identity),
        Err(agentscm::change_capture::CaptureError::Conflict(_))
    ));
    let mut after = retained_bytes(&moved);
    after.sort();
    assert_eq!(after, before);
}

#[test]
fn inspect_and_export_are_bounded_read_only_and_nonresumable() {
    let temp = TempDir::new().unwrap();
    let root = temp.path().join("spool");
    let spool = LocalSpool::open(&root).unwrap();
    let envelope = sample_batch("inspect-operation", "inspect-event");
    spool.append(&envelope).unwrap();
    let mut before = retained_bytes(&root);
    before.sort();

    let report = inspect_spool(&root).unwrap();
    assert_eq!(report.mode, "inspect_only");
    assert_eq!(
        report.classification,
        SpoolClassificationV1::V4IdentityMatch
    );
    assert_eq!(report.committed_identity, Some(spool.identity()));
    assert!(!report.resumable);
    assert_eq!(report.authority, "none");
    assert_eq!(report.completeness, "unknown");
    assert_eq!(report.pending_count.present, 1);
    assert_eq!(report.pending.len(), 1);
    assert_eq!(report.attempt_count.present, 0);
    assert_eq!(report.completeness, "unknown");
    let mut after_inspect = retained_bytes(&root);
    after_inspect.sort();
    assert_eq!(after_inspect, before);

    let destination = temp.path().join("archive");
    let manifest = export_spool_archive(&root, &destination).unwrap();
    assert!(!manifest.resumable);
    assert_eq!(manifest.authority, "none");
    assert_eq!(manifest.completeness, "unknown");
    assert!(
        destination
            .join("capture-export-manifest-v1.json")
            .is_file()
    );
    let mut after_export = retained_bytes(&root);
    after_export.sort();
    assert_eq!(after_export, before);
}

#[test]
fn private_authority_modes_block_mutation_but_not_read_only_inspection() {
    let temp = TempDir::new().unwrap();
    let unsafe_empty = temp.path().join("unsafe-empty");
    fs::create_dir(&unsafe_empty).unwrap();
    fs::set_permissions(&unsafe_empty, fs::Permissions::from_mode(0o755)).unwrap();
    assert!(matches!(
        LocalSpool::open(&unsafe_empty),
        Err(agentscm::change_capture::CaptureError::Conflict(message))
            if message.starts_with("spool_private_permissions:")
    ));
    assert_eq!(fs::metadata(&unsafe_empty).unwrap().mode() & 0o777, 0o755);
    assert_eq!(fs::read_dir(&unsafe_empty).unwrap().count(), 0);

    let root = temp.path().join("spool");
    let spool = LocalSpool::open(&root).unwrap();
    spool
        .append(&sample_batch("private-mode", "private-mode-event"))
        .unwrap();
    let outbox = root.join("outbox");
    fs::set_permissions(&outbox, fs::Permissions::from_mode(0o770)).unwrap();
    fs::set_permissions(&root, fs::Permissions::from_mode(0o770)).unwrap();
    let mut before = retained_bytes(&root);
    before.sort();

    let inspection = inspect_spool(&root).unwrap();
    assert_eq!(inspection.mode, "inspect_only");
    assert_eq!(inspection.authority, "none");
    assert!(matches!(
        LocalSpool::open(&root),
        Err(agentscm::change_capture::CaptureError::Conflict(message))
            if message.starts_with("spool_private_permissions:")
    ));
    assert!(matches!(
        spool.append(&sample_batch("unsafe-write", "unsafe-write-event")),
        Err(agentscm::change_capture::CaptureError::Conflict(message))
            if message.starts_with("spool_private_permissions:")
    ));
    let mut after = retained_bytes(&root);
    after.sort();
    assert_eq!(after, before);

    fs::set_permissions(&outbox, fs::Permissions::from_mode(0o700)).unwrap();
    fs::set_permissions(&root, fs::Permissions::from_mode(0o700)).unwrap();

    let retained = fs::read_dir(&outbox)
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    fs::set_permissions(&retained, fs::Permissions::from_mode(0o640)).unwrap();
    assert_eq!(inspect_spool(&root).unwrap().authority, "none");
    assert!(matches!(
        LocalSpool::open(&root),
        Err(agentscm::change_capture::CaptureError::Conflict(message))
            if message.starts_with("spool_private_permissions:")
    ));
    assert!(matches!(
        spool.pending(10),
        Err(agentscm::change_capture::CaptureError::Conflict(message))
            if message.starts_with("spool_private_permissions:")
    ));
    fs::set_permissions(retained, fs::Permissions::from_mode(0o600)).unwrap();
}

#[test]
fn export_refuses_nested_and_oversized_entries_without_touching_source() {
    let temp = TempDir::new().unwrap();
    let root = temp.path().join("spool");
    let spool = LocalSpool::open(&root).unwrap();
    spool
        .append(&sample_batch("export-refusal", "export-refusal-event"))
        .unwrap();
    let mut before = retained_bytes(&root);
    before.sort();
    let nested = root.join("nested-export");
    assert!(matches!(
        export_spool_archive(&root, &nested),
        Err(agentscm::change_capture::CaptureError::Invalid(message))
            if message.contains("must not be nested")
    ));
    assert!(!nested.exists());
    let mut after_nested = retained_bytes(&root);
    after_nested.sort();
    assert_eq!(after_nested, before);

    let oversized = root.join("artifacts").join("oversized-inspection-entry");
    let file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&oversized)
        .unwrap();
    file.set_len(64 * 1024 * 1024 + 1).unwrap();
    drop(file);
    let outside = temp.path().join("outside-export");
    assert!(matches!(
        export_spool_archive(&root, &outside),
        Err(agentscm::change_capture::CaptureError::Limit(message))
            if message.contains("exact-copy digest bound")
    ));
    assert!(!outside.exists());
    assert_eq!(fs::metadata(oversized).unwrap().len(), 64 * 1024 * 1024 + 1);
}

#[test]
fn spool_limit_fails_without_evicting_retained_evidence() {
    let temp = TempDir::new().unwrap();
    let spool = LocalSpool::open_with_limits(
        temp.path().join("spool"),
        SpoolLimits {
            max_records: 16,
            max_total_bytes: 4096,
            max_artifact_bytes: 4096,
        },
    )
    .unwrap();
    let first = spool
        .retain_artifact(
            &vec![b'a'; 1800],
            ArtifactMediaV1::Binary,
            RetentionClassV1::WorkLocal,
        )
        .unwrap();
    assert!(
        spool
            .retain_artifact(
                &vec![b'b'; 1800],
                ArtifactMediaV1::Binary,
                RetentionClassV1::WorkLocal,
            )
            .is_err()
    );
    assert_eq!(spool.read_artifact(&first).unwrap(), vec![b'a'; 1800]);
}

fn sample_batch(operation_id: &str, event_id: &str) -> CaptureEnvelopeV1 {
    CaptureEnvelopeV1::EventBatch(CaptureBatchV1 {
        schema_version: SCHEMA_VERSION_V1,
        operation_id: operation_id.into(),
        work_id: "wrk_fixture".into(),
        generation: 1,
        session: SessionIdentityV1 {
            harness: AgentHarnessV1::ClaudeCode,
            adapter_version: "1.0.0".into(),
            session_id: "session_fixture".into(),
            parent_session_id: None,
            capture_mode: CaptureModeV1::MetadataOnly,
        },
        events: vec![CapturedEventV1 {
            event_id: event_id.into(),
            sequence: 1,
            observed_at: 1,
            kind: CapturedEventKindV1::SessionEnded,
            tool: None,
            artifact: None,
            setup: None,
            gap: None,
            boundary: None,
            subagent: None,
        }],
    })
}

fn sample_action_start(work_id: &str, operation_id: &str) -> CaptureEnvelopeV1 {
    CaptureEnvelopeV1::ActionStart(ActionStartV1 {
        schema_version: SCHEMA_VERSION_V1,
        operation_id: operation_id.into(),
        work_id: work_id.into(),
        generation: 1,
        session_id: "session_fixture".into(),
        action_id: "shared_action_id".into(),
        source: SourceBindingV1::Native {
            repository: "example/repository".into(),
            revision: "3".repeat(64),
            change_id: "change_fixture".into(),
            context_digest: "4".repeat(64),
            source_generation: 1,
        },
        kind: ActionKindV1::Execution,
        recipe_provenance: RecipeProvenanceV1::ObservedNativeInput,
        recipe_digest: "5".repeat(64),
        setup_digest: "6".repeat(64),
        started_at: 1,
        observed_order: 1,
        observation_scope_id: Some("scope-fixture".into()),
    })
}

fn sample_action_finish(work_id: &str, operation_id: &str) -> CaptureEnvelopeV1 {
    CaptureEnvelopeV1::ActionFinish(ActionFinishV1 {
        schema_version: SCHEMA_VERSION_V1,
        operation_id: operation_id.into(),
        work_id: work_id.into(),
        generation: 1,
        action_id: "shared_action_id".into(),
        outcome: ActionOutcomeV1::Succeeded,
        result: CheckResultV1::Unknown,
        assurance: AssuranceBasisV1::CallerAsserted,
        coverage: CoverageV1::Partial,
        finished_at: 2,
        drift: DriftV1::Unknown,
        output_artifacts: Vec::new(),
    })
}

fn sample_usage(observation_id: &str) -> CaptureEnvelopeV1 {
    CaptureEnvelopeV1::Usage(UsageRecord {
        work_id: "wrk_11111111111111111111111111111111".into(),
        observation_id: observation_id.into(),
        generation: 1,
        session: "session_fixture".into(),
        phase: AttemptPhase::Test,
        outcome: AttemptOutcome::Succeeded,
        agent: "claude".into(),
        provider: "unknown".into(),
        model: "claude-haiku-4-5".into(),
        usage_basis: UsageBasis::ProviderResult,
        source_commit: None,
        input_tokens: Some(7),
        output_tokens: Some(11),
        cache_read_input_tokens: Some(3),
        cache_creation_input_tokens: Some(2),
        action_id: None,
        attempt_id: None,
        provider_request_id: Some("request_fixture".into()),
        provider_event_id: None,
        measurement_scope: Some(UsageMeasurementScope::Request),
        cost: None,
    })
}

#[test]
fn spool_replays_exact_bytes_and_rejects_operation_conflicts() {
    let temp = TempDir::new().unwrap();
    let spool = LocalSpool::open(temp.path().join("spool")).unwrap();
    let original = sample_batch("capture:one", "event:one");
    assert_eq!(spool.append(&original).unwrap(), AppendOutcome::Stored);
    assert_eq!(spool.append(&original).unwrap(), AppendOutcome::Duplicate);

    let conflicting = sample_batch("capture:one", "event:two");
    assert!(spool.append(&conflicting).is_err());
    assert_eq!(spool.pending(10).unwrap(), vec![original.clone()]);
    assert_eq!(spool.acknowledge(&original).unwrap(), AppendOutcome::Stored);
    assert!(spool.pending(10).unwrap().is_empty());

    let reopened = LocalSpool::open(temp.path().join("spool")).unwrap();
    assert_eq!(
        reopened.acknowledge(&original).unwrap(),
        AppendOutcome::Duplicate
    );
    assert_eq!(reopened.sequence_for("session", "event").unwrap(), 1);
    assert_eq!(reopened.sequence_for("session", "event").unwrap(), 1);
    assert_eq!(reopened.sequence_for("session", "next").unwrap(), 2);
}

#[test]
fn acknowledged_compaction_preserves_idempotency_and_completed_actions() {
    let temp = TempDir::new().unwrap();
    let spool = LocalSpool::open(temp.path().join("spool")).unwrap();
    let event = sample_batch("capture:compact", "event:compact");
    assert_eq!(spool.observed_order_for("start:compact").unwrap(), 1);
    let start = sample_action_start("wrk_fixture", "start:compact");
    let finish = sample_action_finish("wrk_fixture", "finish:compact");
    let observation = "d".repeat(64);
    assert_eq!(
        spool
            .bind_observation("finish:compact", &observation)
            .unwrap(),
        AppendOutcome::Stored
    );
    for envelope in [&event, &start, &finish] {
        spool.append(envelope).unwrap();
        spool.acknowledge(envelope).unwrap();
    }
    let report = spool.compact_acknowledged(10).unwrap();
    assert_eq!(report.envelopes, 3);
    assert!(report.bytes_reclaimed > 0);
    assert_eq!(spool.observed_order_for("start:compact").unwrap(), 1);
    assert!(
        !fs::read_to_string(spool.root().join("order-v1.json"))
            .unwrap()
            .contains("start:compact")
    );
    assert!(spool.pending(10).unwrap().is_empty());
    assert_eq!(spool.append(&event).unwrap(), AppendOutcome::Duplicate);
    assert_eq!(
        spool
            .bind_observation("finish:compact", &observation)
            .unwrap(),
        AppendOutcome::Duplicate
    );
    assert!(
        spool
            .bind_observation("finish:compact", &"e".repeat(64))
            .is_err()
    );
    assert_eq!(
        spool.action_start("start:compact").unwrap(),
        match start {
            CaptureEnvelopeV1::ActionStart(start) => Some(start),
            _ => unreachable!(),
        }
    );
    assert_eq!(
        spool.action_finish("finish:compact").unwrap(),
        match finish {
            CaptureEnvelopeV1::ActionFinish(finish) => Some(finish),
            _ => unreachable!(),
        }
    );

    let incomplete = sample_action_start("wrk_other", "start:incomplete");
    spool.append(&incomplete).unwrap();
    spool.acknowledge(&incomplete).unwrap();
    assert_eq!(spool.compact_acknowledged(10).unwrap().envelopes, 0);
    assert!(spool.action_start("start:incomplete").unwrap().is_some());
}

#[test]
fn acknowledged_usage_is_not_pending_and_compacts_with_replay_history() {
    let temp = TempDir::new().unwrap();
    let root = temp.path().join("spool");
    let spool = LocalSpool::open(&root).unwrap();
    let usage = sample_usage("usage_fixture");
    assert_eq!(spool.append(&usage).unwrap(), AppendOutcome::Stored);
    assert_eq!(spool.acknowledge(&usage).unwrap(), AppendOutcome::Stored);

    let inspection = inspect_spool(&root).unwrap();
    assert_eq!(inspection.pending_count.present, 0);
    assert!(inspection.pending.is_empty());
    assert_eq!(inspection.acknowledged_count.present, 1);
    assert!(spool.pending(10).unwrap().is_empty());
    assert!(spool.delivery_page(10).unwrap().is_empty());
    assert_eq!(spool.acknowledge(&usage).unwrap(), AppendOutcome::Duplicate);

    let outbox = fs::read_dir(root.join("outbox"))
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    let acknowledgement = fs::read_dir(root.join("acknowledged"))
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    let outbox_bytes = fs::read(&outbox).unwrap();
    let acknowledgement_bytes = fs::read(&acknowledgement).unwrap();
    assert_eq!(spool.compact_acknowledged(10).unwrap().envelopes, 1);

    // Recover the exact state left by an interruption after the compaction index became durable
    // but before either acknowledged Usage file was removed.
    write_private_file(&outbox, &outbox_bytes);
    write_private_file(&acknowledgement, &acknowledgement_bytes);
    let reopened = LocalSpool::open(&root).unwrap();
    assert!(reopened.pending(10).unwrap().is_empty());
    assert_eq!(reopened.compact_acknowledged(10).unwrap().envelopes, 0);
    assert_eq!(reopened.append(&usage).unwrap(), AppendOutcome::Duplicate);

    let mut conflicting = usage.clone();
    let CaptureEnvelopeV1::Usage(conflicting_usage) = &mut conflicting else {
        unreachable!()
    };
    conflicting_usage.input_tokens = Some(8);
    assert!(reopened.append(&conflicting).is_err());
}

#[test]
fn inspection_and_compaction_do_not_trust_a_corrupt_acknowledgement() {
    let temp = TempDir::new().unwrap();
    let root = temp.path().join("spool");
    let spool = LocalSpool::open(&root).unwrap();
    let usage = sample_usage("usage_corrupt_ack");
    spool.append(&usage).unwrap();
    spool.acknowledge(&usage).unwrap();
    let acknowledgement = fs::read_dir(root.join("acknowledged"))
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    write_private_file(&acknowledgement, b"0");

    let inspection = inspect_spool(&root).unwrap();
    assert_eq!(inspection.pending_count.present, 1);
    assert_eq!(inspection.pending, vec![usage.clone()]);
    assert!(
        inspection
            .issues
            .iter()
            .any(|issue| issue == "invalid acknowledgement marker")
    );
    assert!(spool.pending(10).is_err());
    assert!(spool.delivery_page(10).is_err());
    assert!(spool.acknowledge(&usage).is_err());
    assert!(spool.compact_acknowledged(10).is_err());
    assert_eq!(fs::read_dir(root.join("outbox")).unwrap().count(), 1);
}

#[test]
fn runtime_does_not_trust_nonregular_acknowledgements() {
    for marker_kind in ["symlink", "directory"] {
        let temp = TempDir::new().unwrap();
        let root = temp.path().join(marker_kind);
        let spool = LocalSpool::open(&root).unwrap();
        let usage = sample_usage(&format!("usage_{marker_kind}_ack"));
        spool.append(&usage).unwrap();
        spool.acknowledge(&usage).unwrap();
        let acknowledgement = fs::read_dir(root.join("acknowledged"))
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path();
        fs::remove_file(&acknowledgement).unwrap();
        if marker_kind == "symlink" {
            let outbox = fs::read_dir(root.join("outbox"))
                .unwrap()
                .next()
                .unwrap()
                .unwrap()
                .path();
            std::os::unix::fs::symlink(outbox, &acknowledgement).unwrap();
        } else {
            fs::create_dir(&acknowledgement).unwrap();
        }

        let inspection = inspect_spool(&root).unwrap();
        assert_eq!(inspection.pending_count.present, 1);
        assert_eq!(inspection.pending, vec![usage.clone()]);
        if marker_kind == "symlink" {
            assert!(
                inspection
                    .issues
                    .iter()
                    .any(|issue| issue == "invalid acknowledgement marker")
            );
        }
        assert!(spool.pending(10).is_err());
        assert!(spool.delivery_page(10).is_err());
        assert!(spool.acknowledge(&usage).is_err());
        assert!(spool.compact_acknowledged(10).is_err());
    }
}

#[test]
fn compaction_recovery_repairs_cursor_at_every_deletion_boundary() {
    for cut in 0..4 {
        let temp = TempDir::new().unwrap();
        let root = temp.path().join(format!("spool-{cut}"));
        let spool = LocalSpool::open(&root).unwrap();
        let event = sample_batch("capture:cursor-crash", "event:cursor-crash");
        spool.append(&event).unwrap();
        spool.advance_delivery_cursor(&event).unwrap();
        spool.acknowledge(&event).unwrap();

        let outbox = fs::read_dir(root.join("outbox"))
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path();
        let acknowledgement = fs::read_dir(root.join("acknowledged"))
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path();
        let outbox_bytes = fs::read(&outbox).unwrap();
        let acknowledgement_bytes = fs::read(&acknowledgement).unwrap();
        let cursor = root.join("delivery-cursor-v1.json");
        let cursor_bytes = fs::read(&cursor).unwrap();

        assert_eq!(spool.compact_acknowledged(10).unwrap().envelopes, 1);
        if cut == 0 {
            write_private_file(&outbox, &outbox_bytes);
        }
        if cut <= 1 {
            write_private_file(&acknowledgement, &acknowledgement_bytes);
        }
        if cut <= 2 {
            write_private_file(&cursor, &cursor_bytes);
        }

        let reopened = LocalSpool::open(&root).unwrap();
        assert!(reopened.delivery_page(10).unwrap().is_empty());
        assert!(reopened.pending(10).unwrap().is_empty());
        assert_eq!(reopened.compact_acknowledged(10).unwrap().envelopes, 0);
        assert!(!cursor.exists());
    }
}

#[test]
fn observed_action_order_is_monotonic_and_replay_stable() {
    let temp = TempDir::new().unwrap();
    let root = temp.path().join("spool");
    let spool = LocalSpool::open(&root).unwrap();
    assert_eq!(spool.observed_order_for("native:start:first").unwrap(), 1);
    assert_eq!(spool.observed_order_for("native:start:first").unwrap(), 1);
    assert_eq!(spool.observed_order_for("native:start:second").unwrap(), 2);
    let reopened = LocalSpool::open(&root).unwrap();
    assert_eq!(
        reopened.observed_order_for("native:start:first").unwrap(),
        1
    );
    assert_eq!(
        reopened.observed_order_for("native:start:third").unwrap(),
        3
    );
}

#[test]
fn observation_scope_is_stable_per_spool_and_distinct_between_spools() {
    let temp = TempDir::new().unwrap();
    let first_root = temp.path().join("first");
    let first = LocalSpool::open(&first_root).unwrap();
    let first_scope = first.observation_scope_id().unwrap();
    assert_eq!(first_scope.len(), 32);
    assert_eq!(
        LocalSpool::open(&first_root)
            .unwrap()
            .observation_scope_id()
            .unwrap(),
        first_scope
    );
    let second_scope = LocalSpool::open(temp.path().join("second"))
        .unwrap()
        .observation_scope_id()
        .unwrap();
    assert_ne!(second_scope, first_scope);
}

#[test]
fn delivery_cursor_traverses_multiple_blocked_pages_across_restart() {
    let temp = TempDir::new().unwrap();
    let root = temp.path().join("spool");
    let spool = LocalSpool::open(&root).unwrap();
    for index in 0..25 {
        spool
            .append(&sample_batch(
                &format!("capture:{index:03}"),
                &format!("event:{index:03}"),
            ))
            .unwrap();
    }

    let first = spool.delivery_page(10).unwrap();
    assert_eq!(first.len(), 10);
    for envelope in &first {
        spool.advance_delivery_cursor(envelope).unwrap();
    }
    let reopened = LocalSpool::open(&root).unwrap();
    let second = reopened.delivery_page(10).unwrap();
    assert_eq!(second.len(), 10);
    assert!(second.iter().all(|value| !first.contains(value)));
    for envelope in &second {
        reopened.advance_delivery_cursor(envelope).unwrap();
    }
    let third = reopened.delivery_page(10).unwrap();
    assert_eq!(third.len(), 10);
    assert_eq!(
        third[..5]
            .iter()
            .filter(|value| !first.contains(value) && !second.contains(value))
            .count(),
        5
    );
    for envelope in third.iter().take(5) {
        reopened.advance_delivery_cursor(envelope).unwrap();
    }
    assert_eq!(reopened.delivery_page(10).unwrap(), first);
    assert_eq!(reopened.pending(100).unwrap().len(), 25);
}

#[test]
fn delivery_cursor_rotates_an_unacknowledged_transient_attempt() {
    let temp = TempDir::new().unwrap();
    let root = temp.path().join("spool");
    let spool = LocalSpool::open(&root).unwrap();
    for index in 0..3 {
        spool
            .append(&sample_batch(
                &format!("transient:{index:03}"),
                &format!("transient-event:{index:03}"),
            ))
            .unwrap();
    }
    let initial = spool.delivery_page(3).unwrap();
    spool.advance_delivery_cursor(&initial[0]).unwrap();

    let reopened = LocalSpool::open(&root).unwrap();
    let rotated = reopened.delivery_page(3).unwrap();
    assert_eq!(
        rotated,
        vec![initial[1].clone(), initial[2].clone(), initial[0].clone()]
    );
    assert_eq!(reopened.pending(10).unwrap(), initial);
}

#[test]
fn finish_capacity_is_reserved_per_work_generation_and_action() {
    let temp = TempDir::new().unwrap();
    let spool = LocalSpool::open_with_limits(
        temp.path().join("spool"),
        SpoolLimits {
            max_records: 256,
            max_total_bytes: 64 * 1024,
            max_artifact_bytes: 4096,
        },
    )
    .unwrap();
    spool
        .append(&sample_action_start("wrk_first", "start:first"))
        .unwrap();
    spool
        .append(&sample_action_start("wrk_second", "start:second"))
        .unwrap();

    let mut fill_index = 0_u32;
    loop {
        let mut bytes = format!("before-first-finish-{fill_index}").into_bytes();
        bytes.resize(2048, b'a');
        if spool
            .retain_artifact(&bytes, ArtifactMediaV1::Binary, RetentionClassV1::WorkLocal)
            .is_err()
        {
            break;
        }
        fill_index += 1;
    }
    spool
        .append(&sample_action_finish("wrk_first", "finish:first"))
        .unwrap();
    loop {
        let mut bytes = format!("after-first-finish-{fill_index}").into_bytes();
        bytes.resize(2048, b'b');
        if spool
            .retain_artifact(&bytes, ArtifactMediaV1::Binary, RetentionClassV1::WorkLocal)
            .is_err()
        {
            break;
        }
        fill_index += 1;
    }

    assert_eq!(
        spool
            .append(&sample_action_finish("wrk_second", "finish:second"))
            .unwrap(),
        AppendOutcome::Stored
    );
}

#[test]
fn spool_namespace_and_deferred_hooks_are_bounded_and_never_reassociated() {
    let repo = git_fixture();
    let temp = TempDir::new().unwrap();
    let spool = LocalSpool::open(temp.path().join("spool")).unwrap();
    let namespace = SpoolNamespaceV1 {
        version: 1,
        hub: "https://hub.example.invalid".into(),
        repository: "example/repository".into(),
        worktree_id: "worktree_fixture".into(),
    };
    assert_eq!(
        spool.bind_namespace(namespace.clone()).unwrap(),
        NamespaceBindOutcome::Stored
    );
    assert_eq!(
        spool.bind_namespace(namespace).unwrap(),
        NamespaceBindOutcome::Duplicate
    );
    assert_eq!(
        spool
            .bind_namespace(SpoolNamespaceV1 {
                version: 1,
                hub: "https://other.example.invalid".into(),
                repository: "example/repository".into(),
                worktree_id: "worktree_fixture".into(),
            })
            .unwrap(),
        NamespaceBindOutcome::Conflict
    );

    let rules = vec![ActionRuleV1 {
        native_tool: "Bash".into(),
        kind: ActionKindV1::Execution,
        coverage: CoverageV1::Partial,
        result_source: NativeResultSourceV1::ProcessExitCode,
        recipe_digest: None,
    }];
    let prompt = br#"{"hook_event_name":"UserPromptSubmit","session_id":"session_fixture","prompt":"DEFERRED_PRIVATE_884"}"#;
    assert_eq!(
        deferred_native_hook(
            prompt,
            DeferredReasonV1::NoCurrentAuthorizedWork,
            &ActionCaptureConfig {
                source: git_request(repo.path()),
                setup_digest: "0".repeat(64),
                managed_invocations: Vec::new(),
                rules: rules.clone(),
            },
            &spool,
        )
        .unwrap(),
        AppendOutcome::Stored
    );
    assert_eq!(
        deferred_native_hook(
            prompt,
            DeferredReasonV1::NoCurrentAuthorizedWork,
            &ActionCaptureConfig {
                source: git_request(repo.path()),
                setup_digest: "0".repeat(64),
                managed_invocations: Vec::new(),
                rules: rules.clone(),
            },
            &spool,
        )
        .unwrap(),
        AppendOutcome::Stored
    );

    let pre = br#"{"hook_event_name":"PreToolUse","session_id":"session_fixture","tool_name":"Bash","tool_use_id":"deferred_tool","tool_input":{"command":"DEFERRED_COMMAND_885"}}"#;
    assert_eq!(
        deferred_native_hook(
            pre,
            DeferredReasonV1::TargetLookupUnavailable,
            &ActionCaptureConfig {
                source: git_request(repo.path()),
                setup_digest: "0".repeat(64),
                managed_invocations: Vec::new(),
                rules: rules.clone(),
            },
            &spool,
        )
        .unwrap(),
        AppendOutcome::Stored
    );
    assert_eq!(
        deferred_native_hook(
            pre,
            DeferredReasonV1::TargetLookupUnavailable,
            &ActionCaptureConfig {
                source: git_request(repo.path()),
                setup_digest: "0".repeat(64),
                managed_invocations: Vec::new(),
                rules: rules.clone(),
            },
            &spool,
        )
        .unwrap(),
        AppendOutcome::Duplicate
    );
    let changed_pre = br#"{"hook_event_name":"PreToolUse","session_id":"session_fixture","tool_name":"Bash","tool_use_id":"deferred_tool","tool_input":{"command":"DIFFERENT_COMMAND_886"}}"#;
    assert!(
        deferred_native_hook(
            changed_pre,
            DeferredReasonV1::TargetLookupUnavailable,
            &ActionCaptureConfig {
                source: git_request(repo.path()),
                setup_digest: "0".repeat(64),
                managed_invocations: Vec::new(),
                rules: rules.clone(),
            },
            &spool,
        )
        .is_err()
    );
    let deferred = spool.deferred(10).unwrap();
    assert_eq!(deferred.len(), 3);
    let action = deferred
        .iter()
        .find(|record| record.hook_event_name == "PreToolUse")
        .unwrap();
    assert!(action.source.is_some());
    assert!(action.recipe_digest.is_some());
    assert_eq!(action.action_kind, Some(ActionKindV1::Execution));
    assert!(action.observed_at > 1_700_000_000);
    let sanitized = serde_json::to_string(&deferred).unwrap();
    assert!(!sanitized.contains("DEFERRED_PRIVATE_884"));
    assert!(!sanitized.contains("DEFERRED_COMMAND_885"));
    assert!(spool.pending(10).unwrap().is_empty());
}

#[test]
fn deferred_listing_uses_durable_receipt_order_when_provider_time_is_missing() {
    let temp = TempDir::new().unwrap();
    let root = temp.path().join("spool");
    let spool = LocalSpool::open(&root).unwrap();
    let timed = DeferredHookV1 {
        occurrence_id: "evt:timed".into(),
        reserved_sequence: Some(1),
        session_id: "session_fixture".into(),
        hook_event_name: "SessionStart".into(),
        observed_at: 1_782_284_400,
        reason: DeferredReasonV1::TargetLookupUnavailable,
        input_digest: "1".repeat(64),
        event_kind: Some(CapturedEventKindV1::SessionStarted),
        tool_kind: None,
        source: None,
        recipe_digest: None,
        action_kind: None,
    };
    let untimed = DeferredHookV1 {
        occurrence_id: "evt:untimed".into(),
        reserved_sequence: Some(2),
        observed_at: 0,
        input_digest: "2".repeat(64),
        ..timed.clone()
    };
    assert_eq!(
        spool.retain_deferred(&timed).unwrap(),
        AppendOutcome::Stored
    );
    assert_eq!(
        spool.retain_deferred(&untimed).unwrap(),
        AppendOutcome::Stored
    );
    assert_eq!(
        spool.retain_deferred(&timed).unwrap(),
        AppendOutcome::Duplicate
    );

    let reopened = LocalSpool::open(&root).unwrap();
    let deferred = reopened.deferred(10).unwrap();
    assert_eq!(deferred, vec![timed, untimed]);
    assert_eq!(deferred[1].observed_at, 0);
}

fn pending_event_times(spool: &LocalSpool) -> Vec<(String, i64)> {
    spool
        .pending(20)
        .unwrap()
        .iter()
        .filter_map(|envelope| match envelope {
            CaptureEnvelopeV1::EventBatch(batch) => Some(batch.events.iter()),
            _ => None,
        })
        .flatten()
        .map(|event| (event.event_id.clone(), event.observed_at))
        .collect()
}

#[test]
fn hook_without_timestamp_uses_receipt_time_and_keeps_it_on_rerun() {
    let temp = TempDir::new().unwrap();
    let spool = LocalSpool::open(temp.path().join("spool")).unwrap();
    let request = capture_request(CaptureModeV1::MetadataOnly);
    let session = &request.observed_session_id;
    let untimed = format!(
        r#"{{"hook_event_name":"PreToolUse","session_id":"{session}","tool_name":"Read","tool_use_id":"read_1","tool_input":{{}}}}"#
    );
    let timed = format!(
        r#"{{"hook_event_name":"PostToolUse","session_id":"{session}","tool_name":"Read","tool_use_id":"read_1","tool_input":{{}},"tool_response":{{}},"timestamp":"2026-10-24T10:00:00Z"}}"#
    );
    let before = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64;
    capture_hook(untimed.as_bytes(), &request, &spool).unwrap();
    capture_hook(timed.as_bytes(), &request, &spool).unwrap();
    let first = pending_event_times(&spool);
    let received = first.iter().find(|(_, at)| *at != 1_792_836_000).unwrap().1;
    assert!(received >= before && received <= before + 60, "{first:?}");
    assert!(
        first.iter().any(|(_, at)| *at == 1_792_836_000),
        "{first:?}"
    );

    // A later re-run is the same identity and the same retained bytes: no conflict, no new event.
    std::thread::sleep(std::time::Duration::from_millis(1100));
    capture_hook(untimed.as_bytes(), &request, &spool).unwrap();
    assert_eq!(pending_event_times(&spool), first);
}

fn acknowledge_and_compact(spool: &LocalSpool) {
    for envelope in spool.pending(100).unwrap() {
        spool.acknowledge(&envelope).unwrap();
    }
    spool.compact_acknowledged(100).unwrap();
}

#[test]
fn resumed_session_hooks_survive_spool_compaction_and_receipt_time_changes() {
    let temp = TempDir::new().unwrap();
    let spool = LocalSpool::open(temp.path().join("spool")).unwrap();
    let request = capture_request(CaptureModeV1::MetadataOnly);
    let session = &request.observed_session_id;
    let hook = |tool: &str, id: &str| {
        format!(
            r#"{{"hook_event_name":"PreToolUse","session_id":"{session}","tool_name":"{tool}","tool_use_id":"{id}","tool_input":{{}}}}"#
        )
    };
    // No startup SessionStart, so every hook re-evaluates the adapter-started-late marker.
    let first = hook("Read", "read_1");
    assert_eq!(
        capture_hook(first.as_bytes(), &request, &spool).unwrap(),
        HookCaptureOutcome::Stored
    );
    acknowledge_and_compact(&spool);
    std::thread::sleep(Duration::from_millis(1100));

    let second = hook("Grep", "grep_1");
    assert_eq!(
        capture_hook(second.as_bytes(), &request, &spool).unwrap(),
        HookCaptureOutcome::Stored
    );
    assert_eq!(pending_event_times(&spool).len(), 1);
    // A stable-id event re-delivered after compaction is the original, not a conflict.
    assert_eq!(
        capture_hook(first.as_bytes(), &request, &spool).unwrap(),
        HookCaptureOutcome::Duplicate
    );
    assert_eq!(pending_event_times(&spool).len(), 1);
}

#[test]
fn adapter_started_late_marker_failure_is_reported_and_keeps_the_hook_event() {
    let repo = git_fixture();
    let temp = TempDir::new().unwrap();
    let spool = LocalSpool::open(temp.path().join("spool")).unwrap();
    let request = capture_request(CaptureModeV1::MetadataOnly);
    let actions = ActionCaptureConfig {
        source: git_request(&repo.path().canonicalize().unwrap()),
        setup_digest: "1".repeat(64),
        managed_invocations: Vec::new(),
        rules: Vec::new(),
    };
    let hook = |id: &str| {
        format!(
            r#"{{"hook_event_name":"PreToolUse","session_id":"session_fixture","tool_name":"Read","tool_use_id":"{id}","tool_input":{{}}}}"#
        )
    };
    let first = capture_native_hook(hook("read_1").as_bytes(), &request, &actions, &spool).unwrap();
    assert!(first.warnings.is_empty());
    // Corrupt the retained marker so the next attempt to re-evaluate it conflicts.
    let mut changed = 0;
    for entry in fs::read_dir(spool.root().join("outbox")).unwrap() {
        let path = entry.unwrap().path();
        let text = fs::read_to_string(&path).unwrap();
        if text.contains("adapter_started_late") {
            fs::write(
                &path,
                text.replace("adapter_started_late", "capture_interrupted"),
            )
            .unwrap();
            changed += 1;
        }
    }
    assert_eq!(changed, 1);
    let second =
        capture_native_hook(hook("read_2").as_bytes(), &request, &actions, &spool).unwrap();
    assert_eq!(second.event, HookCaptureOutcome::Stored);
    assert_eq!(second.warnings.len(), 1);
    assert!(second.warnings[0].contains("adapter-started-late marker not retained"));
}

#[test]
fn receipt_timed_redelivery_differing_only_in_time_is_a_duplicate() {
    let temp = TempDir::new().unwrap();
    let spool = LocalSpool::open(temp.path().join("spool")).unwrap();
    let request = capture_request(CaptureModeV1::MetadataOnly);
    let hook = br#"{"hook_event_name":"PreToolUse","session_id":"session_fixture","tool_name":"Read","tool_use_id":"read_1","tool_input":{}}"#;
    capture_hook(hook, &request, &spool).unwrap();
    // Simulate a concurrent hook that stored the same operation a second earlier.
    for entry in fs::read_dir(spool.root().join("outbox")).unwrap() {
        let path = entry.unwrap().path();
        let mut value: serde_json::Value =
            serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        for event in value["events"].as_array_mut().unwrap() {
            let at = event["observed_at"].as_i64().unwrap();
            event["observed_at"] = serde_json::json!(at - 1);
        }
        fs::write(&path, serde_json::to_vec(&value).unwrap()).unwrap();
    }
    let before = pending_event_times(&spool);
    assert_eq!(
        capture_hook(hook, &request, &spool).unwrap(),
        HookCaptureOutcome::Duplicate
    );
    assert_eq!(pending_event_times(&spool), before);
}

#[test]
fn concurrent_untimed_hooks_for_one_operation_store_one_event() {
    let temp = TempDir::new().unwrap();
    let spool = Arc::new(LocalSpool::open(temp.path().join("spool")).unwrap());
    let barrier = Arc::new(Barrier::new(6));
    let handles: Vec<_> = (0..6)
        .map(|_| {
            let (spool, barrier) = (spool.clone(), barrier.clone());
            thread::spawn(move || {
                let request = capture_request(CaptureModeV1::MetadataOnly);
                let hook = br#"{"hook_event_name":"PreToolUse","session_id":"session_fixture","tool_name":"Read","tool_use_id":"read_1","tool_input":{}}"#;
                barrier.wait();
                capture_hook(hook, &request, &spool)
            })
        })
        .collect();
    for handle in handles {
        handle.join().unwrap().unwrap();
    }
    assert_eq!(
        spool
            .pending(20)
            .unwrap()
            .iter()
            .filter(
                |envelope| matches!(envelope, CaptureEnvelopeV1::EventBatch(batch)
                if batch.events.iter().any(|event| event.kind == CapturedEventKindV1::ToolStarted))
            )
            .count(),
        1
    );
}

#[test]
fn deferred_alias_gap_keeps_original_target_across_resumed_generation() {
    let repo = git_fixture();
    let temp = TempDir::new().unwrap();
    let spool = LocalSpool::open(temp.path().join("spool")).unwrap();
    let mut original = capture_request(CaptureModeV1::MetadataOnly);
    original.observed_session_id = "native_session".into();
    original.canonical_session_id = "default".into();
    let actions = ActionCaptureConfig {
        source: git_request(&repo.path().canonicalize().unwrap()),
        setup_digest: "0".repeat(64),
        managed_invocations: Vec::new(),
        rules: vec![ActionRuleV1 {
            native_tool: "Bash".into(),
            kind: ActionKindV1::Execution,
            coverage: CoverageV1::Partial,
            result_source: NativeResultSourceV1::ProcessExitCode,
            recipe_digest: None,
        }],
    };
    let start = br#"{"hook_event_name":"SessionStart","session_id":"native_session","source":"startup","timestamp":"2026-10-24T10:00:00Z"}"#;
    capture_hook(start, &original, &spool).unwrap();

    let deferred = br#"{"hook_event_name":"PreToolUse","session_id":"native_session","tool_name":"Bash","tool_use_id":"outage_tool","tool_input":{"command":"printf private"}}"#;
    assert_eq!(
        deferred_native_hook(
            deferred,
            DeferredReasonV1::TargetLookupUnavailable,
            &actions,
            &spool,
        )
        .unwrap(),
        AppendOutcome::Stored
    );
    assert_eq!(
        deferred_native_hook(
            deferred,
            DeferredReasonV1::TargetLookupUnavailable,
            &actions,
            &spool,
        )
        .unwrap(),
        AppendOutcome::Duplicate
    );
    let deferred_page = spool.deferred(10).unwrap();
    assert_eq!(deferred_page.len(), 1);
    assert!(
        deferred_page[0]
            .reserved_sequence
            .is_some_and(|value| value > 0)
    );

    let mut resumed = original.clone();
    resumed.generation = 2;
    let end = br#"{"hook_event_name":"SessionEnd","session_id":"native_session","timestamp":"2026-10-24T10:00:02Z"}"#;
    capture_hook(end, &resumed, &spool).unwrap();
    capture_hook(end, &resumed, &spool).unwrap();

    let pending = spool.pending(20).unwrap();
    let gap_batches: Vec<_> = pending
        .iter()
        .filter_map(|envelope| match envelope {
            CaptureEnvelopeV1::EventBatch(batch)
                if batch.events.iter().any(|event| {
                    event.gap.as_ref().is_some_and(|gap| {
                        gap.reason == agentscm::change_record::GapReasonV1::CaptureInterrupted
                    })
                }) =>
            {
                Some(batch)
            }
            _ => None,
        })
        .collect();
    assert_eq!(gap_batches.len(), 1);
    assert_eq!(gap_batches[0].work_id, "wrk_fixture");
    assert_eq!(gap_batches[0].generation, 1);
    assert_eq!(gap_batches[0].session.session_id, "default");
    assert!(pending.iter().any(|envelope| matches!(
        envelope,
        CaptureEnvelopeV1::EventBatch(batch)
            if batch.generation == 2 && batch.session.session_id == "default"
    )));

    let wrong_observed = br#"{"hook_event_name":"SessionEnd","session_id":"default","timestamp":"2026-10-24T10:00:03Z"}"#;
    assert!(capture_hook(wrong_observed, &resumed, &spool).is_err());
}

#[test]
fn spool_recovers_owned_outbox_temps_before_and_after_rename() {
    let temp = TempDir::new().unwrap();
    let root = temp.path().join("spool");
    let spool = LocalSpool::open(&root).unwrap();
    let envelope = sample_batch("capture:crash", "event:crash");
    spool.append(&envelope).unwrap();
    let final_path = fs::read_dir(root.join("outbox"))
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    let final_name = final_path.file_name().unwrap().to_str().unwrap();
    let before_rename = root
        .join("outbox")
        .join(format!(".capture-{final_name}-999-1.tmp"));
    fs::rename(&final_path, &before_rename).unwrap();

    let reopened = LocalSpool::open(&root).unwrap();
    assert_eq!(reopened.pending(10).unwrap(), vec![envelope.clone()]);
    assert!(final_path.exists());
    assert!(!before_rename.exists());

    let after_rename = root
        .join("outbox")
        .join(format!(".capture-{final_name}-999-2.tmp"));
    fs::copy(&final_path, &after_rename).unwrap();
    assert_eq!(reopened.pending(10).unwrap(), vec![envelope]);
    assert!(!after_rename.exists());

    let malformed = root.join("outbox").join(".capture-unrecoverable.tmp");
    fs::write(&malformed, b"partial").unwrap();
    fs::set_permissions(&malformed, fs::Permissions::from_mode(0o600)).unwrap();
    reopened.pending(10).unwrap();
    assert!(!malformed.exists());
    assert_eq!(fs::read_dir(root.join("quarantine")).unwrap().count(), 1);
}

#[test]
fn spool_recovers_or_quarantines_deferred_temps_without_wedging_listing() {
    let temp = TempDir::new().unwrap();
    let root = temp.path().join("spool");
    let spool = LocalSpool::open(&root).unwrap();
    let deferred = DeferredHookV1 {
        occurrence_id: "evt:deferred-crash".into(),
        reserved_sequence: Some(
            spool
                .sequence_for("session_fixture", "evt:deferred-crash")
                .unwrap(),
        ),
        session_id: "session_fixture".into(),
        hook_event_name: "SessionEnd".into(),
        observed_at: 0,
        reason: DeferredReasonV1::TargetLookupUnavailable,
        input_digest: "a".repeat(64),
        event_kind: Some(CapturedEventKindV1::SessionEnded),
        tool_kind: None,
        source: None,
        recipe_digest: None,
        action_kind: None,
    };
    spool.retain_deferred(&deferred).unwrap();
    let final_path = fs::read_dir(root.join("deferred"))
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    let final_name = final_path.file_name().unwrap().to_str().unwrap();
    let interrupted = root
        .join("deferred")
        .join(format!(".capture-{final_name}-999-1.tmp"));
    fs::rename(&final_path, &interrupted).unwrap();

    let reopened = LocalSpool::open(&root).unwrap();
    assert_eq!(reopened.deferred(10).unwrap(), vec![deferred]);
    assert!(final_path.exists());
    assert!(!interrupted.exists());

    let malformed = root
        .join("deferred")
        .join(format!(".capture-{final_name}-999-2.tmp"));
    fs::write(&malformed, b"partial").unwrap();
    fs::set_permissions(&malformed, fs::Permissions::from_mode(0o600)).unwrap();
    let recovered = LocalSpool::open(&root).unwrap();
    assert_eq!(recovered.deferred(10).unwrap().len(), 1);
    assert!(!malformed.exists());
    assert_eq!(fs::read_dir(root.join("quarantine")).unwrap().count(), 1);
}

#[test]
fn quarantine_counts_toward_limits_and_requires_exact_explicit_pruning() {
    let temp = TempDir::new().unwrap();
    let root = temp.path().join("spool");
    let limits = SpoolLimits {
        // Six fixed v3 records + one artifact + two reserved health records is the smallest
        // deliberately usable limit for the post-prune admission below.
        max_records: 9,
        max_total_bytes: 64 * 1024,
        max_artifact_bytes: 4096,
    };
    LocalSpool::open_with_limits(&root, limits).unwrap();
    for index in 0..6 {
        let path = root
            .join("deferred")
            .join(format!(".capture-invalid-{index}.json-999-{index}.tmp"));
        fs::write(&path, format!("partial-{index}")).unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(0o600)).unwrap();
    }

    let reopened = LocalSpool::open_with_limits(&root, limits).unwrap();
    let quarantine = reopened.quarantine(100).unwrap();
    assert_eq!(quarantine.len(), 6);
    assert!(
        reopened
            .retain_artifact(
                b"quota is occupied by retained quarantine evidence",
                ArtifactMediaV1::Binary,
                RetentionClassV1::WorkLocal,
            )
            .is_err()
    );
    let names: Vec<_> = quarantine.into_iter().map(|entry| entry.name).collect();
    let report = reopened.prune_quarantine(&names).unwrap();
    assert_eq!(report.records, 6);
    assert!(report.bytes_reclaimed > 0);
    assert!(reopened.quarantine(100).unwrap().is_empty());
    assert!(
        reopened
            .retain_artifact(
                b"capture resumes only after explicit prune",
                ArtifactMediaV1::Binary,
                RetentionClassV1::WorkLocal,
            )
            .is_ok()
    );
}

#[test]
fn legacy_deferred_record_remains_unassociated_with_unknown_sequence() {
    let temp = TempDir::new().unwrap();
    let root = temp.path().join("spool");
    let spool = LocalSpool::open(&root).unwrap();
    let occurrence_id = "evt:legacy-deferred";
    let name = format!("{:x}.json", Sha256::digest(occurrence_id.as_bytes()));
    let legacy = serde_json::json!({
        "receipt_order": 1,
        "hook": {
            "occurrence_id": occurrence_id,
            "session_id": "session_fixture",
            "hook_event_name": "SessionEnd",
            "observed_at": 0,
            "reason": "target_lookup_unavailable",
            "input_digest": "a".repeat(64),
            "event_kind": "session_ended",
            "tool_kind": null,
            "source": null,
            "recipe_digest": null,
            "action_kind": null
        }
    });
    write_private_file(
        &root.join("deferred").join(name),
        &serde_json::to_vec(&legacy).unwrap(),
    );

    let reopened = LocalSpool::open(&root).unwrap();
    let records = reopened.deferred(10).unwrap();
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].reserved_sequence, None);
    let mut current = records[0].clone();
    current.reserved_sequence = Some(
        reopened
            .sequence_for("session_fixture", occurrence_id)
            .unwrap(),
    );
    assert!(reopened.retain_deferred(&current).is_err());
    assert_eq!(spool.deferred(10).unwrap()[0].reserved_sequence, None);
}

#[test]
fn claude_capture_keeps_bodies_local_and_ignores_thinking() {
    let temp = TempDir::new().unwrap();
    let spool = LocalSpool::open(temp.path().join("spool")).unwrap();
    let request = capture_request(CaptureModeV1::LocalConversation);
    let transcript = concat!(
        "{\"type\":\"user\",\"sessionId\":\"session_fixture\",\"timestamp\":\"2026-10-23T10:00:00Z\",\"uuid\":\"u1\",\"message\":{\"content\":[{\"type\":\"text\",\"text\":\"VISIBLE_BODY_742\"}]}}\n",
        "{\"type\":\"assistant\",\"sessionId\":\"session_fixture\",\"timestamp\":\"2026-10-23T10:00:01Z\",\"uuid\":\"a1\",\"message\":{\"content\":[{\"type\":\"thinking\",\"thinking\":\"PRIVATE_THOUGHT_991\"},{\"type\":\"text\",\"text\":\"VISIBLE_REPLY_743\"},{\"type\":\"tool_use\",\"id\":\"tool_1\",\"name\":\"Bash\",\"input\":{\"command\":\"printf hi\"}}]}}\n",
        "{\"type\":\"user\",\"sessionId\":\"session_fixture\",\"timestamp\":\"2026-10-23T10:00:02Z\",\"uuid\":\"u2\",\"message\":{\"content\":[{\"type\":\"tool_result\",\"tool_use_id\":\"tool_1\",\"content\":\"VISIBLE_RESULT_744\"}]}}\n",
        "not-json\n"
    );
    let report = import_jsonl(Cursor::new(transcript), &request, &spool).unwrap();
    assert_eq!(report.private_blocks_ignored, 1);
    assert_eq!(report.gaps, 2);

    let pending = spool.pending(10).unwrap();
    let shared = serde_json::to_string(&pending).unwrap();
    for private in [
        "VISIBLE_BODY_742",
        "VISIBLE_REPLY_743",
        "VISIBLE_RESULT_744",
        "PRIVATE_THOUGHT_991",
        "printf hi",
    ] {
        assert!(!shared.contains(private));
    }
    let retained = retained_bytes(spool.root());
    assert!(contains_bytes(&retained, b"VISIBLE_BODY_742"));
    assert!(contains_bytes(&retained, b"VISIBLE_REPLY_743"));
    assert!(!contains_bytes(&retained, b"PRIVATE_THOUGHT_991"));

    import_jsonl(Cursor::new(transcript), &request, &spool).unwrap();
    assert_eq!(spool.pending(10).unwrap().len(), pending.len());

    let hook = br#"{"hook_event_name":"PreToolUse","session_id":"session_fixture","tool_name":"Bash","tool_use_id":"tool_2","permission_mode":"default","effort":"high","tool_input":{"command":"date"}}"#;
    assert_eq!(
        capture_hook(hook, &request, &spool).unwrap(),
        HookCaptureOutcome::Stored
    );
    assert_eq!(
        capture_hook(hook, &request, &spool).unwrap(),
        HookCaptureOutcome::Duplicate
    );
    let latest = spool.pending(10).unwrap();
    let event = latest
        .iter()
        .filter_map(|envelope| match envelope {
            CaptureEnvelopeV1::EventBatch(batch) => batch.events.first(),
            _ => None,
        })
        .find(|event| {
            event
                .tool
                .as_ref()
                .and_then(|tool| tool.request_id.as_deref())
                == Some("tool_2")
        })
        .unwrap();
    assert_eq!(event.kind, CapturedEventKindV1::ToolStarted);
    let setup = event.setup.as_ref().unwrap();
    assert_eq!(setup.provider, None);
    assert_eq!(setup.permission_mode.as_deref(), Some("default"));
    assert_eq!(setup.effort.as_deref(), Some("high"));
    assert_eq!(setup.artifact.retention, RetentionClassV1::WorkLocal);

    let object_effort_hook = br#"{"hook_event_name":"PreToolUse","session_id":"session_fixture","tool_name":"Bash","tool_use_id":"tool_3","permission_mode":"default","effort":{"level":"medium","unretained":"PRIVATE_EFFORT_JSON"},"tool_input":{"command":"pwd"}}"#;
    assert_eq!(
        capture_hook(object_effort_hook, &request, &spool).unwrap(),
        HookCaptureOutcome::Stored
    );
    let object_effort = spool
        .pending(20)
        .unwrap()
        .into_iter()
        .filter_map(|envelope| match envelope {
            CaptureEnvelopeV1::EventBatch(batch) => batch.events.into_iter().next(),
            _ => None,
        })
        .find(|event| {
            event
                .tool
                .as_ref()
                .and_then(|tool| tool.request_id.as_deref())
                == Some("tool_3")
        })
        .unwrap();
    assert_eq!(
        object_effort.setup.unwrap().effort.as_deref(),
        Some("medium")
    );
    assert!(!contains_bytes(
        &retained_bytes(spool.root()),
        b"PRIVATE_EFFORT_JSON"
    ));

    let assistant_hook = br#"{"hook_event_name":"MessageDisplay","session_id":"session_fixture","message_id":"message_1","index":0,"final":true,"delta":"VISIBLE_LIVE_REPLY_745"}"#;
    assert_eq!(
        capture_hook(assistant_hook, &request, &spool).unwrap(),
        HookCaptureOutcome::Stored
    );
    let shared = serde_json::to_string(&spool.pending(10).unwrap()).unwrap();
    assert!(!shared.contains("VISIBLE_LIVE_REPLY_745"));
    assert!(contains_bytes(
        &retained_bytes(spool.root()),
        b"VISIBLE_LIVE_REPLY_745"
    ));
}

#[test]
fn provider_history_usage_is_session_bound_incremental_and_scope_safe() {
    let temp = TempDir::new().unwrap();
    let history = temp.path().join("history");
    fs::create_dir(&history).unwrap();
    let transcript = history.join("session_fixture.jsonl");
    fs::write(
        &transcript,
        concat!(
            "{\"type\":\"assistant\",\"sessionId\":\"session_fixture\",\"uuid\":\"event-1\",\"requestId\":\"request-1\",\"message\":{\"model\":\"claude-haiku-4-5\",\"content\":[{\"type\":\"text\",\"text\":\"PRIVATE_BODY\"}],\"usage\":{\"input_tokens\":7,\"output_tokens\":11,\"cache_read_input_tokens\":3,\"cache_creation_input_tokens\":2}}}\n",
            "{\"type\":\"assistant\",\"sessionId\":\"session_fixture\",\"uuid\":\"event-2\",\"requestId\":\"request-1\",\"message\":{\"model\":\"claude-haiku-4-5\",\"content\":[{\"type\":\"tool_use\",\"id\":\"tool-1\"}],\"usage\":{\"input_tokens\":7,\"output_tokens\":11,\"cache_read_input_tokens\":3,\"cache_creation_input_tokens\":2}}}\n",
            "{\"type\":\"result\",\"sessionId\":\"session_fixture\",\"uuid\":\"result-1\",\"usage\":{\"input_tokens\":70,\"output_tokens\":110}}\n"
        ),
    )
    .unwrap();
    let spool = LocalSpool::open(temp.path().join("spool")).unwrap();
    let request = CaptureRequest {
        work_id: "wrk_11111111111111111111111111111111".into(),
        ..capture_request(CaptureModeV1::MetadataOnly)
    };
    assert_eq!(
        capture_provider_usage(&transcript, &history, &request, true, &spool).unwrap(),
        3
    );
    assert_eq!(
        capture_provider_usage(&transcript, &history, &request, true, &spool).unwrap(),
        0
    );
    let usage: Vec<_> = spool
        .pending(10)
        .unwrap()
        .into_iter()
        .filter_map(|envelope| match envelope {
            CaptureEnvelopeV1::Usage(usage) => Some(usage),
            _ => None,
        })
        .collect();
    assert_eq!(usage.len(), 2);
    let request_usage = usage
        .iter()
        .find(|usage| usage.measurement_scope == Some(UsageMeasurementScope::Request))
        .unwrap();
    assert_eq!(
        request_usage.provider_request_id.as_deref(),
        Some("request-1")
    );
    assert_eq!(request_usage.provider_event_id, None);
    assert_eq!(request_usage.cache_read_input_tokens, Some(3));
    assert_eq!(request_usage.cache_creation_input_tokens, Some(2));
    assert_eq!(request_usage.input_tokens, Some(7));
    assert_eq!(request_usage.output_tokens, Some(11));
    assert!(
        !serde_json::to_string(&usage)
            .unwrap()
            .contains("PRIVATE_BODY")
    );
    assert!(usage.iter().any(|usage| {
        usage.measurement_scope == Some(UsageMeasurementScope::SessionCumulative)
            && usage.input_tokens == Some(70)
            && usage.output_tokens == Some(110)
    }));

    fs::OpenOptions::new()
        .append(true)
        .open(&transcript)
        .unwrap()
        .write_all(b"{\"type\":\"assistant\",\"session_id\":\"session_fixture\",\"uuid\":\"event-3\",\"requestId\":\"request-2\",\"message\":{\"model\":\"claude-haiku-4-5\",\"usage\":{\"input_tokens\":13,\"output_tokens\":17}}}\n")
        .unwrap();
    assert_eq!(
        capture_provider_usage(&transcript, &history, &request, true, &spool).unwrap(),
        1
    );
    let before_foreign = spool
        .provider_history_cursor(&request.canonical_session_id)
        .unwrap()
        .unwrap();
    fs::OpenOptions::new()
        .append(true)
        .open(&transcript)
        .unwrap()
        .write_all(b"{\"type\":\"assistant\",\"session_id\":\"foreign_session\",\"uuid\":\"event-4\",\"requestId\":\"request-3\",\"message\":{\"usage\":{\"input_tokens\":19,\"output_tokens\":23}}}\n")
        .unwrap();
    assert!(capture_provider_usage(&transcript, &history, &request, true, &spool).is_err());
    assert_eq!(
        spool
            .provider_history_cursor(&request.canonical_session_id)
            .unwrap()
            .unwrap(),
        before_foreign
    );
    assert_eq!(
        spool
            .pending(20)
            .unwrap()
            .into_iter()
            .filter(|envelope| matches!(envelope, CaptureEnvelopeV1::Usage(_)))
            .count(),
        3
    );

    let outside = temp.path().join("session_fixture.jsonl");
    fs::copy(&transcript, &outside).unwrap();
    assert!(capture_provider_usage(&outside, &history, &request, true, &spool).is_err());

    fs::write(&transcript, "").unwrap();
    assert!(capture_provider_usage(&transcript, &history, &request, true, &spool).is_err());
    assert!(spool.pending(20).unwrap().iter().any(|envelope| {
        matches!(envelope, CaptureEnvelopeV1::EventBatch(batch)
            if batch.events.iter().any(|event| event.kind == CapturedEventKindV1::Gap))
    }));
}

#[test]
fn provider_history_conflicting_usage_replays_compacted_gap_across_metadata_change() {
    let temp = TempDir::new().unwrap();
    let history = temp.path().join("history");
    fs::create_dir(&history).unwrap();
    let transcript = history.join("session_fixture.jsonl");
    fs::write(
        &transcript,
        "{\"type\":\"assistant\",\"session_id\":\"session_fixture\",\"uuid\":\"first\",\"requestId\":\"reused-request\",\"message\":{\"model\":\"claude-haiku-4-5\",\"usage\":{\"input_tokens\":7,\"output_tokens\":11}}}\n",
    )
    .unwrap();
    let spool = LocalSpool::open(temp.path().join("spool")).unwrap();
    let request = CaptureRequest {
        work_id: "wrk_11111111111111111111111111111111".into(),
        ..capture_request(CaptureModeV1::MetadataOnly)
    };
    assert_eq!(
        capture_provider_usage(&transcript, &history, &request, true, &spool).unwrap(),
        1
    );
    let before_gap = spool
        .provider_history_cursor(&request.canonical_session_id)
        .unwrap()
        .unwrap();
    fs::OpenOptions::new()
        .append(true)
        .open(&transcript)
        .unwrap()
        .write_all(concat!(
            "{\"type\":\"assistant\",\"session_id\":\"session_fixture\",\"uuid\":\"conflict\",\"requestId\":\"reused-request\",\"message\":{\"model\":\"claude-haiku-4-5\",\"usage\":{\"input_tokens\":7,\"output_tokens\":13}}}\n",
            "{\"type\":\"assistant\",\"session_id\":\"session_fixture\",\"uuid\":\"later\",\"requestId\":\"later-request\",\"message\":{\"model\":\"claude-haiku-4-5\",\"usage\":{\"input_tokens\":17,\"output_tokens\":19}}}\n"
        ).as_bytes())
        .unwrap();
    assert_eq!(
        capture_provider_usage(&transcript, &history, &request, true, &spool).unwrap(),
        2
    );
    let pending = spool.pending(20).unwrap();
    let usage: Vec<_> = pending
        .iter()
        .filter_map(|envelope| match envelope {
            CaptureEnvelopeV1::Usage(usage) => Some(usage),
            _ => None,
        })
        .collect();
    assert_eq!(usage.len(), 2);
    assert!(usage.iter().any(|usage| {
        usage.provider_request_id.as_deref() == Some("reused-request")
            && usage.output_tokens == Some(11)
    }));
    assert!(!usage.iter().any(|usage| {
        usage.provider_request_id.as_deref() == Some("reused-request")
            && usage.output_tokens == Some(13)
    }));
    assert!(usage.iter().any(|usage| {
        usage.provider_request_id.as_deref() == Some("later-request")
            && usage.output_tokens == Some(19)
    }));
    assert_eq!(
        pending
            .iter()
            .filter(
                |envelope| matches!(envelope, CaptureEnvelopeV1::EventBatch(batch)
                if batch.events.iter().any(|event| event.kind == CapturedEventKindV1::Gap))
            )
            .count(),
        1
    );
    let retained_gap = pending
        .iter()
        .find_map(|envelope| match envelope {
            CaptureEnvelopeV1::EventBatch(batch)
                if batch.events[0].kind == CapturedEventKindV1::Gap =>
            {
                Some(batch)
            }
            _ => None,
        })
        .unwrap();
    assert_eq!(
        retained_gap.session.capture_mode,
        CaptureModeV1::MetadataOnly
    );
    assert_eq!(retained_gap.session.parent_session_id, None);
    let gap_observed_at = compact_provider_history_gap(&spool);
    // Simulate interruption after the gap became durable but before the cursor advanced.
    rewind_provider_history_cursor(&spool, &before_gap);
    wait_past_observation_time(gap_observed_at);
    let mut retry_request = request.clone();
    retry_request.capture_mode = CaptureModeV1::LocalConversation;
    retry_request.parent_session_id = Some("parent_after_gap".into());
    assert_eq!(
        capture_provider_usage(&transcript, &history, &retry_request, true, &spool,).unwrap(),
        2
    );
    assert!(!spool.pending(20).unwrap().iter().any(|envelope| {
        matches!(envelope, CaptureEnvelopeV1::EventBatch(batch)
            if batch.events.iter().any(|event| event.kind == CapturedEventKindV1::Gap))
    }));
    assert_eq!(
        spool
            .provider_history_cursor(&request.canonical_session_id)
            .unwrap()
            .unwrap()
            .offset,
        fs::metadata(&transcript).unwrap().len()
    );
}

#[test]
fn provider_history_work_rebind_replays_compacted_gap_then_captures_later_usage() {
    let temp = TempDir::new().unwrap();
    let history = temp.path().join("history");
    fs::create_dir(&history).unwrap();
    let transcript = history.join("session_fixture.jsonl");
    fs::write(
        &transcript,
        b"{\"type\":\"assistant\",\"session_id\":\"session_fixture\",\"uuid\":\"work-a\",\"requestId\":\"work-a-request\",\"message\":{\"usage\":{\"input_tokens\":3,\"output_tokens\":5}}}\n",
    )
    .unwrap();
    let spool = LocalSpool::open(temp.path().join("spool")).unwrap();
    let request_a = CaptureRequest {
        work_id: "wrk_11111111111111111111111111111111".into(),
        ..capture_request(CaptureModeV1::MetadataOnly)
    };
    assert_eq!(
        capture_provider_usage(&transcript, &history, &request_a, true, &spool).unwrap(),
        1
    );
    let before_rebind = spool
        .provider_history_cursor(&request_a.canonical_session_id)
        .unwrap()
        .unwrap();

    fs::OpenOptions::new()
        .append(true)
        .open(&transcript)
        .unwrap()
        .write_all(b"{\"type\":\"assistant\",\"session_id\":\"session_fixture\",\"uuid\":\"at-boundary\",\"requestId\":\"at-boundary-request\",\"message\":{\"usage\":{\"input_tokens\":7,\"output_tokens\":11}}}\n")
        .unwrap();
    let mut request_b = request_a.clone();
    request_b.work_id = "wrk_22222222222222222222222222222222".into();
    request_b.generation = 2;
    let boundary = fs::metadata(&transcript).unwrap().len();
    assert_eq!(
        capture_provider_usage(&transcript, &history, &request_b, true, &spool).unwrap(),
        0
    );
    let rebound = spool
        .provider_history_cursor(&request_b.canonical_session_id)
        .unwrap()
        .unwrap();
    assert_eq!(rebound.work_id, request_b.work_id);
    assert_eq!(rebound.generation, request_b.generation);
    assert_eq!(rebound.offset, boundary);
    let pending = spool.pending(20).unwrap();
    assert!(pending.iter().any(|envelope| {
        matches!(envelope, CaptureEnvelopeV1::EventBatch(batch)
            if batch.work_id.as_str() == request_b.work_id.as_str()
                && batch.events.iter().any(|event| event.kind == CapturedEventKindV1::Gap))
    }));
    assert!(!pending.iter().any(|envelope| {
        matches!(envelope, CaptureEnvelopeV1::Usage(usage)
            if usage.provider_request_id.as_deref() == Some("at-boundary-request"))
    }));
    let gap_observed_at = compact_provider_history_gap(&spool);
    // Simulate interruption after the gap became durable but before the cursor rebind.
    rewind_provider_history_cursor(&spool, &before_rebind);
    wait_past_observation_time(gap_observed_at);
    assert_eq!(
        capture_provider_usage(&transcript, &history, &request_b, true, &spool).unwrap(),
        0
    );
    assert_eq!(
        spool
            .provider_history_cursor(&request_b.canonical_session_id)
            .unwrap()
            .unwrap(),
        rebound
    );
    assert!(!spool.pending(20).unwrap().iter().any(|envelope| {
        matches!(envelope, CaptureEnvelopeV1::EventBatch(batch)
            if batch.events.iter().any(|event| event.kind == CapturedEventKindV1::Gap))
    }));

    fs::OpenOptions::new()
        .append(true)
        .open(&transcript)
        .unwrap()
        .write_all(b"{\"type\":\"assistant\",\"session_id\":\"session_fixture\",\"uuid\":\"later-b\",\"requestId\":\"later-b-request\",\"message\":{\"usage\":{\"input_tokens\":13,\"output_tokens\":17}}}\n")
        .unwrap();
    assert_eq!(
        capture_provider_usage(&transcript, &history, &request_b, true, &spool).unwrap(),
        1
    );
    let pending = spool.pending(20).unwrap();
    assert!(pending.iter().any(|envelope| {
        matches!(envelope, CaptureEnvelopeV1::Usage(usage)
            if usage.work_id.as_str() == request_b.work_id.as_str()
                && usage.generation == request_b.generation
                && usage.provider_request_id.as_deref() == Some("later-b-request"))
    }));
    assert!(pending.iter().any(|envelope| {
        matches!(envelope, CaptureEnvelopeV1::Usage(usage)
            if usage.work_id.as_str() == request_a.work_id.as_str()
                && usage.generation == request_a.generation
                && usage.provider_request_id.as_deref() == Some("work-a-request"))
    }));
}

#[test]
fn provider_history_truncation_and_replacement_remain_fail_closed() {
    let temp = TempDir::new().unwrap();
    let history = temp.path().join("history");
    fs::create_dir(&history).unwrap();
    let transcript = history.join("session_fixture.jsonl");
    let original = b"{\"type\":\"assistant\",\"session_id\":\"session_fixture\",\"uuid\":\"initial\",\"requestId\":\"initial-request\",\"message\":{\"usage\":{\"input_tokens\":23,\"output_tokens\":29}}}\n";
    fs::write(&transcript, original).unwrap();
    let spool = LocalSpool::open(temp.path().join("spool")).unwrap();
    let request = CaptureRequest {
        work_id: "wrk_11111111111111111111111111111111".into(),
        ..capture_request(CaptureModeV1::MetadataOnly)
    };
    capture_provider_usage(&transcript, &history, &request, true, &spool).unwrap();
    let cursor = spool
        .provider_history_cursor(&request.canonical_session_id)
        .unwrap()
        .unwrap();

    fs::write(&transcript, b"").unwrap();
    assert!(matches!(
        capture_provider_usage(&transcript, &history, &request, true, &spool),
        Err(agentscm::change_capture::CaptureError::Conflict(_))
    ));
    assert_eq!(
        spool
            .provider_history_cursor(&request.canonical_session_id)
            .unwrap()
            .unwrap(),
        cursor
    );

    fs::write(&transcript, original).unwrap();
    let retained = history.join("retained-old-session.jsonl");
    fs::rename(&transcript, &retained).unwrap();
    fs::write(&transcript, original).unwrap();
    assert_ne!(
        fs::metadata(&transcript).unwrap().ino(),
        fs::metadata(&retained).unwrap().ino()
    );
    assert!(matches!(
        capture_provider_usage(&transcript, &history, &request, true, &spool),
        Err(agentscm::change_capture::CaptureError::Conflict(_))
    ));
    assert_eq!(
        spool
            .provider_history_cursor(&request.canonical_session_id)
            .unwrap()
            .unwrap(),
        cursor
    );
    assert_eq!(
        spool
            .pending(20)
            .unwrap()
            .iter()
            .filter(
                |envelope| matches!(envelope, CaptureEnvelopeV1::EventBatch(batch)
                if batch.events.iter().any(|event| event.kind == CapturedEventKindV1::Gap))
            )
            .count(),
        2
    );
}

#[test]
fn provider_history_oversized_record_skips_one_snapshot_and_resumes() {
    let temp = TempDir::new().unwrap();
    let history = temp.path().join("history");
    fs::create_dir(&history).unwrap();
    let transcript = history.join("session_fixture.jsonl");
    let mut snapshot = format!(
        "{{\"type\":\"assistant\",\"session_id\":\"session_fixture\",\"padding\":\"{}\"}}\n",
        "x".repeat(4 * 1024 * 1024)
    )
    .into_bytes();
    snapshot.extend_from_slice(b"{\"type\":\"assistant\",\"session_id\":\"session_fixture\",\"uuid\":\"skipped\",\"requestId\":\"skipped-request\",\"message\":{\"usage\":{\"input_tokens\":29,\"output_tokens\":31}}}\n");
    fs::write(&transcript, &snapshot).unwrap();
    let snapshot_end = snapshot.len() as u64;
    let spool = LocalSpool::open(temp.path().join("spool")).unwrap();
    let request = CaptureRequest {
        work_id: "wrk_11111111111111111111111111111111".into(),
        ..capture_request(CaptureModeV1::MetadataOnly)
    };

    assert_eq!(
        capture_provider_usage(&transcript, &history, &request, true, &spool).unwrap(),
        0
    );
    assert_eq!(
        spool
            .provider_history_cursor(&request.canonical_session_id)
            .unwrap()
            .unwrap()
            .offset,
        snapshot_end
    );
    let pending = spool.pending(20).unwrap();
    assert!(pending.iter().any(|envelope| {
        matches!(envelope, CaptureEnvelopeV1::EventBatch(batch)
            if batch.events.iter().any(|event| event.kind == CapturedEventKindV1::Gap
                && event.gap.as_ref().is_some_and(|gap| gap.reason == GapReasonV1::CaptureInterrupted)))
    }));
    assert!(
        !pending
            .iter()
            .any(|envelope| matches!(envelope, CaptureEnvelopeV1::Usage(_)))
    );
    let retained_count = pending.len();
    assert_eq!(
        capture_provider_usage(&transcript, &history, &request, true, &spool).unwrap(),
        0
    );
    assert_eq!(spool.pending(20).unwrap().len(), retained_count);

    fs::OpenOptions::new()
        .append(true)
        .open(&transcript)
        .unwrap()
        .write_all(b"{\"type\":\"assistant\",\"session_id\":\"session_fixture\",\"uuid\":\"appended\",\"requestId\":\"appended-request\",\"message\":{\"usage\":{\"input_tokens\":37,\"output_tokens\":41}}}\n")
        .unwrap();
    assert_eq!(
        capture_provider_usage(&transcript, &history, &request, true, &spool).unwrap(),
        1
    );
    let pending = spool.pending(20).unwrap();
    assert_eq!(
        pending
            .iter()
            .filter(|envelope| matches!(envelope, CaptureEnvelopeV1::Usage(_)))
            .count(),
        1
    );
    assert_eq!(
        spool
            .provider_history_cursor(&request.canonical_session_id)
            .unwrap()
            .unwrap()
            .offset,
        fs::metadata(&transcript).unwrap().len()
    );
}

#[test]
fn jsonl_import_stops_when_one_oversized_record_crosses_the_aggregate_limit() {
    let temp = TempDir::new().unwrap();
    let spool = LocalSpool::open(temp.path().join("spool")).unwrap();
    let request = capture_request(CaptureModeV1::MetadataOnly);
    let input = BufReader::with_capacity(8192, std::io::repeat(b'x').take(64 * 1024 * 1024 + 1));
    assert!(import_jsonl(input, &request, &spool).is_err());
}

#[test]
fn native_session_coverage_and_uncertain_occurrences_are_explicit() {
    let temp = TempDir::new().unwrap();
    let spool = LocalSpool::open(temp.path().join("spool")).unwrap();
    let request = capture_request(CaptureModeV1::MetadataOnly);
    let start =
        br#"{"hook_event_name":"SessionStart","session_id":"session_fixture","source":"startup"}"#;
    assert_eq!(
        capture_hook(start, &request, &spool).unwrap(),
        HookCaptureOutcome::Stored
    );
    let repeated =
        br#"{"hook_event_name":"UserPromptSubmit","session_id":"session_fixture","prompt":"same"}"#;
    assert_eq!(
        capture_hook(repeated, &request, &spool).unwrap(),
        HookCaptureOutcome::Stored
    );
    assert_eq!(
        capture_hook(repeated, &request, &spool).unwrap(),
        HookCaptureOutcome::Stored
    );

    let events: Vec<_> = spool
        .pending(20)
        .unwrap()
        .into_iter()
        .filter_map(|envelope| match envelope {
            CaptureEnvelopeV1::EventBatch(batch) => Some(batch.events),
            _ => None,
        })
        .flatten()
        .collect();
    assert_eq!(
        events
            .iter()
            .filter(|event| event.kind == CapturedEventKindV1::SessionStarted)
            .count(),
        1
    );
    assert_eq!(
        events
            .iter()
            .filter(|event| event.kind == CapturedEventKindV1::UserPrompt)
            .count(),
        2
    );
    assert_eq!(
        events
            .iter()
            .filter(|event| {
                event.gap.as_ref().is_some_and(|gap| {
                    gap.reason == agentscm::change_record::GapReasonV1::IdentityUncertain
                })
            })
            .count(),
        3
    );
    assert!(!events.iter().any(|event| {
        event.gap.as_ref().is_some_and(|gap| {
            gap.reason == agentscm::change_record::GapReasonV1::AdapterStartedLate
        })
    }));

    let late_temp = TempDir::new().unwrap();
    let late_spool = LocalSpool::open(late_temp.path().join("spool")).unwrap();
    let end =
        br#"{"hook_event_name":"SessionEnd","session_id":"session_fixture","reason":"other"}"#;
    capture_hook(end, &request, &late_spool).unwrap();
    let late_events: Vec<_> = late_spool
        .pending(10)
        .unwrap()
        .into_iter()
        .filter_map(|envelope| match envelope {
            CaptureEnvelopeV1::EventBatch(batch) => Some(batch.events),
            _ => None,
        })
        .flatten()
        .collect();
    assert!(late_events.iter().any(|event| {
        event.gap.as_ref().is_some_and(|gap| {
            gap.reason == agentscm::change_record::GapReasonV1::AdapterStartedLate
        })
    }));
    assert!(
        late_events
            .iter()
            .any(|event| event.kind == CapturedEventKindV1::SessionEnded)
    );

    let resumed_temp = TempDir::new().unwrap();
    let resumed_spool = LocalSpool::open(resumed_temp.path().join("spool")).unwrap();
    let resumed =
        br#"{"hook_event_name":"SessionStart","session_id":"session_fixture","source":"resume"}"#;
    capture_hook(resumed, &request, &resumed_spool).unwrap();
    assert!(
        resumed_spool
            .pending(10)
            .unwrap()
            .into_iter()
            .filter_map(|envelope| match envelope {
                CaptureEnvelopeV1::EventBatch(batch) => Some(batch.events),
                _ => None,
            })
            .flatten()
            .any(|event| {
                event.gap.as_ref().is_some_and(|gap| {
                    gap.reason == agentscm::change_record::GapReasonV1::AdapterStartedLate
                })
            })
    );
}

#[test]
fn late_adapter_gap_remains_bound_to_each_work_across_compaction() {
    let temp = TempDir::new().unwrap();
    let spool = LocalSpool::open(temp.path().join("spool")).unwrap();
    let mut first = capture_request(CaptureModeV1::MetadataOnly);
    first.work_id = "wrk_late_first".into();
    first.observed_session_id = "session_late_switch".into();
    first.canonical_session_id = "session_late_switch".into();
    let end =
        br#"{"hook_event_name":"SessionEnd","session_id":"session_late_switch","reason":"other"}"#;
    assert_eq!(
        capture_hook(end, &first, &spool).unwrap(),
        HookCaptureOutcome::Stored
    );
    let first_pending = spool.pending(10).unwrap();
    assert!(first_pending.iter().any(|envelope| {
        matches!(envelope, CaptureEnvelopeV1::EventBatch(batch)
            if batch.work_id == first.work_id
                && batch.events.iter().any(|event| event.gap.as_ref().is_some_and(|gap|
                    gap.reason == agentscm::change_record::GapReasonV1::AdapterStartedLate)))
    }));
    for envelope in first_pending {
        spool.acknowledge(&envelope).unwrap();
    }
    spool.compact_acknowledged(10).unwrap();

    let mut second = first.clone();
    second.operation_prefix = "capture:late-second".into();
    second.work_id = "wrk_late_second".into();
    let prompt = br#"{"hook_event_name":"UserPromptSubmit","session_id":"session_late_switch","prompt":"continued"}"#;
    assert_eq!(
        capture_hook(prompt, &second, &spool).unwrap(),
        HookCaptureOutcome::Stored
    );
    let second_pending = spool.pending(10).unwrap();
    assert!(second_pending.iter().any(|envelope| {
        matches!(envelope, CaptureEnvelopeV1::EventBatch(batch)
            if batch.work_id == second.work_id
                && batch.events.iter().any(|event| event.gap.as_ref().is_some_and(|gap|
                    gap.reason == agentscm::change_record::GapReasonV1::AdapterStartedLate)))
    }));
    assert!(second_pending.iter().any(|envelope| {
        matches!(envelope, CaptureEnvelopeV1::EventBatch(batch)
            if batch.work_id == second.work_id
                && batch.events.iter().any(|event| event.kind == CapturedEventKindV1::UserPrompt))
    }));
}

#[test]
fn dirty_git_snapshot_retains_permitted_bytes_without_mutating_git_state() {
    let repo = git_fixture();
    fs::write(repo.path().join("tracked.txt"), "dirty bytes\n").unwrap();
    fs::write(repo.path().join("untracked.txt"), "untracked bytes\n").unwrap();
    fs::write(
        repo.path().join("ignored-secret.txt"),
        "IGNORED_SECRET_881\n",
    )
    .unwrap();
    let before_head = git(repo.path(), &["rev-parse", "HEAD"]);
    let before_status = git(
        repo.path(),
        &["status", "--porcelain=v2", "--untracked-files=all"],
    );
    let spool_dir = TempDir::new().unwrap();
    let spool = LocalSpool::open(spool_dir.path().join("spool")).unwrap();

    let source = capture_git_source(&git_request(repo.path()), &spool).unwrap();
    let SourceBindingV1::Git {
        captured_snapshot,
        snapshot_coverage,
        omissions,
        ..
    } = source
    else {
        panic!("expected Git binding")
    };
    assert!(captured_snapshot.is_some());
    assert_eq!(snapshot_coverage, CoverageV1::Partial);
    assert!(!omissions.is_empty());
    assert_eq!(git(repo.path(), &["rev-parse", "HEAD"]), before_head);
    assert_eq!(
        git(
            repo.path(),
            &["status", "--porcelain=v2", "--untracked-files=all"]
        ),
        before_status
    );
    let retained = retained_bytes(spool.root());
    assert!(contains_bytes(&retained, b"dirty bytes"));
    assert!(contains_bytes(&retained, b"untracked bytes"));
    assert!(!contains_bytes(&retained, b"IGNORED_SECRET_881"));
}

#[test]
fn git_snapshot_never_follows_an_ancestor_replaced_with_an_outside_symlink() {
    let repo = git_fixture();
    let nested = repo.path().join("nested");
    fs::create_dir(&nested).unwrap();
    fs::write(nested.join("tracked.txt"), "SAFE_WORKTREE_BYTES_910\n").unwrap();
    run_git(repo.path(), &["add", "nested/tracked.txt"]);
    run_git(repo.path(), &["commit", "-m", "add nested fixture"]);
    fs::write(
        nested.join("tracked.txt"),
        "SAFE_WORKTREE_BYTES_910 dirty\n",
    )
    .unwrap();

    let outside = TempDir::new().unwrap();
    fs::write(
        outside.path().join("tracked.txt"),
        "OUTSIDE_PRIVATE_BYTES_911\n",
    )
    .unwrap();
    let parked = repo.path().join("nested-parked");
    let stop = Arc::new(AtomicBool::new(false));
    let toggler_stop = Arc::clone(&stop);
    let toggler_nested = nested.clone();
    let toggler_parked = parked.clone();
    let outside_path = outside.path().to_owned();
    let toggler = thread::spawn(move || {
        while !toggler_stop.load(Ordering::Acquire) {
            if fs::rename(&toggler_nested, &toggler_parked).is_ok() {
                if std::os::unix::fs::symlink(&outside_path, &toggler_nested).is_ok() {
                    thread::yield_now();
                    let _ = fs::remove_file(&toggler_nested);
                }
                let _ = fs::rename(&toggler_parked, &toggler_nested);
            }
            thread::yield_now();
        }
        if fs::symlink_metadata(&toggler_nested)
            .is_ok_and(|metadata| metadata.file_type().is_symlink())
        {
            let _ = fs::remove_file(&toggler_nested);
        }
        if toggler_parked.exists() && !toggler_nested.exists() {
            let _ = fs::rename(&toggler_parked, &toggler_nested);
        }
    });

    let spool_dir = TempDir::new().unwrap();
    let spool = LocalSpool::open(spool_dir.path().join("spool")).unwrap();
    for _ in 0..30 {
        let _ = capture_git_source(&git_request(repo.path()), &spool);
    }
    stop.store(true, Ordering::Release);
    toggler.join().unwrap();

    let retained = retained_bytes(spool.root());
    assert!(!contains_bytes(&retained, b"OUTSIDE_PRIVATE_BYTES_911"));
}

#[test]
fn clean_filtered_git_source_captures_materialized_working_bytes() {
    let repo = git_fixture();
    run_git(
        repo.path(),
        &[
            "config",
            "filter.synthetic.clean",
            "sed s/MATERIALIZED_418/STORED_418/g",
        ],
    );
    run_git(
        repo.path(),
        &[
            "config",
            "filter.synthetic.smudge",
            "sed s/STORED_418/MATERIALIZED_418/g",
        ],
    );
    fs::write(
        repo.path().join(".gitattributes"),
        "filtered.txt filter=synthetic\n",
    )
    .unwrap();
    fs::write(repo.path().join("filtered.txt"), "MATERIALIZED_418\n").unwrap();
    run_git(repo.path(), &["add", ".gitattributes", "filtered.txt"]);
    run_git(repo.path(), &["commit", "-m", "synthetic filtered fixture"]);
    assert!(git(repo.path(), &["status", "--porcelain"]).is_empty());
    assert!(contains_bytes(
        &[git(repo.path(), &["show", "HEAD:filtered.txt"])],
        b"STORED_418"
    ));

    let spool_dir = TempDir::new().unwrap();
    let spool = LocalSpool::open(spool_dir.path().join("spool")).unwrap();
    let source = capture_git_source(&git_request(repo.path()), &spool).unwrap();
    let SourceBindingV1::Git {
        captured_snapshot,
        snapshot_coverage,
        ..
    } = source
    else {
        panic!("expected Git binding")
    };
    assert!(captured_snapshot.is_some());
    assert_eq!(snapshot_coverage, CoverageV1::Partial);
    assert!(contains_bytes(
        &retained_bytes(spool.root()),
        b"MATERIALIZED_418"
    ));
}

#[test]
fn observed_command_records_caller_asserted_result_and_source_drift() {
    let repo = git_fixture();
    let spool_dir = TempDir::new().unwrap();
    let spool = LocalSpool::open(spool_dir.path().join("spool")).unwrap();
    let observed = run_observed_command(
        ObservedCommandRequest {
            start_operation_id: "action:start:1".into(),
            finish_operation_id: "action:finish:1".into(),
            work_id: "wrk_fixture".into(),
            generation: 1,
            session_id: "session_fixture".into(),
            action_id: "action_fixture".into(),
            source: git_request(repo.path()),
            argv: vec![
                "/bin/sh".into(),
                "-c".into(),
                "printf changed >> tracked.txt; printf observed-output".into(),
            ],
            setup_digest: "0".repeat(64),
            coverage: CoverageV1::Partial,
            timeout: Duration::from_secs(10),
            max_output_bytes: 4096,
        },
        &spool,
    )
    .unwrap();
    assert_eq!(observed.finish.assurance, AssuranceBasisV1::CallerAsserted);
    assert_eq!(observed.start.kind, ActionKindV1::Execution);
    assert_eq!(
        observed.start.recipe_provenance,
        RecipeProvenanceV1::ObservedNativeInput
    );
    assert_eq!(observed.finish.outcome, ActionOutcomeV1::Succeeded);
    assert_eq!(observed.finish.result, CheckResultV1::Unknown);
    assert_eq!(observed.finish.drift, DriftV1::SourceChanged);
    assert_eq!(observed.exit_code, Some(0));
    assert!(observed.finish.output_artifacts.len() >= 3);
    assert_eq!(spool.pending(10).unwrap().len(), 2);
}

fn configured_recipe_request(
    repo: &Path,
    spool: &LocalSpool,
    invocation_id: &str,
    recipe_id: &str,
    argv: Vec<String>,
    timeout_seconds: u64,
    max_output_bytes: u64,
) -> ConfiguredRecipeRunRequest {
    let canonical_repo = repo.canonicalize().unwrap();
    let managed_invocation = format!(
        "/synthetic/agentscm change-capture run-recipe --id {recipe_id} --test-occurrence {invocation_id}"
    );
    let request = ConfiguredRecipeRunRequest {
        work_id: "wrk_fixture".into(),
        generation: 1,
        observed_session_id: "session_fixture".into(),
        session_id: "session_fixture".into(),
        source: git_request(&canonical_repo),
        setup_digest: "0".repeat(64),
        recipe: ConfiguredRecipeV1 {
            id: recipe_id.into(),
            argv,
            coverage: CoverageV1::Complete,
            timeout_seconds,
            max_output_bytes,
        },
        managed_invocation: managed_invocation.clone(),
    };
    seed_configured_recipe_marker(&canonical_repo, spool, invocation_id, &request);
    request
}

fn seed_configured_recipe_marker(
    repo: &Path,
    spool: &LocalSpool,
    tool_use_id: &str,
    request: &ConfiguredRecipeRunRequest,
) {
    let canonical_repo = repo.canonicalize().unwrap();
    let capture = capture_request(CaptureModeV1::MetadataOnly);
    let actions = ActionCaptureConfig {
        source: git_request(&canonical_repo),
        setup_digest: request.setup_digest.clone(),
        rules: Vec::new(),
        managed_invocations: vec![request.managed_invocation.clone()],
    };
    let pre = serde_json::to_vec(&serde_json::json!({
        "hook_event_name": "PreToolUse",
        "session_id": capture.observed_session_id,
        "tool_name": "Bash",
        "tool_use_id": tool_use_id,
        "tool_input": {"command": request.managed_invocation},
    }))
    .unwrap();
    capture_native_hook(&pre, &capture, &actions, spool).unwrap();
}

#[test]
fn configured_recipe_uses_real_child_status_and_never_reexecutes_an_identity() {
    let repo = git_fixture();
    let spool_dir = TempDir::new().unwrap();
    let spool = LocalSpool::open(spool_dir.path().join("spool")).unwrap();

    let marker = repo.path().join("recipe-marker");
    let success = configured_recipe_request(
        repo.path(),
        &spool,
        "invocation-success",
        "synthetic-success",
        vec![
            "/bin/sh".into(),
            "-c".into(),
            format!(
                "printf x >> {}; printf configured-success",
                marker.display()
            ),
        ],
        10,
        4096,
    );
    let observed = run_configured_recipe(success, &spool).unwrap();
    assert_eq!(observed.start.kind, ActionKindV1::Verification);
    assert_eq!(
        observed.start.recipe_provenance,
        RecipeProvenanceV1::ConfiguredExactMatch
    );
    assert_eq!(observed.finish.result, CheckResultV1::Passed);
    assert_eq!(observed.exit_code, Some(0));
    assert!(
        observed
            .finish
            .output_artifacts
            .iter()
            .all(|artifact| artifact.availability == ContentAvailabilityV1::Local)
    );
    let replay = run_configured_recipe(
        configured_recipe_request(
            repo.path(),
            &spool,
            "invocation-success",
            "synthetic-success",
            vec![
                "/bin/sh".into(),
                "-c".into(),
                format!(
                    "printf x >> {}; printf configured-success",
                    marker.display()
                ),
            ],
            10,
            4096,
        ),
        &spool,
    )
    .unwrap();
    assert_eq!(replay.finish.result, CheckResultV1::Passed);
    assert_eq!(fs::read(&marker).unwrap(), b"x");

    let failed = run_configured_recipe(
        configured_recipe_request(
            repo.path(),
            &spool,
            "invocation-failed",
            "synthetic-failed",
            vec!["/bin/sh".into(), "-c".into(), "exit 7".into()],
            10,
            4096,
        ),
        &spool,
    )
    .unwrap();
    assert_eq!(failed.finish.outcome, ActionOutcomeV1::Failed);
    assert_eq!(failed.finish.result, CheckResultV1::Failed);
    assert_eq!(failed.exit_code, Some(7));

    let signalled = run_configured_recipe(
        configured_recipe_request(
            repo.path(),
            &spool,
            "invocation-signalled",
            "synthetic-signalled",
            vec!["/bin/sh".into(), "-c".into(), "kill -TERM $$".into()],
            10,
            4096,
        ),
        &spool,
    )
    .unwrap();
    assert_eq!(
        signalled.stop,
        agentscm::change_capture::ObservedStop::Signalled
    );
    assert_eq!(signalled.exit_code, None);
    assert_eq!(signalled.signal, Some(libc::SIGTERM));
    assert_eq!(signalled.finish.outcome, ActionOutcomeV1::Incomplete);
    assert_eq!(signalled.finish.result, CheckResultV1::Unknown);

    let timed_out = run_configured_recipe(
        configured_recipe_request(
            repo.path(),
            &spool,
            "invocation-timeout",
            "synthetic-timeout",
            vec![
                "/bin/sh".into(),
                "-c".into(),
                "printf before; sleep 2".into(),
            ],
            1,
            4096,
        ),
        &spool,
    )
    .unwrap();
    assert_eq!(
        timed_out.stop,
        agentscm::change_capture::ObservedStop::TimedOut
    );
    assert_eq!(timed_out.finish.result, CheckResultV1::Incomplete);

    let output_limited = run_configured_recipe(
        configured_recipe_request(
            repo.path(),
            &spool,
            "invocation-output",
            "synthetic-output",
            vec!["/bin/sh".into(), "-c".into(), "yes x | head -c 4096".into()],
            10,
            256,
        ),
        &spool,
    )
    .unwrap();
    assert_eq!(
        output_limited.stop,
        agentscm::change_capture::ObservedStop::OutputLimit
    );
    assert_eq!(output_limited.finish.result, CheckResultV1::Incomplete);

    let cancellation = Cancellation::default();
    cancellation.cancel();
    let cancelled = run_configured_recipe_with_cancellation(
        configured_recipe_request(
            repo.path(),
            &spool,
            "invocation-cancelled",
            "synthetic-cancelled",
            vec!["/bin/sh".into(), "-c".into(), "exit 0".into()],
            10,
            4096,
        ),
        &spool,
        &cancellation,
    )
    .unwrap();
    assert_eq!(
        cancelled.stop,
        agentscm::change_capture::ObservedStop::Cancelled
    );
    assert_eq!(cancelled.finish.result, CheckResultV1::Incomplete);
}

#[test]
fn configured_recipe_replay_with_only_a_retained_start_refuses_to_launch_again() {
    let repo = git_fixture();
    let spool_dir = TempDir::new().unwrap();
    let spool = LocalSpool::open(spool_dir.path().join("spool")).unwrap();
    let marker = repo.path().join("crash-marker");
    let request = configured_recipe_request(
        repo.path(),
        &spool,
        "invocation-crash",
        "synthetic-crash",
        vec![
            "/bin/sh".into(),
            "-c".into(),
            format!("printf x >> {}", marker.display()),
        ],
        10,
        4096,
    );
    let first = run_configured_recipe(request.clone(), &spool).unwrap();
    for entry in fs::read_dir(spool.root().join("outbox")).unwrap() {
        let path = entry.unwrap().path();
        let envelope: CaptureEnvelopeV1 =
            serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        if matches!(&envelope, CaptureEnvelopeV1::ActionFinish(finish) if finish.action_id == first.finish.action_id)
        {
            fs::remove_file(path).unwrap();
        }
    }
    seed_configured_recipe_marker(repo.path(), &spool, "invocation-crash", &request);
    let replay = run_configured_recipe(request, &spool);
    assert!(replay.is_err());
    let mut resumed = configured_recipe_request(
        repo.path(),
        &spool,
        "invocation-resumed",
        "synthetic-crash",
        vec!["/bin/sh".into(), "-c".into(), "exit 99".into()],
        10,
        4096,
    );
    resumed.generation = 2;
    resumed.observed_session_id = "session_resumed".into();
    resumed.session_id = "session_resumed".into();
    resumed.setup_digest = "9".repeat(64);
    assert!(matches!(
        reconcile_configured_recipe(resumed.clone(), &spool).unwrap(),
        RecipeReconcileOutcome::Unresolved { .. }
    ));
    assert!(run_configured_recipe(resumed.clone(), &spool).is_err());
    let attempt_path = fs::read_dir(spool.root().join("recipe-attempts"))
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    let mut attempt: serde_json::Value =
        serde_json::from_slice(&fs::read(&attempt_path).unwrap()).unwrap();
    attempt["boot_id"] = serde_json::Value::String("f".repeat(64));
    fs::write(&attempt_path, serde_json::to_vec(&attempt).unwrap()).unwrap();
    resumed.recipe.coverage = CoverageV1::Partial;
    let reconciled = reconcile_configured_recipe(resumed, &spool).unwrap();
    let RecipeReconcileOutcome::Reconciled(finish) = reconciled else {
        panic!("changed boot should reconcile the retained attempt")
    };
    assert_eq!(finish.outcome, ActionOutcomeV1::Incomplete);
    assert_eq!(finish.result, CheckResultV1::Unknown);
    assert_eq!(finish.coverage, CoverageV1::Complete);
    assert_eq!(fs::read(marker).unwrap(), b"x");
}

#[test]
fn configured_recipe_quota_failure_before_start_never_launches_child() {
    let repo = git_fixture();
    let spool_dir = TempDir::new().unwrap();
    let spool = LocalSpool::open_with_limits(
        spool_dir.path().join("spool"),
        SpoolLimits {
            max_records: MIN_ACTION_CAPTURE_RECORDS,
            max_total_bytes: MIN_ACTION_CAPTURE_BYTES,
            max_artifact_bytes: 4096,
        },
    )
    .unwrap();
    let marker = repo.path().join("must-not-run");
    let request = configured_recipe_request(
        repo.path(),
        &spool,
        "invocation-quota",
        "synthetic-quota",
        vec![
            "/bin/sh".into(),
            "-c".into(),
            format!("printf launched > {}", marker.display()),
        ],
        10,
        4096,
    );
    let mut quota_reached = false;
    for index in 0..100 {
        let mut content = format!("quota-{index:04}").into_bytes();
        content.resize(128, b'q');
        if spool
            .retain_artifact(
                &content,
                ArtifactMediaV1::Binary,
                RetentionClassV1::WorkLocal,
            )
            .is_err()
        {
            quota_reached = true;
            break;
        }
    }
    assert!(quota_reached);
    assert!(run_configured_recipe(request, &spool).is_err());
    assert!(!marker.exists());
    assert!(
        !spool
            .pending(20)
            .unwrap()
            .iter()
            .any(|envelope| matches!(envelope, CaptureEnvelopeV1::ActionStart(_)))
    );
}

#[test]
fn configured_recipe_refuses_source_enriched_marker_without_durable_pre_event() {
    let repo = git_fixture();
    let spool_dir = TempDir::new().unwrap();
    let spool = LocalSpool::open(spool_dir.path().join("spool")).unwrap();
    let child_marker = repo.path().join("must-not-launch-before-pre");
    let request = configured_recipe_request(
        repo.path(),
        &spool,
        "source-enriched-no-pre",
        "source-enriched-no-pre",
        vec![
            "/bin/sh".into(),
            "-c".into(),
            format!("printf launched > {}", child_marker.display()),
        ],
        10,
        4096,
    );
    let managed_pre = fs::read_dir(spool.root().join("outbox"))
        .unwrap()
        .find_map(|entry| {
            let path = entry.unwrap().path();
            let envelope: CaptureEnvelopeV1 =
                serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
            matches!(
                envelope,
                CaptureEnvelopeV1::EventBatch(ref batch)
                    if batch.operation_id.starts_with("managed:event:")
                        && batch.operation_id.ends_with(":pre")
            )
            .then_some(path)
        })
        .expect("seeded managed Pre envelope");
    fs::remove_file(managed_pre).unwrap();
    assert!(matches!(
        run_configured_recipe(request, &spool),
        Err(agentscm::change_capture::CaptureError::Conflict(_))
    ));
    assert!(!child_marker.exists());
}

#[test]
fn configured_recipe_completes_at_the_supported_minimum_spool() {
    let repo = git_fixture();
    let spool_dir = TempDir::new().unwrap();
    let limits = SpoolLimits {
        max_records: MIN_ACTION_CAPTURE_RECORDS,
        max_total_bytes: MIN_ACTION_CAPTURE_BYTES,
        max_artifact_bytes: 16 * 1024,
    };
    limits.validate_action_capture().unwrap();
    let spool = LocalSpool::open_with_limits(spool_dir.path().join("spool"), limits).unwrap();
    let capture = capture_request(CaptureModeV1::MetadataOnly);
    capture_hook(
        br#"{"hook_event_name":"SessionStart","session_id":"session_fixture","source":"startup"}"#,
        &capture,
        &spool,
    )
    .unwrap();
    let request = configured_recipe_request(
        repo.path(),
        &spool,
        "invocation-supported-minimum",
        "synthetic-supported-minimum",
        vec!["/usr/bin/true".into()],
        10,
        4096,
    );
    let actions = ActionCaptureConfig {
        source: request.source.clone(),
        setup_digest: request.setup_digest.clone(),
        rules: Vec::new(),
        managed_invocations: vec![request.managed_invocation.clone()],
    };
    let observed = run_configured_recipe(request.clone(), &spool).unwrap();
    assert_eq!(observed.finish.outcome, ActionOutcomeV1::Succeeded);
    assert_eq!(observed.finish.result, CheckResultV1::Passed);
    assert_eq!(observed.exit_code, Some(0));
    let post = serde_json::to_vec(&serde_json::json!({
        "hook_event_name": "PostToolUse",
        "session_id": capture.observed_session_id,
        "tool_name": "Bash",
        "tool_use_id": "invocation-supported-minimum",
        "tool_input": {"command": request.managed_invocation},
        "tool_response": {"exit_code": 0},
    }))
    .unwrap();
    let report = capture_native_hook(&post, &capture, &actions, &spool).unwrap();
    assert_eq!(report.event, HookCaptureOutcome::Stored);
    assert_eq!(report.action, NativeActionOutcome::NotApplicable);
}

#[test]
fn action_capture_refuses_one_record_below_the_supported_minimum_before_writing() {
    let spool_dir = TempDir::new().unwrap();
    let root = spool_dir.path().join("spool");
    let limits = SpoolLimits {
        max_records: MIN_ACTION_CAPTURE_RECORDS - 1,
        max_total_bytes: MIN_ACTION_CAPTURE_BYTES,
        max_artifact_bytes: 16 * 1024,
    };
    let opened = (|| {
        limits.validate_action_capture()?;
        LocalSpool::open_with_limits(&root, limits)
    })();
    assert!(matches!(
        opened,
        Err(agentscm::change_capture::CaptureError::Invalid(_))
    ));
    assert!(!root.exists());
}

#[test]
fn configured_recipe_live_fence_excludes_run_and_reconcile_callers() {
    let repo = git_fixture();
    let spool_dir = TempDir::new().unwrap();
    let spool = LocalSpool::open(spool_dir.path().join("spool")).unwrap();
    let entered = repo.path().join("entered");
    let request = configured_recipe_request(
        repo.path(),
        &spool,
        "invocation-live-fence",
        "synthetic-live-fence",
        vec![
            "/bin/sh".into(),
            "-c".into(),
            format!("printf entered > {}; sleep 2", entered.display()),
        ],
        10,
        4096,
    );
    let worker_spool = spool.clone();
    let worker_request = request.clone();
    let worker = thread::spawn(move || run_configured_recipe(worker_request, &worker_spool));
    for _ in 0..200 {
        if entered.exists() {
            break;
        }
        thread::sleep(Duration::from_millis(10));
    }
    assert!(entered.exists(), "first caller never entered the child");
    assert!(matches!(
        run_configured_recipe(request.clone(), &spool),
        Err(agentscm::change_capture::CaptureError::Conflict(_))
    ));
    assert!(matches!(
        reconcile_configured_recipe(request, &spool).unwrap(),
        RecipeReconcileOutcome::Busy { .. }
    ));
    worker.join().unwrap().unwrap();
    assert_eq!(fs::read(entered).unwrap(), b"entered");
}

#[test]
fn configured_recipe_allows_replacement_inode_after_completed_attempt() {
    let repo = git_fixture();
    let spool_dir = TempDir::new().unwrap();
    let spool = LocalSpool::open(spool_dir.path().join("spool")).unwrap();
    let original = configured_recipe_request(
        repo.path(),
        &spool,
        "invocation-original-inode",
        "synthetic-inode-fence",
        vec!["/bin/sh".into(), "-c".into(), "exit 0".into()],
        10,
        4096,
    );
    run_configured_recipe(original, &spool).unwrap();

    let moved = repo.path().with_extension("replaced");
    fs::rename(repo.path(), &moved).unwrap();
    fs::create_dir(repo.path()).unwrap();
    run_git(repo.path(), &["init", "-b", "main"]);
    run_git(
        repo.path(),
        &["config", "user.email", "fixture@example.invalid"],
    );
    run_git(repo.path(), &["config", "user.name", "Synthetic Fixture"]);
    fs::write(repo.path().join("tracked.txt"), "replacement\n").unwrap();
    run_git(repo.path(), &["add", "tracked.txt"]);
    run_git(repo.path(), &["commit", "-m", "replacement"]);

    let replacement = configured_recipe_request(
        repo.path(),
        &spool,
        "invocation-replacement-inode",
        "synthetic-inode-fence",
        vec!["/bin/sh".into(), "-c".into(), "exit 0".into()],
        10,
        4096,
    );
    let replaced = run_configured_recipe(replacement, &spool).unwrap();
    assert_eq!(replaced.finish.result, CheckResultV1::Passed);
}

#[test]
fn completed_works_compact_without_growing_checkout_lock_records() {
    let repo = git_fixture();
    let canonical = repo.path().canonicalize().unwrap();
    let spool_dir = TempDir::new().unwrap();
    let spool = LocalSpool::open_with_limits(
        spool_dir.path().join("spool"),
        SpoolLimits {
            max_records: 48,
            max_total_bytes: 512 * 1024,
            max_artifact_bytes: 64 * 1024,
        },
    )
    .unwrap();
    let managed = "/synthetic/agentscm change-capture run-recipe --id repeated-work";
    let actions = ActionCaptureConfig {
        source: git_request(&canonical),
        setup_digest: "1".repeat(64),
        rules: Vec::new(),
        managed_invocations: vec![managed.into()],
    };
    let startup = CaptureRequest {
        operation_prefix: "capture:repeated:startup".into(),
        work_id: "wrk_repeated_00".into(),
        generation: 1,
        observed_session_id: "session_repeated".into(),
        canonical_session_id: "session_repeated".into(),
        parent_session_id: None,
        capture_mode: CaptureModeV1::MetadataOnly,
        live_capture_mode: CaptureModeV1::MetadataOnly,
    };
    capture_hook(
        br#"{"hook_event_name":"SessionStart","session_id":"session_repeated","source":"startup"}"#,
        &startup,
        &spool,
    )
    .unwrap();
    for envelope in spool.pending(100).unwrap() {
        spool.acknowledge(&envelope).unwrap();
    }
    spool.compact_acknowledged(100).unwrap();

    for index in 0..12 {
        let work_id = format!("wrk_repeated_{index:02}");
        let tool_use_id = format!("repeated_{index:02}");
        let capture = CaptureRequest {
            operation_prefix: format!("capture:repeated:{index:02}"),
            work_id: work_id.clone(),
            generation: 1,
            observed_session_id: "session_repeated".into(),
            canonical_session_id: "session_repeated".into(),
            parent_session_id: None,
            capture_mode: CaptureModeV1::MetadataOnly,
            live_capture_mode: CaptureModeV1::MetadataOnly,
        };
        let pre = serde_json::to_vec(&serde_json::json!({
            "hook_event_name": "PreToolUse",
            "session_id": capture.observed_session_id,
            "tool_name": "Bash",
            "tool_use_id": tool_use_id.clone(),
            "tool_input": {"command": managed},
        }))
        .unwrap();
        assert_eq!(
            capture_native_hook(&pre, &capture, &actions, &spool)
                .unwrap()
                .action,
            NativeActionOutcome::NotApplicable
        );
        let observed = run_configured_recipe(
            ConfiguredRecipeRunRequest {
                work_id,
                generation: 1,
                observed_session_id: "session_repeated".into(),
                session_id: "session_repeated".into(),
                source: git_request(&canonical),
                setup_digest: "1".repeat(64),
                recipe: ConfiguredRecipeV1 {
                    id: "repeated-work".into(),
                    argv: vec!["/bin/sh".into(), "-c".into(), "exit 0".into()],
                    coverage: CoverageV1::Complete,
                    timeout_seconds: 10,
                    max_output_bytes: 4096,
                },
                managed_invocation: managed.into(),
            },
            &spool,
        )
        .unwrap();
        assert_eq!(observed.finish.result, CheckResultV1::Passed);
        let post = serde_json::to_vec(&serde_json::json!({
            "hook_event_name": "PostToolUse",
            "session_id": capture.observed_session_id,
            "tool_name": "Bash",
            "tool_use_id": tool_use_id,
            "tool_input": {"command": managed},
            "tool_response": {"exit_code": 0},
        }))
        .unwrap();
        assert_eq!(
            capture_native_hook(&post, &capture, &actions, &spool)
                .unwrap()
                .action,
            NativeActionOutcome::NotApplicable
        );
        for envelope in spool.pending(100).unwrap() {
            spool.acknowledge(&envelope).unwrap();
        }
        spool.compact_acknowledged(100).unwrap();
        assert_eq!(
            fs::read_dir(spool.root().join("recipe-fences"))
                .unwrap()
                .count(),
            2
        );
    }
}

#[test]
fn configured_recipe_rejects_replaced_permanent_lock_inode() {
    let repo = git_fixture();
    let spool_dir = TempDir::new().unwrap();
    let spool = LocalSpool::open(spool_dir.path().join("spool")).unwrap();
    let original = configured_recipe_request(
        repo.path(),
        &spool,
        "invocation-original-lock",
        "synthetic-lock-fence",
        vec!["/bin/sh".into(), "-c".into(), "exit 0".into()],
        10,
        4096,
    );
    run_configured_recipe(original, &spool).unwrap();
    let lock = fs::read_dir(spool.root().join("recipe-fences"))
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .find(|path| {
            path.extension()
                .is_some_and(|extension| extension == "lock")
        })
        .unwrap();
    // Keep the old inode allocated so filesystems cannot reuse its number for the replacement.
    let retired_lock = spool_dir.path().join("retired-recipe.lock");
    fs::rename(&lock, &retired_lock).unwrap();
    fs::write(&lock, []).unwrap();
    fs::set_permissions(&lock, fs::Permissions::from_mode(0o600)).unwrap();
    assert_ne!(
        fs::metadata(&lock).unwrap().ino(),
        fs::metadata(&retired_lock).unwrap().ino()
    );

    let retry = configured_recipe_request(
        repo.path(),
        &spool,
        "invocation-replaced-lock",
        "synthetic-lock-fence",
        vec!["/bin/sh".into(), "-c".into(), "exit 0".into()],
        10,
        4096,
    );
    assert!(matches!(
        run_configured_recipe(retry, &spool),
        Err(agentscm::change_capture::CaptureError::Conflict(_))
    ));
}

#[test]
fn configured_recipe_refuses_ambiguous_managed_invocations() {
    let repo = git_fixture();
    let spool_dir = TempDir::new().unwrap();
    let spool = LocalSpool::open(spool_dir.path().join("spool")).unwrap();
    let marker = repo.path().join("ambiguous-must-not-run");
    let request = configured_recipe_request(
        repo.path(),
        &spool,
        "ambiguous-first",
        "synthetic-ambiguous",
        vec![
            "/bin/sh".into(),
            "-c".into(),
            format!("printf launched > {}", marker.display()),
        ],
        10,
        4096,
    );
    seed_configured_recipe_marker(repo.path(), &spool, "ambiguous-second", &request);
    assert!(matches!(
        run_configured_recipe(request, &spool),
        Err(agentscm::change_capture::CaptureError::Conflict(_))
    ));
    assert!(!marker.exists());
}

#[test]
fn observed_order_exhaustion_retains_hook_fact_and_one_priority_pause_gap() {
    let repo = git_fixture();
    let spool_dir = TempDir::new().unwrap();
    let spool = LocalSpool::open_with_limits(
        spool_dir.path().join("spool"),
        SpoolLimits {
            max_records: 128,
            max_total_bytes: 16 * 1024,
            max_artifact_bytes: 4096,
        },
    )
    .unwrap();
    let mut exhausted = false;
    for index in 0..1000 {
        if spool
            .observed_order_for(&format!("native:start:ledger-fill-{index:04}"))
            .is_err()
        {
            exhausted = true;
            break;
        }
    }
    assert!(exhausted, "fixture must exhaust the bounded order ledger");

    let request = capture_request(CaptureModeV1::MetadataOnly);
    let actions = ActionCaptureConfig {
        source: git_request(&repo.path().canonicalize().unwrap()),
        setup_digest: "1".repeat(64),
        managed_invocations: Vec::new(),
        rules: vec![ActionRuleV1 {
            native_tool: "Bash".into(),
            kind: ActionKindV1::Execution,
            coverage: CoverageV1::Partial,
            result_source: NativeResultSourceV1::ProcessExitCode,
            recipe_digest: None,
        }],
    };
    let pre = br#"{"hook_event_name":"PreToolUse","session_id":"session_fixture","tool_name":"Bash","tool_use_id":"order_exhausted","tool_input":{"command":"true"}}"#;
    let first = capture_native_hook(pre, &request, &actions, &spool).unwrap();
    assert_eq!(first.action, NativeActionOutcome::Interrupted);
    assert_eq!(first.event, HookCaptureOutcome::Stored);
    assert_eq!(first.warnings.len(), 1);
    let pending = spool.pending(20).unwrap();
    assert!(pending.iter().any(|envelope| matches!(
        envelope,
        CaptureEnvelopeV1::EventBatch(batch)
            if batch.events.iter().any(|event| event.kind == CapturedEventKindV1::ToolStarted)
    )));
    assert_eq!(
        pending
            .iter()
            .filter(|envelope| matches!(
                envelope,
                CaptureEnvelopeV1::EventBatch(batch)
                    if batch.events.iter().any(|event| event.gap.as_ref().is_some_and(|gap|
                        gap.reason == agentscm::change_record::GapReasonV1::CaptureInterrupted
                    ))
            ))
            .count(),
        1
    );
    assert!(
        !pending
            .iter()
            .any(|envelope| matches!(envelope, CaptureEnvelopeV1::ActionStart(_)))
    );

    let replay = capture_native_hook(pre, &request, &actions, &spool).unwrap();
    assert_eq!(replay.action, NativeActionOutcome::Interrupted);
    assert_eq!(replay.event, HookCaptureOutcome::Duplicate);
    assert_eq!(spool.pending(20).unwrap().len(), pending.len());

    // After compaction only digests remain; the pause marker is re-attempted on replay and a
    // new receipt time must neither conflict nor drop the hook's own event.
    acknowledge_and_compact(&spool);
    std::thread::sleep(Duration::from_millis(1100));
    let later = capture_native_hook(pre, &request, &actions, &spool).unwrap();
    assert_eq!(later.action, NativeActionOutcome::Interrupted);
    assert_eq!(later.event, HookCaptureOutcome::Duplicate);
    assert!(
        !later
            .warnings
            .iter()
            .any(|warning| warning.contains("marker failed"))
    );
    let next = br#"{"hook_event_name":"PreToolUse","session_id":"session_fixture","tool_name":"Bash","tool_use_id":"order_exhausted_2","tool_input":{"command":"true"}}"#;
    let next = capture_native_hook(next, &request, &actions, &spool).unwrap();
    assert_eq!(next.event, HookCaptureOutcome::Stored);
    assert!(
        !next
            .warnings
            .iter()
            .any(|warning| warning.contains("marker failed"))
    );
}

#[test]
fn managed_runner_without_durable_start_falls_back_to_explicit_unknown_action() {
    let repo = git_fixture();
    let spool_dir = TempDir::new().unwrap();
    let spool = LocalSpool::open(spool_dir.path().join("spool")).unwrap();
    let request = capture_request(CaptureModeV1::MetadataOnly);
    let managed = "/synthetic/agentscm change-capture run-recipe --id exact";
    let actions = ActionCaptureConfig {
        source: git_request(&repo.path().canonicalize().unwrap()),
        setup_digest: "1".repeat(64),
        rules: vec![ActionRuleV1 {
            native_tool: "Bash".into(),
            kind: ActionKindV1::Execution,
            coverage: CoverageV1::Partial,
            result_source: NativeResultSourceV1::ProcessExitCode,
            recipe_digest: None,
        }],
        managed_invocations: vec![managed.into()],
    };
    let pre = format!(
        "{{\"hook_event_name\":\"PreToolUse\",\"session_id\":\"session_fixture\",\"tool_name\":\"Bash\",\"tool_use_id\":\"managed_exact\",\"tool_input\":{{\"command\":{}}}}}",
        serde_json::to_string(managed).unwrap()
    );
    let post = format!(
        "{{\"hook_event_name\":\"PostToolUse\",\"session_id\":\"session_fixture\",\"tool_name\":\"Bash\",\"tool_use_id\":\"managed_exact\",\"tool_input\":{{\"command\":{}}},\"tool_response\":{{\"exit_code\":0}}}}",
        serde_json::to_string(managed).unwrap()
    );
    assert_eq!(
        capture_native_hook(pre.as_bytes(), &request, &actions, &spool)
            .unwrap()
            .action,
        NativeActionOutcome::NotApplicable
    );
    assert_eq!(
        capture_native_hook(post.as_bytes(), &request, &actions, &spool)
            .unwrap()
            .action,
        NativeActionOutcome::Finished(AppendOutcome::Stored)
    );
    let pending = spool.pending(20).unwrap();
    assert!(pending.iter().any(|envelope| matches!(
        envelope,
        CaptureEnvelopeV1::ActionStart(start)
            if start.kind == ActionKindV1::Execution
                && start.recipe_provenance == RecipeProvenanceV1::ObservedNativeInput
    )));
    assert!(pending.iter().any(|envelope| matches!(
        envelope,
        CaptureEnvelopeV1::ActionFinish(finish)
            if finish.result == CheckResultV1::Unknown
    )));

    let suffix = format!("{managed} --extra");
    let other = format!(
        "{{\"hook_event_name\":\"PreToolUse\",\"session_id\":\"session_fixture\",\"tool_name\":\"Bash\",\"tool_use_id\":\"managed_suffix\",\"tool_input\":{{\"command\":{}}}}}",
        serde_json::to_string(&suffix).unwrap()
    );
    assert!(matches!(
        capture_native_hook(other.as_bytes(), &request, &actions, &spool)
            .unwrap()
            .action,
        NativeActionOutcome::Started(AppendOutcome::Stored)
    ));
}

#[test]
fn managed_post_target_change_keeps_original_pre_binding_in_receipts() {
    let repo = git_fixture();
    let spool_dir = TempDir::new().unwrap();
    let spool = LocalSpool::open(spool_dir.path().join("spool")).unwrap();
    let original = capture_request(CaptureModeV1::MetadataOnly);
    let managed = "/synthetic/agentscm change-capture run-recipe --id target-change";
    let actions = ActionCaptureConfig {
        source: git_request(&repo.path().canonicalize().unwrap()),
        setup_digest: "1".repeat(64),
        rules: Vec::new(),
        managed_invocations: vec![managed.into()],
    };
    let pre = serde_json::to_vec(&serde_json::json!({
        "hook_event_name": "PreToolUse",
        "session_id": original.observed_session_id,
        "tool_name": "Bash",
        "tool_use_id": "managed_target_change",
        "tool_input": {"command": managed},
    }))
    .unwrap();
    capture_native_hook(&pre, &original, &actions, &spool).unwrap();

    let mut changed = original.clone();
    changed.work_id = "wrk_changed_target".into();
    changed.generation = 2;
    changed.canonical_session_id = "session_changed_target".into();
    let post = serde_json::to_vec(&serde_json::json!({
        "hook_event_name": "PostToolUse",
        "session_id": changed.observed_session_id,
        "tool_name": "Bash",
        "tool_use_id": "managed_target_change",
        "tool_input": {"command": managed},
        "tool_response": {"exit_code": 0},
    }))
    .unwrap();
    let report = capture_native_hook(&post, &changed, &actions, &spool).unwrap();
    assert_eq!(
        report.action,
        NativeActionOutcome::Finished(AppendOutcome::Stored)
    );
    let pending = spool.pending(50).unwrap();
    assert!(!pending.iter().any(|envelope| matches!(
        envelope,
        CaptureEnvelopeV1::ActionStart(start)
            if start.work_id == changed.work_id || start.session_id == changed.canonical_session_id
    )));
    assert!(!pending.iter().any(|envelope| matches!(
        envelope,
        CaptureEnvelopeV1::EventBatch(batch)
            if batch.work_id == changed.work_id
                || batch.session.session_id == changed.canonical_session_id
    )));
    assert!(pending.iter().any(|envelope| matches!(
        envelope,
        CaptureEnvelopeV1::EventBatch(batch)
            if batch.work_id == original.work_id
                && batch.generation == original.generation
                && batch.events.iter().any(|event| event.kind == CapturedEventKindV1::ToolStarted)
    )));
    assert_eq!(
        fs::read_dir(spool.root().join("managed-invocations"))
            .unwrap()
            .count(),
        0
    );
    let association_path = fs::read_dir(spool.root().join("associations"))
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    let association: serde_json::Value =
        serde_json::from_slice(&fs::read(association_path).unwrap()).unwrap();
    assert_eq!(association["work_id"], original.work_id);
    assert_eq!(
        association["canonical_session_id"],
        original.canonical_session_id
    );
}

#[test]
fn concurrent_first_managed_pre_claims_one_target_before_any_phase_mutation() {
    let repo = git_fixture();
    let canonical = repo.path().canonicalize().unwrap();
    let spool_dir = TempDir::new().unwrap();
    let spool = LocalSpool::open(spool_dir.path().join("spool")).unwrap();
    let managed = "/synthetic/agentscm change-capture run-recipe --id concurrent-first";
    let actions = ActionCaptureConfig {
        source: git_request(&canonical),
        setup_digest: "1".repeat(64),
        rules: Vec::new(),
        managed_invocations: vec![managed.into()],
    };
    let input = Arc::new(
        serde_json::to_vec(&serde_json::json!({
            "hook_event_name": "PreToolUse",
            "session_id": "session_fixture",
            "tool_name": "Bash",
            "tool_use_id": "concurrent_first",
            "tool_input": {"command": managed},
        }))
        .unwrap(),
    );
    let mut a = capture_request(CaptureModeV1::MetadataOnly);
    a.work_id = "wrk_concurrent_a".into();
    a.canonical_session_id = "canonical_concurrent_a".into();
    let mut b = a.clone();
    b.work_id = "wrk_concurrent_b".into();
    b.canonical_session_id = "canonical_concurrent_b".into();
    let barrier = Arc::new(Barrier::new(3));
    let launch = |request: CaptureRequest| {
        let barrier = barrier.clone();
        let input = input.clone();
        let actions = actions.clone();
        let spool = spool.clone();
        thread::spawn(move || {
            barrier.wait();
            capture_native_hook(&input, &request, &actions, &spool)
        })
    };
    let first = launch(a.clone());
    let second = launch(b.clone());
    barrier.wait();
    let reports = [
        first.join().unwrap().unwrap(),
        second.join().unwrap().unwrap(),
    ];
    assert!(
        reports
            .iter()
            .all(|report| report.action == NativeActionOutcome::NotApplicable)
    );
    assert_eq!(
        reports
            .iter()
            .filter(|report| report.event == HookCaptureOutcome::Stored)
            .count(),
        1
    );
    assert_eq!(
        reports
            .iter()
            .filter(|report| report.event == HookCaptureOutcome::Duplicate)
            .count(),
        1
    );
    let pending = spool.pending(50).unwrap();
    let works: std::collections::BTreeSet<_> = pending
        .iter()
        .filter_map(|envelope| match envelope {
            CaptureEnvelopeV1::EventBatch(batch)
                if batch.events.iter().any(|event| {
                    event.kind == CapturedEventKindV1::ToolStarted
                        && event
                            .tool
                            .as_ref()
                            .and_then(|tool| tool.request_id.as_deref())
                            == Some("concurrent_first")
                }) =>
            {
                Some(batch.work_id.as_str())
            }
            _ => None,
        })
        .collect();
    assert_eq!(works.len(), 1);
    assert!(works.contains(a.work_id.as_str()) || works.contains(b.work_id.as_str()));
    let retained = retained_bytes(spool.root());
    let winner = *works.first().unwrap();
    let loser = if winner == a.work_id.as_str() {
        b.work_id.as_bytes()
    } else {
        a.work_id.as_bytes()
    };
    assert!(!contains_bytes(&retained, loser));
}

#[test]
fn managed_phase_fingerprint_rejects_changed_typed_facts_and_premature_post() {
    let repo = git_fixture();
    let canonical = repo.path().canonicalize().unwrap();
    let spool_dir = TempDir::new().unwrap();
    let spool = LocalSpool::open(spool_dir.path().join("spool")).unwrap();
    let request = capture_request(CaptureModeV1::MetadataOnly);
    let managed = "/synthetic/agentscm change-capture run-recipe --id phase-facts";
    let actions = ActionCaptureConfig {
        source: git_request(&canonical),
        setup_digest: "1".repeat(64),
        rules: Vec::new(),
        managed_invocations: vec![managed.into()],
    };
    let pre = |timestamp: &str, command: &str| {
        serde_json::to_vec(&serde_json::json!({
            "hook_event_name": "PreToolUse",
            "session_id": request.observed_session_id,
            "timestamp": timestamp,
            "tool_name": "Bash",
            "tool_use_id": "phase_facts_pre",
            "tool_input": {"command": command, "description": "exact"},
        }))
        .unwrap()
    };
    capture_native_hook(
        &pre("2026-10-24T01:00:00Z", managed),
        &request,
        &actions,
        &spool,
    )
    .unwrap();
    assert_eq!(
        capture_native_hook(
            &pre("2026-10-24T02:00:00Z", managed),
            &request,
            &actions,
            &spool,
        )
        .unwrap()
        .event,
        HookCaptureOutcome::Duplicate
    );
    assert!(matches!(
        capture_native_hook(
            &pre("2026-10-24T03:00:00Z", &format!("{managed} --changed")),
            &request,
            &actions,
            &spool,
        ),
        Err(agentscm::change_capture::CaptureError::Conflict(_))
    ));

    let post = |exit_code: i64| {
        serde_json::to_vec(&serde_json::json!({
            "hook_event_name": "PostToolUse",
            "session_id": request.observed_session_id,
            "tool_name": "Bash",
            "tool_use_id": "premature_post",
            "tool_input": {"command": managed},
            "tool_response": {"exit_code": exit_code},
        }))
        .unwrap()
    };
    let premature = capture_native_hook(&post(0), &request, &actions, &spool).unwrap();
    assert_eq!(premature.action, NativeActionOutcome::Interrupted);
    assert!(matches!(
        capture_native_hook(&post(7), &request, &actions, &spool),
        Err(agentscm::change_capture::CaptureError::Conflict(_))
    ));
    let late_pre = serde_json::to_vec(&serde_json::json!({
        "hook_event_name": "PreToolUse",
        "session_id": request.observed_session_id,
        "tool_name": "Bash",
        "tool_use_id": "premature_post",
        "tool_input": {"command": managed},
    }))
    .unwrap();
    capture_native_hook(&late_pre, &request, &actions, &spool).unwrap();
    assert!(matches!(
        capture_native_hook(&post(0), &request, &actions, &spool)
            .unwrap()
            .action,
        NativeActionOutcome::Finished(AppendOutcome::Stored)
            | NativeActionOutcome::Finished(AppendOutcome::Duplicate)
    ));
    assert!(matches!(
        capture_native_hook(&post(7), &request, &actions, &spool),
        Err(agentscm::change_capture::CaptureError::Conflict(_))
    ));

    let opposite_pre = serde_json::to_vec(&serde_json::json!({
        "hook_event_name": "PreToolUse",
        "session_id": request.observed_session_id,
        "tool_name": "Bash",
        "tool_use_id": "opposite_phase_pre",
        "tool_input": {"command": managed, "description": "original"},
    }))
    .unwrap();
    capture_native_hook(&opposite_pre, &request, &actions, &spool).unwrap();
    let mut before_changed_post = retained_bytes(spool.root());
    before_changed_post.sort();
    for changed_post in [
        serde_json::json!({
            "hook_event_name": "PostToolUse",
            "session_id": request.observed_session_id,
            "tool_name": "Bash",
            "tool_use_id": "opposite_phase_pre",
            "tool_input": {"command": format!("{managed} --changed"), "description": "original"},
            "tool_response": {"exit_code": 0},
        }),
        serde_json::json!({
            "hook_event_name": "PostToolUse",
            "session_id": request.observed_session_id,
            "tool_name": "Write",
            "tool_use_id": "opposite_phase_pre",
            "tool_input": {"command": managed, "description": "original"},
            "tool_response": {"exit_code": 0},
        }),
    ] {
        assert!(matches!(
            capture_native_hook(
                &serde_json::to_vec(&changed_post).unwrap(),
                &request,
                &actions,
                &spool,
            ),
            Err(agentscm::change_capture::CaptureError::Conflict(_))
        ));
        let mut after = retained_bytes(spool.root());
        after.sort();
        assert_eq!(after, before_changed_post);
    }

    let opposite_post = serde_json::to_vec(&serde_json::json!({
        "hook_event_name": "PostToolUse",
        "session_id": request.observed_session_id,
        "tool_name": "Bash",
        "tool_use_id": "opposite_phase_post",
        "tool_input": {"command": managed, "description": "original"},
        "tool_response": {"exit_code": 0},
    }))
    .unwrap();
    assert_eq!(
        capture_native_hook(&opposite_post, &request, &actions, &spool)
            .unwrap()
            .action,
        NativeActionOutcome::Interrupted
    );
    let mut before_changed_pre = retained_bytes(spool.root());
    before_changed_pre.sort();
    let changed_pre = serde_json::to_vec(&serde_json::json!({
        "hook_event_name": "PreToolUse",
        "session_id": request.observed_session_id,
        "tool_name": "Bash",
        "tool_use_id": "opposite_phase_post",
        "tool_input": {"command": format!("{managed} --changed"), "description": "original"},
    }))
    .unwrap();
    assert!(matches!(
        capture_native_hook(&changed_pre, &request, &actions, &spool),
        Err(agentscm::change_capture::CaptureError::Conflict(_))
    ));
    let mut after_changed_pre = retained_bytes(spool.root());
    after_changed_pre.sort();
    assert_eq!(after_changed_pre, before_changed_pre);
}

#[test]
fn first_native_route_claim_is_authoritative_for_managed_and_ordinary_winners() {
    for managed_first in [false, true] {
        let repo = git_fixture();
        let canonical = repo.path().canonicalize().unwrap();
        let spool_dir = TempDir::new().unwrap();
        let spool = LocalSpool::open(spool_dir.path().join("spool")).unwrap();
        let command = "/synthetic/agentscm change-capture run-recipe --id route-claim";
        let mut ordinary_request = capture_request(CaptureModeV1::MetadataOnly);
        ordinary_request.work_id = "wrk_route_ordinary".into();
        ordinary_request.canonical_session_id = "canonical_route_ordinary".into();
        let mut managed_request = ordinary_request.clone();
        managed_request.work_id = "wrk_route_managed".into();
        managed_request.generation = 2;
        managed_request.canonical_session_id = "canonical_route_managed".into();
        let ordinary = ActionCaptureConfig {
            source: git_request(&canonical),
            setup_digest: "1".repeat(64),
            rules: vec![ActionRuleV1 {
                native_tool: "Bash".into(),
                kind: ActionKindV1::Execution,
                coverage: CoverageV1::Partial,
                result_source: NativeResultSourceV1::Unverified,
                recipe_digest: None,
            }],
            managed_invocations: Vec::new(),
        };
        let managed = ActionCaptureConfig {
            source: git_request(&canonical),
            setup_digest: "2".repeat(64),
            rules: Vec::new(),
            managed_invocations: vec![command.into()],
        };
        let pre = serde_json::to_vec(&serde_json::json!({
            "hook_event_name": "PreToolUse",
            "session_id": ordinary_request.observed_session_id,
            "timestamp": "2026-10-24T01:00:00Z",
            "tool_name": "Bash",
            "tool_use_id": "route_claim",
            "tool_input": {"command": command},
        }))
        .unwrap();
        let (first_request, first_actions, second_request, second_actions) = if managed_first {
            (&managed_request, &managed, &ordinary_request, &ordinary)
        } else {
            (&ordinary_request, &ordinary, &managed_request, &managed)
        };
        let first = capture_native_hook(&pre, first_request, first_actions, &spool).unwrap();
        let mut before_second = retained_bytes(spool.root());
        before_second.sort();
        let second = capture_native_hook(&pre, second_request, second_actions, &spool).unwrap();
        let mut after_second = retained_bytes(spool.root());
        after_second.sort();
        assert_eq!(after_second, before_second);
        assert_eq!(second.event, HookCaptureOutcome::Duplicate);
        if managed_first {
            assert_eq!(first.action, NativeActionOutcome::NotApplicable);
            assert_eq!(second.action, NativeActionOutcome::NotApplicable);
        } else {
            assert!(matches!(first.action, NativeActionOutcome::Started(_)));
            assert!(matches!(
                second.action,
                NativeActionOutcome::Started(AppendOutcome::Duplicate)
            ));
        }
        let retained = retained_bytes(spool.root());
        assert!(contains_bytes(&retained, first_request.work_id.as_bytes()));
        assert!(!contains_bytes(
            &retained,
            second_request.work_id.as_bytes()
        ));
    }
}

#[test]
fn concurrent_first_pre_claims_one_original_binding_before_any_target_mutation() {
    let repo = git_fixture();
    let canonical = repo.path().canonicalize().unwrap();
    let spool_dir = TempDir::new().unwrap();
    let spool = LocalSpool::open(spool_dir.path().join("spool")).unwrap();
    let command = "/synthetic/agentscm change-capture run-recipe --id concurrent-claim";
    let actions = ActionCaptureConfig {
        source: git_request(&canonical),
        setup_digest: "1".repeat(64),
        rules: Vec::new(),
        managed_invocations: vec![command.into()],
    };
    let mut left = capture_request(CaptureModeV1::MetadataOnly);
    left.work_id = "wrk_concurrent_left".into();
    left.canonical_session_id = "canonical_concurrent_left".into();
    let mut right = left.clone();
    right.work_id = "wrk_concurrent_right".into();
    right.generation = 2;
    right.canonical_session_id = "canonical_concurrent_right".into();
    let pre = serde_json::to_vec(&serde_json::json!({
        "hook_event_name": "PreToolUse",
        "session_id": left.observed_session_id,
        "tool_name": "Bash",
        "tool_use_id": "concurrent_claim",
        "tool_input": {"command": command},
    }))
    .unwrap();
    let barrier = Arc::new(Barrier::new(3));
    let spawn = |request: CaptureRequest| {
        let barrier = barrier.clone();
        let actions = actions.clone();
        let spool = spool.clone();
        let pre = pre.clone();
        thread::spawn(move || {
            barrier.wait();
            capture_native_hook(&pre, &request, &actions, &spool)
        })
    };
    let left_result = spawn(left.clone());
    let right_result = spawn(right.clone());
    barrier.wait();
    left_result.join().unwrap().unwrap();
    right_result.join().unwrap().unwrap();
    let retained = retained_bytes(spool.root());
    let left_present = contains_bytes(&retained, left.work_id.as_bytes());
    let right_present = contains_bytes(&retained, right.work_id.as_bytes());
    assert_ne!(left_present, right_present);
    let mut works: Vec<_> = spool
        .pending(100)
        .unwrap()
        .iter()
        .map(|envelope| match envelope {
            CaptureEnvelopeV1::EventBatch(batch) => batch.work_id.clone(),
            CaptureEnvelopeV1::ActionStart(start) => start.work_id.clone(),
            CaptureEnvelopeV1::ActionFinish(finish) => finish.work_id.clone(),
            CaptureEnvelopeV1::Usage(usage) => usage.work_id.clone(),
        })
        .collect();
    works.sort();
    works.dedup();
    assert_eq!(works.len(), 1);
}

#[test]
fn managed_completion_does_not_reset_a_later_ordinary_work_association() {
    let repo = git_fixture();
    let canonical = repo.path().canonicalize().unwrap();
    let spool_dir = TempDir::new().unwrap();
    let spool = LocalSpool::open(spool_dir.path().join("spool")).unwrap();
    let original = capture_request(CaptureModeV1::MetadataOnly);
    let managed = "/synthetic/agentscm change-capture run-recipe --id association-stable";
    let actions = ActionCaptureConfig {
        source: git_request(&canonical),
        setup_digest: "1".repeat(64),
        rules: Vec::new(),
        managed_invocations: vec![managed.into()],
    };
    let pre = serde_json::to_vec(&serde_json::json!({
        "hook_event_name": "PreToolUse",
        "session_id": original.observed_session_id,
        "tool_name": "Bash",
        "tool_use_id": "association_stable",
        "tool_input": {"command": managed},
    }))
    .unwrap();
    capture_native_hook(&pre, &original, &actions, &spool).unwrap();

    let mut later = original.clone();
    later.work_id = "wrk_later_b".into();
    later.generation = 2;
    later.canonical_session_id = "canonical_later_b".into();
    let ordinary = serde_json::to_vec(&serde_json::json!({
        "hook_event_name": "UserPromptSubmit",
        "session_id": later.observed_session_id,
        "prompt_id": "ordinary_b",
        "prompt": "private",
    }))
    .unwrap();
    capture_hook(&ordinary, &later, &spool).unwrap();

    let post = serde_json::to_vec(&serde_json::json!({
        "hook_event_name": "PostToolUse",
        "session_id": original.observed_session_id,
        "tool_name": "Bash",
        "tool_use_id": "association_stable",
        "tool_input": {"command": managed},
        "tool_response": {"exit_code": 0},
    }))
    .unwrap();
    assert!(matches!(
        capture_native_hook(&post, &original, &actions, &spool)
            .unwrap()
            .action,
        NativeActionOutcome::Finished(AppendOutcome::Stored)
    ));
    let replay = capture_native_hook(&post, &later, &actions, &spool).unwrap();
    assert_eq!(
        replay.action,
        NativeActionOutcome::Finished(AppendOutcome::Duplicate)
    );
    let association_path = fs::read_dir(spool.root().join("associations"))
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    let association: serde_json::Value =
        serde_json::from_slice(&fs::read(association_path).unwrap()).unwrap();
    assert_eq!(association["work_id"], later.work_id);
    assert_eq!(association["generation"], later.generation);
    assert_eq!(
        association["canonical_session_id"],
        later.canonical_session_id
    );
    assert!(!spool.pending(100).unwrap().iter().any(|envelope| matches!(
        envelope,
        CaptureEnvelopeV1::ActionStart(start) if start.work_id == later.work_id
    )));
}

#[test]
fn completed_managed_occurrence_replays_original_facts_before_and_after_compaction() {
    for compact in [false, true] {
        let repo = git_fixture();
        let canonical = repo.path().canonicalize().unwrap();
        let spool_dir = TempDir::new().unwrap();
        let spool = LocalSpool::open(spool_dir.path().join("spool")).unwrap();
        let marker = repo.path().join("managed-replay-marker");
        let managed = "/synthetic/agentscm change-capture run-recipe --id replay-stable";
        let original = capture_request(CaptureModeV1::MetadataOnly);
        let actions = ActionCaptureConfig {
            source: git_request(&canonical),
            setup_digest: "1".repeat(64),
            rules: Vec::new(),
            managed_invocations: vec![managed.into()],
        };
        let pre = serde_json::to_vec(&serde_json::json!({
            "hook_event_name": "PreToolUse",
            "session_id": original.observed_session_id,
            "timestamp": "2026-10-24T01:00:00Z",
            "tool_name": "Bash",
            "tool_use_id": "managed_replay_stable",
            "tool_input": {"command": managed},
        }))
        .unwrap();
        assert_eq!(
            capture_native_hook(&pre, &original, &actions, &spool)
                .unwrap()
                .action,
            NativeActionOutcome::NotApplicable
        );
        let run = ConfiguredRecipeRunRequest {
            work_id: original.work_id.clone(),
            generation: original.generation,
            observed_session_id: original.observed_session_id.clone(),
            session_id: original.canonical_session_id.clone(),
            source: git_request(&canonical),
            setup_digest: actions.setup_digest.clone(),
            recipe: ConfiguredRecipeV1 {
                id: "replay-stable".into(),
                argv: vec![
                    "/bin/sh".into(),
                    "-c".into(),
                    format!("printf x >> {}", marker.display()),
                ],
                coverage: CoverageV1::Complete,
                timeout_seconds: 10,
                max_output_bytes: 4096,
            },
            managed_invocation: managed.into(),
        };
        assert_eq!(
            run_configured_recipe(run.clone(), &spool)
                .unwrap()
                .finish
                .result,
            CheckResultV1::Passed
        );
        let mut changed = original.clone();
        changed.work_id = "wrk_replay_changed".into();
        changed.generation = 2;
        changed.canonical_session_id = "session_replay_changed".into();
        let pre_before_post = serde_json::to_vec(&serde_json::json!({
            "hook_event_name": "PreToolUse",
            "session_id": changed.observed_session_id,
            "timestamp": "2026-10-24T01:30:00Z",
            "tool_name": "Bash",
            "tool_use_id": "managed_replay_stable",
            "tool_input": {"command": managed},
        }))
        .unwrap();
        let before_post_replay =
            capture_native_hook(&pre_before_post, &changed, &actions, &spool).unwrap();
        assert_eq!(before_post_replay.event, HookCaptureOutcome::Duplicate);
        assert_eq!(
            before_post_replay.action,
            NativeActionOutcome::NotApplicable
        );
        assert_eq!(
            fs::read_dir(spool.root().join("managed-invocations"))
                .unwrap()
                .count(),
            1
        );
        assert!(!contains_bytes(
            &retained_bytes(spool.root()),
            changed.work_id.as_bytes()
        ));
        let post = serde_json::to_vec(&serde_json::json!({
            "hook_event_name": "PostToolUse",
            "session_id": original.observed_session_id,
            "timestamp": "2026-10-24T01:00:01Z",
            "tool_name": "Bash",
            "tool_use_id": "managed_replay_stable",
            "tool_input": {"command": managed},
            "tool_response": {"exit_code": 0},
        }))
        .unwrap();
        assert_eq!(
            capture_native_hook(&post, &original, &actions, &spool)
                .unwrap()
                .action,
            NativeActionOutcome::NotApplicable
        );
        assert_eq!(
            fs::read_dir(spool.root().join("managed-invocations"))
                .unwrap()
                .count(),
            0
        );

        let replay = |event: &str, timestamp: &str| {
            let mut value = serde_json::json!({
                "hook_event_name": event,
                "session_id": changed.observed_session_id,
                "timestamp": timestamp,
                "tool_name": "Bash",
                "tool_use_id": "managed_replay_stable",
                "tool_input": {"command": managed},
            });
            if event != "PreToolUse" {
                value["tool_response"] = serde_json::json!({"exit_code": 0});
            }
            serde_json::to_vec(&value).unwrap()
        };
        let assert_replays_without_mutation = |pre_time: &str, post_time: &str| {
            let mut before = retained_bytes(spool.root());
            before.sort();
            let pre_report =
                capture_native_hook(&replay("PreToolUse", pre_time), &changed, &actions, &spool)
                    .unwrap();
            assert_eq!(pre_report.event, HookCaptureOutcome::Duplicate);
            assert_eq!(pre_report.action, NativeActionOutcome::NotApplicable);
            let post_report = capture_native_hook(
                &replay("PostToolUse", post_time),
                &changed,
                &actions,
                &spool,
            )
            .unwrap();
            assert_eq!(post_report.event, HookCaptureOutcome::Duplicate);
            assert_eq!(post_report.action, NativeActionOutcome::NotApplicable);
            let mut after = retained_bytes(spool.root());
            after.sort();
            assert_eq!(after, before);
        };
        assert_replays_without_mutation("2026-10-24T02:00:00Z", "2026-10-24T02:00:01Z");
        let assert_changed_facts_conflict = || {
            let changed_post = serde_json::to_vec(&serde_json::json!({
                "hook_event_name": "PostToolUse",
                "session_id": changed.observed_session_id,
                "timestamp": "2026-10-24T04:00:00Z",
                "tool_name": "Bash",
                "tool_use_id": "managed_replay_stable",
                "tool_input": {"command": managed},
                "tool_response": {"exit_code": 7},
            }))
            .unwrap();
            let mut before = retained_bytes(spool.root());
            before.sort();
            assert!(matches!(
                capture_native_hook(&changed_post, &changed, &actions, &spool),
                Err(agentscm::change_capture::CaptureError::Conflict(_))
            ));
            let mut after = retained_bytes(spool.root());
            after.sort();
            assert_eq!(after, before);
        };
        assert_changed_facts_conflict();

        if compact {
            for envelope in spool.pending(100).unwrap() {
                spool.acknowledge(&envelope).unwrap();
            }
            spool.compact_acknowledged(100).unwrap();
            assert_replays_without_mutation("2026-10-24T03:00:00Z", "2026-10-24T03:00:01Z");
            assert_changed_facts_conflict();
        }

        let mut changed_run = run;
        changed_run.work_id = changed.work_id.clone();
        changed_run.generation = changed.generation;
        changed_run.session_id = changed.canonical_session_id.clone();
        assert!(run_configured_recipe(changed_run, &spool).is_err());
        assert_eq!(fs::read(&marker).unwrap(), b"x");
        let retained = retained_bytes(spool.root());
        assert!(!contains_bytes(&retained, changed.work_id.as_bytes()));
        assert!(!contains_bytes(
            &retained,
            changed.canonical_session_id.as_bytes()
        ));
    }
}

#[test]
fn missing_v4_initialization_commit_refuses_nonempty_spool_without_mutation() {
    let repo = git_fixture();
    let spool_dir = TempDir::new().unwrap();
    let spool = LocalSpool::open(spool_dir.path().join("spool")).unwrap();
    let ordinary = sample_batch("opaque-before-v2", "opaque-event");
    spool.append(&ordinary).unwrap();
    spool.acknowledge(&ordinary).unwrap();
    spool.compact_acknowledged(10).unwrap();
    fs::remove_file(spool.root().join("capture-spool-v4.json")).unwrap();
    let mut before = retained_bytes(spool.root());
    before.sort();

    let request = capture_request(CaptureModeV1::MetadataOnly);
    let managed = "/synthetic/agentscm change-capture run-recipe --id opaque-history";
    let actions = ActionCaptureConfig {
        source: git_request(&repo.path().canonicalize().unwrap()),
        setup_digest: "1".repeat(64),
        rules: Vec::new(),
        managed_invocations: vec![managed.into()],
    };
    let pre = serde_json::to_vec(&serde_json::json!({
        "hook_event_name": "PreToolUse",
        "session_id": request.observed_session_id,
        "tool_name": "Bash",
        "tool_use_id": "opaque_history",
        "tool_input": {"command": managed},
    }))
    .unwrap();
    assert!(matches!(
        capture_native_hook(&pre, &request, &actions, &spool),
        Err(agentscm::change_capture::CaptureError::Conflict(_))
    ));
    let mut after = retained_bytes(spool.root());
    after.sort();
    assert_eq!(after, before);
    assert!(!spool.root().join("capture-spool-v4.json").exists());
}

#[test]
fn managed_occurrence_ambiguity_fails_before_target_or_event_mutation() {
    let repo = git_fixture();
    let spool_dir = TempDir::new().unwrap();
    let spool = LocalSpool::open(spool_dir.path().join("spool")).unwrap();
    let original = capture_request(CaptureModeV1::MetadataOnly);
    let managed = "/synthetic/agentscm change-capture run-recipe --id ambiguous-occurrence";
    let actions = ActionCaptureConfig {
        source: git_request(&repo.path().canonicalize().unwrap()),
        setup_digest: "1".repeat(64),
        rules: Vec::new(),
        managed_invocations: vec![managed.into()],
    };
    let pre = serde_json::to_vec(&serde_json::json!({
        "hook_event_name": "PreToolUse",
        "session_id": original.observed_session_id,
        "tool_name": "Bash",
        "tool_use_id": "ambiguous_occurrence",
        "tool_input": {"command": managed},
    }))
    .unwrap();
    capture_native_hook(&pre, &original, &actions, &spool).unwrap();
    let marker_path = fs::read_dir(spool.root().join("managed-invocations"))
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    let mut duplicate: serde_json::Value =
        serde_json::from_slice(&fs::read(&marker_path).unwrap()).unwrap();
    duplicate["work_id"] = "wrk_other".into();
    duplicate["generation"] = 2.into();
    duplicate["canonical_session_id"] = "session_other".into();
    duplicate["version"] = 1.into();
    duplicate.as_object_mut().unwrap().remove("pre_fingerprint");
    duplicate
        .as_object_mut()
        .unwrap()
        .remove("post_fingerprint");
    duplicate.as_object_mut().unwrap().remove("route");
    let duplicate_id = format!(
        "{:x}",
        Sha256::digest(
            format!(
                "{}\0{}\0{}\0{}",
                duplicate["observed_session_id"].as_str().unwrap(),
                duplicate["canonical_session_id"].as_str().unwrap(),
                duplicate["tool_use_id"].as_str().unwrap(),
                duplicate["command_digest"].as_str().unwrap(),
            )
            .as_bytes()
        )
    );
    duplicate["marker_id"] = duplicate_id.clone().into();
    write_private_file(
        &spool
            .root()
            .join("managed-invocations")
            .join(format!("{duplicate_id}.json")),
        &serde_json::to_vec(&duplicate).unwrap(),
    );
    let before = spool.pending(50).unwrap();

    let mut changed = original.clone();
    changed.work_id = "wrk_other".into();
    changed.generation = 2;
    changed.canonical_session_id = "session_other".into();
    let post = serde_json::to_vec(&serde_json::json!({
        "hook_event_name": "PostToolUse",
        "session_id": changed.observed_session_id,
        "tool_name": "Bash",
        "tool_use_id": "ambiguous_occurrence",
        "tool_input": {"command": managed},
        "tool_response": {"exit_code": 0},
    }))
    .unwrap();
    assert!(matches!(
        capture_native_hook(&post, &changed, &actions, &spool),
        Err(agentscm::change_capture::CaptureError::Conflict(_))
    ));
    assert_eq!(spool.pending(50).unwrap(), before);
}

#[test]
fn legacy_managed_marker_without_capture_binding_stays_unassociated() {
    let repo = git_fixture();
    let canonical = repo.path().canonicalize().unwrap();
    let metadata = fs::metadata(&canonical).unwrap();
    let spool_dir = TempDir::new().unwrap();
    let spool = LocalSpool::open(spool_dir.path().join("spool")).unwrap();
    let managed = "/synthetic/agentscm change-capture run-recipe --id legacy";
    let command_digest = format!("{:x}", Sha256::digest(managed.as_bytes()));
    let marker_id = format!(
        "{:x}",
        Sha256::digest(
            format!(
                "{}\0{}\0{}\0{}",
                "legacy_observed", "legacy_canonical", "legacy_tool", command_digest
            )
            .as_bytes(),
        )
    );
    let legacy = serde_json::json!({
        "version": 1,
        "marker_id": marker_id,
        "observed_session_id": "legacy_observed",
        "canonical_session_id": "legacy_canonical",
        "work_id": "wrk_legacy",
        "generation": 1,
        "tool_use_id": "legacy_tool",
        "command_digest": command_digest,
        "repository": "example/repository",
        "worktree_id": "worktree_fixture",
        "root_device": metadata.dev(),
        "root_inode": metadata.ino(),
        "parent_session_id": null,
        "observed_at": 1,
    });
    let marker_path = spool
        .root()
        .join("managed-invocations")
        .join(format!("{marker_id}.json"));
    fs::write(&marker_path, serde_json::to_vec(&legacy).unwrap()).unwrap();
    fs::set_permissions(&marker_path, fs::Permissions::from_mode(0o600)).unwrap();

    let request = CaptureRequest {
        operation_prefix: "capture:current".into(),
        work_id: "wrk_current".into(),
        generation: 2,
        observed_session_id: "legacy_observed".into(),
        canonical_session_id: "legacy_canonical".into(),
        parent_session_id: None,
        capture_mode: CaptureModeV1::MetadataOnly,
        live_capture_mode: CaptureModeV1::MetadataOnly,
    };
    let actions = ActionCaptureConfig {
        source: git_request(&canonical),
        setup_digest: "1".repeat(64),
        rules: Vec::new(),
        managed_invocations: vec![managed.into()],
    };
    let post = serde_json::to_vec(&serde_json::json!({
        "hook_event_name": "PostToolUse",
        "session_id": "legacy_observed",
        "tool_name": "Bash",
        "tool_use_id": "legacy_tool",
        "tool_input": {"command": managed},
        "tool_response": {"exit_code": 0},
    }))
    .unwrap();
    assert!(matches!(
        capture_native_hook(&post, &request, &actions, &spool),
        Err(agentscm::change_capture::CaptureError::Conflict(_))
    ));
    assert!(marker_path.exists());
    let pending = spool.pending(20).unwrap();
    assert!(pending.is_empty());
}

#[test]
fn native_tool_hooks_create_source_bound_caller_asserted_action() {
    let repo = git_fixture();
    let spool_dir = TempDir::new().unwrap();
    let spool = LocalSpool::open(spool_dir.path().join("spool")).unwrap();
    let request = capture_request(CaptureModeV1::MetadataOnly);
    let actions = ActionCaptureConfig {
        source: git_request(&repo.path().canonicalize().unwrap()),
        setup_digest: "1".repeat(64),
        managed_invocations: Vec::new(),
        rules: vec![
            ActionRuleV1 {
                native_tool: "Bash".into(),
                kind: ActionKindV1::Execution,
                coverage: CoverageV1::Partial,
                result_source: NativeResultSourceV1::ProcessExitCode,
                recipe_digest: None,
            },
            ActionRuleV1 {
                native_tool: "Bash".into(),
                kind: ActionKindV1::Verification,
                coverage: CoverageV1::Partial,
                result_source: NativeResultSourceV1::ProcessExitCode,
                recipe_digest: Some(native_recipe_digest_fixture(
                    "Bash",
                    &serde_json::json!({"command": "cargo test"}),
                )),
            },
            ActionRuleV1 {
                native_tool: "Write".into(),
                kind: ActionKindV1::Execution,
                coverage: CoverageV1::Unknown,
                result_source: NativeResultSourceV1::Unverified,
                recipe_digest: None,
            },
        ],
    };
    let pre = br#"{"hook_event_name":"PreToolUse","session_id":"session_fixture","tool_name":"Bash","tool_use_id":"tool_action_1","tool_input":{"command":"printf changed >> tracked.txt"}}"#;
    let started = capture_native_hook(pre, &request, &actions, &spool).unwrap();
    assert_eq!(
        started.action,
        NativeActionOutcome::Started(AppendOutcome::Stored)
    );
    let conflicting_pre = br#"{"hook_event_name":"PreToolUse","session_id":"session_fixture","tool_name":"Bash","tool_use_id":"tool_action_1","tool_input":{"command":"printf different >> tracked.txt"}}"#;
    assert!(capture_native_hook(conflicting_pre, &request, &actions, &spool).is_err());

    fs::write(repo.path().join("tracked.txt"), "base\nchanged\n").unwrap();
    assert_eq!(
        capture_native_hook(pre, &request, &actions, &spool)
            .unwrap()
            .action,
        NativeActionOutcome::Started(AppendOutcome::Duplicate)
    );
    let post = br#"{"hook_event_name":"PostToolUse","session_id":"session_fixture","tool_name":"Bash","tool_use_id":"tool_action_1","tool_input":{"command":"printf changed >> tracked.txt"},"tool_response":{"stdout":"done","exit_code":0}}"#;
    let finished = capture_native_hook(post, &request, &actions, &spool).unwrap();
    assert_eq!(
        finished.action,
        NativeActionOutcome::Finished(AppendOutcome::Stored)
    );
    assert!(finished.warnings.is_empty());
    let pending = spool.pending(10).unwrap();
    assert!(matches!(
        pending.first(),
        Some(CaptureEnvelopeV1::ActionStart(_))
    ));
    assert!(matches!(
        pending.last(),
        Some(CaptureEnvelopeV1::ActionFinish(_))
    ));
    let finish = pending
        .iter()
        .find_map(|envelope| match envelope {
            CaptureEnvelopeV1::ActionFinish(finish) => Some(finish),
            _ => None,
        })
        .unwrap();
    assert_eq!(finish.assurance, AssuranceBasisV1::CallerAsserted);
    assert_eq!(finish.result, CheckResultV1::Unknown);
    assert_eq!(finish.outcome, ActionOutcomeV1::Succeeded);
    assert_eq!(finish.drift, DriftV1::SourceChanged);
    assert_eq!(finish.coverage, CoverageV1::Partial);
    fs::write(
        repo.path().join("tracked.txt"),
        "base\nchanged\nchanged after receipt\n",
    )
    .unwrap();
    assert_eq!(
        capture_native_hook(post, &request, &actions, &spool)
            .unwrap()
            .action,
        NativeActionOutcome::Finished(AppendOutcome::Duplicate)
    );
    let changed_body = br#"{"hook_event_name":"PostToolUse","session_id":"session_fixture","tool_name":"Bash","tool_use_id":"tool_action_1","tool_input":{"command":"printf changed >> tracked.txt"},"tool_response":{"stdout":"different","exit_code":0}}"#;
    assert!(capture_native_hook(changed_body, &request, &actions, &spool).is_err());
    let conflicting_post = br#"{"hook_event_name":"PostToolUse","session_id":"session_fixture","tool_name":"Bash","tool_use_id":"tool_action_1","tool_input":{"command":"printf changed >> tracked.txt"},"tool_response":{"stdout":"done","exit_code":9}}"#;
    assert!(capture_native_hook(conflicting_post, &request, &actions, &spool).is_err());

    let missing_pre = br#"{"hook_event_name":"PreToolUse","session_id":"session_fixture","tool_name":"Bash","tool_use_id":"tool_action_2","tool_input":{"command":"true"}}"#;
    let missing_post = br#"{"hook_event_name":"PostToolUse","session_id":"session_fixture","tool_name":"Bash","tool_use_id":"tool_action_2","tool_input":{"command":"true"},"tool_response":{"stdout":""}}"#;
    capture_native_hook(missing_pre, &request, &actions, &spool).unwrap();
    capture_native_hook(missing_post, &request, &actions, &spool).unwrap();

    let failed_pre = br#"{"hook_event_name":"PreToolUse","session_id":"session_fixture","tool_name":"Bash","tool_use_id":"tool_action_3","tool_input":{"command":"exit 3"}}"#;
    let failed_post = br#"{"hook_event_name":"PostToolUse","session_id":"session_fixture","tool_name":"Bash","tool_use_id":"tool_action_3","tool_input":{"command":"exit 3"},"tool_response":{"stdout":"","stderr":"failed","exitCode":3}}"#;
    capture_native_hook(failed_pre, &request, &actions, &spool).unwrap();
    capture_native_hook(failed_post, &request, &actions, &spool).unwrap();

    let edit_pre = br#"{"hook_event_name":"PreToolUse","session_id":"session_fixture","tool_name":"Write","tool_use_id":"tool_action_4","tool_input":{"file_path":"synthetic.txt","content":"fixture"}}"#;
    let edit_post = br#"{"hook_event_name":"PostToolUse","session_id":"session_fixture","tool_name":"Write","tool_use_id":"tool_action_4","tool_input":{"file_path":"synthetic.txt","content":"fixture"},"tool_response":{"success":true}}"#;
    capture_native_hook(edit_pre, &request, &actions, &spool).unwrap();
    capture_native_hook(edit_post, &request, &actions, &spool).unwrap();

    let interrupted_pre = br#"{"hook_event_name":"PreToolUse","session_id":"session_fixture","tool_name":"Bash","tool_use_id":"tool_action_5","tool_input":{"command":"sleep 1"}}"#;
    let interrupted_post = br#"{"hook_event_name":"PostToolUseFailure","session_id":"session_fixture","tool_name":"Bash","tool_use_id":"tool_action_5","tool_input":{"command":"sleep 1"},"error":"interrupted"}"#;
    capture_native_hook(interrupted_pre, &request, &actions, &spool).unwrap();
    capture_native_hook(interrupted_post, &request, &actions, &spool).unwrap();

    let check_pre = br#"{"hook_event_name":"PreToolUse","session_id":"session_fixture","tool_name":"Bash","tool_use_id":"tool_action_6","tool_input":{"command":"cargo test"}}"#;
    let check_pass = br#"{"hook_event_name":"PostToolUse","session_id":"session_fixture","tool_name":"Bash","tool_use_id":"tool_action_6","tool_input":{"command":"cargo test"},"tool_response":{"exit_code":0}}"#;
    capture_native_hook(check_pre, &request, &actions, &spool).unwrap();
    capture_native_hook(check_pass, &request, &actions, &spool).unwrap();

    let check_failed_pre = br#"{"hook_event_name":"PreToolUse","session_id":"session_fixture","tool_name":"Bash","tool_use_id":"tool_action_7","tool_input":{"command":"cargo test"}}"#;
    let check_failed = br#"{"hook_event_name":"PostToolUse","session_id":"session_fixture","tool_name":"Bash","tool_use_id":"tool_action_7","tool_input":{"command":"cargo test"},"tool_response":{"exit_code":2}}"#;
    capture_native_hook(check_failed_pre, &request, &actions, &spool).unwrap();
    capture_native_hook(check_failed, &request, &actions, &spool).unwrap();

    let results: Vec<_> = spool
        .pending(40)
        .unwrap()
        .into_iter()
        .filter_map(|envelope| match envelope {
            CaptureEnvelopeV1::ActionFinish(finish) => Some(finish.result),
            _ => None,
        })
        .collect();
    assert_eq!(results.len(), 7);
    assert!(results.contains(&CheckResultV1::Passed));
    assert_eq!(
        results
            .iter()
            .filter(|result| **result == CheckResultV1::Unknown)
            .count(),
        5
    );
    assert!(results.contains(&CheckResultV1::Failed));
    assert_eq!(
        spool
            .pending(40)
            .unwrap()
            .iter()
            .filter(|envelope| matches!(
                envelope,
                CaptureEnvelopeV1::ActionStart(start)
                    if start.kind == ActionKindV1::Verification
                        && start.recipe_provenance == RecipeProvenanceV1::ConfiguredExactMatch
            ))
            .count(),
        2
    );
}

#[test]
fn ordinary_native_action_records_setup_drift_and_replays_original_finish() {
    let repo = git_fixture();
    let replay_repo = git_fixture();
    let spool_dir = TempDir::new().unwrap();
    let spool = LocalSpool::open(spool_dir.path().join("spool")).unwrap();
    let original = capture_request(CaptureModeV1::MetadataOnly);
    let execution_rule = ActionRuleV1 {
        native_tool: "Bash".into(),
        kind: ActionKindV1::Execution,
        coverage: CoverageV1::Partial,
        result_source: NativeResultSourceV1::ProcessExitCode,
        recipe_digest: None,
    };
    let pre_actions = ActionCaptureConfig {
        source: git_request(&repo.path().canonicalize().unwrap()),
        setup_digest: "1".repeat(64),
        managed_invocations: Vec::new(),
        rules: vec![execution_rule.clone()],
    };
    let pre = br#"{"hook_event_name":"PreToolUse","session_id":"session_fixture","tool_name":"Bash","tool_use_id":"setup_drift_action","tool_input":{"command":"true"}}"#;
    assert_eq!(
        capture_native_hook(pre, &original, &pre_actions, &spool)
            .unwrap()
            .action,
        NativeActionOutcome::Started(AppendOutcome::Stored)
    );
    let marker_id = format!(
        "{:x}",
        Sha256::digest(
            format!("{}\0{}", original.observed_session_id, "setup_drift_action").as_bytes()
        )
    );
    let marker_path = spool
        .root()
        .join("managed-invocations")
        .join(format!("{marker_id}.json"));
    let marker_before_post: serde_json::Value =
        serde_json::from_slice(&fs::read(&marker_path).unwrap()).unwrap();

    let mut post_actions = pre_actions.clone();
    post_actions.setup_digest = "2".repeat(64);
    let post = br#"{"hook_event_name":"PostToolUse","session_id":"session_fixture","tool_name":"Bash","tool_use_id":"setup_drift_action","tool_input":{"command":"true"},"tool_response":{"exit_code":0}}"#;
    assert_eq!(
        capture_native_hook(post, &original, &post_actions, &spool)
            .unwrap()
            .action,
        NativeActionOutcome::Finished(AppendOutcome::Stored)
    );
    let pending = spool.pending(20).unwrap();
    let start = pending
        .iter()
        .find_map(|envelope| match envelope {
            CaptureEnvelopeV1::ActionStart(start) => Some(start),
            _ => None,
        })
        .unwrap();
    let finish = pending
        .iter()
        .find_map(|envelope| match envelope {
            CaptureEnvelopeV1::ActionFinish(finish) => Some(finish),
            _ => None,
        })
        .unwrap();
    assert_eq!(start.work_id, original.work_id);
    assert_eq!(start.generation, original.generation);
    assert_eq!(start.setup_digest, "1".repeat(64));
    assert_eq!(start.kind, ActionKindV1::Execution);
    assert_eq!(finish.work_id, original.work_id);
    assert_eq!(finish.generation, original.generation);
    assert_eq!(finish.coverage, CoverageV1::Partial);
    assert_eq!(finish.drift, DriftV1::SetupChanged);
    let original_finish = finish.clone();

    let mut replay_request = original.clone();
    replay_request.work_id = "wrk_later_setup".into();
    replay_request.generation = 2;
    replay_request.canonical_session_id = "session_later_setup".into();
    let replay_actions = ActionCaptureConfig {
        source: git_request(&replay_repo.path().canonicalize().unwrap()),
        setup_digest: "3".repeat(64),
        managed_invocations: Vec::new(),
        rules: vec![ActionRuleV1 {
            coverage: CoverageV1::Complete,
            ..execution_rule
        }],
    };
    let mut before_replay = retained_bytes(spool.root());
    before_replay.sort();
    let replay = capture_native_hook(post, &replay_request, &replay_actions, &spool).unwrap();
    assert_eq!(
        replay.action,
        NativeActionOutcome::Finished(AppendOutcome::Duplicate)
    );
    let mut after_replay = retained_bytes(spool.root());
    after_replay.sort();
    assert_eq!(after_replay, before_replay);

    // Model a crash after the finish became durable but before occurrence cleanup committed.
    let compaction_path = spool.root().join("compaction-v1.json");
    let mut compaction: serde_json::Value =
        serde_json::from_slice(&fs::read(&compaction_path).unwrap()).unwrap();
    let occurrence_receipt = compaction["native_occurrences"]
        .as_object_mut()
        .unwrap()
        .remove(&marker_id)
        .unwrap();
    let mut unfinished_marker = marker_before_post;
    unfinished_marker["post_fingerprint"] = occurrence_receipt["post_fingerprint"].clone();
    write_private_file(&compaction_path, &serde_json::to_vec(&compaction).unwrap());
    write_private_file(
        &marker_path,
        &serde_json::to_vec(&unfinished_marker).unwrap(),
    );
    let replay_after_durable_finish =
        capture_native_hook(post, &replay_request, &replay_actions, &spool).unwrap();
    assert_eq!(
        replay_after_durable_finish.action,
        NativeActionOutcome::Finished(AppendOutcome::Duplicate)
    );
    let retained_finish = spool
        .pending(20)
        .unwrap()
        .into_iter()
        .find_map(|envelope| match envelope {
            CaptureEnvelopeV1::ActionFinish(finish)
                if finish.operation_id == original_finish.operation_id =>
            {
                Some(finish)
            }
            _ => None,
        })
        .unwrap();
    assert_eq!(retained_finish, original_finish);

    let unchanged_pre = br#"{"hook_event_name":"PreToolUse","session_id":"session_fixture","tool_name":"Bash","tool_use_id":"unchanged_setup_action","tool_input":{"command":"true"}}"#;
    let unchanged_post = br#"{"hook_event_name":"PostToolUse","session_id":"session_fixture","tool_name":"Bash","tool_use_id":"unchanged_setup_action","tool_input":{"command":"true"},"tool_response":{"exit_code":0}}"#;
    capture_native_hook(unchanged_pre, &replay_request, &replay_actions, &spool).unwrap();
    capture_native_hook(unchanged_post, &replay_request, &replay_actions, &spool).unwrap();
    let unchanged_finish = spool
        .pending(30)
        .unwrap()
        .into_iter()
        .filter_map(|envelope| match envelope {
            CaptureEnvelopeV1::ActionFinish(finish) => Some(finish),
            _ => None,
        })
        .find(|finish| finish.work_id == replay_request.work_id)
        .unwrap();
    assert_eq!(unchanged_finish.coverage, CoverageV1::Complete);
    assert_eq!(unchanged_finish.drift, DriftV1::NoneObserved);
}

#[test]
fn artifact_quota_failure_keeps_native_finish_and_marks_content_missing() {
    let repo = git_fixture();
    let spool_dir = TempDir::new().unwrap();
    let spool = LocalSpool::open_with_limits(
        spool_dir.path().join("spool"),
        SpoolLimits {
            max_records: 128,
            max_total_bytes: 64 * 1024,
            max_artifact_bytes: 1024,
        },
    )
    .unwrap();
    let request = capture_request(CaptureModeV1::LocalConversation);
    let actions = ActionCaptureConfig {
        source: git_request(&repo.path().canonicalize().unwrap()),
        setup_digest: "2".repeat(64),
        managed_invocations: Vec::new(),
        rules: vec![ActionRuleV1 {
            native_tool: "Bash".into(),
            kind: ActionKindV1::Execution,
            coverage: CoverageV1::Partial,
            result_source: NativeResultSourceV1::ProcessExitCode,
            recipe_digest: None,
        }],
    };
    let pre = format!(
        "{{\"hook_event_name\":\"PreToolUse\",\"session_id\":\"session_fixture\",\"tool_name\":\"Bash\",\"tool_use_id\":\"quota_action\",\"tool_input\":{{\"command\":\"{}\"}}}}",
        "x".repeat(256)
    );
    capture_native_hook(pre.as_bytes(), &request, &actions, &spool).unwrap();
    assert!(
        spool
            .retain_artifact(
                &vec![b'q'; 2048],
                ArtifactMediaV1::Binary,
                RetentionClassV1::WorkLocal,
            )
            .is_err(),
        "fixture must exceed the per-artifact content quota"
    );
    let post = format!(
        "{{\"hook_event_name\":\"PostToolUse\",\"session_id\":\"session_fixture\",\"tool_name\":\"Bash\",\"tool_use_id\":\"quota_action\",\"tool_input\":{{\"command\":\"{}\"}},\"tool_response\":{{\"stdout\":\"{}\",\"exit_code\":0}}}}",
        "x".repeat(256),
        "y".repeat(1800)
    );
    capture_native_hook(post.as_bytes(), &request, &actions, &spool).unwrap();

    let pending = spool.pending(20).unwrap();
    let finish = pending
        .iter()
        .find_map(|envelope| match envelope {
            CaptureEnvelopeV1::ActionFinish(finish) => Some(finish),
            _ => None,
        })
        .unwrap();
    assert_eq!(finish.outcome, ActionOutcomeV1::Succeeded);
    assert_eq!(finish.result, CheckResultV1::Unknown);
    assert!(
        finish
            .output_artifacts
            .iter()
            .all(|artifact| artifact.availability == ContentAvailabilityV1::Missing)
    );
    assert!(pending.iter().any(|envelope| match envelope {
        CaptureEnvelopeV1::EventBatch(batch) => batch.events.iter().any(|event| {
            event
                .artifact
                .as_ref()
                .is_some_and(|artifact| artifact.availability == ContentAvailabilityV1::Missing)
                && batch.events.iter().any(|candidate| {
                    candidate.gap.as_ref().is_some_and(|gap| {
                        gap.reason == agentscm::change_record::GapReasonV1::CaptureInterrupted
                    })
                })
        }),
        _ => false,
    }));
}

#[test]
fn spool_quota_emits_priority_gap_and_later_event_shows_resume() {
    let temp = TempDir::new().unwrap();
    let root = temp.path().join("spool");
    let limits = SpoolLimits {
        max_records: 16,
        max_total_bytes: 64 * 1024,
        max_artifact_bytes: 4096,
    };
    let spool = LocalSpool::open_with_limits(&root, limits).unwrap();
    let request = capture_request(CaptureModeV1::MetadataOnly);
    let start = br#"{"hook_event_name":"SessionStart","session_id":"session_fixture","source":"startup","timestamp":"2026-10-24T10:00:00Z"}"#;
    capture_hook(start, &request, &spool).unwrap();
    let mut quota_reached = false;
    for index in 0..100 {
        let bytes = format!("quota-record-{index:03}");
        if spool
            .retain_artifact(
                bytes.as_bytes(),
                ArtifactMediaV1::Binary,
                RetentionClassV1::WorkLocal,
            )
            .is_err()
        {
            quota_reached = true;
            break;
        }
    }
    assert!(quota_reached);
    let prompt = br#"{"hook_event_name":"UserPromptSubmit","session_id":"session_fixture","prompt_id":"quota_prompt","timestamp":"2026-10-24T10:00:01Z","prompt":"kept private"}"#;
    capture_hook(prompt, &request, &spool).unwrap();
    let during_quota = spool.pending(20).unwrap();
    let paused_sequence = during_quota
        .iter()
        .filter_map(|envelope| match envelope {
            CaptureEnvelopeV1::EventBatch(batch) => batch.events.iter().find_map(|event| {
                event.gap.as_ref().and_then(|gap| {
                    (gap.reason == agentscm::change_record::GapReasonV1::CaptureInterrupted)
                        .then_some(event.sequence)
                })
            }),
            _ => None,
        })
        .next()
        .expect("quota must remain Hub-visible through reserved metadata headroom");

    let resumed = LocalSpool::open_with_limits(
        &root,
        SpoolLimits {
            max_records: 64,
            ..limits
        },
    )
    .unwrap();
    let end = br#"{"hook_event_name":"SessionEnd","session_id":"session_fixture","timestamp":"2026-10-24T10:00:02Z"}"#;
    capture_hook(end, &request, &resumed).unwrap();
    assert!(resumed.pending(40).unwrap().iter().any(|envelope| matches!(
        envelope,
        CaptureEnvelopeV1::EventBatch(batch)
            if batch.events.iter().any(|event| {
                event.kind == CapturedEventKindV1::SessionEnded
                    && event.sequence > paused_sequence
            })
    )));
}

fn native_recipe_digest_fixture(tool: &str, input: &serde_json::Value) -> String {
    #[derive(serde::Serialize)]
    struct Recipe<'a> {
        schema_version: u16,
        adapter_version: &'static str,
        native_tool: &'a str,
        tool_input: &'a serde_json::Value,
    }
    let bytes = serde_json::to_vec(&Recipe {
        schema_version: SCHEMA_VERSION_V1,
        adapter_version: "1.0.0",
        native_tool: tool,
        tool_input: input,
    })
    .unwrap();
    format!("{:x}", Sha256::digest(bytes))
}

fn git_fixture() -> TempDir {
    let repo = TempDir::new().unwrap();
    run_git(repo.path(), &["init", "-b", "main"]);
    run_git(repo.path(), &["config", "user.name", "Synthetic Test"]);
    run_git(
        repo.path(),
        &["config", "user.email", "synthetic@example.invalid"],
    );
    run_git(repo.path(), &["config", "commit.gpgsign", "false"]);
    run_git(repo.path(), &["config", "core.hooksPath", "/dev/null"]);
    fs::write(repo.path().join(".gitignore"), "ignored-secret.txt\n").unwrap();
    fs::write(repo.path().join("tracked.txt"), "base\n").unwrap();
    run_git(repo.path(), &["add", ".gitignore", "tracked.txt"]);
    run_git(repo.path(), &["commit", "-m", "synthetic fixture"]);
    repo
}

fn git_request(path: &Path) -> GitSourceRequest {
    GitSourceRequest {
        repository: "example/repository".into(),
        worktree_id: "worktree_fixture".into(),
        repository_path: path.to_owned(),
        base_ref: "main".into(),
    }
}

fn run_git(path: &Path, args: &[&str]) {
    let status = Command::new("git")
        .arg("-C")
        .arg(path)
        .args(args)
        .status()
        .unwrap();
    assert!(status.success(), "git {args:?} failed");
}

fn git(path: &Path, args: &[&str]) -> Vec<u8> {
    let output = Command::new("git")
        .arg("-C")
        .arg(path)
        .args(args)
        .output()
        .unwrap();
    assert!(output.status.success(), "git {args:?} failed");
    output.stdout
}

fn compact_provider_history_gap(spool: &LocalSpool) -> i64 {
    let gap = spool
        .pending(20)
        .unwrap()
        .into_iter()
        .find(|envelope| {
            matches!(envelope, CaptureEnvelopeV1::EventBatch(batch)
                if batch.events.len() == 1
                    && batch.events[0].kind == CapturedEventKindV1::Gap
                    && batch.operation_id.starts_with("provider:history:"))
        })
        .unwrap();
    let observed_at = match &gap {
        CaptureEnvelopeV1::EventBatch(batch) => batch.events[0].observed_at,
        _ => unreachable!(),
    };
    spool.acknowledge(&gap).unwrap();
    assert_eq!(spool.compact_acknowledged(20).unwrap().envelopes, 1);
    observed_at
}

fn rewind_provider_history_cursor(spool: &LocalSpool, cursor: &ProviderHistoryCursorV1) {
    let mut states: Vec<_> = fs::read_dir(spool.root().join("sessions"))
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .collect();
    assert_eq!(states.len(), 1);
    let state_path = states.pop().unwrap();
    let mut state: serde_json::Value =
        serde_json::from_slice(&fs::read(&state_path).unwrap()).unwrap();
    state["provider_history"] = serde_json::to_value(cursor).unwrap();
    write_private_file(&state_path, &serde_json::to_vec(&state).unwrap());
}

fn wait_past_observation_time(observed_at: i64) {
    thread::sleep(Duration::from_millis(1_100));
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64;
    assert!(now > observed_at);
}

fn retained_bytes(root: &Path) -> Vec<Vec<u8>> {
    fn visit(path: &Path, output: &mut Vec<Vec<u8>>) {
        for entry in fs::read_dir(path).unwrap() {
            let entry = entry.unwrap();
            if entry.file_type().unwrap().is_dir() {
                visit(&entry.path(), output);
            } else {
                output.push(fs::read(entry.path()).unwrap());
            }
        }
    }
    let mut output = Vec::new();
    visit(root, &mut output);
    output
}

fn write_private_file(path: &Path, bytes: &[u8]) {
    use std::io::Write;

    let mut file = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)
        .unwrap();
    file.write_all(bytes).unwrap();
    file.sync_all().unwrap();
}

fn contains_bytes(values: &[Vec<u8>], needle: &[u8]) -> bool {
    values
        .iter()
        .any(|value| value.windows(needle.len()).any(|window| window == needle))
}

fn captured_events(spool: &LocalSpool) -> Vec<(SessionIdentityV1, CapturedEventV1)> {
    spool
        .pending(100)
        .unwrap()
        .into_iter()
        .flat_map(|envelope| match envelope {
            CaptureEnvelopeV1::EventBatch(batch) => batch
                .events
                .into_iter()
                .map(|event| (batch.session.clone(), event))
                .collect(),
            _ => Vec::new(),
        })
        .collect()
}

#[test]
fn lifecycle_hooks_record_boundaries_subagents_tool_names_and_completion() {
    let temp = TempDir::new().unwrap();
    let spool = LocalSpool::open(temp.path().join("spool")).unwrap();
    let request = capture_request(CaptureModeV1::MetadataOnly);
    let capture = |payload: &str| capture_hook(payload.as_bytes(), &request, &spool).unwrap();
    let base = r#""session_id":"session_fixture""#;
    for payload in [
        format!(
            r#"{{"hook_event_name":"SessionStart",{base},"source":"startup","timestamp":"2026-10-24T10:00:00Z"}}"#
        ),
        format!(
            r#"{{"hook_event_name":"PostToolUse",{base},"tool_name":"mcp__server__tool","tool_use_id":"tool_a"}}"#
        ),
        format!(
            r#"{{"hook_event_name":"PostToolUseFailure",{base},"tool_name":"Bash","tool_use_id":"tool_b"}}"#
        ),
        format!(r#"{{"hook_event_name":"Stop",{base}}}"#),
        format!(r#"{{"hook_event_name":"Stop",{base}}}"#),
        format!(r#"{{"hook_event_name":"PreCompact",{base}}}"#),
        format!(
            r#"{{"hook_event_name":"SubagentStart",{base},"agent_id":"agent_1","agent_type":"Explore"}}"#
        ),
        format!(r#"{{"hook_event_name":"SubagentStop",{base},"agent_id":"agent_1"}}"#),
        format!(
            r#"{{"hook_event_name":"SessionStart",{base},"source":"compact","timestamp":"2026-10-24T10:00:01Z"}}"#
        ),
        format!(
            r#"{{"hook_event_name":"SessionEnd",{base},"reason":"unlisted reason","timestamp":"2026-10-24T10:00:02Z"}}"#
        ),
    ] {
        assert_eq!(capture(&payload), HookCaptureOutcome::Stored, "{payload}");
    }
    // A subagent marker without a usable agent id would be invalid on the Hub, so it is dropped.
    assert_eq!(
        capture(&format!(
            r#"{{"hook_event_name":"SubagentStart",{base},"agent_type":"Explore"}}"#
        )),
        HookCaptureOutcome::Ignored
    );

    let events: Vec<_> = captured_events(&spool)
        .into_iter()
        .map(|(_, event)| event)
        .collect();
    let of = |kind| events.iter().filter(move |event| event.kind == kind);
    assert_eq!(of(CapturedEventKindV1::TurnEnded).count(), 2);
    assert_eq!(of(CapturedEventKindV1::Compaction).count(), 1);
    // Informational markers without a host identity record no identity-uncertainty gap.
    assert_eq!(of(CapturedEventKindV1::Gap).count(), 0);
    let subagents: Vec<_> = events
        .iter()
        .filter_map(|event| event.subagent.as_ref())
        .collect();
    assert_eq!(subagents.len(), 2);
    assert_eq!(subagents[0].agent_id, "agent_1");
    assert_eq!(subagents[0].agent_type.as_deref(), Some("Explore"));
    assert_eq!(subagents[1].agent_type, None);
    let boundaries: Vec<_> = of(CapturedEventKindV1::SessionStarted)
        .chain(of(CapturedEventKindV1::SessionEnded))
        .map(|event| event.boundary.as_deref())
        .collect();
    assert_eq!(
        boundaries,
        [Some("startup"), Some("compact"), Some("other")]
    );
    let finishes: Vec<_> = of(CapturedEventKindV1::ToolFinished)
        .map(|event| event.tool.as_ref().unwrap())
        .collect();
    assert_eq!(finishes.len(), 2);
    assert_eq!(finishes[0].name.as_deref(), Some("mcp__server__tool"));
    assert_eq!(finishes[0].completion, Some(ToolCompletionV1::Returned));
    assert_eq!(finishes[1].name.as_deref(), Some("Bash"));
    assert_eq!(finishes[1].completion, Some(ToolCompletionV1::Failed));
}

#[test]
fn a_session_that_began_unshared_never_pins_the_shared_mode() {
    let temp = TempDir::new().unwrap();
    let spool = LocalSpool::open(temp.path().join("spool")).unwrap();
    let pin = |work: &str, generation, mode| {
        spool
            .pin_session_identity(
                "session_fixture",
                "session_fixture",
                work,
                generation,
                None,
                mode,
            )
            .unwrap()
            .1
    };
    assert_eq!(
        pin("wrk_a", 1, CaptureModeV1::MetadataOnly),
        CaptureModeV1::MetadataOnly
    );
    assert_eq!(
        pin("wrk_a", 1, CaptureModeV1::SharedConversation),
        CaptureModeV1::MetadataOnly
    );
    // A new generation or Work does not make the session shared either.
    assert_eq!(
        pin("wrk_a", 2, CaptureModeV1::SharedConversation),
        CaptureModeV1::MetadataOnly
    );
    assert_eq!(
        pin("wrk_b", 1, CaptureModeV1::SharedConversation),
        CaptureModeV1::MetadataOnly
    );
    // A downgrade is honoured, and a session that began shared stays eligible.
    assert_eq!(
        pin("wrk_b", 2, CaptureModeV1::LocalConversation),
        CaptureModeV1::LocalConversation
    );
    let spool = LocalSpool::open(temp.path().join("other")).unwrap();
    let pin = |generation, mode| {
        spool
            .pin_session_identity(
                "session_fixture",
                "session_fixture",
                "wrk_a",
                generation,
                None,
                mode,
            )
            .unwrap()
            .1
    };
    assert_eq!(
        pin(1, CaptureModeV1::SharedConversation),
        CaptureModeV1::SharedConversation
    );
    assert_eq!(
        pin(2, CaptureModeV1::SharedConversation),
        CaptureModeV1::SharedConversation
    );
}

#[test]
fn an_import_cannot_start_sharing_a_session_that_began_unshared() {
    let temp = TempDir::new().unwrap();
    let spool = LocalSpool::open(temp.path().join("spool")).unwrap();
    let remember = |mode| {
        spool
            .remember_session_association(
                "session_fixture",
                "session_fixture",
                "wrk_a",
                1,
                None,
                mode,
            )
            .unwrap()
    };
    remember(CaptureModeV1::LocalConversation);
    remember(CaptureModeV1::SharedConversation);
    // The same Work and generation takes the pinned fast path.
    let (_, mode) = spool
        .pin_session_identity(
            "session_fixture",
            "session_fixture",
            "wrk_a",
            1,
            None,
            CaptureModeV1::SharedConversation,
        )
        .unwrap();
    assert_eq!(mode, CaptureModeV1::LocalConversation);
}

#[test]
fn session_identity_is_pinned_while_the_live_mode_gates_retention() {
    let temp = TempDir::new().unwrap();
    let spool = LocalSpool::open(temp.path().join("spool")).unwrap();
    let pin = |parent: Option<&str>, mode, generation| {
        spool
            .pin_session_identity(
                "session_fixture",
                "session_fixture",
                "wrk_fixture",
                generation,
                parent,
                mode,
            )
            .unwrap()
    };
    assert_eq!(
        pin(None, CaptureModeV1::MetadataOnly, 1),
        (None, CaptureModeV1::MetadataOnly)
    );
    // A later toggle or a parent id that appears mid-session does not change the identity...
    assert_eq!(
        pin(Some("parent_fixture"), CaptureModeV1::LocalConversation, 1),
        (None, CaptureModeV1::MetadataOnly)
    );
    // ...but a new Work generation is a new identity.
    assert_eq!(
        pin(Some("parent_fixture"), CaptureModeV1::LocalConversation, 2),
        (
            Some("parent_fixture".to_owned()),
            CaptureModeV1::LocalConversation
        )
    );
    pin(None, CaptureModeV1::MetadataOnly, 1);

    let mut request = capture_request(CaptureModeV1::MetadataOnly);
    request.live_capture_mode = CaptureModeV1::LocalConversation;
    let prompt = |id: &str, body: &str| {
        format!(
            r#"{{"hook_event_name":"UserPromptSubmit","session_id":"session_fixture","prompt_id":"{id}","prompt":"{body}"}}"#
        )
    };
    capture_hook(prompt("p1", "LIVE_BODY_ONE").as_bytes(), &request, &spool).unwrap();
    request.live_capture_mode = CaptureModeV1::MetadataOnly;
    capture_hook(prompt("p2", "LIVE_BODY_TWO").as_bytes(), &request, &spool).unwrap();

    let captured = captured_events(&spool);
    assert!(!captured.is_empty());
    assert!(captured.iter().all(|(session, _)| session.capture_mode
        == CaptureModeV1::MetadataOnly
        && session.parent_session_id.is_none()));
    let artifact_for = |sequence_kind: usize| {
        captured
            .iter()
            .filter(|(_, event)| event.kind == CapturedEventKindV1::UserPrompt)
            .nth(sequence_kind)
            .unwrap()
            .1
            .artifact
            .is_some()
    };
    assert!(
        artifact_for(0),
        "live local-conversation mode retains the body"
    );
    assert!(!artifact_for(1), "live metadata-only mode retains nothing");
}

fn synthetic_secrets() -> (String, String) {
    (
        ["gh", "p_"].concat() + &"a1B2".repeat(9),
        ["hunter", "2-synthetic"].concat(),
    )
}

fn shared_prompt_and_tool_hooks(token: &str, password: &str) -> [String; 3] {
    [
        format!(
            r#"{{"hook_event_name":"UserPromptSubmit","session_id":"session_fixture","prompt_id":"p1","prompt":"please deploy with {token} today"}}"#
        ),
        format!(
            r#"{{"hook_event_name":"PreToolUse","session_id":"session_fixture","tool_name":"Bash","tool_use_id":"t1","tool_input":{{"command":"deploy","password":"{password}","env":"DB_PASSWORD={password}"}}}}"#
        ),
        format!(
            r#"{{"hook_event_name":"PostToolUse","session_id":"session_fixture","tool_name":"Read","tool_use_id":"t2","tool_response":{{"content":"Authorization: Bearer {token}"}}}}"#
        ),
    ]
}

#[test]
fn shared_conversation_redacts_before_retention_and_marks_bodies_shared_approved() {
    let temp = TempDir::new().unwrap();
    let spool = LocalSpool::open(temp.path().join("spool")).unwrap();
    let request = capture_request(CaptureModeV1::SharedConversation);
    let (token, password) = synthetic_secrets();
    for hook in shared_prompt_and_tool_hooks(&token, &password) {
        assert_eq!(
            capture_hook(hook.as_bytes(), &request, &spool).unwrap(),
            HookCaptureOutcome::Stored
        );
    }

    let retained = retained_bytes(spool.root());
    assert!(!contains_bytes(&retained, token.as_bytes()));
    assert!(!contains_bytes(&retained, password.as_bytes()));
    assert!(contains_bytes(&retained, b"[REDACTED:github_token]"));
    assert!(contains_bytes(&retained, b"[REDACTED:field]"));

    let captured = captured_events(&spool);
    let bodies: Vec<_> = captured
        .iter()
        .filter_map(|(_, event)| event.artifact.as_ref().map(|artifact| (event, artifact)))
        .collect();
    assert_eq!(bodies.len(), 3);
    for (_, artifact) in &bodies {
        assert_eq!(artifact.retention, RetentionClassV1::SharedApproved);
        assert_eq!(artifact.media, ArtifactMediaV1::Json);
        assert_eq!(artifact.availability, ContentAvailabilityV1::Local);
        let bytes = spool.read_artifact(artifact).unwrap();
        assert_eq!(format!("{:x}", Sha256::digest(&bytes)), artifact.digest);
        let body: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert!(body["redactions"].as_u64().unwrap() >= 1);
    }
    let redacted_gaps = captured
        .iter()
        .filter(|(_, event)| {
            event
                .gap
                .as_ref()
                .is_some_and(|gap| gap.reason == GapReasonV1::Redacted)
        })
        .count();
    assert_eq!(redacted_gaps, 3);
    assert!(
        captured
            .iter()
            .all(|(session, _)| session.capture_mode == CaptureModeV1::SharedConversation)
    );

    // Redaction is deterministic, so a replay is a duplicate rather than a second record.
    let replay = shared_prompt_and_tool_hooks(&token, &password);
    assert_eq!(
        capture_hook(replay[0].as_bytes(), &request, &spool).unwrap(),
        HookCaptureOutcome::Duplicate
    );
}

#[test]
fn local_conversation_keeps_unredacted_work_local_bodies() {
    let temp = TempDir::new().unwrap();
    let spool = LocalSpool::open(temp.path().join("spool")).unwrap();
    let request = capture_request(CaptureModeV1::LocalConversation);
    let (token, password) = synthetic_secrets();
    for hook in shared_prompt_and_tool_hooks(&token, &password) {
        capture_hook(hook.as_bytes(), &request, &spool).unwrap();
    }
    let retained = retained_bytes(spool.root());
    assert!(contains_bytes(&retained, token.as_bytes()));
    assert!(contains_bytes(&retained, password.as_bytes()));
    let captured = captured_events(&spool);
    assert!(captured.iter().all(|(_, event)| {
        event
            .artifact
            .as_ref()
            .is_none_or(|artifact| artifact.retention == RetentionClassV1::WorkLocal)
            && event
                .gap
                .as_ref()
                .is_none_or(|gap| gap.reason != GapReasonV1::Redacted)
    }));
}

#[test]
fn shared_mode_switch_mid_session_keeps_bodies_local_but_redacted() {
    let temp = TempDir::new().unwrap();
    let spool = LocalSpool::open(temp.path().join("spool")).unwrap();
    // The session was pinned while metadata-only; the binding now says shared.
    let mut request = capture_request(CaptureModeV1::MetadataOnly);
    request.live_capture_mode = CaptureModeV1::SharedConversation;
    let (token, password) = synthetic_secrets();
    for hook in shared_prompt_and_tool_hooks(&token, &password) {
        capture_hook(hook.as_bytes(), &request, &spool).unwrap();
    }
    assert!(!contains_bytes(
        &retained_bytes(spool.root()),
        token.as_bytes()
    ));
    let captured = captured_events(&spool);
    assert!(captured.iter().all(|(session, event)| {
        session.capture_mode == CaptureModeV1::MetadataOnly
            && event
                .artifact
                .as_ref()
                .is_none_or(|artifact| artifact.retention == RetentionClassV1::WorkLocal)
    }));
    assert!(captured.iter().any(|(_, event)| event.artifact.is_some()));

    // The reverse switch (pinned shared, live local) also never produces a shared artifact.
    let reverse_spool = LocalSpool::open(temp.path().join("reverse")).unwrap();
    let mut reverse = capture_request(CaptureModeV1::SharedConversation);
    reverse.live_capture_mode = CaptureModeV1::LocalConversation;
    capture_hook(
        shared_prompt_and_tool_hooks(&token, &password)[0].as_bytes(),
        &reverse,
        &reverse_spool,
    )
    .unwrap();
    assert!(captured_events(&reverse_spool).iter().all(|(_, event)| {
        event
            .artifact
            .as_ref()
            .is_none_or(|artifact| artifact.retention == RetentionClassV1::WorkLocal)
    }));
}

#[test]
fn oversized_shared_bodies_stay_work_local() {
    let temp = TempDir::new().unwrap();
    let spool = LocalSpool::open(temp.path().join("spool")).unwrap();
    let request = capture_request(CaptureModeV1::SharedConversation);
    let big = "a".repeat(agentscm::change_record::MAX_SHARED_BODY_BYTES as usize + 1024);
    let transcript = format!(
        "{{\"type\":\"user\",\"sessionId\":\"session_fixture\",\"timestamp\":\"2026-10-23T10:00:00Z\",\"uuid\":\"u1\",\"message\":{{\"content\":[{{\"type\":\"text\",\"text\":\"{big}\"}}]}}}}\n"
    );
    let report = import_jsonl(Cursor::new(transcript), &request, &spool).unwrap();
    assert_eq!(report.events, 2);
    let artifacts: Vec<_> = captured_events(&spool)
        .into_iter()
        .filter_map(|(_, event)| event.artifact)
        .collect();
    assert_eq!(artifacts.len(), 1);
    assert_eq!(artifacts[0].retention, RetentionClassV1::WorkLocal);
    assert_eq!(artifacts[0].availability, ContentAvailabilityV1::Local);
}

#[test]
fn shared_import_with_many_redacted_bodies_splits_batches_within_gap_limit() {
    let temp = TempDir::new().unwrap();
    let spool = LocalSpool::open(temp.path().join("spool")).unwrap();
    let request = capture_request(CaptureModeV1::SharedConversation);
    let (token, _) = synthetic_secrets();
    let mut transcript = String::new();
    for index in 0..45 {
        transcript.push_str(&format!(
            "{{\"type\":\"user\",\"sessionId\":\"session_fixture\",\"timestamp\":\"2026-10-23T10:00:00Z\",\"uuid\":\"u{index}\",\"message\":{{\"content\":[{{\"type\":\"text\",\"text\":\"deploy {index} with {token}\"}}]}}}}\n"
        ));
    }
    let report = import_jsonl(Cursor::new(transcript), &request, &spool).unwrap();
    assert!(report.batches >= 2);

    let pending = spool.pending(100).unwrap();
    let mut redacted = 0;
    let mut all_events = Vec::new();
    for envelope in &pending {
        envelope.validate().unwrap();
        let CaptureEnvelopeV1::EventBatch(batch) = envelope else {
            panic!("only event batches expected");
        };
        assert!(batch.events.len() <= agentscm::change_record::MAX_EVENTS_PER_BATCH);
        all_events.extend(batch.events.iter());
    }
    for event in &all_events {
        let Some(gap) = event
            .gap
            .as_ref()
            .filter(|g| g.reason == GapReasonV1::Redacted)
        else {
            continue;
        };
        redacted += 1;
        // A redaction gap points at the altered body event, which exists with that sequence.
        assert_eq!(gap.first_missing, gap.last_missing);
        assert_ne!(gap.first_missing, event.sequence);
        assert!(
            all_events
                .iter()
                .any(|other| other.sequence == gap.first_missing
                    && other.kind == CapturedEventKindV1::UserPrompt
                    && other.artifact.is_some())
        );
    }
    assert_eq!(redacted, 45);
}
