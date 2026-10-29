#![cfg(unix)]

use std::collections::BTreeSet;
use std::ffi::CString;
use std::fs;
use std::io::Cursor;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use agentscm::change_capture::claude_code::{CaptureRequest, import_jsonl};
use agentscm::change_capture::{LocalSpool, SpoolNamespaceV1};
use agentscm::change_record::{
    ActionFinishV1, ActionKindV1, ActionOutcomeV1, ActionStartV1, AssuranceBasisV1,
    CaptureEnvelopeV1, CaptureModeV1, CaptureOmissionV1, CheckResultV1, CoverageV1, DriftV1,
    RecipeProvenanceV1, SCHEMA_VERSION_V1, SourceBindingV1,
};
use agentscm::repo_explorer::evidence::{
    BodyAvailabilityV1, EvidenceApplicabilityV1, EvidenceEntryV1, EvidenceExplorer,
    EvidenceRelationV1, PathRelationV1, RetainedEnvelopeStateV1,
};
use tempfile::TempDir;

const REPOSITORY: &str = "owner/repo";
const COMMIT_ONE: &str = "1111111111111111111111111111111111111111";
const COMMIT_TWO: &str = "2222222222222222222222222222222222222222";

fn spool_fixture() -> (TempDir, PathBuf, LocalSpool) {
    let temp = TempDir::new().unwrap();
    let root = temp.path().join("spool");
    let spool = LocalSpool::open(&root).unwrap();
    spool
        .bind_namespace(SpoolNamespaceV1 {
            version: 1,
            hub: "http://127.0.0.1:7345".into(),
            repository: REPOSITORY.into(),
            worktree_id: "worktree-fixture".into(),
        })
        .unwrap();
    (temp, root, spool)
}

fn action_pair(
    work_id: &str,
    session_id: &str,
    action_id: &str,
    commit: &str,
    observed_at: i64,
) -> (CaptureEnvelopeV1, CaptureEnvelopeV1) {
    let start = CaptureEnvelopeV1::ActionStart(ActionStartV1 {
        schema_version: SCHEMA_VERSION_V1,
        operation_id: format!("start:{action_id}"),
        work_id: work_id.into(),
        generation: 1,
        session_id: session_id.into(),
        action_id: action_id.into(),
        source: SourceBindingV1::Git {
            repository: REPOSITORY.into(),
            worktree_id: "worktree-fixture".into(),
            head: commit.into(),
            tree: "a".repeat(40),
            base: "b".repeat(40),
            dirty: false,
            untracked_count: 0,
            omissions: Vec::<CaptureOmissionV1>::new(),
            captured_snapshot: None,
            snapshot_coverage: CoverageV1::Complete,
        },
        kind: ActionKindV1::Verification,
        recipe_provenance: RecipeProvenanceV1::ConfiguredExactMatch,
        recipe_digest: "c".repeat(64),
        setup_digest: "d".repeat(64),
        observed_order: observed_at as u64,
        observation_scope_id: Some("scope-fixture".into()),
        started_at: observed_at,
    });
    let finish = CaptureEnvelopeV1::ActionFinish(ActionFinishV1 {
        schema_version: SCHEMA_VERSION_V1,
        operation_id: format!("finish:{action_id}"),
        work_id: work_id.into(),
        generation: 1,
        action_id: action_id.into(),
        outcome: ActionOutcomeV1::Succeeded,
        result: CheckResultV1::Passed,
        assurance: AssuranceBasisV1::ObservedTrustedLocal,
        coverage: CoverageV1::Complete,
        finished_at: observed_at + 1,
        drift: DriftV1::NoneObserved,
        output_artifacts: Vec::new(),
    });
    (start, finish)
}

fn capture_request(work_id: &str, session_id: &str, prefix: &str) -> CaptureRequest {
    CaptureRequest {
        operation_prefix: prefix.into(),
        work_id: work_id.into(),
        generation: 1,
        observed_session_id: session_id.into(),
        canonical_session_id: session_id.into(),
        parent_session_id: None,
        capture_mode: CaptureModeV1::LocalConversation,
        live_capture_mode: CaptureModeV1::LocalConversation,
    }
}

