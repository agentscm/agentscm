use std::fs;
use std::path::Path;
use std::sync::{Arc, Barrier};

use agentscm::Error;
use agentscm::authority::Authority;
use agentscm::checks::{
    CheckLimits, CheckState, SOURCE_PATHS_CAPABILITY, SOURCE_PATHS_EVIDENCE, SourcePathExpectation,
    SourcePathsRecipe, SourcePathsRequest,
};
use agentscm::context::{ChangeId, InputIdentity, RequiredEvidence, ReviewContext};
use agentscm::model::{Approval, EntryChange, EntryKind, FileChange, IntegrateRequest};
use agentscm::store::Engine;
use rusqlite::{Connection, params};
use sha2::{Digest, Sha256};
use tempfile::TempDir;

const TOOLCHAIN: &str = "cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc";
const ENVIRONMENT: &str = "dddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddd";

fn sha256(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn recipe(path: &str, bytes: &[u8]) -> SourcePathsRecipe {
    SourcePathsRecipe {
        version: 1,
        expectations: vec![SourcePathExpectation {
            path: path.to_owned(),
            sha256: sha256(bytes),
        }],
        limits: CheckLimits {
            max_paths: 8,
            max_total_bytes: 1024 * 1024,
            max_output_bytes: 4096,
            timeout_ms: 30_000,
        },
    }
}

fn make_context(
    repository: &str,
    id: &str,
    version: u64,
    revision: &str,
    recipe: &SourcePathsRecipe,
) -> ReviewContext {
    let recipe_digest = recipe.digest().expect("recipe digest");
    ReviewContext {
        version: 1,
        change: ChangeId {
            repository: repository.to_owned(),
            id: id.to_owned(),
        },
        change_version: version,
        revision: revision.to_owned(),
        base: None,
        target: "main".to_owned(),
        policy_epoch: 0,
        intent: "verify exact retained source paths".to_owned(),
        acceptance_criteria: vec!["required source bytes match".to_owned()],
        scope: vec!["README.md".to_owned()],
        exclusions: vec![],
        omissions: vec![],
        dependencies: vec![],
        recipe: InputIdentity::Digest {
            sha256: recipe_digest.clone(),
        },
        toolchain: InputIdentity::Digest {
            sha256: TOOLCHAIN.to_owned(),
        },
        environment: InputIdentity::Digest {
            sha256: ENVIRONMENT.to_owned(),
        },
        required_evidence: vec![RequiredEvidence {
            identity: SOURCE_PATHS_EVIDENCE.to_owned(),
            digest: InputIdentity::Digest {
                sha256: recipe_digest,
            },
        }],
        required_capabilities: vec![
            "agentscm.review-context/v1".to_owned(),
            SOURCE_PATHS_CAPABILITY.to_owned(),
        ],
    }
}

fn request(
    context: &ReviewContext,
    recipe: SourcePathsRecipe,
    attempt_id: &str,
) -> SourcePathsRequest {
    SourcePathsRequest {
        attempt_id: attempt_id.to_owned(),
        change: context.change.clone(),
        change_version: context.change_version,
        context_digest: context.digest().expect("context digest"),
        revision: context.revision.clone(),
        recipe,
    }
}

fn approval(context: &ReviewContext, id: &str) -> Approval {
    Approval {
        approval_id: id.to_owned(),
        target: context.target.clone(),
        revision: context.revision.clone(),
        context_digest: context.digest().expect("context digest"),
        change: Some(context.change.clone()),
        change_version: Some(context.change_version),
        policy_epoch: context.policy_epoch,
    }
}

fn fixture() -> (TempDir, Engine, Authority, ReviewContext, SourcePathsRecipe) {
    let temporary = TempDir::new().expect("tempdir");
    let engine = Engine::open(temporary.path().join("source")).expect("engine");
    let revision = engine
        .checkpoint(
            None,
            &[FileChange {
                path: "README.md".to_owned(),
                content: Some(b"trusted bytes".to_vec()),
            }],
        )
        .expect("revision");
    let mut authority = Authority::open(temporary.path().join("authority")).expect("authority");
    let repository = authority.repository_id().expect("repository");
    let recipe = recipe("README.md", b"trusted bytes");
    let context = make_context(&repository, "change-1", 1, &revision, &recipe);
    authority.start_change(&context).expect("start change");
    (temporary, engine, authority, context, recipe)
}

#[test]
fn recipe_is_canonical_and_success_is_required_for_approval() {
    let (_temporary, engine, mut authority, context, recipe) = fixture();
    let mut reversed = recipe.clone();
    reversed.expectations.push(SourcePathExpectation {
        path: "Cargo.toml".to_owned(),
        sha256: sha256(b"manifest"),
    });
    let mut sorted = reversed.clone();
    sorted.expectations.reverse();
    assert_eq!(
        reversed.digest().expect("digest"),
        sorted.digest().expect("digest")
    );

    assert!(matches!(
        authority.approve(&approval(&context, "before-check")),
        Err(Error::Invalid(message)) if message.contains("no exact successful controller attempt")
    ));
    let attempt_request = request(&context, recipe, "attempt-1");
    let pending = authority
        .create_source_paths_attempt(&attempt_request)
        .expect("create attempt");
    assert_eq!(pending.state, CheckState::Pending);
    assert_eq!(pending.run_generation, 0);
    let success = authority
        .run_source_paths_attempt(&engine, "attempt-1", 0)
        .expect("run attempt");
    assert_eq!(success.state, CheckState::Success);
    assert_eq!(success.run_generation, 2);
    let evidence = authority
        .check_evidence(success.evidence_digest.as_deref().expect("evidence digest"))
        .expect("evidence");
    assert_eq!(evidence.state, CheckState::Success);
    assert_eq!(evidence.execution_basis, "controller_builtin");
    assert_eq!(evidence.observations.len(), 1);
    authority
        .approve(&approval(&context, "after-check"))
        .expect("approval after exact success");
    let event = authority
        .integrate(
            &engine,
            &IntegrateRequest {
                operation_id: "integrate-after-check".to_owned(),
                target: context.target.clone(),
                expected_target: None,
                revision: context.revision.clone(),
                context_digest: context.digest().expect("context digest"),
                approval_id: "after-check".to_owned(),
                change: Some(context.change.clone()),
                change_version: Some(context.change_version),
                policy_epoch: context.policy_epoch,
            },
        )
        .expect("integration after exact success");
    assert_eq!(event.after.revision, context.revision);
    assert_eq!(
        authority
            .create_source_paths_attempt(&attempt_request)
            .expect("retry retained attempt after target advance"),
        success
    );
}

#[test]
fn failed_check_and_conflicting_attempt_replay_cannot_authorize() {
    let (_temporary, engine, mut authority, context, mut recipe) = fixture();
    recipe.expectations[0].sha256 = sha256(b"different bytes");
    let failed_context = make_context(
        &context.change.repository,
        "change-2",
        1,
        &context.revision,
        &recipe,
    );
    authority
        .start_change(&failed_context)
        .expect("start failed change");
    let request = request(&failed_context, recipe.clone(), "attempt-fail");
    let first = authority
        .create_source_paths_attempt(&request)
        .expect("create attempt");
    assert_eq!(
        authority
            .create_source_paths_attempt(&request)
            .expect("durable retry"),
        first
    );
    let failed = authority
        .run_source_paths_attempt(&engine, "attempt-fail", 0)
        .expect("run failed check");
    assert_eq!(failed.state, CheckState::Fail);
    assert!(
        authority
            .approve(&approval(&failed_context, "failed-approval"))
            .is_err()
    );

    let mut conflicting = request;
    conflicting.revision =
        "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".to_owned();
    assert!(matches!(
        authority.create_source_paths_attempt(&conflicting),
        Err(Error::Conflict(_))
    ));
}

#[test]
fn changed_context_needs_new_exact_success() {
    let (_temporary, engine, mut authority, first, recipe) = fixture();
    let first_request = request(&first, recipe.clone(), "first");
    authority
        .create_source_paths_attempt(&first_request)
        .expect("create first");
    let first_success = authority
        .run_source_paths_attempt(&engine, "first", 0)
        .expect("run first");

    let mut second = make_context(
        &first.change.repository,
        &first.change.id,
        2,
        &first.revision,
        &recipe,
    );
    second.acceptance_criteria = vec!["same source under changed criteria".to_owned()];
    authority
        .update_change_context(1, &second)
        .expect("update context");
    assert_eq!(
        authority
            .create_source_paths_attempt(&first_request)
            .expect("retry retained first attempt after context advance"),
        first_success
    );
    let mut conflicting_retry = first_request;
    conflicting_retry.revision =
        "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".to_owned();
    assert!(matches!(
        authority.create_source_paths_attempt(&conflicting_retry),
        Err(Error::Conflict(_))
    ));
    assert!(
        authority
            .approve(&approval(&second, "stale-evidence"))
            .is_err()
    );
    authority
        .create_source_paths_attempt(&request(&second, recipe, "second"))
        .expect("create second");
    authority
        .run_source_paths_attempt(&engine, "second", 0)
        .expect("run second");
    authority
        .approve(&approval(&second, "fresh-evidence"))
        .expect("fresh approval");
}

#[test]
fn explicit_cancel_and_recover_use_generation_cas() {
    let (temporary, _engine, mut authority, context, recipe) = fixture();
    authority
        .create_source_paths_attempt(&request(&context, recipe.clone(), "cancelled"))
        .expect("create cancelled");
    let cancelled = authority
        .cancel_check_attempt("cancelled", 0, "operator cancelled before execution")
        .expect("cancel");
    assert_eq!(cancelled.state, CheckState::Cancelled);
    assert_eq!(
        authority
            .run_source_paths_attempt(&_engine, "cancelled", 0)
            .expect("terminal retry"),
        cancelled
    );

    authority
        .create_source_paths_attempt(&request(&context, recipe, "orphaned"))
        .expect("create orphaned");
    drop(authority);
    let database = temporary.path().join("authority/state.sqlite");
    let connection = Connection::open(&database).expect("raw connection");
    connection
        .execute(
            "UPDATE check_attempts SET state = 'running', run_generation = 1 WHERE attempt_id = 'orphaned'",
            [],
        )
        .expect("simulate claimed run");
    drop(connection);

    let mut authority = Authority::open(temporary.path().join("authority")).expect("reopen");
    assert_eq!(
        authority
            .check_attempt("orphaned")
            .expect("still running")
            .state,
        CheckState::Running
    );
    let recovered = authority
        .recover_check_attempt("orphaned", 1, "collector restart left outcome unknown")
        .expect("recover");
    assert_eq!(recovered.state, CheckState::Incomplete);
    assert_eq!(recovered.run_generation, 2);
    assert!(matches!(
        authority.recover_check_attempt(
            "orphaned",
            1,
            "collector restart left outcome unknown"
        ),
        Ok(attempt) if attempt == recovered
    ));
    assert!(matches!(
        authority.recover_check_attempt("orphaned", 1, "different recovery claim"),
        Err(Error::Conflict(_))
    ));
}

#[test]
fn concurrent_recovery_wins_over_a_late_builtin_finisher() {
    let temporary = TempDir::new().expect("tempdir");
    let source_path = temporary.path().join("source");
    let authority_path = temporary.path().join("authority");
    let engine = Engine::open(&source_path).expect("engine");
    let bytes = vec![b'x'; 64 * 1024 * 1024];
    let expected_digest = sha256(&bytes);
    let revision = engine
        .checkpoint(
            None,
            &[FileChange {
                path: "large.bin".to_owned(),
                content: Some(bytes),
            }],
        )
        .expect("revision");
    let mut authority = Authority::open(&authority_path).expect("authority");
    let repository = authority.repository_id().expect("repository");
    let recipe = SourcePathsRecipe {
        version: 1,
        expectations: vec![SourcePathExpectation {
            path: "large.bin".to_owned(),
            sha256: expected_digest,
        }],
        limits: CheckLimits {
            max_paths: 1,
            max_total_bytes: 64 * 1024 * 1024,
            max_output_bytes: 4096,
            timeout_ms: 300_000,
        },
    };
    let context = make_context(&repository, "raced", 1, &revision, &recipe);
    authority.start_change(&context).expect("start change");
    authority
        .create_source_paths_attempt(&request(&context, recipe, "raced-attempt"))
        .expect("create attempt");

    let barrier = Arc::new(Barrier::new(2));
    let runner_barrier = Arc::clone(&barrier);
    let runner_path = authority_path.clone();
    let runner_engine = engine.clone();
    let runner = std::thread::spawn(move || {
        let mut authority = Authority::open(runner_path).expect("runner authority");
        runner_barrier.wait();
        authority
            .run_source_paths_attempt(&runner_engine, "raced-attempt", 0)
            .expect("runner terminal state")
    });
    barrier.wait();

    loop {
        let attempt = authority
            .check_attempt("raced-attempt")
            .expect("observe attempt");
        match attempt.state {
            CheckState::Running => break,
            CheckState::Pending => std::thread::yield_now(),
            state => panic!("runner reached {state:?} before recovery observed running"),
        }
    }
    let recovered = authority
        .recover_check_attempt(
            "raced-attempt",
            1,
            "controller lost ownership before observing a result",
        )
        .expect("recover running attempt");
    let late = runner.join().expect("runner thread");
    assert_eq!(late, recovered);
    assert_eq!(late.state, CheckState::Incomplete);
    assert_eq!(late.run_generation, 2);
}

#[test]
fn tampered_evidence_fails_canonical_validation_and_admission() {
    let (temporary, engine, mut authority, context, recipe) = fixture();
    authority
        .create_source_paths_attempt(&request(&context, recipe, "tamper"))
        .expect("create attempt");
    let success = authority
        .run_source_paths_attempt(&engine, "tamper", 0)
        .expect("run attempt");
    let digest = success.evidence_digest.expect("evidence digest");
    drop(authority);

    let database = temporary.path().join("authority/state.sqlite");
    let connection = Connection::open(&database).expect("raw connection");
    connection
        .execute(
            "UPDATE check_evidence SET canonical_json = ?1 WHERE digest = ?2",
            params![br#"{}"#, digest],
        )
        .expect("tamper evidence");
    drop(connection);

    let mut authority = Authority::open(temporary.path().join("authority")).expect("reopen");
    assert!(authority.check_evidence(&digest).is_err());
    assert!(
        authority
            .approve(&approval(&context, "tampered-evidence"))
            .is_err()
    );
}

#[test]
fn unknown_or_mismatched_context_binding_blocks_creation() {
    let (_temporary, _engine, mut authority, context, recipe) = fixture();
    let mut missing = make_context(
        &context.change.repository,
        "missing-evidence",
        1,
        &context.revision,
        &recipe,
    );
    missing.required_evidence.clear();
    authority
        .start_change(&missing)
        .expect("store incomplete context");
    assert!(matches!(
        authority.create_source_paths_attempt(&request(&missing, recipe.clone(), "missing")),
        Err(Error::Invalid(_))
    ));

    let mut unknown = make_context(
        &context.change.repository,
        "unknown-toolchain",
        1,
        &context.revision,
        &recipe,
    );
    unknown.toolchain = InputIdentity::Unknown {};
    authority
        .start_change(&unknown)
        .expect("store unknown context");
    assert!(matches!(
        authority.create_source_paths_attempt(&request(&unknown, recipe, "unknown")),
        Err(Error::Invalid(_))
    ));
}

#[test]
fn schema_v2_upgrades_to_v8_without_losing_change_history() {
    let temporary = TempDir::new().expect("tempdir");
    let path = temporary.path().join("authority");
    let (repository, state) = {
        let mut authority = Authority::open(&path).expect("authority");
        let repository = authority.repository_id().expect("repository");
        let recipe = recipe("README.md", b"bytes");
        let context = make_context(
            &repository,
            "preserved",
            1,
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            &recipe,
        );
        let state = authority.start_change(&context).expect("change");
        (repository, state)
    };
    let connection = Connection::open(path.join("state.sqlite")).expect("database");
    connection
        .execute_batch(
            "ALTER TABLE integration_events DROP COLUMN kind;
             DROP TABLE landing_preparations;
             DROP INDEX check_success_subject;
             DROP TABLE check_evidence;
             DROP TABLE check_attempts;
             DROP INDEX command_success_subject;
             DROP TABLE command_evidence;
             DROP TABLE command_attempts;
             DROP TABLE workflow_executor_revocations;
             DROP TABLE workflow_executor_enrollments;
             DROP TABLE workflow_receipts;
             DROP TABLE workflow_requirements;
             PRAGMA user_version = 2;",
        )
        .expect("make v2 fixture");
    drop(connection);
    let authority = Authority::open(&path).expect("migrate v2");
    assert_eq!(authority.repository_id().expect("repository"), repository);
    assert_eq!(
        authority.change(&state.id).expect("preserved change"),
        state
    );
    let version: i64 = Connection::open(path.join("state.sqlite"))
        .expect("database")
        .pragma_query_value(None, "user_version", |row| row.get(0))
        .expect("version");
    assert_eq!(version, 8);
}

#[test]
fn schema_v1_upgrades_to_v8_and_preserves_legacy_event() {
    let temporary = TempDir::new().expect("tempdir");
    create_v1_database(temporary.path());
    let authority = Authority::open(temporary.path()).expect("migrate v1");
    let events = authority.events(0, 10).expect("events");
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].operation_id, "legacy-operation");
    assert_eq!(events[0].semantics_version, 1);
    let connection = Connection::open(temporary.path().join("state.sqlite")).expect("database");
    let version: i64 = connection
        .pragma_query_value(None, "user_version", |row| row.get(0))
        .expect("version");
    assert_eq!(version, 8);
    let tables: i64 = connection
        .query_row(
            "SELECT count(*) FROM sqlite_master WHERE type = 'table' AND name IN ('check_attempts', 'check_evidence', 'command_attempts', 'command_evidence')",
            [],
            |row| row.get(0),
        )
        .expect("check tables");
    assert_eq!(tables, 4);
}

