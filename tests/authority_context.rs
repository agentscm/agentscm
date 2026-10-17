use std::path::Path;
use std::sync::{Arc, Barrier};

use agentscm::Error;
use agentscm::authority::Authority;
use agentscm::context::{BaseState, ChangeId, InputIdentity, ReviewContext};
use agentscm::model::{Approval, IntegrateRequest, TargetState};
use agentscm::store::Engine;
use rusqlite::{Connection, params};
use sha2::{Digest, Sha256};
use tempfile::TempDir;

const REVISION: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const RECIPE: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
const TOOLCHAIN: &str = "cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc";
const ENVIRONMENT: &str = "dddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddd";
const LEGACY_CONTEXT: &str = "eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee";

fn complete_context(repository: &str, id: &str, version: u64) -> ReviewContext {
    ReviewContext {
        version: 1,
        change: ChangeId {
            repository: repository.to_owned(),
            id: id.to_owned(),
        },
        change_version: version,
        revision: REVISION.to_owned(),
        base: None,
        target: "main".to_owned(),
        policy_epoch: 0,
        intent: "review an exact source revision".to_owned(),
        acceptance_criteria: vec!["criterion-a".to_owned()],
        scope: vec!["src".to_owned()],
        exclusions: vec![],
        omissions: vec![],
        dependencies: vec![],
        recipe: InputIdentity::Digest {
            sha256: RECIPE.to_owned(),
        },
        toolchain: InputIdentity::Digest {
            sha256: TOOLCHAIN.to_owned(),
        },
        environment: InputIdentity::Digest {
            sha256: ENVIRONMENT.to_owned(),
        },
        required_evidence: vec![],
        required_capabilities: vec!["agentscm.review-context/v1".to_owned()],
    }
}

fn approval(id: &str, state: &agentscm::context::ChangeState) -> Approval {
    Approval {
        approval_id: id.to_owned(),
        target: "main".to_owned(),
        revision: REVISION.to_owned(),
        context_digest: state.context_digest.clone(),
        change: Some(state.id.clone()),
        change_version: Some(state.version),
        policy_epoch: 0,
    }
}

fn assert_conflict<T: std::fmt::Debug>(value: agentscm::Result<T>) {
    assert!(matches!(value, Err(Error::Conflict(_))), "{value:?}");
}

#[test]
fn repository_identity_and_context_history_survive_reopen() {
    let temporary = TempDir::new().expect("tempdir");
    let path = temporary.path().join("authority");
    let (repository, first, second) = {
        let mut authority = Authority::open(&path).expect("authority");
        let repository = authority.repository_id().expect("repository");
        assert_eq!(repository.len(), 32);
        assert!(
            repository
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        );
        let first = authority
            .start_change(&complete_context(&repository, "change", 1))
            .expect("start");
        let mut next = complete_context(&repository, "change", 2);
        next.intent = "updated review intent".to_owned();
        let second = authority.update_change_context(1, &next).expect("update");
        assert_eq!(
            authority
                .review_context(&first.context_digest)
                .expect("historical"),
            complete_context(&repository, "change", 1)
        );
        (repository, first, second)
    };
    let authority = Authority::open(&path).expect("reopen");
    assert_eq!(authority.repository_id().expect("repository"), repository);
    assert_eq!(authority.change(&second.id).expect("change"), second);
    assert_eq!(
        authority
            .review_context(&first.context_digest)
            .expect("historical")
            .change_version,
        1
    );
}

#[test]
fn incomplete_contexts_are_inspectable_but_cannot_be_approved() {
    let temporary = TempDir::new().expect("tempdir");
    let mut authority = Authority::open(temporary.path()).expect("authority");
    let repository = authority.repository_id().expect("repository");
    let mut unknown_input = complete_context(&repository, "incomplete", 1);
    unknown_input.recipe = InputIdentity::Unknown {};
    let state = authority
        .start_change(&unknown_input)
        .expect("register incomplete");
    assert_eq!(
        authority
            .review_context(&state.context_digest)
            .expect("inspect"),
        unknown_input
    );
    assert!(
        matches!(authority.approve(&approval("incomplete", &state)), Err(Error::Invalid(message)) if message.contains("recipe is unknown"))
    );

    let mut unknown_capability = complete_context(&repository, "incomplete", 2);
    unknown_capability
        .required_capabilities
        .push("unknown/capability".to_owned());
    let state = authority
        .update_change_context(1, &unknown_capability)
        .expect("update incomplete");
    assert!(
        matches!(authority.approve(&approval("unknown-capability", &state)), Err(Error::Invalid(message)) if message.contains("unsupported"))
    );
}

