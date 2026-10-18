use std::path::Path;
use std::sync::{Arc, Barrier};

use agentscm::authority::Authority;
use agentscm::context::{ChangeId, InputIdentity, ReviewContext};
use agentscm::model::{
    Approval, FileChange, IntegrateRequest, LandingPreparation, PrepareLandingRequest, TargetState,
};
use agentscm::store::Engine;
use agentscm::{Error, Result};
use tempfile::TempDir;

const DIGEST_B: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
const DIGEST_C: &str = "cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc";
const DIGEST_D: &str = "dddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddd";

fn checkpoint(engine: &Engine, parent: Option<&str>, path: &str, bytes: &[u8]) -> String {
    engine
        .checkpoint(
            parent,
            &[FileChange {
                path: path.to_owned(),
                content: Some(bytes.to_vec()),
            }],
        )
        .expect("checkpoint")
}

fn context(
    repository: &str,
    id: &str,
    version: u64,
    revision: &str,
    target: &str,
    base: Option<TargetState>,
    policy_epoch: u64,
) -> ReviewContext {
    ReviewContext {
        version: 1,
        change: ChangeId {
            repository: repository.to_owned(),
            id: id.to_owned(),
        },
        change_version: version,
        revision: revision.to_owned(),
        base: base.map(|state| agentscm::context::BaseState {
            revision: state.revision,
            target_version: state.version,
        }),
        target: target.to_owned(),
        policy_epoch,
        intent: "integrate the tested source revision".to_owned(),
        acceptance_criteria: vec!["authority invariants pass".to_owned()],
        scope: vec!["source".to_owned()],
        exclusions: vec![],
        omissions: vec![],
        dependencies: vec![],
        recipe: InputIdentity::Digest {
            sha256: DIGEST_B.to_owned(),
        },
        toolchain: InputIdentity::Digest {
            sha256: DIGEST_C.to_owned(),
        },
        environment: InputIdentity::Digest {
            sha256: DIGEST_D.to_owned(),
        },
        required_evidence: vec![],
        required_capabilities: vec!["agentscm.review-context/v1".to_owned()],
    }
}

fn approval(
    id: &str,
    state: &agentscm::context::ChangeState,
    target: &str,
    revision: &str,
    policy_epoch: u64,
) -> Approval {
    Approval {
        approval_id: id.to_owned(),
        target: target.to_owned(),
        revision: revision.to_owned(),
        context_digest: state.context_digest.clone(),
        change: Some(state.id.clone()),
        change_version: Some(state.version),
        policy_epoch,
    }
}

fn request(
    operation_id: &str,
    state: &agentscm::context::ChangeState,
    target: &str,
    expected_target: Option<TargetState>,
    revision: &str,
    approval_id: &str,
    policy_epoch: u64,
) -> IntegrateRequest {
    IntegrateRequest {
        operation_id: operation_id.to_owned(),
        target: target.to_owned(),
        expected_target,
        revision: revision.to_owned(),
        context_digest: state.context_digest.clone(),
        approval_id: approval_id.to_owned(),
        change: Some(state.id.clone()),
        change_version: Some(state.version),
        policy_epoch,
    }
}

fn open_pair(root: &Path) -> (Engine, Authority) {
    (
        Engine::open(root.join("source")).expect("engine"),
        Authority::open(root.join("authority")).expect("authority"),
    )
}

fn assert_conflict<T: std::fmt::Debug>(result: Result<T>) {
    assert!(matches!(result, Err(Error::Conflict(_))), "{result:?}");
}

#[test]
fn integrates_direct_lineage_and_returns_durable_retry() {
    let temporary = TempDir::new().expect("tempdir");
    let (engine, mut authority) = open_pair(temporary.path());
    let repository = authority.repository_id().expect("repository");
    let genesis = checkpoint(&engine, None, "hello.txt", b"one");
    let first_change = authority
        .start_change(&context(
            &repository,
            "change-1",
            1,
            &genesis,
            "main",
            None,
            0,
        ))
        .expect("start");
    authority
        .approve(&approval("approval-1", &first_change, "main", &genesis, 0))
        .expect("approve");
    let first_request = request(
        "operation-1",
        &first_change,
        "main",
        None,
        &genesis,
        "approval-1",
        0,
    );
    let first = authority
        .integrate(&engine, &first_request)
        .expect("integrate genesis");
    assert_eq!(first.semantics_version, 2);
    assert_eq!(first.change.as_ref(), Some(&first_change.id));
    assert_eq!(first.change_version, Some(1));
    assert_eq!(
        authority.integrate(&engine, &first_request).expect("retry"),
        first
    );

    for operation_id in ["operation-1", "partial-new"] {
        let mut partial = first_request.clone();
        partial.operation_id = operation_id.to_owned();
        partial.change = None;
        assert!(matches!(
            authority.integrate(&engine, &partial),
            Err(Error::Invalid(_))
        ));
        partial.change = first_request.change.clone();
        partial.change_version = None;
        assert!(matches!(
            authority.integrate(&engine, &partial),
            Err(Error::Invalid(_))
        ));
    }
    assert_eq!(authority.events(0, 10).unwrap().len(), 1);

    let child = checkpoint(&engine, Some(&genesis), "hello.txt", b"two");
    let second_change = authority
        .start_change(&context(
            &repository,
            "change-2",
            1,
            &child,
            "main",
            Some(first.after.clone()),
            0,
        ))
        .expect("start child");
    authority
        .approve(&approval("approval-2", &second_change, "main", &child, 0))
        .expect("approve child");
    let second = authority
        .integrate(
            &engine,
            &request(
                "operation-2",
                &second_change,
                "main",
                Some(first.after.clone()),
                &child,
                "approval-2",
                0,
            ),
        )
        .expect("integrate child");
    assert_eq!(second.before, Some(first.after.clone()));
    assert_eq!(second.after.version, 2);
    assert_eq!(
        authority.events(0, 10).expect("events"),
        vec![first, second.clone()]
    );
    assert_eq!(
        authority.target("main").expect("target"),
        Some(second.after)
    );
}

#[test]
fn approval_cannot_move_another_target_or_revision() {
    let temporary = TempDir::new().expect("tempdir");
    let (engine, mut authority) = open_pair(temporary.path());
    let repository = authority.repository_id().expect("repository");
    let genesis = checkpoint(&engine, None, "a", b"genesis");
    let other = checkpoint(&engine, None, "b", b"other");
    let state = authority
        .start_change(&context(
            &repository,
            "change",
            1,
            &genesis,
            "main",
            None,
            0,
        ))
        .expect("start");
    authority
        .approve(&approval("main-only", &state, "main", &genesis, 0))
        .expect("approve");

    let mut wrong_target = request(
        "wrong-target",
        &state,
        "release",
        None,
        &genesis,
        "main-only",
        0,
    );
    wrong_target.context_digest = state.context_digest.clone();
    assert_conflict(authority.integrate(&engine, &wrong_target));
    let mut wrong_revision = request(
        "wrong-revision",
        &state,
        "main",
        None,
        &other,
        "main-only",
        0,
    );
    wrong_revision.context_digest = state.context_digest.clone();
    assert_conflict(authority.integrate(&engine, &wrong_revision));
    let mut wrong_context = request(
        "wrong-context",
        &state,
        "main",
        None,
        &genesis,
        "main-only",
        0,
    );
    wrong_context.context_digest = DIGEST_B.to_owned();
    assert_conflict(authority.integrate(&engine, &wrong_context));
    assert!(authority.events(0, 10).expect("events").is_empty());
    authority
        .integrate(
            &engine,
            &request("right", &state, "main", None, &genesis, "main-only", 0),
        )
        .expect("failed attempts did not consume approval");
}