#[test]
fn claude_visible_capture_projects_two_commits_and_concurrent_sessions_without_hunk_claims() {
    let (_temp, root, spool) = spool_fixture();
    let (first_start, first_finish) =
        action_pair("wrk_one", "session-one", "action-one", COMMIT_ONE, 10);
    let (second_start, second_finish) =
        action_pair("wrk_one", "session-one", "action-two", COMMIT_TWO, 20);
    for envelope in [&first_start, &first_finish, &second_start, &second_finish] {
        spool.append(envelope).unwrap();
    }

    let first_transcript = concat!(
        "{\"type\":\"user\",\"sessionId\":\"session-one\",\"timestamp\":\"2026-10-23T10:00:00Z\",\"uuid\":\"u1\",\"message\":{\"content\":[{\"type\":\"text\",\"text\":\"VISIBLE_PROMPT_ONE\"}]}}\n",
        "{\"type\":\"assistant\",\"sessionId\":\"session-one\",\"timestamp\":\"2026-10-23T10:00:01Z\",\"uuid\":\"a1\",\"message\":{\"content\":[{\"type\":\"thinking\",\"thinking\":\"HIDDEN_REASONING_NEVER_RETURN\"},{\"type\":\"text\",\"text\":\"VISIBLE_REPLY_ONE\"}]}}\n"
    );
    let report = import_jsonl(
        Cursor::new(first_transcript),
        &capture_request("wrk_one", "session-one", "capture:first"),
        &spool,
    )
    .unwrap();
    assert_eq!(report.private_blocks_ignored, 1);

    let second_transcript = "{\"type\":\"user\",\"sessionId\":\"session-two\",\"timestamp\":\"2026-10-23T10:01:00Z\",\"uuid\":\"u2\",\"message\":{\"content\":[{\"type\":\"text\",\"text\":\"VISIBLE_CONCURRENT_SESSION\"}]}}\n";
    import_jsonl(
        Cursor::new(second_transcript),
        &capture_request("wrk_one", "session-two", "capture:second"),
        &spool,
    )
    .unwrap();

    let explorer = EvidenceExplorer::open(&root, REPOSITORY).unwrap();
    let selected = explorer
        .evidence(COMMIT_ONE, Some("src/lib.rs"), None, None, 100)
        .unwrap();
    let actions: Vec<_> = selected
        .entries
        .iter()
        .filter_map(|entry| match entry {
            EvidenceEntryV1::Action(action) => Some(action),
            EvidenceEntryV1::Session(_) => None,
        })
        .collect();
    assert_eq!(actions.len(), 1);
    assert_eq!(actions[0].action_id, "action-one");
    assert_eq!(actions[0].relation, EvidenceRelationV1::ExactSourceBinding);
    assert_eq!(actions[0].path_relation, PathRelationV1::NotRecorded);
    assert_eq!(
        actions[0].applicability,
        EvidenceApplicabilityV1::MatchingRecordedCleanSource
    );

    let selected_work = explorer
        .evidence(COMMIT_ONE, Some("src/lib.rs"), Some("wrk_one"), None, 100)
        .unwrap();
    let mut action_applicability = selected_work
        .entries
        .iter()
        .filter_map(|entry| match entry {
            EvidenceEntryV1::Action(action) => {
                Some((action.action_id.as_str(), action.applicability))
            }
            EvidenceEntryV1::Session(_) => None,
        })
        .collect::<Vec<_>>();
    action_applicability.sort_by_key(|(action, _)| *action);
    assert_eq!(
        action_applicability,
        vec![
            (
                "action-one",
                EvidenceApplicabilityV1::MatchingRecordedCleanSource
            ),
            (
                "action-two",
                EvidenceApplicabilityV1::DifferentRecordedSource
            )
        ]
    );
    let sessions = selected_work
        .entries
        .iter()
        .filter_map(|entry| match entry {
            EvidenceEntryV1::Session(session) => Some(session.session_id.as_str()),
            EvidenceEntryV1::Action(_) => None,
        })
        .collect::<BTreeSet<_>>();
    assert_eq!(sessions, BTreeSet::from(["session-one", "session-two"]));

    let first_conversation = explorer
        .conversation("wrk_one", "session-one", None, 100)
        .unwrap();
    let serialized = serde_json::to_string(&first_conversation).unwrap();
    assert!(serialized.contains("VISIBLE_PROMPT_ONE"));
    assert!(serialized.contains("VISIBLE_REPLY_ONE"));
    assert!(!serialized.contains("HIDDEN_REASONING_NEVER_RETURN"));
    assert!(!serialized.contains("VISIBLE_CONCURRENT_SESSION"));
    assert!(
        first_conversation
            .events
            .iter()
            .all(|event| event.relation == EvidenceRelationV1::SameWorkSession)
    );
    let second_conversation = explorer
        .conversation("wrk_one", "session-two", None, 100)
        .unwrap();
    assert!(
        second_conversation
            .events
            .iter()
            .any(|event| { event.body.content.as_deref() == Some("VISIBLE_CONCURRENT_SESSION") })
    );
    assert!(second_conversation.events.iter().all(|event| {
        event.relation == EvidenceRelationV1::SameWorkSession
            && event.body.content.as_deref() != Some("VISIBLE_PROMPT_ONE")
            && event.body.content.as_deref() != Some("VISIBLE_REPLY_ONE")
    }));
}

