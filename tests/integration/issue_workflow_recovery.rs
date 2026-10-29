//! Typed issue-journal recovery fixtures with exact inventories and quarantine.

use agentscm::authority::Authority;
use agentscm::context::{ChangeId, InputIdentity, ReviewContext};
use agentscm::forge::{Forge, Host};
use agentscm::issue_workflow::recovery::{RecoveryBinding, database_path, validate_recovery};
use agentscm::issue_workflow::*;
use agentscm::local_recovery::{self, AccessGuard, IssueWorkflowRecoveryConfig, RecoveryManifest};
use agentscm::model::{Approval, FileChange};
use agentscm::store::Engine;
use agentscm::{Error, Result};
use rusqlite::{Connection, OpenFlags, params};
use sha2::{Digest, Sha256};
use std::fs;
use std::path::{Path, PathBuf};
use tempfile::TempDir;

const INSTALLATION: &str = "installation-a";
const A: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const B: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
const C: &str = "cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc";
const WAL_REFUSAL: &str = "issue recovery requires DELETE journal mode";

struct Fixture {
    temporary: TempDir,
    data: PathBuf,
    native: String,
    pending: IntakeRecord,
    claim: ClaimRecord,
    acquire: OperationRecord,
}

fn repository() -> RepositoryIdentity {
    RepositoryIdentity {
        forge: Forge::GitHub,
        host: Host::GitHubCom,
        remote_id: 7,
        path: "owner/project".into(),
    }
}
fn request(native: &str, digest: &str) -> IntakeRequest {
    IntakeRequest {
        version: 1,
        repository: repository(),
        accepted_request_sha256: digest.into(),
        native_repository: native.into(),
    }
}
fn candidate() -> IssueCandidate {
    IssueCandidate {
        identity: IssueIdentity {
            repository: repository(),
            remote_id: 41,
            number: 40,
        },
        state: IssueState::Open,
        basis: MatchBasis::ExactRequest,
        request_digest: B.into(),
        related_proposal: None,
    }
}
fn observation(open: Vec<IssueCandidate>) -> IntakeObservation {
    IntakeObservation {
        version: 1,
        repository: repository(),
        open_issues: Coverage::Complete { candidates: open },
        closed_issues: Coverage::Complete { candidates: vec![] },
        proposals: Coverage::Complete { candidates: vec![] },
        native_work: Coverage::Complete { candidates: vec![] },
    }
}