#[test]
fn target_version_parent_policy_and_approval_lifecycle_are_enforced() {
    let temporary = TempDir::new().expect("tempdir");
    let (engine, mut authority) = open_pair(temporary.path());
    let repository = authority.repository_id().expect("repository");
    let genesis = checkpoint(&engine, None, "a", b"base");
    let base_state = authority
        .start_change(&context(&repository, "base", 1, &genesis, "main", None, 0))
        .expect("start base");
    authority
        .approve(&approval("base", &base_state, "main", &genesis, 0))
        .expect("approve base");
    let base = authority
        .integrate(
            &engine,
            &request("base-op", &base_state, "main", None, &genesis, "base", 0),
        )
        .expect("integrate base");

    let unrelated = checkpoint(&engine, None, "other", b"other");
    let unrelated_state = authority
        .start_change(&context(
            &repository,
            "unrelated",
            1,
            &unrelated,
            "main",
            Some(base.after.clone()),
            0,
        ))
        .expect("start unrelated");
    authority
        .approve(&approval(
            "unrelated",
            &unrelated_state,
            "main",
            &unrelated,
            0,
        ))
        .expect("approve unrelated");
    assert_conflict(authority.integrate(
        &engine,
        &request(
            "unrelated-op",
            &unrelated_state,
            "main",
            Some(base.after.clone()),
            &unrelated,
            "unrelated",
            0,
        ),
    ));

    let child = checkpoint(&engine, Some(&genesis), "a", b"child");
    let child_state = authority
        .start_change(&context(
            &repository,
            "child",
            1,
            &child,
            "main",
            Some(base.after.clone()),
            0,
        ))
        .expect("start child");
    authority
        .approve(&approval("revoked", &child_state, "main", &child, 0))
        .expect("approve revoked");
    authority.revoke_approval("revoked").expect("revoke");
    assert_conflict(authority.integrate(
        &engine,
        &request(
            "revoked-op",
            &child_state,
            "main",
            Some(base.after.clone()),
            &child,
            "revoked",
            0,
        ),
    ));

    authority
        .approve(&approval("stale-policy", &child_state, "main", &child, 0))
        .expect("approve stale");
    assert_eq!(authority.advance_policy(0).expect("advance"), 1);
    assert_conflict(authority.integrate(
        &engine,
        &request(
            "stale-policy-op",
            &child_state,
            "main",
            Some(base.after),
            &child,
            "stale-policy",
            0,
        ),
    ));
}

#[test]
fn rejects_legacy_fresh_requests_injection_overflow_and_unbounded_reads() {
    let temporary = TempDir::new().expect("tempdir");
    let (engine, mut authority) = open_pair(temporary.path());
    let repository = authority.repository_id().expect("repository");
    let genesis = checkpoint(&engine, None, "a", b"a");
    let state = authority
        .start_change(&context(&repository, "valid", 1, &genesis, "main", None, 0))
        .expect("start");
    let mut legacy = approval("legacy", &state, "main", &genesis, 0);
    legacy.change = None;
    legacy.change_version = None;
    assert!(matches!(authority.approve(&legacy), Err(Error::Invalid(_))));
    let mut legacy_request = request(
        "legacy-request",
        &state,
        "main",
        None,
        &genesis,
        "legacy",
        0,
    );
    legacy_request.change = None;
    legacy_request.change_version = None;
    assert!(matches!(
        authority.integrate(&engine, &legacy_request),
        Err(Error::Invalid(_))
    ));
    let mut injected = approval("x'); DROP TABLE targets; --", &state, "main", &genesis, 0);
    assert!(matches!(
        authority.approve(&injected),
        Err(Error::Invalid(_))
    ));
    let injected_target = approval(
        "injected-target",
        &state,
        "main'); DROP TABLE approvals; --",
        &genesis,
        0,
    );
    assert!(matches!(
        authority.approve(&injected_target),
        Err(Error::Invalid(_))
    ));
    injected.approval_id = "overflow".to_owned();
    injected.policy_epoch = u64::MAX;
    assert!(matches!(authority.approve(&injected), Err(Error::Limit(_))));
    assert!(matches!(authority.events(0, 1_001), Err(Error::Limit(_))));
    authority
        .approve(&approval("valid", &state, "main", &genesis, 0))
        .expect("schema intact");
}

#[test]
fn populated_target_substitution_stale_version_and_consumed_approval_fail() {
    let temporary = TempDir::new().expect("tempdir");
    let (engine, mut authority) = open_pair(temporary.path());
    let repository = authority.repository_id().expect("repository");
    let genesis = checkpoint(&engine, None, "a", b"base");
    let main = authority
        .start_change(&context(
            &repository,
            "main-base",
            1,
            &genesis,
            "main",
            None,
            0,
        ))
        .expect("main context");
    authority
        .approve(&approval("main-base", &main, "main", &genesis, 0))
        .expect("main approval");
    let main_event = authority
        .integrate(
            &engine,
            &request(
                "main-base-op",
                &main,
                "main",
                None,
                &genesis,
                "main-base",
                0,
            ),
        )
        .expect("main integration");
    let release = authority
        .start_change(&context(
            &repository,
            "release-base",
            1,
            &genesis,
            "release",
            None,
            0,
        ))
        .expect("release context");
    authority
        .approve(&approval("release-base", &release, "release", &genesis, 0))
        .expect("release approval");
    let release_event = authority
        .integrate(
            &engine,
            &request(
                "release-base-op",
                &release,
                "release",
                None,
                &genesis,
                "release-base",
                0,
            ),
        )
        .expect("release integration");

    let child = checkpoint(&engine, Some(&genesis), "a", b"child");
    let child_state = authority
        .start_change(&context(
            &repository,
            "child",
            1,
            &child,
            "main",
            Some(main_event.after.clone()),
            0,
        ))
        .expect("child context");
    authority
        .approve(&approval("child", &child_state, "main", &child, 0))
        .expect("child approval");
    assert_conflict(authority.integrate(
        &engine,
        &request(
            "substitute-target",
            &child_state,
            "release",
            Some(release_event.after),
            &child,
            "child",
            0,
        ),
    ));
    let mut stale = main_event.after.clone();
    stale.version += 1;
    assert_conflict(authority.integrate(
        &engine,
        &request(
            "stale-version",
            &child_state,
            "main",
            Some(stale),
            &child,
            "child",
            0,
        ),
    ));
    authority
        .integrate(
            &engine,
            &request(
                "child-op",
                &child_state,
                "main",
                Some(main_event.after),
                &child,
                "child",
                0,
            ),
        )
        .expect("child integration");
    let current = authority.target("main").expect("target");
    assert_conflict(authority.integrate(
        &engine,
        &request(
            "reuse-approval",
            &child_state,
            "main",
            current,
            &child,
            "child",
            0,
        ),
    ));
}