#[test]
fn acknowledged_and_compacted_receipts_remain_visible_while_conversation_loss_is_explicit() {
    let (_temp, root, spool) = spool_fixture();
    let (start, finish) = action_pair("wrk_one", "session-one", "action-one", COMMIT_ONE, 10);
    spool.append(&start).unwrap();
    spool.append(&finish).unwrap();
    let transcript = "{\"type\":\"user\",\"sessionId\":\"session-one\",\"timestamp\":\"2026-10-23T10:00:00Z\",\"uuid\":\"u1\",\"message\":{\"content\":[{\"type\":\"text\",\"text\":\"VISIBLE_BEFORE_COMPACTION\"}]}}\n";
    import_jsonl(
        Cursor::new(transcript),
        &capture_request("wrk_one", "session-one", "capture:compact"),
        &spool,
    )
    .unwrap();
    let pending = spool.pending(20).unwrap();
    for envelope in &pending {
        spool.acknowledge(envelope).unwrap();
    }
    let report = spool.compact_acknowledged(20).unwrap();
    assert!(report.envelopes >= 3);

    let explorer = EvidenceExplorer::open(&root, REPOSITORY).unwrap();
    let evidence = explorer
        .evidence(COMMIT_ONE, None, Some("wrk_one"), None, 100)
        .unwrap();
    let action = evidence
        .entries
        .iter()
        .find_map(|entry| match entry {
            EvidenceEntryV1::Action(action) => Some(action),
            EvidenceEntryV1::Session(_) => None,
        })
        .unwrap();
    assert_eq!(action.start_retention, RetainedEnvelopeStateV1::Compacted);
    assert_eq!(
        action.result.as_ref().unwrap().retention,
        RetainedEnvelopeStateV1::Compacted
    );
    assert_eq!(
        evidence
            .projection_coverage
            .compacted_event_batches_unattributed,
        1
    );
    assert!(!evidence.projection_coverage.complete);
    let conversation = explorer
        .conversation("wrk_one", "session-one", None, 100)
        .unwrap();
    assert!(conversation.events.is_empty());
    assert_eq!(
        conversation
            .projection_coverage
            .compacted_event_batches_unattributed,
        1
    );
}