#[test]
fn registration_rejects_foreign_repository_stale_base_and_policy_atomically() {
    let temporary = TempDir::new().expect("tempdir");
    let mut authority = Authority::open(temporary.path()).expect("authority");
    let repository = authority.repository_id().expect("repository");
    let mut foreign = complete_context("foreign-repository", "foreign", 1);
    let foreign_digest = foreign.digest().expect("digest");
    assert_conflict(authority.start_change(&foreign));
    assert!(matches!(
        authority.review_context(&foreign_digest),
        Err(Error::NotFound(_))
    ));

    foreign = complete_context(&repository, "bad-base", 1);
    foreign.base = Some(BaseState {
        revision: REVISION.to_owned(),
        target_version: 1,
    });
    let base_digest = foreign.digest().expect("digest");
    assert_conflict(authority.start_change(&foreign));
    assert!(matches!(
        authority.review_context(&base_digest),
        Err(Error::NotFound(_))
    ));

    foreign = complete_context(&repository, "bad-policy", 1);
    foreign.policy_epoch = 1;
    let policy_digest = foreign.digest().expect("digest");
    assert_conflict(authority.start_change(&foreign));
    assert!(matches!(
        authority.review_context(&policy_digest),
        Err(Error::NotFound(_))
    ));

    let valid = authority
        .start_change(&complete_context(&repository, "valid", 1))
        .expect("valid change");
    let mut foreign_approval = approval("foreign", &valid);
    foreign_approval.change.as_mut().expect("change").repository = "foreign-repository".to_owned();
    assert_conflict(authority.approve(&foreign_approval));
}

#[test]
fn same_source_criteria_change_and_aba_generation_invalidate_approval() {
    let temporary = TempDir::new().expect("tempdir");
    let source = temporary.path().join("source");
    let engine = Engine::open(&source).expect("engine");
    let actual_revision = engine.checkpoint(None, &[]).expect("revision");
    let mut authority = Authority::open(temporary.path().join("authority")).expect("authority");
    let repository = authority.repository_id().expect("repository");
    let mut first_context = complete_context(&repository, "change", 1);
    first_context.revision = actual_revision.clone();
    let first = authority.start_change(&first_context).expect("start");
    authority
        .approve(&Approval {
            revision: actual_revision.clone(),
            ..approval("old", &first)
        })
        .expect("approve");

    let mut second_context = first_context.clone();
    second_context.change_version = 2;
    second_context.acceptance_criteria = vec!["criterion-b".to_owned()];
    authority
        .update_change_context(1, &second_context)
        .expect("criteria update");
    let old_request = IntegrateRequest {
        operation_id: "old-op".to_owned(),
        target: "main".to_owned(),
        expected_target: None,
        revision: actual_revision.clone(),
        context_digest: first.context_digest.clone(),
        approval_id: "old".to_owned(),
        change: Some(first.id.clone()),
        change_version: Some(1),
        policy_epoch: 0,
    };
    assert_conflict(authority.integrate(&engine, &old_request));

    let mut third_context = first_context;
    third_context.change_version = 3;
    let third = authority
        .update_change_context(2, &third_context)
        .expect("ABA update");
    assert_eq!(third.version, 3);
    assert_ne!(third.context_digest, first.context_digest);
    assert_conflict(authority.integrate(&engine, &old_request));
}

#[test]
fn concurrent_compare_and_swap_has_one_winner_and_rolls_back_loser_context() {
    let temporary = TempDir::new().expect("tempdir");
    let path = temporary.path().join("authority");
    let mut setup = Authority::open(&path).expect("authority");
    let repository = setup.repository_id().expect("repository");
    setup
        .start_change(&complete_context(&repository, "change", 1))
        .expect("start");
    drop(setup);

    let barrier = Arc::new(Barrier::new(3));
    let handles: Vec<_> = ["left", "right"]
        .into_iter()
        .map(|criterion| {
            let path = path.clone();
            let repository = repository.clone();
            let barrier = Arc::clone(&barrier);
            std::thread::spawn(move || {
                let mut authority = Authority::open(path).expect("thread authority");
                let mut context = complete_context(&repository, "change", 2);
                context.acceptance_criteria = vec![criterion.to_owned()];
                let digest = context.digest().expect("digest");
                barrier.wait();
                (digest, authority.update_change_context(1, &context))
            })
        })
        .collect();
    barrier.wait();
    let results: Vec<_> = handles
        .into_iter()
        .map(|handle| handle.join().expect("join"))
        .collect();
    assert_eq!(
        results.iter().filter(|(_, result)| result.is_ok()).count(),
        1
    );
    assert_eq!(
        results
            .iter()
            .filter(|(_, result)| matches!(result, Err(Error::Conflict(_))))
            .count(),
        1
    );
    let authority = Authority::open(&path).expect("reopen");
    let winner = authority
        .change(&ChangeId {
            repository,
            id: "change".to_owned(),
        })
        .expect("change");
    for (digest, result) in results {
        if result.is_ok() {
            assert_eq!(digest, winner.context_digest);
        } else {
            assert!(matches!(
                authority.review_context(&digest),
                Err(Error::NotFound(_))
            ));
        }
    }
}