#[test]
fn restart_retry_survives_and_concurrent_target_writers_have_one_winner() {
    let temporary = TempDir::new().expect("tempdir");
    let source_path = temporary.path().join("source");
    let authority_path = temporary.path().join("authority");
    let engine = Engine::open(&source_path).expect("engine");
    let genesis = checkpoint(&engine, None, "a", b"base");
    let (repository, base_event, base_request) = {
        let mut authority = Authority::open(&authority_path).expect("authority");
        let repository = authority.repository_id().expect("repository");
        let base = authority
            .start_change(&context(&repository, "base", 1, &genesis, "main", None, 0))
            .expect("base context");
        authority
            .approve(&approval("base", &base, "main", &genesis, 0))
            .expect("base approval");
        let request = request("base-op", &base, "main", None, &genesis, "base", 0);
        let event = authority
            .integrate(&engine, &request)
            .expect("base integration");
        drop(authority);
        let mut reopened = Authority::open(&authority_path).expect("reopen");
        assert_eq!(reopened.integrate(&engine, &request).expect("retry"), event);
        (repository, event, request)
    };

    let mut moved = Authority::open(&authority_path).expect("reopen for movement");
    assert_eq!(moved.advance_policy(0).expect("advance policy"), 1);
    let mut moved_context = context(
        &repository,
        "moved-context",
        1,
        &genesis,
        "main",
        Some(base_event.after.clone()),
        1,
    );
    moved
        .start_change(&moved_context)
        .expect("start moved context");
    moved_context.change_version = 2;
    moved_context.intent = "move review context after integration".to_owned();
    moved
        .update_change_context(1, &moved_context)
        .expect("move context");
    assert_eq!(
        moved
            .integrate(&engine, &base_request)
            .expect("retry after movement"),
        base_event
    );
    let mut altered_retry = base_request.clone();
    altered_retry.context_digest = DIGEST_B.to_owned();
    assert_conflict(moved.integrate(&engine, &altered_retry));
    drop(moved);

    let left = checkpoint(&engine, Some(&genesis), "a", b"left");
    let right = checkpoint(&engine, Some(&genesis), "a", b"right");
    let mut setup = Authority::open(&authority_path).expect("authority");
    let mut candidates = Vec::new();
    for (name, revision) in [("left", &left), ("right", &right)] {
        let state = setup
            .start_change(&context(
                &repository,
                name,
                1,
                revision,
                "main",
                Some(base_event.after.clone()),
                1,
            ))
            .expect("candidate context");
        let mut candidate_approval = approval(name, &state, "main", revision, 1);
        candidate_approval.policy_epoch = 1;
        setup
            .approve(&candidate_approval)
            .expect("candidate approval");
        candidates.push((name.to_owned(), revision.clone(), state));
    }
    drop(setup);
    drop(engine);

    let barrier = Arc::new(Barrier::new(3));
    let handles: Vec<_> = candidates
        .into_iter()
        .map(|(name, revision, state)| {
            let source_path = source_path.clone();
            let authority_path = authority_path.clone();
            let expected = base_event.after.clone();
            let barrier = Arc::clone(&barrier);
            std::thread::spawn(move || {
                let engine = Engine::open(source_path).expect("thread engine");
                let mut authority = Authority::open(authority_path).expect("thread authority");
                barrier.wait();
                authority.integrate(
                    &engine,
                    &request(
                        &format!("{name}-op"),
                        &state,
                        "main",
                        Some(expected),
                        &revision,
                        &name,
                        1,
                    ),
                )
            })
        })
        .collect();
    barrier.wait();
    let results: Vec<_> = handles
        .into_iter()
        .map(|handle| handle.join().expect("join"))
        .collect();
    assert_eq!(results.iter().filter(|result| result.is_ok()).count(), 1);
    assert_eq!(
        results
            .iter()
            .filter(|result| matches!(result, Err(Error::Conflict(_))))
            .count(),
        1
    );
    let authority = Authority::open(authority_path).expect("authority");
    assert_eq!(
        authority
            .target("main")
            .expect("target")
            .expect("present")
            .version,
        2
    );
    assert_eq!(authority.events(0, 10).expect("events").len(), 2);
}

#[test]
fn rejects_unknown_schema_version() {
    let temporary = TempDir::new().expect("tempdir");
    let path = temporary.path().join("authority");
    drop(Authority::open(&path).expect("authority"));
    let connection = rusqlite::Connection::open(path.join("state.sqlite")).expect("database");
    connection
        .pragma_update(None, "user_version", 8)
        .expect("version");
    drop(connection);
    assert!(
        matches!(Authority::open(path), Err(Error::Invalid(message)) if message.contains("unsupported authority schema version"))
    );
}

#[test]
fn multi_parent_admission_requires_exact_first_parent_and_fresh_context() {
    let temporary = TempDir::new().unwrap();
    let (engine, mut authority) = open_pair(temporary.path());
    let repository = authority.repository_id().unwrap();
    let genesis = checkpoint(&engine, None, "file", b"base");
    let change = authority
        .start_change(&context(
            &repository,
            "genesis",
            1,
            &genesis,
            "main",
            None,
            0,
        ))
        .unwrap();
    authority
        .approve(&approval("genesis", &change, "main", &genesis, 0))
        .unwrap();
    let initial = authority
        .integrate(
            &engine,
            &request("genesis", &change, "main", None, &genesis, "genesis", 0),
        )
        .unwrap();
    let offline = checkpoint(&engine, Some(&genesis), "file", b"offline");
    let wrong_order = engine
        .checkpoint_with_parents(&[offline.clone(), genesis.clone()], &[])
        .unwrap();
    let wrong = authority
        .start_change(&context(
            &repository,
            "wrong",
            1,
            &wrong_order,
            "main",
            Some(initial.after.clone()),
            0,
        ))
        .unwrap();
    authority
        .approve(&approval("wrong", &wrong, "main", &wrong_order, 0))
        .unwrap();
    assert_conflict(authority.integrate(
        &engine,
        &request(
            "wrong",
            &wrong,
            "main",
            Some(initial.after.clone()),
            &wrong_order,
            "wrong",
            0,
        ),
    ));
    assert_eq!(
        authority.target("main").unwrap(),
        Some(initial.after.clone())
    );
    let landing = engine
        .checkpoint_with_parents(
            &[genesis.clone(), offline],
            &[FileChange {
                path: "file".into(),
                content: Some(b"offline".to_vec()),
            }],
        )
        .unwrap();
    let fresh = authority
        .start_change(&context(
            &repository,
            "landing",
            1,
            &landing,
            "main",
            Some(initial.after.clone()),
            0,
        ))
        .unwrap();
    authority
        .approve(&approval("landing", &fresh, "main", &landing, 0))
        .unwrap();
    let mut stale_approval = request(
        "stale",
        &fresh,
        "main",
        Some(initial.after.clone()),
        &landing,
        "wrong",
        0,
    );
    assert_conflict(authority.integrate(&engine, &stale_approval));
    stale_approval.operation_id = "landing".into();
    stale_approval.approval_id = "landing".into();
    let event = authority.integrate(&engine, &stale_approval).unwrap();
    assert_eq!(event.after.revision, landing);
    assert_eq!(
        authority.integrate(&engine, &stale_approval).unwrap(),
        event
    );
    assert_eq!(authority.events(0, 100).unwrap().len(), 2);

    // Imported multi-parent history cannot silently initialize another target.
    let imported = authority
        .start_change(&context(
            &repository,
            "imported",
            1,
            &landing,
            "empty",
            None,
            0,
        ))
        .unwrap();
    authority
        .approve(&approval("imported", &imported, "empty", &landing, 0))
        .unwrap();
    assert_conflict(authority.integrate(
        &engine,
        &request(
            "imported", &imported, "empty", None, &landing, "imported", 0,
        ),
    ));
    assert!(authority.target("empty").unwrap().is_none());
}