#[test]
fn missing_and_tampered_visible_bodies_are_explicit_without_artifact_identity_disclosure() {
    let (_temp, root, spool) = spool_fixture();
    let transcript = concat!(
        "{\"type\":\"user\",\"sessionId\":\"session-one\",\"timestamp\":\"2026-10-23T10:00:00Z\",\"uuid\":\"u1\",\"message\":{\"content\":[{\"type\":\"text\",\"text\":\"BODY_ONE\"}]}}\n",
        "{\"type\":\"user\",\"sessionId\":\"session-one\",\"timestamp\":\"2026-10-23T10:00:01Z\",\"uuid\":\"u2\",\"message\":{\"content\":[{\"type\":\"text\",\"text\":\"BODY_TWO\"}]}}\n"
    );
    import_jsonl(
        Cursor::new(transcript),
        &capture_request("wrk_one", "session-one", "capture:bodies"),
        &spool,
    )
    .unwrap();
    let mut artifacts = fs::read_dir(root.join("artifacts"))
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .collect::<Vec<_>>();
    artifacts.sort();
    assert_eq!(artifacts.len(), 2);
    fs::remove_file(&artifacts[0]).unwrap();
    let original = fs::read(&artifacts[1]).unwrap();
    fs::write(&artifacts[1], vec![b'x'; original.len()]).unwrap();

    let explorer = EvidenceExplorer::open(&root, REPOSITORY).unwrap();
    let page = explorer
        .conversation("wrk_one", "session-one", None, 100)
        .unwrap();
    let availability = page
        .events
        .iter()
        .map(|event| event.body.availability)
        .collect::<BTreeSet<_>>();
    assert!(availability.contains(&BodyAvailabilityV1::Missing));
    assert!(availability.contains(&BodyAvailabilityV1::IntegrityFailure));
    assert!(availability.iter().all(|value| matches!(
        value,
        BodyAvailabilityV1::Missing
            | BodyAvailabilityV1::IntegrityFailure
            | BodyAvailabilityV1::NotApplicable
    )));
    let json = serde_json::to_string(&page).unwrap();
    for artifact in artifacts {
        let digest = artifact.file_name().unwrap().to_string_lossy();
        assert!(!json.contains(digest.as_ref()));
    }
}

#[test]
fn conversation_page_stays_below_the_http_response_budget() {
    let (_temp, root, spool) = spool_fixture();
    let content = "v".repeat(220 * 1024);
    let mut transcript = Vec::new();
    for index in 0..20 {
        let line = serde_json::json!({
            "type": "user",
            "sessionId": "session-one",
            "timestamp": format!("2026-10-23T10:00:{index:02}Z"),
            "uuid": format!("u{index}"),
            "message": { "content": [{ "type": "text", "text": content }] }
        });
        transcript.extend_from_slice(&serde_json::to_vec(&line).unwrap());
        transcript.push(b'\n');
    }
    import_jsonl(
        Cursor::new(transcript),
        &capture_request("wrk_one", "session-one", "capture:bounded-page"),
        &spool,
    )
    .unwrap();

    let page = EvidenceExplorer::open(&root, REPOSITORY)
        .unwrap()
        .conversation("wrk_one", "session-one", None, 100)
        .unwrap();
    assert!(page.events.len() < 20);
    assert!(page.next_cursor.is_some());
    let verified_bodies = page
        .events
        .iter()
        .filter(|event| event.body.availability == BodyAvailabilityV1::Available)
        .count();
    assert_eq!(
        page.projection_coverage.unverified_bodies,
        20 - verified_bodies
    );
    assert!(!page.projection_coverage.complete);
    assert!(serde_json::to_vec(&page).unwrap().len() < 4 * 1024 * 1024);
}