impl Fixture {
    fn new() -> Self {
        let temporary = TempDir::new().unwrap();
        let data = temporary.path().join("data");
        let engine = Engine::open(&data).unwrap();
        let revision = engine
            .checkpoint(
                None,
                &[FileChange {
                    path: "retained.txt".into(),
                    content: Some(b"retained source".to_vec()),
                }],
            )
            .unwrap();
        let mut authority = Authority::open(&data).unwrap();
        let native = authority.repository_id().unwrap();
        let change = ChangeId {
            repository: native.clone(),
            id: "recovery-subject".into(),
        };
        let known = InputIdentity::Digest { sha256: A.into() };
        let context = ReviewContext {
            version: 1,
            change: change.clone(),
            change_version: 1,
            revision: revision.clone(),
            base: None,
            target: "main".into(),
            policy_epoch: 0,
            intent: "retain issue uncertainty and historical claim receipts".into(),
            acceptance_criteria: vec!["typed recovery preserves bound local state".into()],
            scope: vec!["retained.txt".into()],
            exclusions: vec![],
            omissions: vec![],
            dependencies: vec![],
            recipe: known.clone(),
            toolchain: known.clone(),
            environment: known,
            required_evidence: vec![],
            required_capabilities: vec!["agentscm.review-context/v1".into()],
        };
        let started = authority.start_change(&context).unwrap();
        authority
            .approve(&Approval {
                approval_id: "retained-approval".into(),
                target: "main".into(),
                revision,
                context_digest: started.context_digest,
                change: Some(change.clone()),
                change_version: Some(1),
                policy_epoch: 0,
            })
            .unwrap();
        drop(authority);
        drop(engine);
        let mut journal = IssueJournal::open(&data, &native, INSTALLATION).unwrap();
        let first = journal
            .register(
                "register-pending",
                request(&native, A),
                observation(vec![]),
                None,
            )
            .unwrap();
        let pending = journal
            .reserve_create("reserve-pending", &first.outcome.key, 1)
            .unwrap()
            .outcome;
        let selected = candidate();
        let registered = journal
            .register(
                "register-issue",
                request(&native, B),
                observation(vec![selected.clone()]),
                None,
            )
            .unwrap();
        let intent = AcquireIntent {
            intake_generation: 1,
            issue: selected.identity.clone(),
            selected_change: None,
        };
        journal
            .acquire(
                "acquire",
                &registered.outcome.key,
                "worker-1",
                None,
                &intent,
            )
            .unwrap();
        journal
            .mutate_claim(
                "progress",
                &selected.identity,
                "worker-1",
                1,
                ClaimAction::Progress {
                    digest: C.into(),
                    change: Some(change.clone()),
                },
            )
            .unwrap();
        journal
            .mutate_claim(
                "transfer",
                &selected.identity,
                "worker-1",
                2,
                ClaimAction::Transfer {
                    worker_id: "worker-2".into(),
                },
            )
            .unwrap();
        journal
            .mutate_claim(
                "release",
                &selected.identity,
                "worker-2",
                3,
                ClaimAction::Release,
            )
            .unwrap();
        let renewed = AcquireIntent {
            intake_generation: 2,
            issue: selected.identity.clone(),
            selected_change: None,
        };
        let claim = journal
            .acquire(
                "reacquire",
                &registered.outcome.key,
                "worker-3",
                Some(4),
                &renewed,
            )
            .unwrap()
            .outcome;
        // Include a retained negative marker and its typed reassessment receipt.
        assert!(
            journal
                .register(
                    "contradiction",
                    request(&native, B),
                    observation(vec![]),
                    None
                )
                .is_err()
        );
        let conflicted = journal.intake(&registered.outcome.key).unwrap().unwrap();
        assert!(conflicted.assessment_conflicted);
        journal
            .reassess(
                "reassess",
                &registered.outcome.key,
                conflicted.generation,
                request(&native, B),
                observation(vec![selected]),
                None,
            )
            .unwrap();
        let acquire = journal.operation("reacquire").unwrap().unwrap();
        drop(journal);
        Self {
            temporary,
            data,
            native,
            pending,
            claim,
            acquire,
        }
    }
    fn path(&self, name: &str) -> PathBuf {
        self.temporary.path().join(name)
    }
    fn journal(&self) -> PathBuf {
        self.data.join("issue-workflow/state.sqlite")
    }
}

fn exact_invalid<T>(result: Result<T>, expected: &str) {
    match result {
        Err(Error::Invalid(message)) => assert_eq!(message, expected),
        Err(other) => panic!("unexpected rejection: {other}"),
        Ok(_) => panic!("unsupported fixture was accepted"),
    }
}

fn inspect_journal(data: &Path, native: &str) -> Result<RecoveryBinding> {
    let source = database_path(data)?.expect("fixture has issue journal");
    let temporary = TempDir::new().unwrap();
    let copy = temporary.path().join("state.sqlite");
    fs::copy(source, &copy).unwrap();
    let db = Connection::open_with_flags(copy, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    validate_recovery(&db, native, INSTALLATION)
}

fn issue_config(native: &str) -> IssueWorkflowRecoveryConfig {
    IssueWorkflowRecoveryConfig::new(native, INSTALLATION).unwrap()
}

fn native_state(data: &Path) -> (i64, i64, Option<i64>) {
    // Baseline/post-denial observations cannot perturb a sealed inventory or
    // silently ignore writes confined to native WAL.
    assert!(!data.join("state.sqlite-wal").exists());
    assert!(!data.join("state.sqlite-shm").exists());
    let temporary = TempDir::new().unwrap();
    let copy = temporary.path().join("state.sqlite");
    fs::copy(data.join("state.sqlite"), &copy).unwrap();
    let db = Connection::open_with_flags(copy, OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
    db.query_row("SELECT policy_epoch,(SELECT revoked FROM approvals WHERE approval_id='retained-approval'),(SELECT consumed_event FROM approvals WHERE approval_id='retained-approval') FROM authority_state WHERE singleton=1", [],
        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?))).unwrap()
}

fn edit_journal(path: &Path, edit: impl FnOnce(&Connection)) {
    let db = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_WRITE).unwrap();
    db.pragma_update(None, "foreign_keys", "OFF").unwrap();
    edit(&db);
}

fn refresh_entry(root: &Path, manifest: &mut RecoveryManifest) {
    let relative = "data/issue-workflow/state.sqlite";
    let bytes = fs::read(root.join(relative)).unwrap();
    let entry = manifest
        .entries
        .iter_mut()
        .find(|entry| entry.path == relative)
        .unwrap();
    entry.bytes = bytes.len() as u64;
    entry.sha256 = format!("{:x}", Sha256::digest(&bytes));
}

