#![cfg(unix)]
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use agentscm::authority::Authority;
use agentscm::context::ReviewContext;
use agentscm::git_reconcile::{
    self, InitializeRequest, ObserveRequest, OperationStatus, Phase, Reconciler, Status,
};
use agentscm::local_recovery;
use agentscm::model::{Approval, FileChange, IntegrateRequest, IntegrationEvent};
use agentscm::store::Engine;
use serde_json::json;
use tempfile::TempDir;

fn git() -> PathBuf {
    PathBuf::from(std::env::var("AGENTSCM_TEST_GIT").unwrap_or_else(|_| "/usr/bin/git".into()))
}

fn run(root: &Path, args: &[&str]) -> Vec<u8> {
    // This fresh template-free fixture has no hooks or user signing settings.
    let output = Command::new(git())
        .env_clear()
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .arg("-C")
        .arg(root)
        .args([
            "-c",
            "user.name=Fixture",
            "-c",
            "user.email=fixture@example.invalid",
        ])
        .args(args)
        .output()
        .unwrap();
    assert!(output.status.success(), "{args:?}: {:?}", output.stderr);
    output.stdout
}

fn commit(source: &Path, bytes: &[u8]) -> String {
    fs::write(source.join("file"), bytes).unwrap();
    run(source, &["add", "--", "file"]);
    run(source, &["commit", "-m", "fixture"]);
    String::from_utf8(run(source, &["rev-parse", "HEAD"]))
        .unwrap()
        .trim()
        .into()
}

fn publish(engine: &Engine, authority: &mut Authority, revision: String) -> IntegrationEvent {
    let before = authority.target("main").unwrap();
    let number = before.as_ref().map_or(1, |target| target.version + 1);
    let context: ReviewContext = serde_json::from_value(json!({
        "version":1, "change":{"repository":authority.repository_id().unwrap(),"id":format!("change-{number}")},
        "change_version":1, "revision":revision,
        "base":before.as_ref().map(|target| json!({"revision":target.revision,"target_version":target.version})),
        "target":"main", "policy_epoch":0, "intent":"retain reconciliation recovery evidence",
        "acceptance_criteria":["retain exact history without external adoption"], "scope":["file"],
        "exclusions":[], "omissions":[], "dependencies":[],
        "recipe":{"state":"digest","sha256":"a".repeat(64)},
        "toolchain":{"state":"digest","sha256":"b".repeat(64)},
        "environment":{"state":"digest","sha256":"c".repeat(64)},
        "required_evidence":[], "required_capabilities":["agentscm.review-context/v1"]
    })).unwrap();
    let change = authority.start_change(&context).unwrap();
    let approval = Approval {
        approval_id: format!("approval-{number}"),
        target: "main".into(),
        revision: revision.clone(),
        context_digest: change.context_digest.clone(),
        change: Some(change.id.clone()),
        change_version: Some(1),
        policy_epoch: 0,
    };
    authority.approve(&approval).unwrap();
    authority
        .integrate(
            engine,
            &IntegrateRequest {
                operation_id: format!("integrate-{number}"),
                target: "main".into(),
                expected_target: before,
                revision,
                context_digest: change.context_digest,
                approval_id: approval.approval_id,
                change: Some(change.id),
                change_version: Some(1),
                policy_epoch: 0,
            },
        )
        .unwrap()
}

struct Fixture {
    root: TempDir,
    data: PathBuf,
    source: PathBuf,
    initial: String,
    engine: Engine,
    authority: Authority,
    reconciler: Reconciler,
}

fn fixture() -> Fixture {
    let root = TempDir::new().unwrap();
    let data = root.path().join("data");
    let source = root.path().join("source");
    fs::create_dir(&source).unwrap();
    run(
        &source,
        &["init", "--template=", "--object-format=sha1", "."],
    );
    let initial = commit(&source, b"base");
    let engine = Engine::open(&data).unwrap();
    let mut authority = Authority::open(&data).unwrap();
    let revision = engine
        .checkpoint(
            None,
            &[FileChange {
                path: "file".into(),
                content: Some(b"base".to_vec()),
            }],
        )
        .unwrap();
    let event = publish(&engine, &mut authority, revision);
    let reconciler = Reconciler::initialize(
        &data,
        &InitializeRequest {
            git: git(),
            repository: source.clone(),
            target: "main".into(),
            git_commit: initial.clone(),
            native_target: event.after,
        },
    )
    .unwrap();
    Fixture {
        root,
        data,
        source,
        initial,
        engine,
        authority,
        reconciler,
    }
}