fn create_v1_database(path: &Path) {
    fs::create_dir_all(path).expect("directory");
    let connection = Connection::open(path.join("state.sqlite")).expect("database");
    connection
        .execute_batch(
            "PRAGMA application_id = 1095975757;
             PRAGMA user_version = 1;
             CREATE TABLE authority_state (singleton INTEGER PRIMARY KEY CHECK (singleton = 1), policy_epoch INTEGER NOT NULL CHECK (policy_epoch >= 0)) STRICT;
             INSERT INTO authority_state(singleton, policy_epoch) VALUES (1, 0);
             CREATE TABLE approvals (approval_id TEXT PRIMARY KEY, target TEXT NOT NULL, revision TEXT NOT NULL, context_digest TEXT NOT NULL, policy_epoch INTEGER NOT NULL CHECK (policy_epoch >= 0), revoked INTEGER NOT NULL DEFAULT 0 CHECK (revoked IN (0, 1)), consumed_event INTEGER UNIQUE, CHECK (length(approval_id) BETWEEN 1 AND 128), CHECK (length(target) BETWEEN 1 AND 128), CHECK (length(revision) = 64), CHECK (length(context_digest) = 64)) STRICT;
             CREATE TABLE targets (name TEXT PRIMARY KEY, revision TEXT NOT NULL, version INTEGER NOT NULL CHECK (version > 0), CHECK (length(name) BETWEEN 1 AND 128), CHECK (length(revision) = 64)) STRICT;
             CREATE TABLE integration_events (sequence INTEGER PRIMARY KEY AUTOINCREMENT, operation_id TEXT NOT NULL UNIQUE, target TEXT NOT NULL, before_revision TEXT, before_version INTEGER, after_revision TEXT NOT NULL, after_version INTEGER NOT NULL CHECK (after_version > 0), approval_id TEXT NOT NULL, context_digest TEXT NOT NULL, policy_epoch INTEGER NOT NULL CHECK (policy_epoch >= 0), CHECK ((before_revision IS NULL) = (before_version IS NULL)), CHECK (before_version IS NULL OR before_version > 0), FOREIGN KEY (approval_id) REFERENCES approvals(approval_id)) STRICT;
             CREATE TABLE operations (operation_id TEXT PRIMARY KEY, request_digest TEXT NOT NULL CHECK (length(request_digest) = 64), event_sequence INTEGER NOT NULL UNIQUE, FOREIGN KEY (event_sequence) REFERENCES integration_events(sequence)) STRICT;",
        )
        .expect("schema");
    let revision = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    let digest = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
    connection.execute("INSERT INTO approvals(approval_id, target, revision, context_digest, policy_epoch, consumed_event) VALUES ('legacy-approval', 'main', ?1, ?2, 0, 1)", params![revision, digest]).expect("approval");
    connection
        .execute(
            "INSERT INTO targets(name, revision, version) VALUES ('main', ?1, 1)",
            [revision],
        )
        .expect("target");
    connection.execute("INSERT INTO integration_events(sequence, operation_id, target, before_revision, before_version, after_revision, after_version, approval_id, context_digest, policy_epoch) VALUES (1, 'legacy-operation', 'main', NULL, NULL, ?1, 1, 'legacy-approval', ?2, 0)", params![revision, digest]).expect("event");
    connection.execute("INSERT INTO operations(operation_id, request_digest, event_sequence) VALUES ('legacy-operation', ?1, 1)", [digest]).expect("operation");
}

