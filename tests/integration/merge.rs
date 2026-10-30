use std::fs;
use std::path::Path;

use agentscm::Error;
use agentscm::context::ChangeId;
use agentscm::merge::{
    BaseChoice, ConflictKind, MergeJournal, MergeOutcome, MergePhase, Resolution,
};
use agentscm::model::{EntryChange, EntryKind, FileChange};
use agentscm::store::Engine;
use agentscm::sync::{import_bundle, qualify_source, write_bundle, write_offer, write_request};
use agentscm::workspace::{OperationOutcome, Workspace};
use rusqlite::{Connection, params};
use tempfile::TempDir;

fn put(path: &str, bytes: &[u8]) -> FileChange {
    FileChange {
        path: path.into(),
        content: Some(bytes.to_vec()),
    }
}

fn prepared(outcome: MergeOutcome) -> (String, u64, MergePhase) {
    match outcome {
        MergeOutcome::Prepared {
            session,
            generation,
            phase,
        } => (session, generation, phase),
        _ => panic!("expected prepared merge"),
    }
}

fn completed(outcome: MergeOutcome) -> (String, u64) {
    match outcome {
        MergeOutcome::Completed {
            revision,
            generation,
            ..
        } => (revision, generation),
        _ => panic!("expected completed merge"),
    }
}

fn transfer(from: &Engine, to: &Engine, root: &str) {
    let files = TempDir::new().unwrap();
    let offer = files.path().join("offer.json");
    let request = files.path().join("request.json");
    let bundle = files.path().join("source.bundle");
    write_offer(from, "repository", &[root.to_owned()], &offer).unwrap();
    write_request(to, &offer, &request).unwrap();
    write_bundle(from, &offer, &request, &bundle).unwrap();
    let outcome = import_bundle(to, &bundle, &files.path().join("transfer")).unwrap();
    assert_eq!(outcome.roots, [root]);
    qualify_source(to, &[root.to_owned()]).unwrap();
}

fn object_path(data: &Path, id: &str) -> std::path::PathBuf {
    data.join("objects").join(&id[..2]).join(&id[2..])
}

#[test]
fn two_offline_stores_merge_disjoint_same_file_edits_and_apply_identical_trees() {
    let a = TempDir::new().unwrap();
    let b = TempDir::new().unwrap();
    let ea = Engine::open(a.path()).unwrap();
    let eb = Engine::open(b.path()).unwrap();
    let base = ea
        .checkpoint(
            None,
            &[
                put("file", b"one\ntwo\nthree\nfour\n"),
                put("keep", b"unchanged"),
            ],
        )
        .unwrap();
    transfer(&ea, &eb, &base);
    let left = ea
        .checkpoint(
            Some(&base),
            &[put("file", b"ONE\ntwo\nthree\nfour\n"), put("left", b"a")],
        )
        .unwrap();
    let right = eb
        .checkpoint(
            Some(&base),
            &[put("file", b"one\ntwo\nthree\nFOUR\n"), put("right", b"b")],
        )
        .unwrap();
    transfer(&ea, &eb, &left);
    transfer(&eb, &ea, &right);
    let mut ja = MergeJournal::open(a.path(), "repository").unwrap();
    let mut jb = MergeJournal::open(b.path(), "repository").unwrap();
    let (sa, ga, pa) = prepared(ja.begin(&ea, "begin-a", &left, &right).unwrap());
    let (sb, gb, pb) = prepared(jb.begin(&eb, "begin-b", &right, &left).unwrap());
    assert_eq!((pa, pb), (MergePhase::Ready, MergePhase::Ready));
    let (ma, ca) = completed(ja.complete(&ea, &sa, "complete-a", ga).unwrap());
    let (mb, cb) = completed(jb.complete(&eb, &sb, "complete-b", gb).unwrap());
    assert_ne!(ma, mb);
    assert_eq!(
        ea.load_revision(&ma).unwrap().parents,
        [left.clone(), right.clone()]
    );
    assert_eq!(
        eb.load_revision(&mb).unwrap().parents,
        [right.clone(), left.clone()]
    );
    assert_eq!(
        ea.load_revision(&ma).unwrap().tree,
        eb.load_revision(&mb).unwrap().tree
    );
    assert_eq!(
        ea.read_file(&ma, "file").unwrap(),
        b"ONE\ntwo\nthree\nFOUR\n"
    );
    transfer(&ea, &eb, &ma);
    transfer(&eb, &ea, &mb);
    let work = TempDir::new().unwrap();
    let change = ChangeId {
        repository: "repository".into(),
        id: "change".into(),
    };
    let mut wa = Workspace::create(&ea, work.path().join("a"), change.clone(), &left).unwrap();
    let mut wb = Workspace::create(&eb, work.path().join("b"), change, &right).unwrap();
    let aa = ja.completion(&sa, ca).unwrap();
    let ab = jb.completion(&sb, cb).unwrap();
    assert!(matches!(
        wa.apply(&ea, "apply-a", 0, &aa).unwrap(),
        OperationOutcome::Applied { .. }
    ));
    assert!(matches!(
        wb.apply(&eb, "apply-b", 0, &ab).unwrap(),
        OperationOutcome::Applied { .. }
    ));
    for name in ["file", "keep", "left", "right"] {
        assert_eq!(
            fs::read(work.path().join("a").join(name)).unwrap(),
            fs::read(work.path().join("b").join(name)).unwrap()
        );
    }
    assert_eq!(
        ja.complete(&ea, &sa, "complete-a", ga).unwrap(),
        MergeOutcome::Completed {
            session: sa,
            generation: ca,
            revision: ma
        }
    );
}