fn convert_to_wal(path: &Path) {
    edit_journal(path, |db| {
        let mode: String = db
            .query_row("PRAGMA journal_mode=WAL", [], |row| row.get(0))
            .unwrap();
        assert_eq!(mode.to_ascii_lowercase(), "wal");
        db.pragma_update(None, "synchronous", "FULL").unwrap();
        let checkpoint: (i64, i64, i64) = db
            .query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |row| {
                Ok((row.get(0)?, row.get(1)?, row.get(2)?))
            })
            .unwrap();
        assert_eq!(checkpoint.0, 0);
    });
    assert_eq!(&fs::read(path).unwrap()[18..20], &[2, 2]);
    assert!(!path.with_file_name("state.sqlite-wal").exists());
    assert!(!path.with_file_name("state.sqlite-shm").exists());
}

#[test]
fn real_delete_journal_validates_every_current_and_historical_record_without_writes() {
    let fixture = Fixture::new();
    let before = fs::read(fixture.journal()).unwrap();
    let binding = inspect_journal(&fixture.data, &fixture.native).unwrap();
    assert_eq!(
        binding,
        RecoveryBinding {
            native_repository: fixture.native.clone(),
            installation_id: INSTALLATION.into()
        }
    );
    assert_eq!(fs::read(fixture.journal()).unwrap(), before);
    assert_eq!(
        fs::read_dir(fixture.data.join("issue-workflow"))
            .unwrap()
            .count(),
        1
    );
}

#[test]
fn delete_backup_verify_restore_quarantine_activation_preserves_issue_state() {
    let fixture = Fixture::new();
    let backup = fixture.path("backup");
    let config = issue_config(&fixture.native);
    let manifest =
        local_recovery::backup_with_issue_config(&fixture.data, &backup, &[], &[], true, &config)
            .unwrap();
    assert_eq!(
        local_recovery::verify_with_issue_config(&backup, &config).unwrap(),
        manifest
    );
    let restored = fixture.path("restored");
    local_recovery::restore_with_issue_config(&backup, &restored, &config).unwrap();
    let data = restored.join("data");
    let before = fs::read(data.join("issue-workflow/state.sqlite")).unwrap();
    assert_eq!(native_state(&data), (0, 0, None));
    assert!(IssueJournal::open(&data, &fixture.native, INSTALLATION).is_err());
    assert_eq!(
        fs::read(data.join("issue-workflow/state.sqlite")).unwrap(),
        before
    );
    assert!(
        restored
            .join(".agentscm-recovery/quarantine.json")
            .is_file()
    );
    assert_eq!(
        local_recovery::activate_with_issue_config(&restored, true, true, &config).unwrap(),
        1
    );
    assert_eq!(native_state(&data), (1, 1, None));
    assert!(!restored.join(".agentscm-recovery/quarantine.json").exists());
    assert_eq!(
        inspect_journal(&data, &fixture.native)
            .unwrap()
            .installation_id,
        INSTALLATION
    );
    let mut journal = IssueJournal::open(&data, &fixture.native, INSTALLATION).unwrap();
    assert_eq!(
        journal.intake(&fixture.pending.key).unwrap(),
        Some(fixture.pending.clone())
    );
    assert_eq!(
        journal.claim(&fixture.claim.issue).unwrap(),
        Some(fixture.claim.clone())
    );
    assert_eq!(
        journal.operation("reacquire").unwrap(),
        Some(fixture.acquire.clone())
    );
    assert!(
        journal
            .reserve_create(
                "new-create",
                &fixture.pending.key,
                fixture.pending.generation
            )
            .is_err()
    );
    let replay = journal
        .reserve_create("reserve-pending", &fixture.pending.key, 1)
        .unwrap();
    assert!(replay.replay);
    assert_eq!(replay.outcome, fixture.pending);
    // A retained local claim is not proof of an external worker's freshness.
    assert!(!replay.current_fence);
}