fn staged(
    authority: &mut Authority,
    repository: &str,
    name: &str,
    revision: &str,
    target: &str,
    base: Option<TargetState>,
    policy_epoch: u64,
) -> agentscm::context::ChangeState {
    let state = authority
        .start_change(&context(
            repository,
            name,
            1,
            revision,
            target,
            base,
            policy_epoch,
        ))
        .unwrap();
    authority
        .approve(&approval(name, &state, target, revision, policy_epoch))
        .unwrap();
    state
}

fn approval_consumed(path: &Path, id: &str) -> bool {
    let connection = rusqlite::Connection::open(path.join("state.sqlite")).unwrap();
    connection
        .query_row(
            "SELECT consumed_event IS NOT NULL FROM approvals WHERE approval_id = ?1",
            [id],
            |row| row.get(0),
        )
        .unwrap()
}

#[test]
fn initialize_accepts_non_genesis_and_genesis_heads_with_durable_retry() {
    use agentscm::model::EventKind;
    let temporary = TempDir::new().unwrap();
    let (engine, mut authority) = open_pair(temporary.path());
    let repository = authority.repository_id().unwrap();
    let root = checkpoint(&engine, None, "a", b"root");
    let child = checkpoint(&engine, Some(&root), "a", b"child");

    let state = staged(
        &mut authority,
        &repository,
        "init",
        &child,
        "imported",
        None,
        0,
    );
    let init = request("init-op", &state, "imported", None, &child, "init", 0);
    let event = authority.initialize_target(&engine, &init).unwrap();
    assert_eq!(event.kind, EventKind::Initialize);
    assert_eq!(event.before, None);
    assert_eq!(event.after.revision, child);
    assert_eq!(event.after.version, 1);
    assert_eq!(
        authority.target("imported").unwrap(),
        Some(event.after.clone())
    );
    assert!(approval_consumed(
        &temporary.path().join("authority"),
        "init"
    ));
    assert_eq!(authority.initialize_target(&engine, &init).unwrap(), event);
    assert_eq!(authority.events(0, 10).unwrap(), vec![event.clone()]);
    drop(authority);
    let mut reopened = Authority::open(temporary.path().join("authority")).unwrap();
    assert_eq!(reopened.initialize_target(&engine, &init).unwrap(), event);
    assert_eq!(reopened.events(0, 10).unwrap(), vec![event.clone()]);

    // Same operation ID for a different request, or via integrate, conflicts.
    let mut altered = init.clone();
    altered.target = "other".to_owned();
    assert_conflict(reopened.initialize_target(&engine, &altered));
    assert_conflict(reopened.integrate(&engine, &init));

    // A genesis root also initializes through this operation.
    let state = staged(&mut reopened, &repository, "root", &root, "rooted", None, 0);
    let rooted = reopened
        .initialize_target(
            &engine,
            &request("root-op", &state, "rooted", None, &root, "root", 0),
        )
        .unwrap();
    assert_eq!(rooted.kind, EventKind::Initialize);
    assert_eq!(rooted.after.version, 1);

    // Integrate genesis keeps its own kind and the same operation ID cannot be initialized.
    let state = staged(&mut reopened, &repository, "gen", &root, "plain", None, 0);
    let plain = request("gen-op", &state, "plain", None, &root, "gen", 0);
    let integrated = reopened.integrate(&engine, &plain).unwrap();
    assert_eq!(integrated.kind, EventKind::Integrate);
    assert_conflict(reopened.initialize_target(&engine, &plain));
    let kinds: Vec<_> = reopened
        .events(0, 10)
        .unwrap()
        .into_iter()
        .map(|event| event.kind)
        .collect();
    assert_eq!(
        kinds,
        [
            EventKind::Initialize,
            EventKind::Initialize,
            EventKind::Integrate
        ]
    );
}

#[test]
fn initialize_rejects_existing_targets_expected_state_and_stale_bindings() {
    let temporary = TempDir::new().unwrap();
    let (engine, mut authority) = open_pair(temporary.path());
    let repository = authority.repository_id().unwrap();
    let root = checkpoint(&engine, None, "a", b"root");
    let child = checkpoint(&engine, Some(&root), "a", b"child");
    let other = checkpoint(&engine, None, "b", b"other");

    // Existing target: no adoption or replacement, approval stays unconsumed.
    let base = staged(&mut authority, &repository, "base", &root, "main", None, 0);
    let existing = authority
        .integrate(
            &engine,
            &request("base-op", &base, "main", None, &root, "base", 0),
        )
        .unwrap();
    let adopt = staged(
        &mut authority,
        &repository,
        "adopt",
        &child,
        "main",
        Some(existing.after.clone()),
        0,
    );
    assert_conflict(authority.initialize_target(
        &engine,
        &request("adopt-op", &adopt, "main", None, &child, "adopt", 0),
    ));
    assert_eq!(
        authority.target("main").unwrap(),
        Some(existing.after.clone())
    );
    assert!(!approval_consumed(
        &temporary.path().join("authority"),
        "adopt"
    ));
    assert_eq!(authority.events(0, 10).unwrap().len(), 1);

    // Expected target is not accepted, and legacy (unbound) requests are invalid.
    let fresh = staged(&mut authority, &repository, "fresh", &child, "new", None, 0);
    let expected = request(
        "exp-op",
        &fresh,
        "new",
        Some(existing.after.clone()),
        &child,
        "fresh",
        0,
    );
    assert!(matches!(
        authority.initialize_target(&engine, &expected),
        Err(Error::Invalid(_))
    ));
    let mut legacy = request("leg-op", &fresh, "new", None, &child, "fresh", 0);
    legacy.change = None;
    legacy.change_version = None;
    assert!(matches!(
        authority.initialize_target(&engine, &legacy),
        Err(Error::Invalid(_))
    ));

    // Approval for a different revision is rejected.
    let wrong = staged(&mut authority, &repository, "wrong", &other, "new", None, 0);
    let mut mismatched = request("wrong-op", &wrong, "new", None, &other, "fresh", 0);
    mismatched.context_digest = wrong.context_digest.clone();
    assert_conflict(authority.initialize_target(&engine, &mismatched));
    assert!(authority.target("new").unwrap().is_none());

    // Stale change context.
    let mut moved = context(&repository, "fresh", 2, &child, "new", None, 0);
    moved.intent = "changed after approval".to_owned();
    authority.update_change_context(1, &moved).unwrap();
    assert_conflict(authority.initialize_target(
        &engine,
        &request("stale-op", &fresh, "new", None, &child, "fresh", 0),
    ));

    // Stale policy epoch.
    let late = staged(&mut authority, &repository, "late", &child, "late", None, 0);
    authority.advance_policy(0).unwrap();
    assert_conflict(authority.initialize_target(
        &engine,
        &request("late-op", &late, "late", None, &child, "late", 0),
    ));
    assert!(authority.target("late").unwrap().is_none());
    assert!(!approval_consumed(
        &temporary.path().join("authority"),
        "late"
    ));
}