#[test]
fn overlap_remains_durable_until_exact_resolution_and_retries_bind_inputs() {
    let data = TempDir::new().unwrap();
    let engine = Engine::open(data.path()).unwrap();
    let base = engine.checkpoint(None, &[put("file", b"base\n")]).unwrap();
    let left = engine
        .checkpoint(Some(&base), &[put("file", b"left\n")])
        .unwrap();
    let right = engine
        .checkpoint(Some(&base), &[put("file", b"right\n")])
        .unwrap();
    let mut journal = MergeJournal::open(data.path(), "repository").unwrap();
    let begin = journal.begin(&engine, "begin", &left, &right).unwrap();
    let (session, generation, phase) = prepared(begin.clone());
    assert_eq!(phase, MergePhase::Conflicts);
    assert_eq!(
        journal.begin(&engine, "begin", &left, &right).unwrap(),
        begin
    );
    assert!(matches!(
        journal.begin(&engine, "begin", &right, &left),
        Err(Error::Conflict(_))
    ));
    assert!(matches!(
        journal.complete(&engine, &session, "early", generation),
        Err(Error::Conflict(_))
    ));
    drop(journal);
    let mut journal = MergeJournal::open(data.path(), "repository").unwrap();
    let conflicts = journal.conflicts(&session, None, 1).unwrap();
    let id = &conflicts.entries[0].id;
    assert!(conflicts.entries[0].preview.is_none());
    let detail = journal.conflict(&session, id).unwrap();
    assert_eq!(detail.kind, ConflictKind::Content);
    assert!(!detail.resolved);
    assert!(
        detail
            .preview
            .unwrap()
            .windows(7)
            .any(|bytes| bytes == b"<<<<<<<")
    );
    assert_eq!(engine.read_file(&left, "file").unwrap(), b"left\n");
    assert_eq!(engine.read_file(&right, "file").unwrap(), b"right\n");
    assert!(matches!(
        journal.resolve(
            &engine,
            &session,
            "stale",
            generation - 1,
            id,
            Resolution::Delete
        ),
        Err(Error::Conflict(_))
    ));
    let first = journal
        .resolve(
            &engine,
            &session,
            "resolve",
            generation,
            id,
            Resolution::Bytes(b"combined\n".to_vec()),
        )
        .unwrap();
    assert_eq!(
        journal
            .resolve(
                &engine,
                &session,
                "resolve",
                generation,
                id,
                Resolution::Bytes(b"combined\n".to_vec())
            )
            .unwrap(),
        first
    );
    assert!(matches!(
        journal.resolve(
            &engine,
            &session,
            "resolve",
            generation,
            id,
            Resolution::Right
        ),
        Err(Error::Conflict(_))
    ));
    let current = journal.inspect(&session).unwrap();
    // A fresh generation may revise an earlier choice before publication.
    journal
        .resolve(
            &engine,
            &session,
            "change-choice",
            current.generation,
            id,
            Resolution::Right,
        )
        .unwrap();
    let current = journal.inspect(&session).unwrap();
    let (merged, final_generation) = completed(
        journal
            .complete(&engine, &session, "complete", current.generation)
            .unwrap(),
    );
    assert_eq!(engine.read_file(&merged, "file").unwrap(), b"right\n");
    assert!(journal.completion(&session, final_generation - 1).is_err());
    assert_eq!(
        journal
            .completion(&session, final_generation)
            .unwrap()
            .revision(),
        merged
    );
    assert!(
        journal
            .abort(&session, "late-abort", final_generation)
            .is_err()
    );
}