#[test]
fn legacy_recovery_entrypoints_refuse_configured_issue_namespace_at_every_phase() {
    let fixture = Fixture::new();
    let config = issue_config(&fixture.native);
    let backup = fixture.path("configured-backup");
    exact_invalid(
        local_recovery::backup(
            &fixture.data,
            &fixture.path("legacy-backup"),
            &[],
            &[],
            true,
        ),
        "issue-workflow recovery is unsupported until a typed journal adapter is available",
    );
    local_recovery::backup_with_issue_config(&fixture.data, &backup, &[], &[], true, &config)
        .unwrap();
    exact_invalid(
        local_recovery::verify(&backup),
        "issue-workflow recovery is unsupported until a typed journal adapter is available",
    );
    let refused_restore = fixture.path("legacy-restore");
    exact_invalid(
        local_recovery::restore(&backup, &refused_restore),
        "issue-workflow recovery is unsupported until a typed journal adapter is available",
    );
    assert!(!refused_restore.exists());
    let restored = fixture.path("configured-restore");
    local_recovery::restore_with_issue_config(&backup, &restored, &config).unwrap();
    exact_invalid(
        local_recovery::activate(&restored, true, true),
        "issue-workflow recovery is unsupported until a typed journal adapter is available",
    );
    assert!(
        restored
            .join(".agentscm-recovery/quarantine.json")
            .is_file()
    );
}

#[test]
fn configured_identity_mismatch_refuses_before_output_or_activation_mutation() {
    let fixture = Fixture::new();
    let wrong_native = IssueWorkflowRecoveryConfig::new(&"f".repeat(32), INSTALLATION).unwrap();
    let wrong_installation =
        IssueWorkflowRecoveryConfig::new(&fixture.native, "another-installation").unwrap();
    let output = fixture.path("wrong-native-backup");
    exact_invalid(
        local_recovery::backup_with_issue_config(
            &fixture.data,
            &output,
            &[],
            &[],
            true,
            &wrong_native,
        ),
        "configured issue recovery repository does not match authority",
    );
    assert!(!output.exists());
    let output = fixture.path("wrong-installation-backup");
    exact_invalid(
        local_recovery::backup_with_issue_config(
            &fixture.data,
            &output,
            &[],
            &[],
            true,
            &wrong_installation,
        ),
        "issue recovery configured installation mismatch",
    );
    assert!(!output.exists());

    let config = issue_config(&fixture.native);
    let backup = fixture.path("backup");
    local_recovery::backup_with_issue_config(&fixture.data, &backup, &[], &[], true, &config)
        .unwrap();
    let restored = fixture.path("restored");
    local_recovery::restore_with_issue_config(&backup, &restored, &config).unwrap();
    let authority_before = fs::read(restored.join("data/state.sqlite")).unwrap();
    let quarantine_before = fs::read(restored.join(".agentscm-recovery/quarantine.json")).unwrap();
    exact_invalid(
        local_recovery::activate_with_issue_config(&restored, true, true, &wrong_installation),
        "issue recovery configured installation mismatch",
    );
    assert_eq!(
        fs::read(restored.join("data/state.sqlite")).unwrap(),
        authority_before
    );
    assert_eq!(
        fs::read(restored.join(".agentscm-recovery/quarantine.json")).unwrap(),
        quarantine_before,
    );
}

#[test]
fn active_issue_handle_refuses_exclusive_lease_and_backup_before_output() {
    let fixture = Fixture::new();
    let journal = IssueJournal::open(&fixture.data, &fixture.native, INSTALLATION).unwrap();
    assert!(AccessGuard::exclusive(&fixture.data).is_err());
    let output = fixture.path("active-backup");
    assert!(
        local_recovery::backup_with_issue_config(
            &fixture.data,
            &output,
            &[],
            &[],
            true,
            &issue_config(&fixture.native),
        )
        .is_err()
    );
    assert!(!output.exists());
    drop(journal);
    assert!(AccessGuard::exclusive(&fixture.data).is_ok());
    let config = issue_config(&fixture.native);
    local_recovery::backup_with_issue_config(&fixture.data, &output, &[], &[], true, &config)
        .unwrap();
}