#[test]
fn source_paths_check_is_incomplete_for_non_regular_entries() {
    for kind in [EntryKind::Executable, EntryKind::Symlink] {
        let temporary = TempDir::new().expect("tempdir");
        let engine = Engine::open(temporary.path().join("source")).expect("engine");
        let revision = engine
            .checkpoint_entries(
                None,
                &[EntryChange {
                    path: "special".to_owned(),
                    content: Some(b"bytes".to_vec()),
                    kind,
                }],
            )
            .expect("revision");
        let mut authority = Authority::open(temporary.path().join("authority")).expect("authority");
        let repository = authority.repository_id().expect("repository");
        let recipe = recipe("special", b"bytes");
        let context = make_context(&repository, "change-1", 1, &revision, &recipe);
        authority.start_change(&context).expect("start change");
        authority
            .create_source_paths_attempt(&request(&context, recipe, "attempt-1"))
            .expect("create attempt");
        let attempt = authority
            .run_source_paths_attempt(&engine, "attempt-1", 0)
            .expect("run attempt");
        assert_eq!(attempt.state, CheckState::Incomplete);
        let evidence = authority
            .check_evidence(attempt.evidence_digest.as_deref().expect("evidence digest"))
            .expect("evidence");
        assert!(
            evidence.reason.contains("unsupported entry kind: special"),
            "{}",
            evidence.reason
        );
    }
}