fn observe(f: &mut Fixture, id: &str, commit: String) -> Status {
    let request = ObserveRequest {
        operation_id: id.into(),
        expected_generation: f.reconciler.status().unwrap().generation,
        git_commit: commit,
        native_target: f.authority.target("main").unwrap().unwrap(),
    };
    f.reconciler.observe(&request).unwrap()
}

struct ClosedFixture {
    root: TempDir,
    data: PathBuf,
    source: PathBuf,
}

fn close(f: Fixture) -> ClosedFixture {
    let Fixture {
        root,
        data,
        source,
        engine,
        authority,
        reconciler,
        ..
    } = f;
    drop(reconciler);
    drop(authority);
    drop(engine);
    ClosedFixture { root, data, source }
}

fn restored(f: &ClosedFixture) -> PathBuf {
    let backup = f.root.path().join("backup");
    let original = fs::read(f.data.join("git-reconcile.sqlite")).unwrap();
    let head = run(&f.source, &["rev-parse", "HEAD"]);
    let index = fs::read(f.source.join(".git/index")).unwrap();
    let work = fs::read(f.source.join("file")).unwrap();
    let manifest = local_recovery::backup(&f.data, &backup, &[], &[], true).unwrap();
    assert_eq!(local_recovery::verify(&backup).unwrap(), manifest);
    assert_eq!(
        fs::read(backup.join("payload/data/git-reconcile.sqlite")).unwrap(),
        original
    );
    let destination = f.root.path().join("restored");
    local_recovery::restore(&backup, &destination).unwrap();
    let data = destination.join("data");
    assert!(!data.join(".agentscm-local-access.lock").exists());
    assert!(matches!(git_reconcile::read_status(&data),
        Err(agentscm::Error::Conflict(message)) if message.contains("explicit recovery activation")));
    assert!(matches!(git_reconcile::lookup(&data, "unobserved"),
        Err(agentscm::Error::Conflict(message)) if message.contains("explicit recovery activation")));
    assert!(!data.join(".agentscm-local-access.lock").exists());
    local_recovery::activate(&destination, true, true).unwrap();
    assert_eq!(
        fs::read(data.join("git-reconcile.sqlite")).unwrap(),
        original
    );
    assert!(
        matches!(Reconciler::open(&data),
        Err(agentscm::Error::Conflict(message)) if message.contains("original data binding differs")),
        "historical binding cannot adopt relocated roots"
    );
    assert_eq!(
        fs::read(data.join("git-reconcile.sqlite")).unwrap(),
        original
    );
    assert_eq!(run(&f.source, &["rev-parse", "HEAD"]), head);
    assert_eq!(fs::read(f.source.join(".git/index")).unwrap(), index);
    assert_eq!(fs::read(f.source.join("file")).unwrap(), work);
    data
}

#[test]
fn native_only_observation_survives_backup_as_historical_without_reattachment() {
    let mut f = fixture();
    let before = f.authority.target("main").unwrap().unwrap();
    let revision = f
        .engine
        .checkpoint(
            Some(&before.revision),
            &[FileChange {
                path: "file".into(),
                content: Some(b"native edit".to_vec()),
            }],
        )
        .unwrap();
    publish(&f.engine, &mut f.authority, revision);
    let initial = f.initial.clone();
    let status = observe(&mut f, "native-only", initial);
    assert_eq!(status.state, Phase::NativeChanged);
    let f = close(f);
    let data = restored(&f);
    assert_eq!(git_reconcile::read_status(&data).unwrap(), status);
    assert_eq!(
        git_reconcile::lookup(&data, "native-only").unwrap(),
        OperationStatus::Complete {
            operation_id: "native-only".into(),
            result: Box::new(status),
        }
    );
}

#[test]
fn incoming_candidate_and_its_ancestor_baseline_are_valid_retained_roots() {
    let mut f = fixture();
    let commit = commit(&f.source, b"incoming edit");
    let status = observe(&mut f, "incoming", commit);
    assert_eq!(status.state, Phase::GitChanged);
    let candidate = status
        .observation
        .as_ref()
        .unwrap()
        .candidate_revision
        .clone()
        .unwrap();
    assert_eq!(
        f.engine.load_revision(&candidate).unwrap().parents,
        vec![status.baseline.native_target.revision.clone()]
    );
    let f = close(f);
    let data = restored(&f);
    assert_eq!(git_reconcile::read_status(&data).unwrap(), status);
    assert_eq!(
        git_reconcile::lookup(&data, "never-retained").unwrap(),
        OperationStatus::Missing
    );
    assert_eq!(
        Engine::open(&data)
            .unwrap()
            .read_file(&candidate, "file")
            .unwrap(),
        b"incoming edit"
    );
}