#[test]
fn real_typed_wal_profile_is_refused_before_backup_publication() {
    let fixture = Fixture::new();
    inspect_journal(&fixture.data, &fixture.native).unwrap();
    convert_to_wal(&fixture.journal());
    assert!(IssueJournal::open(&fixture.data, &fixture.native, INSTALLATION).is_err());
    exact_invalid(database_path(&fixture.data), WAL_REFUSAL);
    let output = fixture.path("wal-backup");
    exact_invalid(
        local_recovery::backup_with_issue_config(
            &fixture.data,
            &output,
            &[],
            &[],
            true,
            &issue_config(&fixture.native),
        ),
        WAL_REFUSAL,
    );
    assert!(!output.exists());
    // Direct typed validation also rejects the persistent WAL header rather
    // than relying only on what PRAGMA journal_mode reports to a reader.
    let db =
        Connection::open_with_flags(fixture.journal(), OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
    exact_invalid(
        validate_recovery(&db, &fixture.native, INSTALLATION),
        WAL_REFUSAL,
    );
}

#[test]
fn correctly_inventoried_legacy_wal_backup_cannot_verify_or_create_restore() {
    let fixture = Fixture::new();
    let backup = fixture.path("backup");
    let config = issue_config(&fixture.native);
    let mut manifest =
        local_recovery::backup_with_issue_config(&fixture.data, &backup, &[], &[], true, &config)
            .unwrap();
    let payload = backup.join("payload");
    convert_to_wal(&payload.join("data/issue-workflow/state.sqlite"));
    refresh_entry(&payload, &mut manifest);
    fs::write(
        backup.join("recovery-manifest.json"),
        serde_json::to_vec(&manifest).unwrap(),
    )
    .unwrap();
    exact_invalid(
        local_recovery::verify_with_issue_config(&backup, &config),
        WAL_REFUSAL,
    );
    let destination = fixture.path("must-not-exist");
    exact_invalid(
        local_recovery::restore_with_issue_config(&backup, &destination, &config),
        WAL_REFUSAL,
    );
    assert!(!destination.exists());
}

fn denied_activation_preserves_authority(
    fixture: &Fixture,
    edit: impl FnOnce(&Path),
    expected: &str,
) {
    let backup = fixture.path("backup");
    let config = issue_config(&fixture.native);
    local_recovery::backup_with_issue_config(&fixture.data, &backup, &[], &[], true, &config)
        .unwrap();
    let restored = fixture.path("restored");
    local_recovery::restore_with_issue_config(&backup, &restored, &config).unwrap();
    let data = restored.join("data");
    let complete = restored.join(".agentscm-recovery/restore-complete.json");
    let mut manifest: RecoveryManifest =
        serde_json::from_slice(&fs::read(&complete).unwrap()).unwrap();
    edit(&data.join("issue-workflow/state.sqlite"));
    refresh_entry(&restored, &mut manifest);
    fs::write(&complete, serde_json::to_vec(&manifest).unwrap()).unwrap();
    let quarantine = restored.join(".agentscm-recovery/quarantine.json");
    let marker_before = fs::read(&quarantine).unwrap();
    let complete_before = fs::read(&complete).unwrap();
    let authority_before = fs::read(data.join("state.sqlite")).unwrap();
    let journal_before = fs::read(data.join("issue-workflow/state.sqlite")).unwrap();
    let state_before = native_state(&data);
    assert_eq!(state_before, (0, 0, None));
    exact_invalid(
        local_recovery::activate_with_issue_config(&restored, true, true, &config),
        expected,
    );
    assert_eq!(native_state(&data), state_before);
    assert_eq!(
        fs::read(data.join("state.sqlite")).unwrap(),
        authority_before
    );
    assert_eq!(
        fs::read(data.join("issue-workflow/state.sqlite")).unwrap(),
        journal_before
    );
    assert_eq!(fs::read(&quarantine).unwrap(), marker_before);
    assert_eq!(fs::read(&complete).unwrap(), complete_before);
    assert!(IssueJournal::open(&data, &fixture.native, INSTALLATION).is_err());
    assert!(Authority::open(&data).is_err());
}

#[test]
fn matching_quarantined_wal_restore_cannot_activate_or_change_authority() {
    let fixture = Fixture::new();
    denied_activation_preserves_authority(&fixture, convert_to_wal, WAL_REFUSAL);
}

#[test]
fn matching_quarantined_typed_corruption_cannot_activate_or_change_authority() {
    let fixture = Fixture::new();
    denied_activation_preserves_authority(
        &fixture,
        |path| {
            edit_journal(path, |db| {
                db.execute(
                    "UPDATE operations SET outcome_digest=?1 WHERE id='reacquire'",
                    [A],
                )
                .unwrap();
            })
        },
        "operation outcome consistency mismatch",
    );
}

#[test]
fn recognized_identity_without_required_relations_or_exact_schema_is_refused() {
    for case in ["future", "foreign", "missing", "changed", "extra"] {
        let fixture = Fixture::new();
        edit_journal(&fixture.journal(), |db| match case {
            "future" => db.pragma_update(None, "user_version", 2).unwrap(),
            "foreign" => db.pragma_update(None, "application_id", 123).unwrap(),
            "missing" => db.execute_batch("DROP TABLE operations").unwrap(),
            "changed" => db
                .execute_batch("ALTER TABLE claims RENAME COLUMN worker_id TO renamed_worker")
                .unwrap(),
            "extra" => db
                .execute_batch("CREATE VIEW hidden_state AS SELECT * FROM metadata")
                .unwrap(),
            _ => unreachable!(),
        });
        assert!(
            inspect_journal(&fixture.data, &fixture.native).is_err(),
            "{case}"
        );
        let output = fixture.path("rejected-backup");
        assert!(
            local_recovery::backup_with_issue_config(
                &fixture.data,
                &output,
                &[],
                &[],
                true,
                &issue_config(&fixture.native),
            )
            .is_err(),
            "{case}"
        );
        assert!(!output.exists());
    }
}

#[test]
fn metadata_and_native_bindings_are_checked_without_installation_authentication_claim() {
    for case in [
        "native",
        "installation",
        "identifier",
        "oversize",
        "missing",
    ] {
        let fixture = Fixture::new();
        edit_journal(&fixture.journal(), |db| match case {
            "native" => {
                db.execute("UPDATE metadata SET native_repository=?1", ["f".repeat(32)])
                    .unwrap();
            }
            "installation" => {
                db.execute(
                    "UPDATE metadata SET installation_id='another-valid-installation'",
                    [],
                )
                .unwrap();
            }
            "identifier" => {
                db.execute(
                    "UPDATE metadata SET installation_id='invalid installation'",
                    [],
                )
                .unwrap();
            }
            "oversize" => {
                db.execute("UPDATE metadata SET installation_id=?1", ["x".repeat(129)])
                    .unwrap();
            }
            "missing" => {
                db.execute("DELETE FROM metadata", []).unwrap();
            }
            _ => unreachable!(),
        });
        assert!(
            inspect_journal(&fixture.data, &fixture.native).is_err(),
            "{case}"
        );
    }
    let fixture = Fixture::new();
    let foreign = "f".repeat(32);
    assert!(inspect_journal(&fixture.data, &foreign).is_err());
    // An installation comparison is meaningful only with an explicit configured
    // caller value; a retained ID must never supply its own expected binding.
}

#[test]
fn envelope_scalar_and_receipt_corruption_are_not_accepted_as_generic_sqlite() {
    for case in ["envelope", "key", "scalar", "receipt", "relationship"] {
        let fixture = Fixture::new();
        edit_journal(&fixture.journal(), |db| {
            db.pragma_update(None, "ignore_check_constraints", "ON")
                .unwrap();
            match case {
                "envelope" => {
                    db.execute("UPDATE claims SET record_json=zeroblob(16385)", [])
                        .unwrap();
                }
                "key" => {
                    db.execute(
                        "UPDATE operations SET id=?1 WHERE id='progress'",
                        ["x".repeat(129)],
                    )
                    .unwrap();
                }
                "scalar" => {
                    db.execute("UPDATE claims SET generation=generation+1", [])
                        .unwrap();
                }
                "receipt" => {
                    db.execute(
                        "UPDATE operations SET outcome_digest=?1 WHERE id='progress'",
                        [A],
                    )
                    .unwrap();
                }
                "relationship" => {
                    db.execute("DELETE FROM operations WHERE id='reserve-pending'", [])
                        .unwrap();
                }
                _ => unreachable!(),
            }
        });
        if matches!(case, "envelope" | "key") {
            exact_invalid(
                inspect_journal(&fixture.data, &fixture.native),
                "issue recovery row bounds exceeded",
            );
        } else {
            assert!(
                inspect_journal(&fixture.data, &fixture.native).is_err(),
                "{case}"
            );
        }
        let output = fixture.path("invalid-backup");
        assert!(
            local_recovery::backup_with_issue_config(
                &fixture.data,
                &output,
                &[],
                &[],
                true,
                &issue_config(&fixture.native),
            )
            .is_err(),
            "{case}"
        );
        assert!(!output.exists());
    }
}

fn receipt_checksum(record: &OperationRecord, bytes: &[u8]) -> String {
    let scope = serde_json::to_vec(&(
        &record.id,
        &record.kind,
        &record.intake_key,
        &record.issue_key,
        record.expected_generation,
        &record.request_digest,
        record.expected_intake_generation,
        &record.selected_change,
    ))
    .unwrap();
    let mut digest = Sha256::new();
    for part in [
        b"agentscm.issue-outcome.v1".as_slice(),
        scope.as_slice(),
        bytes,
    ] {
        digest.update((part.len() as u64).to_be_bytes());
        digest.update(part);
    }
    format!("{:x}", digest.finalize())
}

#[test]
fn aggregate_bound_accepts_4096_consistent_typed_rows_and_refuses_one_more() {
    let fixture = Fixture::new();
    let journal = IssueJournal::open(&fixture.data, &fixture.native, INSTALLATION).unwrap();
    let template = journal.operation("register-pending").unwrap().unwrap();
    drop(journal);
    let mut db =
        Connection::open_with_flags(fixture.journal(), OpenFlags::SQLITE_OPEN_READ_WRITE).unwrap();
    let bytes: Vec<u8> = db
        .query_row(
            "SELECT outcome_json FROM operations WHERE id='register-pending'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    let count: i64 = db.query_row("SELECT (SELECT count(*) FROM intakes)+(SELECT count(*) FROM claims)+(SELECT count(*) FROM operations)", [], |row| row.get(0)).unwrap();
    let tx = db.transaction().unwrap();
    // Synthetic history rows use exact retained outcome bytes and recomputed
    // consistency checksums; this is a parsing/cap fixture, not execution proof.
    for index in count..4096 {
        let mut record = template.clone();
        record.id = format!("retained-{index}");
        let checksum = receipt_checksum(&record, &bytes);
        tx.execute("INSERT INTO operations SELECT ?1,kind,intake_key,issue_key,expected_generation,request_digest,expected_intake_generation,selected_change_repository,selected_change_id,?2,outcome_json FROM operations WHERE id='register-pending'",
            params![record.id, checksum]).unwrap();
    }
    tx.commit().unwrap();
    drop(db);
    inspect_journal(&fixture.data, &fixture.native).unwrap();
    let mut final_record = template;
    final_record.id = "one-too-many".into();
    edit_journal(&fixture.journal(), |db| {
        db.execute("INSERT INTO operations SELECT ?1,kind,intake_key,issue_key,expected_generation,request_digest,expected_intake_generation,selected_change_repository,selected_change_id,?2,outcome_json FROM operations WHERE id='register-pending'",
            params![final_record.id, receipt_checksum(&final_record, &bytes)]).unwrap();
    });
    exact_invalid(
        inspect_journal(&fixture.data, &fixture.native),
        "issue recovery aggregate row cap exceeded",
    );
    let output = fixture.path("over-cap-backup");
    exact_invalid(
        local_recovery::backup_with_issue_config(
            &fixture.data,
            &output,
            &[],
            &[],
            true,
            &issue_config(&fixture.native),
        ),
        "issue recovery aggregate row cap exceeded",
    );
    assert!(!output.exists());
}

#[test]
fn invalid_namespace_and_orphan_companions_refuse_before_output() {
    for entry in ["empty", "file", "wal", "shm", "rollback", "extra"] {
        let fixture = Fixture::new();
        let directory = fixture.data.join("issue-workflow");
        if entry != "extra" {
            fs::remove_file(fixture.journal()).unwrap();
        }
        match entry {
            "empty" => {}
            "file" => {
                fs::remove_dir(&directory).unwrap();
                fs::write(&directory, b"reserved").unwrap();
            }
            "wal" => fs::write(directory.join("state.sqlite-wal"), b"orphan").unwrap(),
            "shm" => fs::write(directory.join("state.sqlite-shm"), b"orphan").unwrap(),
            "rollback" => fs::write(directory.join("state.sqlite-journal"), b"orphan").unwrap(),
            "extra" => fs::write(directory.join("unrecognized.sqlite"), b"unknown").unwrap(),
            _ => unreachable!(),
        }
        assert!(database_path(&fixture.data).is_err(), "{entry}");
        let output = fixture.path("namespace-backup");
        assert!(
            local_recovery::backup_with_issue_config(
                &fixture.data,
                &output,
                &[],
                &[],
                true,
                &issue_config(&fixture.native),
            )
            .is_err(),
            "{entry}"
        );
        assert!(!output.exists());
    }
}

#[cfg(unix)]
#[test]
fn linked_namespace_or_main_file_never_reads_or_changes_external_target() {
    for link_main in [false, true] {
        let fixture = Fixture::new();
        let external = fixture.path("external-state");
        fs::copy(fixture.journal(), &external).unwrap();
        let before = fs::read(&external).unwrap();
        fs::remove_file(fixture.journal()).unwrap();
        if link_main {
            std::os::unix::fs::symlink(&external, fixture.journal()).unwrap();
        } else {
            let directory = fixture.data.join("issue-workflow");
            fs::remove_dir(&directory).unwrap();
            // A dangling namespace link is rejected by lstat, not followed.
            std::os::unix::fs::symlink(fixture.path("absent-target"), &directory).unwrap();
        }
        assert!(database_path(&fixture.data).is_err());
        let output = fixture.path("link-backup");
        assert!(
            local_recovery::backup_with_issue_config(
                &fixture.data,
                &output,
                &[],
                &[],
                true,
                &issue_config(&fixture.native),
            )
            .is_err()
        );
        assert!(!output.exists());
        assert_eq!(fs::read(&external).unwrap(), before);
        assert!(!fixture.path("absent-target").exists());
    }
}

#[test]
fn truncated_main_database_is_refused_without_publication_or_reinitialization() {
    let fixture = Fixture::new();
    fs::OpenOptions::new()
        .write(true)
        .open(fixture.journal())
        .unwrap()
        .set_len(16)
        .unwrap();
    let before = fs::read(fixture.journal()).unwrap();
    assert!(database_path(&fixture.data).is_err());
    let output = fixture.path("truncated-backup");
    assert!(
        local_recovery::backup_with_issue_config(
            &fixture.data,
            &output,
            &[],
            &[],
            true,
            &issue_config(&fixture.native),
        )
        .is_err()
    );
    assert!(!output.exists());
    assert_eq!(fs::read(fixture.journal()).unwrap(), before);
}

#[test]
fn configured_installation_mismatch_is_rejected_even_for_an_empty_journal() {
    let temporary = TempDir::new().unwrap();
    let data = temporary.path().join("data");
    let authority = Authority::open(&data).unwrap();
    let native = authority.repository_id().unwrap();
    drop(authority);
    drop(IssueJournal::open(&data, &native, INSTALLATION).unwrap());
    edit_journal(&data.join("issue-workflow/state.sqlite"), |db| {
        db.execute(
            "UPDATE metadata SET installation_id='another-valid-installation'",
            [],
        )
        .unwrap();
    });
    exact_invalid(
        inspect_journal(&data, &native),
        "issue recovery configured installation mismatch",
    );
    let copy_root = TempDir::new().unwrap();
    let copy = copy_root.path().join("state.sqlite");
    fs::copy(data.join("issue-workflow/state.sqlite"), &copy).unwrap();
    let db = Connection::open_with_flags(copy, OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
    assert_eq!(
        validate_recovery(&db, &native, "another-valid-installation")
            .unwrap()
            .installation_id,
        "another-valid-installation"
    );
    assert!(IssueJournal::open(&data, &native, INSTALLATION).is_err());
}

#[test]
fn recovery_validation_does_not_reconfigure_or_upgrade_its_connection() {
    let fixture = Fixture::new();
    let temporary = TempDir::new().unwrap();
    let path = temporary.path().join("state.sqlite");
    fs::copy(fixture.journal(), &path).unwrap();
    let before = fs::read(&path).unwrap();
    let db = Connection::open_with_flags(&path, OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
    db.pragma_update(None, "foreign_keys", "OFF").unwrap();
    validate_recovery(&db, &fixture.native, INSTALLATION).unwrap();
    assert_eq!(
        db.pragma_query_value(None, "foreign_keys", |row| row.get::<_, i64>(0))
            .unwrap(),
        0
    );
    db.pragma_update(None, "synchronous", "OFF").unwrap();
    exact_invalid(
        validate_recovery(&db, &fixture.native, INSTALLATION),
        "issue recovery requires DELETE/FULL",
    );
    assert_eq!(
        db.pragma_query_value(None, "synchronous", |row| row.get::<_, i64>(0))
            .unwrap(),
        0
    );
    assert_eq!(fs::read(&path).unwrap(), before);
}

#[test]
fn physical_page_corruption_uses_existing_sqlite_integrity_refusal() {
    let fixture = Fixture::new();
    inspect_journal(&fixture.data, &fixture.native).unwrap();
    let mut bytes = fs::read(fixture.journal()).unwrap();
    assert!(bytes.len() > 100);
    // Damage the first schema B-tree page while retaining the SQLite header,
    // app/schema fields and DELETE read/write-format bytes.
    bytes[100] ^= 0xff;
    fs::write(fixture.journal(), &bytes).unwrap();
    assert!(database_path(&fixture.data).is_ok());
    assert!(inspect_journal(&fixture.data, &fixture.native).is_err());
    let output = fixture.path("corrupt-backup");
    assert!(
        local_recovery::backup_with_issue_config(
            &fixture.data,
            &output,
            &[],
            &[],
            true,
            &issue_config(&fixture.native),
        )
        .is_err()
    );
    assert!(!output.exists());
    assert_eq!(fs::read(fixture.journal()).unwrap(), bytes);
}