#[test]
fn eof_boundary_join_stays_a_durable_content_conflict_and_blocks_completion() {
    let data = TempDir::new().unwrap();
    let engine = Engine::open(data.path()).unwrap();
    let base = engine.checkpoint(None, &[put("file", b"b\n")]).unwrap();
    let left = engine
        .checkpoint(Some(&base), &[put("file", b"B")])
        .unwrap();
    let right = engine
        .checkpoint(Some(&base), &[put("file", b"b\nc\n")])
        .unwrap();
    let mut journal = MergeJournal::open(data.path(), "repository").unwrap();
    let (session, generation, phase) =
        prepared(journal.begin(&engine, "begin-eof", &left, &right).unwrap());
    assert_eq!(phase, MergePhase::Conflicts);
    drop(journal);

    let mut journal = MergeJournal::open(data.path(), "repository").unwrap();
    let state = journal.inspect(&session).unwrap();
    assert_eq!(state.phase, MergePhase::Conflicts);
    assert_eq!(state.unresolved, 1);
    let page = journal.conflicts(&session, None, 1).unwrap();
    assert_eq!(page.entries.len(), 1);
    let conflict = journal.conflict(&session, &page.entries[0].id).unwrap();
    assert_eq!(conflict.kind, ConflictKind::Content);
    assert!(!conflict.resolved);
    assert!(conflict.resolution.is_none());
    assert!(
        conflict
            .preview
            .unwrap()
            .windows(b"<<<<<<<".len())
            .any(|window| window == b"<<<<<<<")
    );
    assert!(matches!(
        journal.complete(&engine, &session, "complete-eof", generation),
        Err(Error::Conflict(_))
    ));
    assert_eq!(engine.read_file(&left, "file").unwrap(), b"B");
    assert_eq!(engine.read_file(&right, "file").unwrap(), b"b\nc\n");
}