#[test]
fn later_page_body_damage_is_unverified_in_evidence_and_earlier_conversation_pages() {
    let (_temp, root, spool) = spool_fixture();
    let mut transcript = Vec::new();
    for index in 0..27 {
        let line = serde_json::json!({
            "type": "user",
            "sessionId": "session-one",
            "timestamp": format!("2026-10-23T10:00:{index:02}Z"),
            "uuid": format!("later-page-{index}"),
            "message": { "content": [{ "type": "text", "text": format!("BODY_{index:02}") }] }
        });
        transcript.extend_from_slice(&serde_json::to_vec(&line).unwrap());
        transcript.push(b'\n');
    }
    import_jsonl(
        Cursor::new(transcript),
        &capture_request("wrk_one", "session-one", "capture:later-page"),
        &spool,
    )
    .unwrap();

    let retained = spool.pending(100).unwrap();
    let mut artifact_paths = retained
        .iter()
        .filter_map(|envelope| match envelope {
            CaptureEnvelopeV1::EventBatch(batch) => Some(&batch.events),
            CaptureEnvelopeV1::ActionStart(_)
            | CaptureEnvelopeV1::ActionFinish(_)
            | CaptureEnvelopeV1::Usage(_) => None,
        })
        .flatten()
        .filter_map(|event| {
            event.artifact.as_ref().map(|artifact| {
                (
                    event.sequence,
                    root.join("artifacts").join(&artifact.digest),
                )
            })
        })
        .collect::<Vec<_>>();
    artifact_paths.sort_by_key(|(sequence, _)| *sequence);
    assert_eq!(artifact_paths.len(), 27);
    fs::remove_file(&artifact_paths[25].1).unwrap();
    let original = fs::read(&artifact_paths[26].1).unwrap();
    fs::write(&artifact_paths[26].1, vec![b'x'; original.len()]).unwrap();

    let explorer = EvidenceExplorer::open(&root, REPOSITORY).unwrap();
    let evidence = explorer
        .evidence(COMMIT_ONE, None, Some("wrk_one"), None, 100)
        .unwrap();
    assert_eq!(evidence.projection_coverage.unavailable_bodies, 0);
    assert_eq!(evidence.projection_coverage.unverified_bodies, 27);
    assert!(!evidence.projection_coverage.complete);

    let first = explorer
        .conversation("wrk_one", "session-one", None, 25)
        .unwrap();
    assert_eq!(first.events.len(), 25);
    assert_eq!(first.projection_coverage.unavailable_bodies, 0);
    assert_eq!(first.projection_coverage.unverified_bodies, 3);
    assert!(!first.projection_coverage.complete);
    let second = explorer
        .conversation("wrk_one", "session-one", first.next_cursor.as_deref(), 25)
        .unwrap();
    assert_eq!(second.events.len(), 3);
    assert_eq!(second.projection_coverage.unavailable_bodies, 2);
    assert_eq!(second.projection_coverage.unverified_bodies, 24);
    assert!(!second.projection_coverage.complete);
    let availability = second
        .events
        .iter()
        .map(|event| event.body.availability)
        .collect::<BTreeSet<_>>();
    assert_eq!(
        availability,
        BTreeSet::from([
            BodyAvailabilityV1::Available,
            BodyAvailabilityV1::Missing,
            BodyAvailabilityV1::IntegrityFailure
        ])
    );
}

#[test]
fn read_only_open_rejects_wrong_namespace_fifo_and_descriptor_replacement_without_writes() {
    let (temp, root, spool) = spool_fixture();
    let (start, finish) = action_pair("wrk_one", "session-one", "action-one", COMMIT_ONE, 10);
    spool.append(&start).unwrap();
    spool.append(&finish).unwrap();
    let before = retained_files(&root);
    assert!(EvidenceExplorer::open(&root, "other-repository").is_err());
    assert_eq!(retained_files(&root), before);

    let explorer = EvidenceExplorer::open(&root, REPOSITORY).unwrap();
    let displaced = temp.path().join("displaced-spool");
    fs::rename(&root, &displaced).unwrap();
    fs::create_dir(&root).unwrap();
    fs::set_permissions(&root, fs::Permissions::from_mode(0o700)).unwrap();
    assert!(explorer.evidence(COMMIT_ONE, None, None, None, 10).is_err());

    fs::remove_dir(&root).unwrap();
    fs::rename(&displaced, &root).unwrap();
    let child_explorer = EvidenceExplorer::open(&root, REPOSITORY).unwrap();
    fs::rename(root.join("outbox"), root.join("old-outbox")).unwrap();
    fs::create_dir(root.join("outbox")).unwrap();
    fs::set_permissions(root.join("outbox"), fs::Permissions::from_mode(0o700)).unwrap();
    assert!(
        child_explorer
            .evidence(COMMIT_ONE, None, None, None, 10)
            .is_err()
    );

    fs::remove_dir(root.join("outbox")).unwrap();
    fs::rename(root.join("old-outbox"), root.join("outbox")).unwrap();
    fs::remove_file(root.join("namespace-v1.json")).unwrap();
    let fifo = CString::new(root.join("namespace-v1.json").as_os_str().as_bytes()).unwrap();
    // SAFETY: `fifo` is a valid NUL-terminated path and the selected path is inside the fixture.
    assert_eq!(unsafe { libc::mkfifo(fifo.as_ptr(), 0o600) }, 0);
    assert!(EvidenceExplorer::open(&root, REPOSITORY).is_err());
}

