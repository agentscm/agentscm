//! Test-binary-only pauses at durable recovery boundaries; never in the CLI.
use super::*;
use crate::authority::Authority;
use crate::context::{BaseState, ChangeId, InputIdentity, ReviewContext};
use crate::model::{Approval, FileChange, IntegrateRequest, IntegrationEvent, TargetState};
use crate::store::Engine;
use std::os::unix::process::ExitStatusExt;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};
use tempfile::TempDir;

pub(super) fn boundary(stage: &str, destination: &Path) -> Result<()> {
    if std::env::var("AGENTSCM_RECOVERY_CRASH_STAGE").as_deref() != Ok(stage) {
        return Ok(());
    }
    let expected = std::env::var_os("AGENTSCM_RECOVERY_CRASH_DESTINATION")
        .ok_or_else(|| Error::Invalid("test destination missing".into()))?;
    if destination != Path::new(&expected) {
        return Err(Error::Invalid("test destination mismatch".into()));
    }
    let ready = std::env::var_os("AGENTSCM_RECOVERY_CRASH_READY")
        .ok_or_else(|| Error::Invalid("test ready path missing".into()))?;
    write_new(Path::new(&ready), stage.as_bytes())?;
    let deadline = Instant::now() + Duration::from_secs(20);
    while Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(10));
    }
    Err(Error::Limit("test boundary pause expired".into()))
}

struct Fixture {
    data: PathBuf,
    backup: PathBuf,
    before: IntegrateRequest,
    after: IntegrateRequest,
    before_event: IntegrationEvent,
    after_event: IntegrationEvent,
}

fn request(
    authority: &mut Authority,
    revision: String,
    base: Option<TargetState>,
    id: &str,
) -> IntegrateRequest {
    let known = InputIdentity::Digest {
        sha256: "a".repeat(64),
    };
    let context = ReviewContext {
        version: 1,
        change: ChangeId {
            repository: authority.repository_id().unwrap(),
            id: id.into(),
        },
        change_version: 1,
        revision: revision.clone(),
        base: base.as_ref().map(|base| BaseState {
            revision: base.revision.clone(),
            target_version: base.version,
        }),
        target: "main".into(),
        policy_epoch: 0,
        intent: "Exercise native recovery outcomes".into(),
        acceptance_criteria: vec!["Retain exact outcomes without inferring missing ones".into()],
        scope: vec!["file.txt".into()],
        exclusions: vec![],
        omissions: vec![],
        dependencies: vec![],
        recipe: known.clone(),
        toolchain: known.clone(),
        environment: known,
        required_evidence: vec![],
        required_capabilities: vec!["agentscm.review-context/v1".into()],
    };
    let change = authority.start_change(&context).unwrap();
    let approval = Approval {
        approval_id: format!("approval-{id}"),
        target: "main".into(),
        revision,
        context_digest: change.context_digest,
        change: Some(change.id),
        change_version: Some(1),
        policy_epoch: 0,
    };
    authority.approve(&approval).unwrap();
    IntegrateRequest {
        operation_id: id.into(),
        target: approval.target,
        expected_target: base,
        revision: approval.revision,
        context_digest: approval.context_digest,
        approval_id: approval.approval_id,
        change: approval.change,
        change_version: approval.change_version,
        policy_epoch: 0,
    }
}

fn fixture(root: &Path) -> Fixture {
    let data = root.join("original");
    let engine = Engine::open(&data).unwrap();
    let first = engine
        .checkpoint(
            None,
            &[FileChange {
                path: "file.txt".into(),
                content: Some(b"before".to_vec()),
            }],
        )
        .unwrap();
    let mut authority = Authority::open(&data).unwrap();
    let before = request(&mut authority, first.clone(), None, "before");
    let before_event = authority.integrate(&engine, &before).unwrap();
    let second = engine
        .checkpoint(
            Some(&first),
            &[FileChange {
                path: "file.txt".into(),
                content: Some(b"after".to_vec()),
            }],
        )
        .unwrap();
    let after = request(
        &mut authority,
        second,
        Some(before_event.after.clone()),
        "after",
    );
    drop(authority);
    drop(engine);
    let backup = root.join("backup");
    super::backup(&data, &backup, &[], &[], true).unwrap();
    // This acknowledged result is deliberately later than the backup. All old
    // handles close before restore activation; no original writer runs below.
    let after_event = {
        let engine = Engine::open(&data).unwrap();
        Authority::open(&data)
            .unwrap()
            .integrate(&engine, &after)
            .unwrap()
    };
    Fixture {
        data,
        backup,
        before,
        after,
        before_event,
        after_event,
    }
}