#[test]
fn multiple_best_bases_and_unrelated_histories_need_explicit_recorded_choice() {
    let data = TempDir::new().unwrap();
    let e = Engine::open(data.path()).unwrap();
    let base = e.checkpoint(None, &[put("file", b"base")]).unwrap();
    let a = e.checkpoint(Some(&base), &[put("a", b"a")]).unwrap();
    let b = e.checkpoint(Some(&base), &[put("b", b"b")]).unwrap();
    let left = e
        .checkpoint_with_parents(&[a.clone(), b.clone()], &[])
        .unwrap();
    let right = e
        .checkpoint_with_parents(&[b.clone(), a.clone()], &[])
        .unwrap();
    let mut j = MergeJournal::open(data.path(), "repository").unwrap();
    let (s, g, p) = prepared(j.begin(&e, "crisscross", &left, &right).unwrap());
    assert_eq!(p, MergePhase::AwaitingBase);
    let first = j.bases(&s, None, 1).unwrap();
    let second = j.bases(&s, first.next.as_deref(), 1).unwrap();
    let mut actual = vec![first.entries[0].clone(), second.entries[0].clone()];
    actual.sort();
    let mut expected = vec![a.clone(), b];
    expected.sort();
    assert_eq!(actual, expected);
    assert!(
        j.select_base(&e, &s, "wrong-base", g, BaseChoice::Revision(base))
            .is_err()
    );
    assert!(
        j.select_base(&e, &s, "wrong-empty", g, BaseChoice::Empty)
            .is_err()
    );
    j.select_base(&e, &s, "base", g, BaseChoice::Revision(a))
        .unwrap();
    assert!(j.bases(&s, first.next.as_deref(), 1).is_err());
    let unrelated = e.checkpoint(None, &[put("other", b"other")]).unwrap();
    let (s, g, p) = prepared(j.begin(&e, "unrelated", &left, &unrelated).unwrap());
    assert_eq!(p, MergePhase::AwaitingBase);
    assert_eq!(j.inspect(&s).unwrap().base_count, 0);
    j.select_base(&e, &s, "empty", g, BaseChoice::Empty)
        .unwrap();
    let state = j.inspect(&s).unwrap();
    let (merged, _) = completed(
        j.complete(&e, &s, "unrelated-complete", state.generation)
            .unwrap(),
    );
    assert_eq!(e.read_file(&merged, "other").unwrap(), b"other");
}

#[test]
fn failed_planning_recovers_after_real_bundle_arrival_and_abort_cancels_pending_retry() {
    let source = TempDir::new().unwrap();
    let receiver = TempDir::new().unwrap();
    let e = Engine::open(source.path()).unwrap();
    let r = Engine::open(receiver.path()).unwrap();
    let base = e.checkpoint(None, &[put("file", b"base")]).unwrap();
    let left = e.checkpoint(Some(&base), &[put("a", b"a")]).unwrap();
    let right = e.checkpoint(Some(&base), &[put("b", b"b")]).unwrap();
    let mut j = MergeJournal::open(receiver.path(), "repository").unwrap();
    assert!(j.begin(&r, "begin", &left, &right).is_err());
    let session = j.operation_session("begin").unwrap();
    assert_eq!(j.inspect(&session).unwrap().phase, MergePhase::Planning);
    drop(j);
    transfer(&e, &r, &left);
    transfer(&e, &r, &right);
    let mut j = MergeJournal::open(receiver.path(), "repository").unwrap();
    assert!(matches!(
        j.recover(&r, &session).unwrap(),
        MergeOutcome::Prepared {
            phase: MergePhase::Ready,
            ..
        }
    ));
    let unknown = "f".repeat(64);
    assert!(j.begin(&r, "missing", &left, &unknown).is_err());
    let pending = j.operation_session("missing").unwrap();
    let abort = j.abort(&pending, "abort", 0).unwrap();
    assert_eq!(j.begin(&r, "missing", &left, &unknown).unwrap(), abort);
    assert_eq!(j.abort(&pending, "abort", 0).unwrap(), abort);
    assert!(j.recover(&r, &pending).is_err());
    assert!(MergeJournal::open(receiver.path(), "foreign-repository").is_err());
}