#[test]
fn recovery_qualifies_more_than_256_independent_source_heads_with_asgr_history() {
    let mut f = fixture();
    let commit = commit(&f.source, b"incoming edit");
    let status = observe(&mut f, "incoming", commit);
    let mut roots = std::collections::BTreeSet::new();
    for number in 0..257 {
        let revision = f
            .engine
            .checkpoint(
                None,
                &[FileChange {
                    path: "detached".into(),
                    content: Some(format!("retained-{number}").into_bytes()),
                }],
            )
            .unwrap();
        assert!(
            f.engine
                .load_revision(&revision)
                .unwrap()
                .parents
                .is_empty()
        );
        roots.insert(revision);
    }
    assert_eq!(roots.len(), 257);
    let f = close(f);
    let data = restored(&f);
    assert_eq!(git_reconcile::read_status(&data).unwrap(), status);
    let engine = Engine::open(&data).unwrap();
    for root in roots {
        assert!(engine.load_revision(&root).is_ok());
    }
}

#[test]
fn pending_intent_with_absent_request_revision_remains_unknown_after_restore() {
    for diagnostic in [
        None,
        Some("completion unknown after a lost acknowledgement"),
    ] {
        let f = fixture();
        let before = f.reconciler.status().unwrap();
        let request = ObserveRequest {
            operation_id: "pending-absent".into(),
            expected_generation: 0,
            git_commit: f.initial.clone(),
            native_target: agentscm::model::TargetState {
                revision: "f".repeat(64),
                version: 1,
            },
        };
        assert!(
            f.engine
                .load_revision(&request.native_target.revision)
                .is_err()
        );
        let f = close(f);
        // Modeled interruption after reservation, not a process-kill experiment.
        let db = rusqlite::Connection::open(f.data.join("git-reconcile.sqlite")).unwrap();
        let bytes: Vec<u8> = db
            .query_row("SELECT record FROM state WHERE singleton=1", [], |row| {
                row.get(0)
            })
            .unwrap();
        let mut record: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        let mut pending = before.clone();
        pending.generation = 1;
        pending.state = Phase::Unknown;
        pending.active_operation = Some(request.operation_id.clone());
        pending.observation = None;
        pending.exit = None;
        pending.error = diagnostic.map(str::to_owned);
        record["status"] = serde_json::to_value(&pending).unwrap();
        db.execute(
            "INSERT INTO operations(id,request,before,result) VALUES(?1,?2,?3,NULL)",
            rusqlite::params![
                request.operation_id,
                serde_json::to_vec(&json!({"kind":"observe","request":request})).unwrap(),
                serde_json::to_vec(&before).unwrap(),
            ],
        )
        .unwrap();
        db.execute(
            "UPDATE state SET record=?1 WHERE singleton=1",
            [serde_json::to_vec(&record).unwrap()],
        )
        .unwrap();
        drop(db);
        let data = restored(&f);
        let bytes = fs::read(data.join("git-reconcile.sqlite")).unwrap();
        assert_eq!(git_reconcile::read_status(&data).unwrap(), pending);
        assert_eq!(
            git_reconcile::lookup(&data, "pending-absent").unwrap(),
            OperationStatus::Pending {
                operation_id: "pending-absent".into(),
                generation: 1,
            }
        );
        assert_eq!(
            git_reconcile::lookup(&data, "missing").unwrap(),
            OperationStatus::Missing
        );
        assert_eq!(fs::read(data.join("git-reconcile.sqlite")).unwrap(), bytes);
    }
}

#[test]
fn failed_observation_retains_complete_unknown_without_confirming_intent_revision() {
    let mut f = fixture();
    let request = ObserveRequest {
        operation_id: "failed-absent".into(),
        expected_generation: 0,
        git_commit: f.initial.clone(),
        native_target: agentscm::model::TargetState {
            revision: "f".repeat(64),
            version: 1,
        },
    };
    assert!(f.reconciler.observe(&request).is_err());
    let status = git_reconcile::read_status(&f.data).unwrap();
    assert_eq!(status.state, Phase::Unknown);
    assert!(status.active_operation.is_none());
    assert!(status.observation.is_none());
    assert!(status.error.is_some());
    let expected = OperationStatus::Complete {
        operation_id: "failed-absent".into(),
        result: Box::new(status.clone()),
    };
    assert_eq!(
        git_reconcile::lookup(&f.data, "failed-absent").unwrap(),
        expected
    );
    let f = close(f);
    let data = restored(&f);
    let bytes = fs::read(data.join("git-reconcile.sqlite")).unwrap();
    assert_eq!(git_reconcile::read_status(&data).unwrap(), status);
    assert_eq!(
        git_reconcile::lookup(&data, "failed-absent").unwrap(),
        expected
    );
    assert_eq!(fs::read(data.join("git-reconcile.sqlite")).unwrap(), bytes);
}