#[test]
fn initialize_requires_complete_published_source_closure() {
    let temporary = TempDir::new().unwrap();
    let (engine, mut authority) = open_pair(temporary.path());
    let repository = authority.repository_id().unwrap();
    let root = checkpoint(&engine, None, "a", b"root");
    let child = checkpoint(&engine, Some(&root), "a", b"child");
    let state = staged(
        &mut authority,
        &repository,
        "gap",
        &child,
        "imported",
        None,
        0,
    );
    std::fs::remove_file(
        temporary
            .path()
            .join("source/objects")
            .join(&root[..2])
            .join(&root[2..]),
    )
    .unwrap();
    let result = authority.initialize_target(
        &engine,
        &request("gap-op", &state, "imported", None, &child, "gap", 0),
    );
    assert!(result.is_err(), "{result:?}");
    assert!(authority.target("imported").unwrap().is_none());
    assert!(!approval_consumed(
        &temporary.path().join("authority"),
        "gap"
    ));
    assert!(authority.events(0, 10).unwrap().is_empty());
}

#[test]
fn concurrent_initialization_has_one_winner() {
    let temporary = TempDir::new().unwrap();
    let source_path = temporary.path().join("source");
    let authority_path = temporary.path().join("authority");
    let engine = Engine::open(&source_path).unwrap();
    let left = checkpoint(&engine, None, "a", b"left");
    let right = checkpoint(&engine, None, "a", b"right");
    let mut setup = Authority::open(&authority_path).unwrap();
    let repository = setup.repository_id().unwrap();
    let candidates: Vec<_> = [("left", left), ("right", right)]
        .into_iter()
        .map(|(name, revision)| {
            let state = staged(&mut setup, &repository, name, &revision, "main", None, 0);
            (name, revision, state)
        })
        .collect();
    drop(setup);
    let barrier = Arc::new(Barrier::new(3));
    let handles: Vec<_> = candidates
        .into_iter()
        .map(|(name, revision, state)| {
            let source_path = source_path.clone();
            let authority_path = authority_path.clone();
            let barrier = Arc::clone(&barrier);
            std::thread::spawn(move || {
                let engine = Engine::open(source_path).unwrap();
                let mut authority = Authority::open(authority_path).unwrap();
                barrier.wait();
                authority.initialize_target(
                    &engine,
                    &request(
                        &format!("{name}-op"),
                        &state,
                        "main",
                        None,
                        &revision,
                        name,
                        0,
                    ),
                )
            })
        })
        .collect();
    barrier.wait();
    let results: Vec<_> = handles.into_iter().map(|h| h.join().unwrap()).collect();
    assert_eq!(results.iter().filter(|r| r.is_ok()).count(), 1);
    assert_eq!(
        results
            .iter()
            .filter(|r| matches!(r, Err(Error::Conflict(_))))
            .count(),
        1
    );
    let authority = Authority::open(authority_path).unwrap();
    assert_eq!(authority.target("main").unwrap().unwrap().version, 1);
    assert_eq!(authority.events(0, 10).unwrap().len(), 1);
}

#[test]
fn v3_database_migrates_to_v7_and_keeps_events_as_integrate() {
    let temporary = TempDir::new().unwrap();
    let (engine, mut authority) = open_pair(temporary.path());
    let repository = authority.repository_id().unwrap();
    let root = checkpoint(&engine, None, "a", b"root");
    let state = staged(&mut authority, &repository, "base", &root, "main", None, 0);
    let event = authority
        .integrate(
            &engine,
            &request("base-op", &state, "main", None, &root, "base", 0),
        )
        .unwrap();
    drop(authority);
    let path = temporary.path().join("authority");
    let connection = rusqlite::Connection::open(path.join("state.sqlite")).unwrap();
    connection
        .execute_batch(
            "ALTER TABLE integration_events DROP COLUMN kind;
             DROP TABLE landing_preparations;
             DROP INDEX command_success_subject;
             DROP TABLE command_evidence;
             DROP TABLE command_attempts;
             DROP TABLE workflow_receipts;
             DROP TABLE workflow_requirements;
             PRAGMA user_version = 3;",
        )
        .unwrap();
    drop(connection);
    let migrated = Authority::open(&path).unwrap();
    assert_eq!(migrated.events(0, 10).unwrap(), vec![event]);
    let connection = rusqlite::Connection::open(path.join("state.sqlite")).unwrap();
    let version: i64 = connection
        .pragma_query_value(None, "user_version", |row| row.get(0))
        .unwrap();
    assert_eq!(version, 7);
    let insert = |operation_id: &str, kind: &str| {
        connection.execute(
            "INSERT INTO integration_events(operation_id, target, after_revision, after_version, approval_id, context_digest, policy_epoch, semantics_version, kind) VALUES (?1, 't', ?2, 1, 'base', ?2, 0, 2, ?3)",
            rusqlite::params![operation_id, root, kind],
        )
    };
    // The same row is valid for a real kind, so the CHECK alone rejects the bogus one.
    assert!(insert("kind-bogus", "bogus").is_err());
    assert_eq!(insert("kind-integrate", "integrate").unwrap(), 1);
}

struct Landing {
    dir: TempDir,
    engine: Engine,
    authority: Authority,
    repository: String,
    t: String,
    a: String,
    b: String,
    base: TargetState,
    work: agentscm::context::ChangeState,
}

fn landing_fixture() -> Landing {
    let dir = TempDir::new().unwrap();
    let (engine, mut authority) = open_pair(dir.path());
    let repository = authority.repository_id().unwrap();
    let t = checkpoint(&engine, None, "base.txt", b"base");
    let init = staged(&mut authority, &repository, "init", &t, "main", None, 0);
    let base = authority
        .integrate(
            &engine,
            &request("init", &init, "main", None, &t, "init", 0),
        )
        .unwrap()
        .after;
    let a = checkpoint(&engine, Some(&t), "a.txt", b"a");
    let b = checkpoint(&engine, Some(&a), "b.txt", b"b");
    let work = staged(
        &mut authority,
        &repository,
        "work",
        &b,
        "main",
        Some(base.clone()),
        0,
    );
    Landing {
        dir,
        engine,
        authority,
        repository,
        t,
        a,
        b,
        base,
        work,
    }
}

impl Landing {
    fn prepare_request(&self, operation_id: &str) -> PrepareLandingRequest {
        PrepareLandingRequest {
            operation_id: operation_id.to_owned(),
            target: "main".to_owned(),
            expected_target: self.base.clone(),
            source_tip: self.b.clone(),
            change_version: self.work.version,
            context: context(
                &self.repository,
                "work",
                self.work.version + 1,
                &self.b,
                "main",
                Some(self.base.clone()),
                0,
            ),
        }
    }