#[test]
fn publishing_failure_recovers_actual_candidate_or_can_abort_at_row_capacity() {
    let data = TempDir::new().unwrap();
    let e = Engine::open(data.path()).unwrap();
    let base = e.checkpoint(None, &[put("file", b"base\n")]).unwrap();
    let left = e
        .checkpoint(Some(&base), &[put("file", b"left\n")])
        .unwrap();
    let right = e
        .checkpoint(Some(&base), &[put("file", b"right\n")])
        .unwrap();
    let mut j = MergeJournal::open(data.path(), "repository").unwrap();
    let (s, g, _) = prepared(j.begin(&e, "begin", &left, &right).unwrap());
    let id = j.conflicts(&s, None, 1).unwrap().entries[0].id.clone();
    j.resolve(&e, &s, "resolve", g, &id, Resolution::Left)
        .unwrap();
    let state = j.inspect(&s).unwrap();
    let mut connection = Connection::open(data.path().join("merges/state.sqlite")).unwrap();
    let tx = connection.transaction().unwrap();
    let outcome = serde_json::to_vec(&MergeOutcome::Resolved {
        session: s.clone(),
        generation: state.generation,
        phase: MergePhase::Ready,
    })
    .unwrap();
    for n in 0..9_996 {
        tx.execute(
            "INSERT INTO merge_operations(operation,session,kind,request_digest,expected_generation,outcome_json) VALUES(?1,?2,'resolve',?3,?4,?5)",
            params![
                format!("retained-{n}"),
                s,
                "a".repeat(64),
                state.generation,
                outcome
            ],
        )
        .unwrap();
    }
    tx.commit().unwrap();
    assert!(matches!(
        j.resolve(
            &e,
            &s,
            "over-quota",
            state.generation,
            &id,
            Resolution::Right
        ),
        Err(Error::Limit(_))
    ));
    let path = object_path(data.path(), &left);
    let bytes = fs::read(&path).unwrap();
    fs::remove_file(&path).unwrap();
    assert!(j.complete(&e, &s, "complete", state.generation).is_err());
    assert_eq!(j.inspect(&s).unwrap().phase, MergePhase::Publishing);
    // The remaining terminal slot is reserved even after an interrupted complete.
    let abort = j.abort(&s, "abort", state.generation).unwrap();
    fs::write(path, bytes).unwrap();
    assert_eq!(
        j.complete(&e, &s, "complete", state.generation).unwrap(),
        abort
    );
    assert!(matches!(
        j.begin(&e, "new-at-cap", &left, &right),
        Err(Error::Limit(_))
    ));
}

#[test]
fn publication_restart_qualifies_missing_bytes_before_acknowledging_completed() {
    let data = TempDir::new().unwrap();
    let e = Engine::open(data.path()).unwrap();
    let base = e.checkpoint(None, &[put("file", b"base")]).unwrap();
    let left = e.checkpoint(Some(&base), &[put("a", b"a")]).unwrap();
    let right = e.checkpoint(Some(&base), &[put("b", b"b")]).unwrap();
    let mut j = MergeJournal::open(data.path(), "repository").unwrap();
    let (s, g, _) = prepared(j.begin(&e, "begin", &left, &right).unwrap());
    let path = object_path(data.path(), &right);
    let bytes = fs::read(&path).unwrap();
    fs::remove_file(&path).unwrap();
    assert!(j.complete(&e, &s, "complete", g).is_err());
    assert!(j.completion(&s, g).is_err());
    drop(j);
    fs::write(path, bytes).unwrap();
    let mut j = MergeJournal::open(data.path(), "repository").unwrap();
    let outcome = j.recover(&e, &s).unwrap();
    let (merged, generation) = completed(outcome.clone());
    qualify_source(&e, &[merged]).unwrap();
    assert_eq!(j.complete(&e, &s, "complete", g).unwrap(), outcome);
    assert_eq!(j.inspect(&s).unwrap().generation, generation);
}