#[test]
fn recovery_rejects_unknown_asgr_schema_and_missing_confirmed_source() {
    for wrong_schema in [true, false] {
        let f = close(fixture());
        let db = rusqlite::Connection::open(f.data.join("git-reconcile.sqlite")).unwrap();
        if wrong_schema {
            db.pragma_update(None, "user_version", 2).unwrap();
        } else {
            let bytes: Vec<u8> = db
                .query_row("SELECT record FROM state WHERE singleton=1", [], |row| {
                    row.get(0)
                })
                .unwrap();
            let mut record: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
            record["status"]["baseline"]["native_target"]["revision"] = "f".repeat(64).into();
            db.execute(
                "UPDATE state SET record=?1 WHERE singleton=1",
                [serde_json::to_vec(&record).unwrap()],
            )
            .unwrap();
        }
        drop(db);
        let output = f.root.path().join("must-fail");
        assert!(local_recovery::backup(&f.data, &output, &[], &[], true).is_err());
        assert!(!output.join("recovery-manifest.json").exists());
    }
}

#[test]
fn recovery_rejects_inconsistent_active_operation_rows() {
    for scenario in ["missing", "complete", "wrong-generation", "wrong-baseline"] {
        let mut f = fixture();
        let before = f.reconciler.status().unwrap();
        let request = ObserveRequest {
            operation_id: "claimed-active".into(),
            expected_generation: 0,
            git_commit: f.initial.clone(),
            native_target: f.authority.target("main").unwrap().unwrap(),
        };
        if scenario == "complete" {
            let initial = f.initial.clone();
            observe(&mut f, "claimed-active", initial);
        }
        let f = close(f);
        let db = rusqlite::Connection::open(f.data.join("git-reconcile.sqlite")).unwrap();
        let bytes: Vec<u8> = db
            .query_row("SELECT record FROM state WHERE singleton=1", [], |row| {
                row.get(0)
            })
            .unwrap();
        let mut record: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        record["status"]["state"] = "unknown".into();
        record["status"]["active_operation"] = "claimed-active".into();
        record["status"]["generation"] = 1.into();
        record["status"]["observation"] = serde_json::Value::Null;
        record["status"]["exit"] = serde_json::Value::Null;
        record["status"]["error"] = serde_json::Value::Null;
        if scenario.starts_with("wrong-") {
            db.execute(
                "INSERT INTO operations(id,request,before,result) VALUES(?1,?2,?3,NULL)",
                rusqlite::params![
                    request.operation_id,
                    serde_json::to_vec(&json!({"kind":"observe","request":request})).unwrap(),
                    serde_json::to_vec(&before).unwrap(),
                ],
            )
            .unwrap();
        }
        db.execute(
            "UPDATE state SET record=?1 WHERE singleton=1",
            [serde_json::to_vec(&record).unwrap()],
        )
        .unwrap();
        drop(db);
        if scenario.starts_with("wrong-") {
            assert_eq!(git_reconcile::read_status(&f.data).unwrap().generation, 1);
            assert!(matches!(
                git_reconcile::lookup(&f.data, "claimed-active").unwrap(),
                OperationStatus::Pending { generation: 1, .. }
            ));
            // Change one field of an otherwise valid pending row.
            if scenario == "wrong-generation" {
                record["status"]["generation"] = 2.into();
            } else {
                record["status"]["baseline"]["native_target"]["version"] = 2.into();
            }
            let db = rusqlite::Connection::open(f.data.join("git-reconcile.sqlite")).unwrap();
            db.execute(
                "UPDATE state SET record=?1 WHERE singleton=1",
                [serde_json::to_vec(&record).unwrap()],
            )
            .unwrap();
        }
        let status = git_reconcile::read_status(&f.data).unwrap();
        assert_eq!(status.active_operation.as_deref(), Some("claimed-active"));
        let operation = git_reconcile::lookup(&f.data, "claimed-active").unwrap();
        match scenario {
            "complete" => assert!(matches!(operation, OperationStatus::Complete { .. })),
            "missing" => assert_eq!(operation, OperationStatus::Missing),
            "wrong-generation" => {
                assert_eq!(status.generation, 2);
                assert!(matches!(
                    operation,
                    OperationStatus::Pending { generation: 1, .. }
                ));
            }
            "wrong-baseline" => {
                assert_eq!(status.baseline.native_target.version, 2);
                assert_eq!(before.baseline.native_target.version, 1);
                assert!(matches!(
                    operation,
                    OperationStatus::Pending { generation: 1, .. }
                ));
            }
            _ => unreachable!(),
        }
        let output = f.root.path().join("must-fail");
        assert!(local_recovery::backup(&f.data, &output, &[], &[], true).is_err());
        assert!(!output.join("recovery-manifest.json").exists());
    }
}