struct RecoveryChild(Option<Child>);
impl Drop for RecoveryChild {
    fn drop(&mut self) {
        if let Some(mut child) = self.0.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

fn interrupt_at(stage: &str, root: &Path, backup: &Path, destination: &Path) {
    let ready = root.join("ready");
    let stdout_path = root.join("child.stdout");
    let stderr_path = root.join("child.stderr");
    let mut owned = RecoveryChild(Some(
        Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "local_recovery::crash_tests::recovery_crash_child",
                "--nocapture",
            ])
            .env_clear()
            .env("AGENTSCM_RECOVERY_CRASH_STAGE", stage)
            .env("AGENTSCM_RECOVERY_CRASH_DESTINATION", destination)
            .env("AGENTSCM_RECOVERY_CRASH_BACKUP", backup)
            .env("AGENTSCM_RECOVERY_CRASH_READY", &ready)
            .stdin(Stdio::null())
            .stdout(File::create(&stdout_path).unwrap())
            .stderr(File::create(&stderr_path).unwrap())
            .spawn()
            .unwrap(),
    ));
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        assert!(fs::metadata(&stdout_path).unwrap().len() <= 64 * 1024);
        assert!(fs::metadata(&stderr_path).unwrap().len() <= 64 * 1024);
        if matches!(fs::read(&ready), Ok(bytes) if bytes == stage.as_bytes()) {
            break;
        }
        assert!(
            owned.0.as_mut().unwrap().try_wait().unwrap().is_none(),
            "child exited before {stage}: {}",
            fs::read_to_string(&stderr_path).unwrap()
        );
        assert!(Instant::now() < deadline, "boundary not reached: {stage}");
        std::thread::sleep(Duration::from_millis(10));
    }
    let child = owned.0.as_mut().unwrap();
    child.kill().unwrap();
    let status = child.wait().unwrap();
    drop(owned.0.take());
    assert!(fs::metadata(&stdout_path).unwrap().len() <= 64 * 1024);
    assert!(fs::metadata(&stderr_path).unwrap().len() <= 64 * 1024);
    assert_eq!(status.signal(), Some(9), "not SIGKILL at {stage}");
}

fn epoch_and_revocation(destination: &Path) -> (u64, bool) {
    // A test observation only: normal constructors must reject this quarantined
    // data. Read committed WAL state to distinguish pre/post transaction kill.
    let db = Connection::open_with_flags(
        destination.join("data/state.sqlite"),
        OpenFlags::SQLITE_OPEN_READ_ONLY,
    )
    .unwrap();
    db.query_row("SELECT policy_epoch,(SELECT revoked FROM approvals WHERE approval_id='approval-after') FROM authority_state WHERE singleton=1",
        [], |row| Ok((row.get(0)?, row.get(1)?))).unwrap()
}

fn assert_released_outcomes(fixture: &Fixture, destination: &Path) {
    assert!(!destination.join(QUARANTINE).exists());
    assert_eq!(epoch_and_revocation(destination), (1, true));
    let engine = Engine::open(destination.join("data")).unwrap();
    let mut authority = Authority::open(destination.join("data")).unwrap();
    let events = authority.events(0, 10).unwrap();
    assert_eq!(events, vec![fixture.before_event.clone()]);
    assert_eq!(
        authority.integrate(&engine, &fixture.before).unwrap(),
        fixture.before_event
    );
    let mut changed = fixture.before.clone();
    changed.policy_epoch = 1;
    assert!(authority.integrate(&engine, &changed).is_err());
    // Its absence cannot mean "never ran": the original acknowledged it.
    assert_eq!(fixture.after_event.operation_id, fixture.after.operation_id);
    assert!(authority.integrate(&engine, &fixture.after).is_err());
    assert_eq!(authority.events(0, 10).unwrap(), events);
    assert_eq!(
        authority.target("main").unwrap(),
        Some(fixture.before_event.after.clone())
    );
}