#[test]
fn failed_update_rolls_back_new_immutable_context() {
    let temporary = TempDir::new().expect("tempdir");
    let mut authority = Authority::open(temporary.path()).expect("authority");
    let repository = authority.repository_id().expect("repository");
    let missing = complete_context(&repository, "missing", 2);
    let digest = missing.digest().expect("digest");
    assert_conflict(authority.update_change_context(1, &missing));
    assert!(matches!(
        authority.review_context(&digest),
        Err(Error::NotFound(_))
    ));
}

#[test]
fn migrates_true_v1_state_preserves_retry_and_revokes_opaque_approval() {
    let temporary = TempDir::new().expect("tempdir");
    let authority_path = temporary.path().join("authority");
    std::fs::create_dir_all(&authority_path).expect("authority directory");
    let request = legacy_request();
    create_v1_fixture(&authority_path.join("state.sqlite"), &request);
    let engine = Engine::open(temporary.path().join("source")).expect("engine");
    let mut authority = Authority::open(&authority_path).expect("migrate");
    let repository = authority.repository_id().expect("repository");
    assert_eq!(repository.len(), 32);
    assert_eq!(authority.advance_policy(0).expect("advance policy"), 1);
    let mut context = complete_context(&repository, "post-migration", 1);
    context.policy_epoch = 1;
    context.base = Some(BaseState {
        revision: REVISION.to_owned(),
        target_version: 1,
    });
    let first = authority.start_change(&context).expect("start context");
    context.change_version = 2;
    context.intent = "move current context".to_owned();
    authority
        .update_change_context(1, &context)
        .expect("move context");

    let event = authority
        .integrate(&engine, &request)
        .expect("historical retry");
    assert_eq!(event.semantics_version, 1);
    assert_eq!(event.change, None);
    assert_eq!(event.change_version, None);
    assert_eq!(event.operation_id, "legacy-operation");
    assert_eq!(
        event.after,
        TargetState {
            revision: REVISION.to_owned(),
            version: 1
        }
    );
    assert_eq!(
        authority.events(0, 10).expect("events"),
        vec![event.clone()]
    );
    let serialized = serde_json::to_value(&event).expect("serialize");
    assert_eq!(serialized["semantics_version"], 1);
    assert!(serialized.get("change").is_none());
    assert!(serialized.get("change_version").is_none());
    assert_eq!(serialized["context_digest"], LEGACY_CONTEXT);
    assert_eq!(authority.change(&first.id).expect("change").version, 2);
    drop(authority);

    let connection = Connection::open(authority_path.join("state.sqlite")).expect("database");
    let revoked: i64 = connection
        .query_row(
            "SELECT revoked FROM approvals WHERE approval_id = 'unused-legacy'",
            [],
            |row| row.get(0),
        )
        .expect("revoked");
    let stored_digest: String = connection
        .query_row(
            "SELECT request_digest FROM operations WHERE operation_id = 'legacy-operation'",
            [],
            |row| row.get(0),
        )
        .expect("request digest");
    assert_eq!(revoked, 1);
    assert_eq!(stored_digest, legacy_digest(&request));
}

fn legacy_request() -> IntegrateRequest {
    IntegrateRequest {
        operation_id: "legacy-operation".to_owned(),
        target: "main".to_owned(),
        expected_target: None,
        revision: REVISION.to_owned(),
        context_digest: LEGACY_CONTEXT.to_owned(),
        approval_id: "legacy-approval".to_owned(),
        change: None,
        change_version: None,
        policy_epoch: 0,
    }
}