    fn rows(&self, table: &str) -> i64 {
        let connection =
            rusqlite::Connection::open(self.dir.path().join("authority/state.sqlite")).unwrap();
        connection
            .query_row(&format!("SELECT count(*) FROM {table}"), [], |row| {
                row.get(0)
            })
            .unwrap()
    }
}

fn prepared(outcome: LandingPreparation) -> (String, agentscm::context::ChangeState) {
    match outcome {
        LandingPreparation::Prepared {
            revision, change, ..
        } => (revision, change),
        other => panic!("expected Prepared, got {other:?}"),
    }
}

#[test]
fn prepares_landing_with_ordered_parents_and_requires_fresh_approval() {
    let mut fx = landing_fixture();
    let req = fx.prepare_request("prep-1");
    let outcome = fx.authority.prepare_landing(&fx.engine, &req).unwrap();
    let (landing, change) = prepared(outcome);
    let revision = fx.engine.load_revision(&landing).unwrap();
    assert_eq!(revision.parents, vec![fx.t.clone(), fx.b.clone()]);
    assert_eq!(revision.tree, fx.engine.load_revision(&fx.b).unwrap().tree);
    assert_eq!(fx.engine.read_file(&landing, "a.txt").unwrap(), b"a");
    assert_eq!(fx.engine.read_file(&landing, "b.txt").unwrap(), b"b");
    assert_eq!(change.version, 2);
    assert_eq!(fx.authority.change(&change.id).unwrap(), change);
    let current = fx.authority.review_context(&change.context_digest).unwrap();
    assert_eq!(current.revision, landing);
    assert_eq!(current.base.unwrap().revision, fx.t);
    // Preparation moved nothing and consumed nothing.
    assert_eq!(fx.authority.target("main").unwrap(), Some(fx.base.clone()));
    assert!(!approval_consumed(&fx.dir.path().join("authority"), "work"));

    // The approval for B cannot land B or L.
    assert_conflict(fx.authority.integrate(
        &fx.engine,
        &request(
            "old-b",
            &fx.work,
            "main",
            Some(fx.base.clone()),
            &fx.b,
            "work",
            0,
        ),
    ));
    assert_conflict(fx.authority.integrate(
        &fx.engine,
        &request(
            "old-l",
            &change,
            "main",
            Some(fx.base.clone()),
            &landing,
            "work",
            0,
        ),
    ));
    // A new approval ID bound to the pre-preparation Change version and context is rejected.
    assert_conflict(fx.authority.approve(&approval(
        "stale-binding",
        &fx.work,
        "main",
        &landing,
        0,
    )));
    fx.authority
        .approve(&approval("fresh", &change, "main", &landing, 0))
        .unwrap();
    let event = fx
        .authority
        .integrate(
            &fx.engine,
            &request(
                "land",
                &change,
                "main",
                Some(fx.base.clone()),
                &landing,
                "fresh",
                0,
            ),
        )
        .unwrap();
    assert_eq!(event.after.revision, landing);
    assert_eq!(event.after.version, fx.base.version + 1);
    assert!(!approval_consumed(&fx.dir.path().join("authority"), "work"));
}

#[test]
fn preparation_retries_are_exact_across_reopen_and_ids_are_disjoint() {
    let mut fx = landing_fixture();
    let req = fx.prepare_request("prep-1");
    let first = fx.authority.prepare_landing(&fx.engine, &req).unwrap();
    // Lost reply: the caller retries the identical request, also after a restart.
    assert_eq!(
        fx.authority.prepare_landing(&fx.engine, &req).unwrap(),
        first
    );
    fx.authority = Authority::open(fx.dir.path().join("authority")).unwrap();
    assert_eq!(
        fx.authority.prepare_landing(&fx.engine, &req).unwrap(),
        first
    );
    assert_eq!(fx.authority.change(&fx.work.id).unwrap().version, 2);
    assert_eq!(fx.rows("landing_preparations"), 1);

    let mut changed = req.clone();
    changed.context.intent = "different intent".to_owned();
    assert_conflict(fx.authority.prepare_landing(&fx.engine, &changed));
    // Operation IDs are shared with integration operations in both directions.
    assert_conflict(fx.authority.integrate(
        &fx.engine,
        &request(
            "prep-1",
            &fx.work,
            "main",
            Some(fx.base.clone()),
            &fx.b,
            "work",
            0,
        ),
    ));
    assert_conflict(
        fx.authority
            .prepare_landing(&fx.engine, &fx.prepare_request("init")),
    );
    assert_eq!(fx.rows("landing_preparations"), 1);
}

#[test]
fn stale_target_and_stale_change_leave_no_journal_row() {
    let mut fx = landing_fixture();
    let next = checkpoint(&fx.engine, Some(&fx.t), "other.txt", b"x");
    let other = staged(
        &mut fx.authority,
        &fx.repository,
        "other",
        &next,
        "main",
        Some(fx.base.clone()),
        0,
    );
    let moved = fx
        .authority
        .integrate(
            &fx.engine,
            &request(
                "other",
                &other,
                "main",
                Some(fx.base.clone()),
                &next,
                "other",
                0,
            ),
        )
        .unwrap()
        .after;
    assert_conflict(
        fx.authority
            .prepare_landing(&fx.engine, &fx.prepare_request("stale")),
    );
    assert_eq!(fx.authority.change(&fx.work.id).unwrap(), fx.work);
    assert_eq!(fx.authority.target("main").unwrap(), Some(moved));
    assert_eq!(fx.rows("landing_preparations"), 0);

    // A stale Change version fails with the target untouched.
    let mut fx = landing_fixture();
    let mut req = fx.prepare_request("stale-change");
    req.change_version = 5;
    req.context.change_version = 6;
    assert_conflict(fx.authority.prepare_landing(&fx.engine, &req));
    assert_eq!(fx.authority.target("main").unwrap(), Some(fx.base.clone()));
    assert_eq!(fx.authority.change(&fx.work.id).unwrap(), fx.work);
    assert_eq!(fx.rows("landing_preparations"), 0);
}

#[test]
fn divergent_base_is_rejected_with_recovery_guidance() {
    let mut fx = landing_fixture();
    let sibling = checkpoint(&fx.engine, None, "base.txt", b"other root");
    let mut req = fx.prepare_request("diverged");
    req.source_tip = sibling.clone();
    req.context.revision = sibling;
    let Err(Error::Conflict(message)) = fx.authority.prepare_landing(&fx.engine, &req) else {
        panic!("expected divergence conflict");
    };
    assert!(
        message.contains("diverged") && message.contains("preserved"),
        "{message}"
    );
    assert!(
        message.contains("merge") && message.contains("three-way"),
        "{message}"
    );
    assert_eq!(fx.authority.change(&fx.work.id).unwrap(), fx.work);
    assert_eq!(fx.rows("landing_preparations"), 0);
}