#[test]
fn recovery_crash_boundaries_preserve_quarantine_and_outcome_uncertainty() {
    for stage in [
        "restore_quarantine",
        "restore_verified",
        "restore_complete",
        "activation_before_commit",
        "activation_committed",
        "activation_released",
    ] {
        let root = TempDir::new().unwrap();
        // Exercise a symlinked temporary parent, as on macOS /var. Derive all
        // paths from one canonical root, matching restore's destination rules.
        let owned = root.path().join("owned");
        fs::create_dir(&owned).unwrap();
        let alias = root.path().join("alias");
        std::os::unix::fs::symlink(&owned, &alias).unwrap();
        let canonical_root = fs::canonicalize(&alias).unwrap();
        let fixture = fixture(&canonical_root);
        let destination = canonical_root.join("restored");
        if stage.starts_with("activation_") {
            restore(&fixture.backup, &destination).unwrap();
        }
        interrupt_at(stage, &canonical_root, &fixture.backup, &destination);
        if stage == "activation_released" {
            assert_released_outcomes(&fixture, &destination);
        } else {
            assert!(destination.join(QUARANTINE).exists());
            assert!(Engine::open(destination.join("data")).is_err());
            assert!(Authority::open(destination.join("data")).is_err());
            assert!(activate(&destination, false, true).is_err());
            assert!(activate(&destination, true, false).is_err());
            match stage {
                "restore_quarantine" => {
                    assert!(!destination.join("data").exists());
                    assert!(activate(&destination, true, true).is_err());
                }
                "restore_verified" => {
                    assert!(!destination.join(COMPLETE).exists());
                    assert!(activate(&destination, true, true).is_err());
                }
                "restore_complete" => {
                    assert!(destination.join(COMPLETE).is_file());
                    assert_eq!(activate(&destination, true, true).unwrap(), 1);
                    assert_released_outcomes(&fixture, &destination);
                }
                "activation_before_commit" => {
                    assert_eq!(epoch_and_revocation(&destination), (0, false))
                }
                "activation_committed" => {
                    assert_eq!(epoch_and_revocation(&destination), (1, true));
                    assert!(activate(&destination, true, true).is_err());
                    assert_eq!(epoch_and_revocation(&destination), (1, true));
                    assert!(destination.join(QUARANTINE).exists());
                }
                _ => unreachable!(),
            }
        }
        eprintln!("native recovery boundary observed and child reaped: {stage}");
    }
}

#[test]
fn activation_acknowledgements_do_not_fence_an_unisolated_original_writer() {
    let root = TempDir::new().unwrap();
    let fixture = fixture(root.path());
    let destination = root.path().join("restored");
    restore(&fixture.backup, &destination).unwrap();
    let mut old = Authority::open(&fixture.data).unwrap();
    // Deliberately false operator claims in this negative fixture demonstrate
    // the boundary: local activation is not an external writer fence.
    activate(&destination, true, true).unwrap();
    assert_eq!(old.advance_policy(0).unwrap(), 1);
    assert_eq!(old.target("main").unwrap(), Some(fixture.after_event.after));
    assert_eq!(
        Authority::open(destination.join("data"))
            .unwrap()
            .target("main")
            .unwrap(),
        Some(fixture.before_event.after)
    );
}

#[test]
fn recovery_crash_child() {
    let Ok(stage) = std::env::var("AGENTSCM_RECOVERY_CRASH_STAGE") else {
        return;
    };
    let destination =
        PathBuf::from(std::env::var_os("AGENTSCM_RECOVERY_CRASH_DESTINATION").unwrap());
    if stage.starts_with("restore_") {
        let backup = PathBuf::from(std::env::var_os("AGENTSCM_RECOVERY_CRASH_BACKUP").unwrap());
        restore(&backup, &destination).unwrap();
    } else {
        activate(&destination, true, true).unwrap();
    }
    panic!("child escaped its requested boundary {stage}");
}