#[test]
fn writer_outbox_temp_is_ignored_without_mutation_but_malformed_entries_fail() {
    let (_temp, root, spool) = spool_fixture();
    let (start, finish) = action_pair("wrk_one", "session-one", "action-one", COMMIT_ONE, 10);
    spool.append(&start).unwrap();
    spool.append(&finish).unwrap();
    let transcript = "{\"type\":\"user\",\"sessionId\":\"session-one\",\"timestamp\":\"2026-10-23T10:00:00Z\",\"uuid\":\"u1\",\"message\":{\"content\":[{\"type\":\"text\",\"text\":\"VISIBLE_DURING_STAGING\"}]}}\n";
    import_jsonl(
        Cursor::new(transcript),
        &capture_request("wrk_one", "session-one", "capture:staging"),
        &spool,
    )
    .unwrap();

    let key = "e".repeat(64);
    let valid_temp = root
        .join("outbox")
        .join(format!(".capture-{key}.json-123-1.tmp"));
    fs::write(&valid_temp, b"incomplete writer staging bytes").unwrap();
    fs::set_permissions(&valid_temp, fs::Permissions::from_mode(0o600)).unwrap();
    let before = fs::read(&valid_temp).unwrap();

    let explorer = EvidenceExplorer::open(&root, REPOSITORY).unwrap();
    let evidence = explorer
        .evidence(COMMIT_ONE, None, Some("wrk_one"), None, 100)
        .unwrap();
    assert!(evidence.entries.iter().any(|entry| matches!(
        entry,
        EvidenceEntryV1::Action(action) if action.action_id == "action-one"
    )));
    let conversation = explorer
        .conversation("wrk_one", "session-one", None, 100)
        .unwrap();
    assert!(
        conversation
            .events
            .iter()
            .any(|event| { event.body.content.as_deref() == Some("VISIBLE_DURING_STAGING") })
    );
    assert_eq!(fs::read(&valid_temp).unwrap(), before);

    let malformed = root
        .join("outbox")
        .join(format!(".capture-{key}.json-x-1.tmp"));
    fs::write(&malformed, b"malformed staging name").unwrap();
    fs::set_permissions(&malformed, fs::Permissions::from_mode(0o600)).unwrap();
    assert!(
        explorer
            .evidence(COMMIT_ONE, None, Some("wrk_one"), None, 100)
            .is_err()
    );
    assert_eq!(fs::read(&valid_temp).unwrap(), before);
    assert_eq!(fs::read(&malformed).unwrap(), b"malformed staging name");
}

#[test]
fn projection_record_bound_and_projection_bound_cursor_fail_closed() {
    let (_temp, root, spool) = spool_fixture();
    let (start, finish) = action_pair("wrk_one", "session-one", "action-one", COMMIT_ONE, 10);
    spool.append(&start).unwrap();
    spool.append(&finish).unwrap();
    let (second_start, second_finish) =
        action_pair("wrk_one", "session-one", "action-two", COMMIT_ONE, 20);
    spool.append(&second_start).unwrap();
    spool.append(&second_finish).unwrap();
    let explorer = EvidenceExplorer::open(&root, REPOSITORY).unwrap();
    let first = explorer
        .evidence(COMMIT_ONE, None, Some("wrk_one"), None, 1)
        .unwrap();
    let cursor = first.next_cursor.unwrap();
    let (next_start, next_finish) =
        action_pair("wrk_one", "session-one", "action-three", COMMIT_ONE, 30);
    spool.append(&next_start).unwrap();
    spool.append(&next_finish).unwrap();
    assert!(
        explorer
            .evidence(COMMIT_ONE, None, Some("wrk_one"), Some(&cursor), 1,)
            .is_err()
    );

    for index in 0..=8_192 {
        fs::write(
            root.join("outbox").join(format!("overflow-{index}.json")),
            b"{}",
        )
        .unwrap();
    }
    assert!(explorer.evidence(COMMIT_ONE, None, None, None, 10).is_err());
}

fn retained_files(root: &Path) -> Vec<(PathBuf, Vec<u8>)> {
    fn visit(root: &Path, current: &Path, values: &mut Vec<(PathBuf, Vec<u8>)>) {
        for entry in fs::read_dir(current).unwrap() {
            let path = entry.unwrap().path();
            let relative = path.strip_prefix(root).unwrap().to_owned();
            if path.is_dir() {
                visit(root, &path, values);
            } else if path.is_file() {
                values.push((relative, fs::read(path).unwrap()));
            }
        }
    }
    let mut values = Vec::new();
    visit(root, root, &mut values);
    values.sort_by(|left, right| left.0.cmp(&right.0));
    values
}