#[test]
fn binary_profile_and_type_collisions_remain_inspectable_conflicts() {
    let data = TempDir::new().unwrap();
    let e = Engine::open(data.path()).unwrap();
    let base = e
        .checkpoint(
            None,
            &[
                put("binary", b"\0base"),
                put("long", &b"base\n".repeat(513)),
            ],
        )
        .unwrap();
    let left = e
        .checkpoint(
            Some(&base),
            &[
                put("binary", b"\0left"),
                put("long", &b"left\n".repeat(513)),
                put("Dir", b"file"),
            ],
        )
        .unwrap();
    let right = e
        .checkpoint(
            Some(&base),
            &[
                put("binary", b"\0right"),
                put("long", &b"right\n".repeat(513)),
                put("dir/child", b"child"),
            ],
        )
        .unwrap();
    let mut j = MergeJournal::open(data.path(), "repository").unwrap();
    let (s, _, phase) = prepared(j.begin(&e, "begin", &left, &right).unwrap());
    assert_eq!(phase, MergePhase::Conflicts);
    let conflicts = j.conflicts(&s, None, 100).unwrap().entries;
    assert!(conflicts.iter().any(|c| c.kind == ConflictKind::Binary));
    assert!(
        conflicts
            .iter()
            .any(|c| c.kind == ConflictKind::TextProfile)
    );
    assert!(
        conflicts
            .iter()
            .any(|c| matches!(c.kind, ConflictKind::Portable | ConflictKind::FileDirectory))
    );
    for c in conflicts {
        let generation = j.inspect(&s).unwrap().generation;
        j.resolve(
            &e,
            &s,
            &format!("resolve-{}", &c.id[..12]),
            generation,
            &c.id,
            Resolution::Right,
        )
        .unwrap();
    }
    let state = j.inspect(&s).unwrap();
    let (merged, _) = completed(j.complete(&e, &s, "complete", state.generation).unwrap());
    assert_eq!(e.read_file(&merged, "binary").unwrap(), b"\0right");
    assert_eq!(e.read_file(&merged, "dir/child").unwrap(), b"child");
}

#[test]
fn equal_heads_are_noop_but_equal_trees_keep_distinct_ordered_ancestry() {
    let data = TempDir::new().unwrap();
    let e = Engine::open(data.path()).unwrap();
    let base = e.checkpoint(None, &[put("file", b"base")]).unwrap();
    let same_tree = e.checkpoint(Some(&base), &[]).unwrap();
    let mut j = MergeJournal::open(data.path(), "repository").unwrap();
    let (s, g, p) = prepared(j.begin(&e, "same", &base, &base).unwrap());
    assert_eq!(p, MergePhase::Ready);
    assert_eq!(
        completed(j.complete(&e, &s, "same-complete", g).unwrap()).0,
        base
    );
    let (s, g, p) = prepared(j.begin(&e, "tree", &base, &same_tree).unwrap());
    assert_eq!(p, MergePhase::Ready);
    let merged = completed(j.complete(&e, &s, "tree-complete", g).unwrap()).0;
    let revision = e.load_revision(&merged).unwrap();
    assert_eq!(revision.version, 2);
    assert_eq!(revision.parents, [base.clone(), same_tree]);
    assert_eq!(revision.tree, e.load_revision(&base).unwrap().tree);
}