#[test]
fn already_current_has_durable_no_op_result() {
    let mut fx = landing_fixture();
    // Source tip equal to the target.
    let mut req = fx.prepare_request("noop-1");
    req.source_tip = fx.t.clone();
    req.context.revision = fx.t.clone();
    let outcome = fx.authority.prepare_landing(&fx.engine, &req).unwrap();
    assert!(matches!(outcome, LandingPreparation::AlreadyCurrent { .. }));
    assert_eq!(
        fx.authority.prepare_landing(&fx.engine, &req).unwrap(),
        outcome
    );
    assert_eq!(fx.authority.change(&fx.work.id).unwrap(), fx.work);
    assert_eq!(fx.rows("landing_preparations"), 1);

    // Same tree as the target in a descendant revision.
    let same = fx.engine.checkpoint(Some(&fx.t), &[]).unwrap();
    let mut req = fx.prepare_request("noop-2");
    req.source_tip = same.clone();
    req.context.revision = same;
    let outcome = fx.authority.prepare_landing(&fx.engine, &req).unwrap();
    assert!(matches!(outcome, LandingPreparation::AlreadyCurrent { .. }));
    assert_eq!(fx.authority.change(&fx.work.id).unwrap(), fx.work);
    // A no-op still checks the Change version.
    let mut stale = req.clone();
    stale.operation_id = "noop-3".to_owned();
    stale.change_version = 9;
    stale.context.change_version = 10;
    assert_conflict(fx.authority.prepare_landing(&fx.engine, &stale));
}

#[test]
fn failed_attempt_with_different_digest_leaves_no_row_and_keeps_operation_id() {
    let mut fx = landing_fixture();
    // A different request fails on a stale Change version: no journal row, and the operation ID
    // stays usable. This is not a failure injected between publication and commit.
    let mut broken = fx.prepare_request("crash");
    broken.change_version = 7;
    broken.context.change_version = 8;
    assert_conflict(fx.authority.prepare_landing(&fx.engine, &broken));
    assert_eq!(fx.rows("landing_preparations"), 0);
    let outcome = fx
        .authority
        .prepare_landing(&fx.engine, &fx.prepare_request("crash"))
        .unwrap();
    let (landing, change) = prepared(outcome);
    assert_eq!(change.version, 2);
    assert_eq!(
        fx.engine.load_revision(&landing).unwrap().parents,
        vec![fx.t.clone(), fx.b.clone()]
    );
    assert_eq!(fx.rows("landing_preparations"), 1);
    assert_eq!(fx.rows("changes"), 2);
}

#[test]
fn contexts_that_do_not_describe_the_work_are_invalid() {
    let mut fx = landing_fixture();
    let good = fx.prepare_request("bad");
    let mut wrong_revision = good.clone();
    wrong_revision.context.revision = fx.a.clone();
    let mut wrong_base = good.clone();
    wrong_base.context.base = None;
    let mut wrong_target = good.clone();
    wrong_target.context.target = "other".to_owned();
    let mut wrong_version = good.clone();
    wrong_version.context.change_version = 5;
    for req in [wrong_revision, wrong_base, wrong_target, wrong_version] {
        let result = fx.authority.prepare_landing(&fx.engine, &req);
        assert!(matches!(result, Err(Error::Invalid(_))), "{result:?}");
    }
    assert_eq!(fx.authority.change(&fx.work.id).unwrap(), fx.work);
    assert_eq!(fx.rows("landing_preparations"), 0);
}

#[test]
fn concurrent_preparations_of_one_change_have_one_winner() {
    let fx = landing_fixture();
    let authority_path = fx.dir.path().join("authority");
    let source_path = fx.dir.path().join("source");
    let barrier = Arc::new(Barrier::new(3));
    let handles: Vec<_> = ["left", "right"]
        .into_iter()
        .map(|name| {
            let req = fx.prepare_request(name);
            let (authority_path, source_path) = (authority_path.clone(), source_path.clone());
            let barrier = Arc::clone(&barrier);
            std::thread::spawn(move || {
                let engine = Engine::open(source_path).unwrap();
                let mut authority = Authority::open(authority_path).unwrap();
                barrier.wait();
                authority.prepare_landing(&engine, &req)
            })
        })
        .collect();
    barrier.wait();
    let results: Vec<_> = handles.into_iter().map(|h| h.join().unwrap()).collect();
    assert_eq!(results.iter().filter(|r| r.is_ok()).count(), 1);
    assert_eq!(
        results
            .iter()
            .filter(|r| matches!(r, Err(Error::Conflict(_))))
            .count(),
        1
    );
    assert_eq!(fx.authority.change(&fx.work.id).unwrap().version, 2);
    assert_eq!(fx.rows("landing_preparations"), 1);
}

#[test]
fn preparation_racing_target_movement_stays_consistent() {
    let mut fx = landing_fixture();
    let next = checkpoint(&fx.engine, Some(&fx.t), "other.txt", b"x");
    let other = staged(
        &mut fx.authority,
        &fx.repository,
        "other",
        &next,
        "main",
        Some(fx.base.clone()),
        0,
    );
    let integrate = request(
        "other",
        &other,
        "main",
        Some(fx.base.clone()),
        &next,
        "other",
        0,
    );
    let prep = fx.prepare_request("race");
    let authority_path = fx.dir.path().join("authority");
    let source_path = fx.dir.path().join("source");
    let barrier = Arc::new(Barrier::new(3));
    let preparer = {
        let (authority_path, source_path, barrier) = (
            authority_path.clone(),
            source_path.clone(),
            Arc::clone(&barrier),
        );
        std::thread::spawn(move || {
            let engine = Engine::open(source_path).unwrap();
            let mut authority = Authority::open(authority_path).unwrap();
            barrier.wait();
            authority.prepare_landing(&engine, &prep)
        })
    };
    let integrator = {
        let barrier = Arc::clone(&barrier);
        std::thread::spawn(move || {
            let engine = Engine::open(source_path).unwrap();
            let mut authority = Authority::open(authority_path).unwrap();
            barrier.wait();
            authority.integrate(&engine, &integrate)
        })
    };
    barrier.wait();
    let prepared_result = preparer.join().unwrap();
    // Preparation never moves the target, so integration always wins its own CAS.
    integrator.join().unwrap().unwrap();
    assert_eq!(
        fx.authority.target("main").unwrap().unwrap().version,
        fx.base.version + 1
    );
    match prepared_result {
        Ok(_) => {
            assert_eq!(fx.authority.change(&fx.work.id).unwrap().version, 2);
            assert_eq!(fx.rows("landing_preparations"), 1);
        }
        Err(Error::Conflict(_)) => {
            assert_eq!(fx.authority.change(&fx.work.id).unwrap(), fx.work);
            assert_eq!(fx.rows("landing_preparations"), 0);
        }
        Err(other) => panic!("{other:?}"),
    }
}

/// Holds the SQLite write lock from a second connection until dropped or rolled back.
fn hold_write_lock(fx: &Landing) -> rusqlite::Connection {
    let connection =
        rusqlite::Connection::open(fx.dir.path().join("authority/state.sqlite")).unwrap();
    connection.execute_batch("BEGIN IMMEDIATE").unwrap();
    connection
}