#[test]
fn a_conversation_without_recorded_actions_is_listed_by_work_not_by_commit() {
    let (_temp, root, spool) = spool_fixture();
    let transcript = "{\"type\":\"user\",\"sessionId\":\"session-lone\",\"timestamp\":\"2026-10-23T11:00:00Z\",\"uuid\":\"u9\",\"message\":{\"content\":[{\"type\":\"text\",\"text\":\"SYNTHETIC_LONE_PROMPT\"}]}}\n";
    import_jsonl(
        Cursor::new(transcript),
        &capture_request("wrk_lone", "session-lone", "capture:lone"),
        &spool,
    )
    .unwrap();

    let explorer = EvidenceExplorer::open(&root, REPOSITORY).unwrap();
    // No action binds this Work to any commit, so commit evidence must not invent one.
    let at_commit = explorer
        .evidence(COMMIT_ONE, None, None, None, 100)
        .unwrap();
    assert!(at_commit.entries.is_empty());

    let listed = explorer.sessions(None, 25).unwrap();
    assert_eq!(listed.sessions.len(), 1);
    let session = &listed.sessions[0];
    assert_eq!(
        (session.work_id.as_str(), session.session_id.as_str()),
        ("wrk_lone", "session-lone")
    );
    assert_eq!(session.relation, EvidenceRelationV1::SameWorkSession);
    assert!(listed.complete);

    let conversation = explorer
        .conversation(&session.work_id, &session.session_id, None, 25)
        .unwrap();
    assert!(conversation.events.iter().any(|event| {
        event
            .body
            .content
            .as_deref()
            .is_some_and(|text| text.contains("SYNTHETIC_LONE_PROMPT"))
    }));

    let first = explorer.sessions(None, 1).unwrap();
    assert!(first.next_cursor.is_none());
    assert!(explorer.sessions(Some("x1:s:bogus:0"), 1).is_err());
}

#[test]
fn saved_sessions_page_newest_first_and_cursors_stay_bound_to_their_query() {
    let (_temp, root, spool) = spool_fixture();
    let import = |work: &str, session: &str, at: &str| {
        let line = format!(
            "{{\"type\":\"user\",\"sessionId\":\"{session}\",\"timestamp\":\"{at}\",\"uuid\":\"u-{session}\",\"message\":{{\"content\":[{{\"type\":\"text\",\"text\":\"SYNTHETIC\"}}]}}}}\n"
        );
        import_jsonl(
            Cursor::new(line),
            &capture_request(work, session, &format!("capture:{session}")),
            &spool,
        )
        .unwrap();
    };
    import("wrk_a", "session-old", "2026-10-23T09:00:00Z");
    import("wrk_b", "session-tie-b", "2026-10-23T10:00:00Z");
    import("wrk_a", "session-tie-a", "2026-10-23T10:00:00Z");

    let explorer = EvidenceExplorer::open(&root, REPOSITORY).unwrap();
    let all = explorer.sessions(None, 25).unwrap();
    let order: Vec<_> = all.sessions.iter().map(|s| s.session_id.as_str()).collect();
    // Newest first; equal times fall back to Work and session identity, descending.
    assert_eq!(order, ["session-tie-b", "session-tie-a", "session-old"]);

    let mut walked = Vec::new();
    let mut cursor = None;
    loop {
        let page = explorer.sessions(cursor.as_deref(), 1).unwrap();
        assert_eq!(page.complete, page.next_cursor.is_none());
        walked.extend(page.sessions.into_iter().map(|s| s.session_id));
        match page.next_cursor {
            Some(next) => cursor = Some(next),
            None => break,
        }
    }
    assert_eq!(walked, order);

    let first = explorer.sessions(None, 1).unwrap();
    let next = first.next_cursor.unwrap();
    assert!(
        explorer
            .conversation("wrk_a", "session-old", Some(&next), 25)
            .is_err(),
        "a sessions cursor cannot page a conversation"
    );
    import("wrk_c", "session-new", "2026-10-23T11:00:00Z");
    assert!(
        explorer.sessions(Some(&next), 1).is_err(),
        "a cursor from an earlier projection is refused"
    );
}