#[test]
fn conflict_pages_visit_every_digest_exactly_once_and_reject_stale_cursor() {
    let data = TempDir::new().unwrap();
    let engine = Engine::open(data.path()).unwrap();
    let changes = |bytes: &[u8]| {
        (0..8)
            .map(|index| put(&format!("file-{index}"), bytes))
            .collect::<Vec<_>>()
    };
    let base = engine.checkpoint(None, &changes(b"base\n")).unwrap();
    let left = engine.checkpoint(Some(&base), &changes(b"left\n")).unwrap();
    let right = engine
        .checkpoint(Some(&base), &changes(b"right\n"))
        .unwrap();
    let mut journal = MergeJournal::open(data.path(), "repository").unwrap();
    let (session, generation, phase) =
        prepared(journal.begin(&engine, "begin", &left, &right).unwrap());
    assert_eq!(phase, MergePhase::Conflicts);
    let expected: Vec<_> = journal
        .conflicts(&session, None, 100)
        .unwrap()
        .entries
        .into_iter()
        .map(|c| c.id)
        .collect();
    assert_eq!(expected.len(), 8);
    assert!(expected.windows(2).all(|ids| ids[0] < ids[1]));
    let first = journal.conflicts(&session, None, 1).unwrap();
    let stale = first.next.clone().unwrap();
    let mut actual = Vec::new();
    let mut cursor = None;
    loop {
        let page = journal.conflicts(&session, cursor.as_deref(), 1).unwrap();
        assert!(page.entries.len() <= 1);
        actual.extend(page.entries.into_iter().map(|c| c.id));
        cursor = page.next;
        if cursor.is_none() {
            break;
        }
        assert!(actual.len() <= 8);
    }
    assert_eq!(actual, expected);
    journal
        .resolve(
            &engine,
            &session,
            "resolve",
            generation,
            &expected[0],
            Resolution::Left,
        )
        .unwrap();
    assert!(matches!(
        journal.conflicts(&session, Some(&stale), 1),
        Err(Error::Conflict(_))
    ));
}

#[test]
fn offline_landing_keeps_current_target_first_and_retains_every_offline_checkpoint() {
    let data = TempDir::new().unwrap();
    let engine = Engine::open(data.path()).unwrap();
    let target = engine.checkpoint(None, &[put("file", b"target")]).unwrap();
    let first = engine
        .checkpoint(Some(&target), &[put("file", b"offline first")])
        .unwrap();
    let tip = engine
        .checkpoint(Some(&first), &[put("file", b"offline final")])
        .unwrap();
    let mut journal = MergeJournal::open(data.path(), "repository").unwrap();
    let (session, generation, phase) = prepared(
        journal
            .begin(&engine, "prepare-landing", &target, &tip)
            .unwrap(),
    );
    assert_eq!(phase, MergePhase::Ready);
    let (landing, _) = completed(
        journal
            .complete(&engine, &session, "publish-landing", generation)
            .unwrap(),
    );
    let revision = engine.load_revision(&landing).unwrap();
    assert_eq!(revision.parents, [target.clone(), tip.clone()]);
    assert_eq!(revision.tree, engine.load_revision(&tip).unwrap().tree);
    assert_eq!(
        engine.load_revision(&tip).unwrap().parents.as_slice(),
        std::slice::from_ref(&first)
    );
    assert_eq!(
        engine.load_revision(&first).unwrap().parents.as_slice(),
        std::slice::from_ref(&target)
    );
    assert_eq!(engine.read_file(&target, "file").unwrap(), b"target");
    assert_eq!(
        engine.read_file(&landing, "file").unwrap(),
        b"offline final"
    );
    qualify_source(&engine, &[landing]).unwrap();
}

#[test]
fn merge_refuses_non_regular_entries_on_any_side() {
    for kind in [EntryKind::Executable, EntryKind::Symlink] {
        let data = TempDir::new().unwrap();
        let engine = Engine::open(data.path()).unwrap();
        let base = engine.checkpoint(None, &[put("file", b"one\n")]).unwrap();
        let left = engine
            .checkpoint(Some(&base), &[put("left", b"a")])
            .unwrap();
        let right = engine
            .checkpoint_entries(
                Some(&base),
                &[EntryChange {
                    path: "special".into(),
                    content: Some(b"target".to_vec()),
                    kind,
                }],
            )
            .unwrap();
        let mut journal = MergeJournal::open(data.path(), "repository").unwrap();
        for (operation, a, b) in [("one", &left, &right), ("two", &right, &left)] {
            let error = journal.begin(&engine, operation, a, b).unwrap_err();
            let message = error.to_string();
            assert!(
                matches!(error, Error::Invalid(_))
                    && message.contains("unsupported entry kind: special")
                    && message.contains("merge"),
                "{message}"
            );
        }
    }
}