#[test]
fn prepare_landing_write_lock_failure_changes_nothing_and_identical_retry_succeeds() {
    let mut fx = landing_fixture();
    let req = fx.prepare_request("locked");
    let lock = hold_write_lock(&fx);
    // The landing revision is published, then the write transaction cannot begin.
    let failed = fx.authority.prepare_landing(&fx.engine, &req);
    assert!(failed.is_err(), "{failed:?}");
    assert_eq!(fx.rows("landing_preparations"), 0);
    assert_eq!(fx.authority.change(&fx.work.id).unwrap(), fx.work);
    assert_eq!(fx.authority.target("main").unwrap(), Some(fx.base.clone()));
    lock.execute_batch("ROLLBACK").unwrap();
    drop(lock);

    let first = fx.authority.prepare_landing(&fx.engine, &req).unwrap();
    assert_eq!(
        fx.authority.prepare_landing(&fx.engine, &req).unwrap(),
        first
    );
    let (_, change) = prepared(first);
    assert_eq!(change.version, fx.work.version + 1);
    assert_eq!(fx.authority.change(&fx.work.id).unwrap(), change);
    assert_eq!(fx.rows("landing_preparations"), 1);
    assert!(fx.authority.events(0, 10).unwrap().len() == 1);
    assert!(!approval_consumed(&fx.dir.path().join("authority"), "work"));
}

#[test]
fn initialize_write_lock_failure_changes_nothing_and_identical_retry_succeeds() {
    let temporary = TempDir::new().unwrap();
    let (engine, mut authority) = open_pair(temporary.path());
    let repository = authority.repository_id().unwrap();
    let root = checkpoint(&engine, None, "a", b"root");
    let head = checkpoint(&engine, Some(&root), "a", b"head");
    let state = staged(&mut authority, &repository, "init", &head, "fresh", None, 0);
    let init = request("init-locked", &state, "fresh", None, &head, "init", 0);
    let path = temporary.path().join("authority");
    let lock = rusqlite::Connection::open(path.join("state.sqlite")).unwrap();
    lock.execute_batch("BEGIN IMMEDIATE").unwrap();
    let failed = authority.initialize_target(&engine, &init);
    assert!(failed.is_err(), "{failed:?}");
    assert!(authority.target("fresh").unwrap().is_none());
    assert!(authority.events(0, 10).unwrap().is_empty());
    assert!(!approval_consumed(&path, "init"));
    assert_eq!(authority.change(&state.id).unwrap(), state);
    lock.execute_batch("ROLLBACK").unwrap();
    drop(lock);

    let event = authority.initialize_target(&engine, &init).unwrap();
    assert_eq!(authority.initialize_target(&engine, &init).unwrap(), event);
    assert_eq!(authority.events(0, 10).unwrap(), vec![event]);
    assert_eq!(authority.target("fresh").unwrap().unwrap().version, 1);
    assert!(approval_consumed(&path, "init"));
}

#[test]
fn already_current_rejects_stale_policy_context_without_journal_row() {
    let mut fx = landing_fixture();
    assert_eq!(fx.authority.advance_policy(0).unwrap(), 1);
    let mut req = fx.prepare_request("noop-stale-policy");
    req.source_tip = fx.t.clone();
    req.context.revision = fx.t.clone();
    // The context still names the old policy epoch 0.
    assert_conflict(fx.authority.prepare_landing(&fx.engine, &req));
    assert_eq!(fx.rows("landing_preparations"), 0);
}

#[test]
fn event_json_without_kind_deserializes_as_integrate() {
    let temporary = TempDir::new().unwrap();
    let (engine, mut authority) = open_pair(temporary.path());
    let repository = authority.repository_id().unwrap();
    let root = checkpoint(&engine, None, "a", b"root");
    let state = staged(&mut authority, &repository, "base", &root, "main", None, 0);
    let event = authority
        .integrate(
            &engine,
            &request("base-op", &state, "main", None, &root, "base", 0),
        )
        .unwrap();
    let mut json = serde_json::to_value(&event).unwrap();
    assert!(json.as_object_mut().unwrap().remove("kind").is_some());
    let legacy: agentscm::model::IntegrationEvent = serde_json::from_value(json).unwrap();
    assert_eq!(legacy, event);
}

#[test]
fn v4_database_gains_command_check_tables() {
    let temporary = TempDir::new().unwrap();
    let (_engine, authority) = open_pair(temporary.path());
    drop(authority);
    let path = temporary.path().join("authority");
    let connection = rusqlite::Connection::open(path.join("state.sqlite")).unwrap();
    connection
        .execute_batch(
            "DROP INDEX command_success_subject;
             DROP TABLE command_evidence;
             DROP TABLE command_attempts;
             DROP TABLE workflow_receipts;
             DROP TABLE workflow_requirements;
             PRAGMA user_version = 4;",
        )
        .unwrap();
    drop(connection);
    drop(Authority::open(&path).unwrap());
    let connection = rusqlite::Connection::open(path.join("state.sqlite")).unwrap();
    let version: i64 = connection
        .pragma_query_value(None, "user_version", |row| row.get(0))
        .unwrap();
    assert_eq!(version, 7);
    let tables: i64 = connection
        .query_row(
            "SELECT count(*) FROM sqlite_master WHERE name IN ('command_attempts', 'command_evidence', 'command_success_subject')",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(tables, 3);
}

#[test]
fn v5_database_gains_command_check_custody() {
    let temporary = TempDir::new().unwrap();
    let (_engine, authority) = open_pair(temporary.path());
    drop(authority);
    let path = temporary.path().join("authority");
    let connection = rusqlite::Connection::open(path.join("state.sqlite")).unwrap();
    connection
        .execute_batch(
            "ALTER TABLE command_attempts DROP COLUMN custody_json;
             DROP TABLE workflow_receipts;
             DROP TABLE workflow_requirements;
             PRAGMA user_version = 5;",
        )
        .unwrap();
    drop(connection);
    drop(Authority::open(&path).unwrap());
    let connection = rusqlite::Connection::open(path.join("state.sqlite")).unwrap();
    let version: i64 = connection
        .pragma_query_value(None, "user_version", |row| row.get(0))
        .unwrap();
    assert_eq!(version, 7);
    connection
        .prepare("SELECT custody_json FROM command_attempts LIMIT 0")
        .unwrap();
}

#[test]
fn v5_database_with_a_running_command_attempt_refuses_to_upgrade() {
    let temporary = TempDir::new().unwrap();
    let (_engine, authority) = open_pair(temporary.path());
    drop(authority);
    let path = temporary.path().join("authority");
    let connection = rusqlite::Connection::open(path.join("state.sqlite")).unwrap();
    let digest = "a".repeat(64);
    connection
        .execute_batch(&format!(
            "ALTER TABLE command_attempts DROP COLUMN custody_json;
             INSERT INTO command_attempts(attempt_id, request_digest, repository, change_id,
                 change_version, context_digest, revision, recipe_sha256, toolchain_sha256,
                 environment_sha256, recipe_json, state, run_generation)
             VALUES ('v5-run', '{digest}', 'r', 'c', 1, '{digest}', '{digest}', '{digest}',
                 '{digest}', '{digest}', X'00', 'running', 1);
             DROP TABLE workflow_receipts;
             DROP TABLE workflow_requirements;
             PRAGMA user_version = 5;"
        ))
        .unwrap();
    drop(connection);
    assert!(matches!(
        Authority::open(&path),
        Err(agentscm::Error::Conflict(message)) if message.contains("before upgrading")
    ));
    let connection = rusqlite::Connection::open(path.join("state.sqlite")).unwrap();
    let version: i64 = connection
        .pragma_query_value(None, "user_version", |row| row.get(0))
        .unwrap();
    assert_eq!(version, 5);
}