fn legacy_digest(request: &IntegrateRequest) -> String {
    let mut hasher = Sha256::new();
    hasher.update(b"agentscm.integration-request.v1\0");
    hash_field(&mut hasher, request.operation_id.as_bytes());
    hash_field(&mut hasher, request.target.as_bytes());
    hasher.update([0]);
    hash_field(&mut hasher, request.revision.as_bytes());
    hash_field(&mut hasher, request.context_digest.as_bytes());
    hash_field(&mut hasher, request.approval_id.as_bytes());
    hasher.update(request.policy_epoch.to_be_bytes());
    format!("{:x}", hasher.finalize())
}

fn hash_field(hasher: &mut Sha256, value: &[u8]) {
    hasher.update((value.len() as u64).to_be_bytes());
    hasher.update(value);
}

fn create_v1_fixture(path: &Path, request: &IntegrateRequest) {
    let connection = Connection::open(path).expect("fixture database");
    connection.execute_batch(
        "PRAGMA application_id = 1095975757;
         PRAGMA user_version = 1;
         CREATE TABLE authority_state (singleton INTEGER PRIMARY KEY CHECK (singleton = 1), policy_epoch INTEGER NOT NULL CHECK (policy_epoch >= 0)) STRICT;
         INSERT INTO authority_state(singleton, policy_epoch) VALUES (1, 0);
         CREATE TABLE approvals (approval_id TEXT PRIMARY KEY, target TEXT NOT NULL, revision TEXT NOT NULL, context_digest TEXT NOT NULL, policy_epoch INTEGER NOT NULL CHECK (policy_epoch >= 0), revoked INTEGER NOT NULL DEFAULT 0 CHECK (revoked IN (0, 1)), consumed_event INTEGER UNIQUE, CHECK (length(approval_id) BETWEEN 1 AND 128), CHECK (length(target) BETWEEN 1 AND 128), CHECK (length(revision) = 64), CHECK (length(context_digest) = 64)) STRICT;
         CREATE TABLE targets (name TEXT PRIMARY KEY, revision TEXT NOT NULL, version INTEGER NOT NULL CHECK (version > 0), CHECK (length(name) BETWEEN 1 AND 128), CHECK (length(revision) = 64)) STRICT;
         CREATE TABLE integration_events (sequence INTEGER PRIMARY KEY AUTOINCREMENT, operation_id TEXT NOT NULL UNIQUE, target TEXT NOT NULL, before_revision TEXT, before_version INTEGER, after_revision TEXT NOT NULL, after_version INTEGER NOT NULL CHECK (after_version > 0), approval_id TEXT NOT NULL, context_digest TEXT NOT NULL, policy_epoch INTEGER NOT NULL CHECK (policy_epoch >= 0), CHECK ((before_revision IS NULL) = (before_version IS NULL)), CHECK (before_version IS NULL OR before_version > 0), FOREIGN KEY (approval_id) REFERENCES approvals(approval_id)) STRICT;
         CREATE TABLE operations (operation_id TEXT PRIMARY KEY, request_digest TEXT NOT NULL CHECK (length(request_digest) = 64), event_sequence INTEGER NOT NULL UNIQUE, FOREIGN KEY (event_sequence) REFERENCES integration_events(sequence)) STRICT;",
    ).expect("v1 schema");
    connection.execute("INSERT INTO approvals(approval_id, target, revision, context_digest, policy_epoch, consumed_event) VALUES ('legacy-approval', 'main', ?1, ?2, 0, 1)", params![REVISION, LEGACY_CONTEXT]).expect("approval");
    connection.execute("INSERT INTO approvals(approval_id, target, revision, context_digest, policy_epoch) VALUES ('unused-legacy', 'main', ?1, ?2, 0)", params![REVISION, LEGACY_CONTEXT]).expect("unused approval");
    connection
        .execute(
            "INSERT INTO targets(name, revision, version) VALUES ('main', ?1, 1)",
            [REVISION],
        )
        .expect("target");
    connection.execute("INSERT INTO integration_events(sequence, operation_id, target, before_revision, before_version, after_revision, after_version, approval_id, context_digest, policy_epoch) VALUES (1, 'legacy-operation', 'main', NULL, NULL, ?1, 1, 'legacy-approval', ?2, 0)", params![REVISION, LEGACY_CONTEXT]).expect("event");
    connection.execute("INSERT INTO operations(operation_id, request_digest, event_sequence) VALUES ('legacy-operation', ?1, 1)", [legacy_digest(request)]).expect("operation");
}
