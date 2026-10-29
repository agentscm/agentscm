#![cfg(unix)]
use std::fs;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::process::Command;

use agentscm::authority::Authority;
use agentscm::context::ReviewContext;
use agentscm::git_export::{CommitMetadata, ExportRequest, Mirror};
use agentscm::git_reconcile::{
    self, ExitRequest, InitializeRequest, ObserveRequest, OperationStatus, Phase, Reconciler,
    Status,
};
use agentscm::model::{Approval, FileChange, IntegrateRequest, IntegrationEvent};
use agentscm::store::Engine;
use serde_json::json;
use tempfile::TempDir;

fn git() -> PathBuf {
    PathBuf::from(std::env::var("AGENTSCM_TEST_GIT").unwrap_or_else(|_| "/usr/bin/git".into()))
}
fn run(root: &Path, args: &[&str]) -> Vec<u8> {
    let output = Command::new(git())
        .env_clear()
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .arg("-C")
        .arg(root)
        .args([
            "-c",
            "core.hooksPath=/dev/null",
            "-c",
            "commit.gpgSign=false",
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
fn head(root: &Path) -> String {
    String::from_utf8(run(root, &["rev-parse", "HEAD"]))
        .unwrap()
        .trim()
        .into()
}
fn commit(root: &Path, path: &str, bytes: &[u8]) -> String {
    fs::write(root.join(path), bytes).unwrap();
    run(root, &["add", "--", path]);
    run(root, &["commit", "-m", "fixture"]);
    head(root)
}
fn publish(engine: &Engine, authority: &mut Authority, revision: String) -> IntegrationEvent {
    let before = authority.target("main").unwrap();
    let number = before.as_ref().map_or(1, |target| target.version + 1);
    let repository = authority.repository_id().unwrap();
    let context:ReviewContext=serde_json::from_value(json!({"version":1,"change":{"repository":repository,"id":format!("change-{number}")},"change_version":1,"revision":revision,"base":before.as_ref().map(|target|json!({"revision":target.revision,"target_version":target.version})),"target":"main","policy_epoch":0,"intent":"test reconciliation","acceptance_criteria":["preserve both sides"],"scope":["files"],"exclusions":[],"omissions":[],"dependencies":[],"recipe":{"state":"digest","sha256":"a".repeat(64)},"toolchain":{"state":"digest","sha256":"b".repeat(64)},"environment":{"state":"digest","sha256":"c".repeat(64)},"required_evidence":[],"required_capabilities":["agentscm.review-context/v1"]})).unwrap();
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
    engine: Engine,
    authority: Authority,
    event: IntegrationEvent,
    reconciler: Reconciler,
    initial: String,
}
fn setup() -> Fixture {
    let root = TempDir::new().unwrap();
    let data = root.path().join("data");
    let source = root.path().join("source");
    fs::create_dir(&source).unwrap();
    run(
        &source,
        &["init", "--template=", "--object-format=sha1", "."],
    );
    let initial = commit(&source, "file", b"base");
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
            native_target: event.after.clone(),
        },
    )
    .unwrap();
    Fixture {
        root,
        data,
        source,
        engine,
        authority,
        event,
        reconciler,
        initial,
    }
}
fn observation(f: &Fixture, id: &str, generation: u64, git_commit: String) -> ObserveRequest {
    ObserveRequest {
        operation_id: id.into(),
        expected_generation: generation,
        git_commit,
        native_target: f.authority.target("main").unwrap().unwrap(),
    }
}
fn pending(data: &Path, before: &Status, request: &serde_json::Value) {
    let database = rusqlite::Connection::open(data.join("git-reconcile.sqlite")).unwrap();
    let id = request["request"]["operation_id"].as_str().unwrap();
    let mut state: serde_json::Value = serde_json::from_slice(
        &database
            .query_row("SELECT record FROM state WHERE singleton=1", [], |row| {
                row.get::<_, Vec<u8>>(0)
            })
            .unwrap(),
    )
    .unwrap();
    let mut status = serde_json::to_value(before).unwrap();
    status["generation"] = (before.generation + 1).into();
    status["state"] = "unknown".into();
    status["active_operation"] = id.into();
    status["observation"] = serde_json::Value::Null;
    status["exit"] = serde_json::Value::Null;
    state["status"] = status;
    let binding = if request["kind"] == "prepare_exit" {
        let destination = Path::new(request["request"]["destination"].as_str().unwrap());
        let owner = fs::symlink_metadata(destination.join("agentscm-mirror.json")).unwrap();
        let marker = destination.join("agentscm-reconcile-instance");
        if !marker.exists() {
            fs::write(&marker, b"11111111111111111111111111111111").unwrap();
        }
        let instance = fs::read_to_string(marker).unwrap();
        Some(
            serde_json::to_vec(
                &json!({"device":owner.dev(),"inode":owner.ino(),"instance":instance}),
            )
            .unwrap(),
        )
    } else {
        None
    };
    database.execute("INSERT INTO operations(id,request,before,result,binding) VALUES(?1,?2,?3,NULL,?4) ON CONFLICT(id) DO UPDATE SET result=NULL",rusqlite::params![id,serde_json::to_vec(request).unwrap(),serde_json::to_vec(before).unwrap(),binding]).unwrap();
    database
        .execute(
            "UPDATE state SET record=?1 WHERE singleton=1",
            [serde_json::to_vec(&state).unwrap()],
        )
        .unwrap();
}
fn exit_request(f: &Fixture) -> ExitRequest {
    let destination = f.root.path().join("mirror.git");
    Mirror::initialize(
        &git(),
        &destination,
        &f.authority.repository_id().unwrap(),
        "main",
    )
    .unwrap();
    let destination = destination.canonicalize().unwrap();
    ExitRequest {
        operation_id: "exit-1".into(),
        expected_generation: 0,
        git_commit: f.initial.clone(),
        native_target: f.event.after.clone(),
        destination,
        export: ExportRequest {
            event_sequence: f.event.sequence,
            expected_oid: None,
            metadata: CommitMetadata {
                name: "Fixture".into(),
                email: "fixture@example.invalid".into(),
                timestamp: 1780000000,
                message: "Prepare local Git handoff".into(),
            },
        },
    }
}

#[test]
fn incoming_candidate_has_verified_native_base_and_exact_retry_without_target_move() {
    let mut f = setup();
    let changed = commit(&f.source, "file", b"git edit");
    let request = observation(&f, "incoming", 0, changed);
    let original = f.authority.target("main").unwrap();
    let status = f.reconciler.observe(&request).unwrap();
    assert_eq!(status.state, Phase::GitChanged);
    let candidate = status
        .observation
        .as_ref()
        .unwrap()
        .candidate_revision
        .as_ref()
        .unwrap();
    assert_eq!(
        f.engine.load_revision(candidate).unwrap().parents,
        vec![f.event.after.revision.clone()]
    );
    assert_eq!(f.engine.read_file(candidate, "file").unwrap(), b"git edit");
    assert_eq!(f.authority.target("main").unwrap(), original);
    let mut reopened = Reconciler::open(&f.data).unwrap();
    assert_eq!(reopened.observe(&request).unwrap(), status);
    let mut changed = request.clone();
    changed.git_commit = f.initial;
    assert!(reopened.observe(&changed).is_err());
    assert_eq!(git_reconcile::read_status(&f.data).unwrap(), status);
}

#[test]
fn divergence_persists_and_actual_incorporated_candidate_is_not_a_new_conflict() {
    let mut f = setup();
    let git_commit = commit(&f.source, "file", b"git edit");
    let revision = f
        .engine
        .checkpoint(
            Some(&f.event.after.revision),
            &[FileChange {
                path: "native".into(),
                content: Some(b"native edit".to_vec()),
            }],
        )
        .unwrap();
    let native = publish(&f.engine, &mut f.authority, revision);
    let request = observation(&f, "diverged", 0, git_commit.clone());
    let status = f.reconciler.observe(&request).unwrap();
    assert_eq!(status.state, Phase::Conflict);
    assert_eq!(git_reconcile::read_status(&f.data).unwrap(), status);
    let candidate = status
        .observation
        .as_ref()
        .unwrap()
        .candidate_revision
        .clone()
        .unwrap();
    assert!(!status.observation.as_ref().unwrap().candidate_incorporated);
    let merged = f
        .engine
        .checkpoint_with_parents(
            &[native.after.revision, candidate],
            &[FileChange {
                path: "file".into(),
                content: Some(b"git edit".to_vec()),
            }],
        )
        .unwrap();
    publish(&f.engine, &mut f.authority, merged);
    let request = observation(&f, "incorporated", 1, git_commit);
    let next = f.reconciler.observe(&request).unwrap();
    assert_eq!(next.state, Phase::NativeChanged);
    assert!(next.observation.unwrap().candidate_incorporated);
    assert_eq!(next.baseline, status.baseline);
}

#[test]
fn dirty_observation_is_retained_unknown_and_never_advances_the_baseline() {
    let mut f = setup();
    let request = observation(&f, "dirty", 0, f.initial.clone());
    fs::write(f.source.join("file"), b"preserve dirty bytes").unwrap();
    assert!(f.reconciler.observe(&request).is_err());
    let status = git_reconcile::read_status(&f.data).unwrap();
    assert_eq!(status.state, Phase::Unknown);
    assert!(status.active_operation.is_none());
    assert_eq!(status.baseline.git_commit, f.initial);
    assert!(matches!(
        git_reconcile::lookup(&f.data, "dirty").unwrap(),
        OperationStatus::Complete { .. }
    ));
    assert_eq!(
        fs::read(f.source.join("file")).unwrap(),
        b"preserve dirty bytes"
    );
    fs::write(f.source.join("file"), b"base").unwrap();
    let request = observation(&f, "clean", 1, f.initial.clone());
    assert_eq!(
        f.reconciler.observe(&request).unwrap().state,
        Phase::Synchronized
    );
}

#[test]
fn modeled_pending_capture_blocks_overtaking_and_exact_retry_reconciles() {
    let mut f = setup();
    let request = observation(&f, "capture-pending", 0, f.initial.clone());
    let before = f.reconciler.status().unwrap();
    pending(
        &f.data,
        &before,
        &json!({"kind":"observe","request":request}),
    );
    assert!(matches!(
        git_reconcile::lookup(&f.data, &request.operation_id).unwrap(),
        OperationStatus::Pending { generation: 1, .. }
    ));
    let later = observation(&f, "overtake", 1, f.initial.clone());
    assert!(f.reconciler.observe(&later).is_err());
    assert_eq!(
        f.reconciler.observe(&request).unwrap().state,
        Phase::Synchronized
    );
    assert!(matches!(
        git_reconcile::lookup(&f.data, &request.operation_id).unwrap(),
        OperationStatus::Complete { .. }
    ));
}

#[test]
fn local_exit_preparation_and_modeled_lost_reply_reuse_one_verified_commit() {
    let mut f = setup();
    let request = exit_request(&f);
    let before = f.reconciler.status().unwrap();
    let index = fs::read(f.source.join(".git/index")).unwrap();
    let result = f.reconciler.prepare_exit(&request).unwrap();
    assert_eq!(result.state, Phase::ExitPrepared);
    assert_eq!(result.source_authority, "git");
    let receipt = result.exit.as_ref().unwrap();
    assert_eq!(
        run(
            &request.destination,
            &["show", &format!("{}:file", receipt.commit)]
        ),
        b"base"
    );
    assert_eq!(
        f.authority.target("main").unwrap(),
        Some(f.event.after.clone())
    );
    assert_eq!(head(&f.source), f.initial);
    assert_eq!(fs::read(f.source.join(".git/index")).unwrap(), index);
    assert_eq!(fs::read(f.source.join("file")).unwrap(), b"base");
    // A modeled journal boundary, not a process-kill or power-loss experiment.
    pending(
        &f.data,
        &before,
        &json!({"kind":"prepare_exit","request":request}),
    );
    let mut reopened = Reconciler::open(&f.data).unwrap();
    assert_eq!(reopened.prepare_exit(&request).unwrap(), result);
    assert_eq!(
        String::from_utf8(run(
            &request.destination,
            &["rev-list", "--count", "refs/heads/agentscm"]
        ))
        .unwrap()
        .trim(),
        "1"
    );
}

#[test]
fn unknown_export_with_ref_drift_blocks_exit_and_alternative_observations() {
    let mut f = setup();
    let request = exit_request(&f);
    let before = f.reconciler.status().unwrap();
    let result = f.reconciler.prepare_exit(&request).unwrap();
    pending(
        &f.data,
        &before,
        &json!({"kind":"prepare_exit","request":request}),
    );
    run(
        &request.destination,
        &[
            "update-ref",
            "-d",
            "refs/heads/agentscm",
            &result.exit.unwrap().commit,
        ],
    );
    assert!(f.reconciler.prepare_exit(&request).is_err());
    let unknown = f.reconciler.status().unwrap();
    assert_eq!(unknown.state, Phase::Unknown);
    assert_eq!(unknown.active_operation.as_deref(), Some("exit-1"));
    assert!(unknown.exit.is_none());
    let next = observation(&f, "cannot-overtake-exit", 1, f.initial.clone());
    assert!(f.reconciler.observe(&next).is_err());
    assert!(matches!(
        git_reconcile::lookup(&f.data, "exit-1").unwrap(),
        OperationStatus::Pending { .. }
    ));
}

#[test]
fn current_guard_pins_reconcile_generation_and_rejects_actual_native_drift() {
    let mut f = setup();
    let mut second = Reconciler::open(&f.data).unwrap();
    let request = observation(&f, "blocked", 0, f.initial.clone());
    let guard = f.reconciler.current(0).unwrap();
    assert_eq!(guard.status().generation, 0);
    assert!(second.observe(&request).is_err());
    drop(guard);
    let revision = f
        .engine
        .checkpoint(
            Some(&f.event.after.revision),
            &[FileChange {
                path: "native".into(),
                content: Some(b"later".to_vec()),
            }],
        )
        .unwrap();
    publish(&f.engine, &mut f.authority, revision);
    assert!(f.reconciler.current(0).is_err());
}

#[test]
fn readonly_status_lookup_never_create_missing_journal_and_preserve_exact_bytes() {
    let f = setup();
    let before = fs::read(f.data.join("git-reconcile.sqlite")).unwrap();
    let status = git_reconcile::read_status(&f.data).unwrap();
    assert_eq!(status.observation_freshness, "historical");
    assert_eq!(
        git_reconcile::lookup(&f.data, "missing").unwrap(),
        OperationStatus::Missing
    );
    assert_eq!(
        fs::read(f.data.join("git-reconcile.sqlite")).unwrap(),
        before
    );
    let other = f.root.path().join("no-journal");
    let _engine = Engine::open(&other).unwrap();
    let _authority = Authority::open(&other).unwrap();
    assert!(git_reconcile::read_status(&other).is_err());
    assert!(git_reconcile::lookup(&other, "missing").is_err());
    assert!(!other.join("git-reconcile.sqlite").exists());
}

#[test]
fn rejected_new_exit_never_reserves_an_operation_even_with_an_existing_mirror_record() {
    let mut f = setup();
    let mut exit = exit_request(&f);
    let mut mirror = Mirror::open(
        &git(),
        &exit.destination,
        &f.authority.repository_id().unwrap(),
    )
    .unwrap();
    mirror
        .export(&f.engine, &f.authority, &exit.export)
        .unwrap();
    let git_commit = commit(&f.source, "file", b"incoming");
    let request = observation(&f, "incoming", 0, git_commit.clone());
    let before = f.reconciler.observe(&request).unwrap();
    exit.expected_generation = 1;
    exit.git_commit = git_commit.clone();
    assert!(f.reconciler.prepare_exit(&exit).is_err());
    assert_eq!(f.reconciler.status().unwrap(), before);
    assert_eq!(
        git_reconcile::lookup(&f.data, &exit.operation_id).unwrap(),
        OperationStatus::Missing
    );
    let request = observation(&f, "still-observable", 1, git_commit);
    assert_eq!(
        f.reconciler.observe(&request).unwrap().state,
        Phase::GitChanged
    );
}

#[test]
fn incoming_delta_across_batches_preserves_deletion_and_one_direct_baseline_parent() {
    let mut f = setup();
    run(&f.source, &["rm", "file"]);
    for number in 0..4097 {
        fs::write(
            f.source.join(format!("f{number:05}")),
            [0, 255, (number % 256) as u8],
        )
        .unwrap();
    }
    run(&f.source, &["add", "--all"]);
    run(&f.source, &["commit", "-m", "batched incoming"]);
    let request = observation(&f, "batched", 0, head(&f.source));
    let status = f.reconciler.observe(&request).unwrap();
    assert_eq!(status.state, Phase::GitChanged);
    let observation = status.observation.unwrap();
    let candidate = observation.candidate_revision.unwrap();
    let revision = f.engine.load_revision(&candidate).unwrap();
    assert_eq!(revision.version, 1);
    assert_eq!(revision.parents, vec![f.event.after.revision]);
    assert_eq!(
        revision.tree,
        f.engine
            .load_revision(&observation.git_snapshot)
            .unwrap()
            .tree
    );
    assert!(f.engine.read_file(&candidate, "file").is_err());
    assert_eq!(
        f.engine.read_file(&candidate, "f04096").unwrap(),
        [0, 255, 0]
    );
}

#[test]
fn completed_exit_retry_returns_historical_receipt_when_mirror_is_unavailable() {
    let mut f = setup();
    let request = exit_request(&f);
    let result = f.reconciler.prepare_exit(&request).unwrap();
    fs::rename(
        &request.destination,
        f.root.path().join("mirror-held-for-inspection"),
    )
    .unwrap();
    assert_eq!(f.reconciler.prepare_exit(&request).unwrap(), result);
    assert_eq!(result.observation_freshness, "historical");
}

#[test]
fn pending_exit_cannot_redirect_a_canonical_destination_to_a_second_owned_mirror() {
    let mut f = setup();
    let request = exit_request(&f);
    let before = f.reconciler.status().unwrap();
    pending(
        &f.data,
        &before,
        &json!({"kind":"prepare_exit","request":request}),
    );
    let kept = f.root.path().join("original-mirror");
    fs::rename(&request.destination, &kept).unwrap();
    let other = f.root.path().join("other-mirror");
    Mirror::initialize(
        &git(),
        &other,
        &f.authority.repository_id().unwrap(),
        "main",
    )
    .unwrap();
    let original = fs::read(kept.join("agentscm-export.sqlite")).unwrap();
    let other_before = fs::read(other.join("agentscm-export.sqlite")).unwrap();
    std::os::unix::fs::symlink(other.canonicalize().unwrap(), &request.destination).unwrap();
    assert!(f.reconciler.prepare_exit(&request).is_err());
    assert!(matches!(
        git_reconcile::lookup(&f.data, "exit-1").unwrap(),
        OperationStatus::Pending { .. }
    ));
    assert_eq!(
        fs::read(kept.join("agentscm-export.sqlite")).unwrap(),
        original
    );
    assert_eq!(
        fs::read(other.join("agentscm-export.sqlite")).unwrap(),
        other_before
    );
    assert!(!kept.join("refs/heads/agentscm").exists());
    assert!(!other.join("refs/heads/agentscm").exists());
}

#[test]
fn missing_baseline_manifest_dependency_records_unknown_without_retrying_the_same_page() {
    let mut f = setup();
    let revision = f
        .engine
        .checkpoint(
            Some(&f.event.after.revision),
            &[FileChange {
                path: "old-only".into(),
                content: Some(vec![7]),
            }],
        )
        .unwrap();
    let native = publish(&f.engine, &mut f.authority, revision);
    let git_commit = commit(&f.source, "old-only", &[7]);
    let request = observation(&f, "new-baseline", 0, git_commit);
    assert_eq!(
        f.reconciler.observe(&request).unwrap().state,
        Phase::Synchronized
    );
    run(&f.source, &["rm", "old-only"]);
    run(&f.source, &["commit", "-m", "remove old path"]);
    // Fault-inject only descendant index leaves containing this path. Root
    // branch nodes remain readable, but paginated diff reports needed_objects.
    let mut removed = 0;
    for directory in fs::read_dir(f.data.join("objects")).unwrap() {
        let directory = directory.unwrap();
        for file in fs::read_dir(directory.path()).unwrap() {
            let file = file.unwrap();
            let bytes = fs::read(file.path()).unwrap();
            if bytes
                .windows(b"old-only".len())
                .any(|part| part == b"old-only")
            {
                fs::remove_file(file.path()).unwrap();
                removed += 1;
            }
        }
    }
    assert!(removed > 0);
    f.engine.load_revision(&native.after.revision).unwrap();
    let page = f
        .engine
        .diff_page(&native.after.revision, &f.event.after.revision, None, 4096)
        .unwrap();
    assert!(!page.needed_objects.is_empty());
    let request = observation(&f, "incomplete", 1, head(&f.source));
    assert!(matches!(
        f.reconciler.observe(&request),
        Err(agentscm::Error::MissingObject(_))
    ));
    let status = f.reconciler.status().unwrap();
    assert_eq!(status.state, Phase::Unknown);
    assert!(status.active_operation.is_none());
    assert!(status.error.unwrap().contains("object"));
}

#[test]
fn pending_exit_refuses_fresh_owned_mirror_at_the_same_canonical_path() {
    let mut f = setup();
    let request = exit_request(&f);
    let before = f.reconciler.status().unwrap();
    let result = f.reconciler.prepare_exit(&request).unwrap();
    let commit = result.exit.unwrap().commit;
    pending(
        &f.data,
        &before,
        &json!({"kind":"prepare_exit","request":request}),
    );
    let kept = f.root.path().join("original-export");
    fs::rename(&request.destination, &kept).unwrap();
    Mirror::initialize(
        &git(),
        &request.destination,
        &f.authority.repository_id().unwrap(),
        "main",
    )
    .unwrap();
    let replacement = fs::read(request.destination.join("agentscm-export.sqlite")).unwrap();
    assert!(f.reconciler.prepare_exit(&request).is_err());
    assert!(matches!(
        git_reconcile::lookup(&f.data, "exit-1").unwrap(),
        OperationStatus::Pending { .. }
    ));
    assert_eq!(
        fs::read(request.destination.join("agentscm-export.sqlite")).unwrap(),
        replacement
    );
    assert!(!request.destination.join("refs/heads/agentscm").exists());
    assert_eq!(
        String::from_utf8(run(&kept, &["rev-parse", "refs/heads/agentscm"]))
            .unwrap()
            .trim(),
        commit
    );
}

#[test]
fn pending_exit_without_its_retained_instance_binding_fails_closed() {
    let mut f = setup();
    let request = exit_request(&f);
    let before = f.reconciler.status().unwrap();
    pending(
        &f.data,
        &before,
        &json!({"kind":"prepare_exit","request":request}),
    );
    let database = rusqlite::Connection::open(f.data.join("git-reconcile.sqlite")).unwrap();
    database
        .execute("UPDATE operations SET binding=NULL WHERE id='exit-1'", [])
        .unwrap();
    assert!(git_reconcile::lookup(&f.data, "exit-1").is_err());
    assert!(f.reconciler.prepare_exit(&request).is_err());
    assert!(!request.destination.join("refs/heads/agentscm").exists());
}

#[test]
fn pending_exit_marker_refuses_replacement_even_when_device_and_inode_match() {
    let mut f = setup();
    let request = exit_request(&f);
    let before = f.reconciler.status().unwrap();
    f.reconciler.prepare_exit(&request).unwrap();
    pending(
        &f.data,
        &before,
        &json!({"kind":"prepare_exit","request":request}),
    );
    let kept = f.root.path().join("original-marker-export");
    fs::rename(&request.destination, &kept).unwrap();
    Mirror::initialize(
        &git(),
        &request.destination,
        &f.authority.repository_id().unwrap(),
        "main",
    )
    .unwrap();
    // Model ordinary inode reuse deterministically: match B's device/inode in
    // the retained A binding while preserving A's original random instance.
    let database = rusqlite::Connection::open(f.data.join("git-reconcile.sqlite")).unwrap();
    let mut binding: serde_json::Value = serde_json::from_slice(
        &database
            .query_row(
                "SELECT binding FROM operations WHERE id='exit-1'",
                [],
                |row| row.get::<_, Vec<u8>>(0),
            )
            .unwrap(),
    )
    .unwrap();
    let owner = fs::metadata(request.destination.join("agentscm-mirror.json")).unwrap();
    binding["device"] = owner.dev().into();
    binding["inode"] = owner.ino().into();
    database
        .execute(
            "UPDATE operations SET binding=?1 WHERE id='exit-1'",
            [serde_json::to_vec(&binding).unwrap()],
        )
        .unwrap();
    let replacement = fs::read(request.destination.join("agentscm-export.sqlite")).unwrap();
    assert!(f.reconciler.prepare_exit(&request).is_err());
    assert!(
        !request
            .destination
            .join("agentscm-reconcile-instance")
            .exists()
    );
    let different = if binding["instance"] == "22222222222222222222222222222222" {
        "33333333333333333333333333333333"
    } else {
        "22222222222222222222222222222222"
    };
    fs::write(
        request.destination.join("agentscm-reconcile-instance"),
        different,
    )
    .unwrap();
    assert!(f.reconciler.prepare_exit(&request).is_err());
    assert_eq!(
        fs::read(request.destination.join("agentscm-export.sqlite")).unwrap(),
        replacement
    );
    assert!(!request.destination.join("refs/heads/agentscm").exists());
    assert!(matches!(
        git_reconcile::lookup(&f.data, "exit-1").unwrap(),
        OperationStatus::Pending { .. }
    ));
}

#[test]
fn new_exit_refuses_malformed_or_symlinked_marker_without_reserving_work() {
    let mut f = setup();
    let request = exit_request(&f);
    let marker = request.destination.join("agentscm-reconcile-instance");
    for value in [
        "",
        "bad",
        "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA",
        "111111111111111111111111111111111",
    ] {
        fs::write(&marker, value).unwrap();
        assert!(f.reconciler.prepare_exit(&request).is_err());
        assert!(matches!(
            git_reconcile::lookup(&f.data, "exit-1").unwrap(),
            OperationStatus::Missing
        ));
    }
    fs::remove_file(&marker).unwrap();
    let other = f.root.path().join("other-marker");
    fs::write(&other, b"11111111111111111111111111111111").unwrap();
    std::os::unix::fs::symlink(&other, &marker).unwrap();
    assert!(f.reconciler.prepare_exit(&request).is_err());
    assert!(matches!(
        git_reconcile::lookup(&f.data, "exit-1").unwrap(),
        OperationStatus::Missing
    ));
    assert!(!request.destination.join("refs/heads/agentscm").exists());
}

#[test]
fn corrupt_replaced_baseline_blob_records_unknown_and_restored_retry_keeps_exact_parent() {
    let mut f = setup();
    let changed = commit(&f.source, "file", b"git edit");
    let blob = f
        .engine
        .list_files(&f.event.after.revision, None, 10)
        .unwrap()
        .entries
        .into_iter()
        .find(|entry| entry.path == "file")
        .unwrap()
        .blob;
    let path = f.data.join("objects").join(&blob[..2]).join(&blob[2..]);
    let encoded = fs::read(&path).unwrap();
    fs::write(&path, b"damaged").unwrap();
    let target = f.authority.target("main").unwrap();
    let baseline = f.reconciler.status().unwrap().baseline;
    let request = observation(&f, "corrupt-old-payload", 0, changed.clone());
    assert!(f.reconciler.observe(&request).is_err());
    let status = f.reconciler.status().unwrap();
    assert_eq!(status.state, Phase::Unknown);
    assert!(status.active_operation.is_none());
    assert_eq!(status.baseline, baseline);
    assert_eq!(f.authority.target("main").unwrap(), target);
    fs::write(&path, encoded).unwrap();
    let retry = observation(&f, "restored-old-payload", status.generation, changed);
    let observed = f.reconciler.observe(&retry).unwrap();
    let candidate = observed.observation.unwrap().candidate_revision.unwrap();
    assert_eq!(
        f.engine.load_revision(&candidate).unwrap().parents,
        [f.event.after.revision]
    );
    assert_eq!(f.engine.read_file(&candidate, "file").unwrap(), b"git edit");
    assert_eq!(f.authority.target("main").unwrap(), target);
}

#[test]
fn initialization_refuses_a_non_regular_native_baseline_entry() {
    use agentscm::model::{EntryChange, EntryKind};
    let root = TempDir::new().unwrap();
    let data = root.path().join("data");
    let source = root.path().join("source");
    fs::create_dir(&source).unwrap();
    run(
        &source,
        &["init", "--template=", "--object-format=sha1", "."],
    );
    let initial = commit(&source, "file", b"base");
    let engine = Engine::open(&data).unwrap();
    let mut authority = Authority::open(&data).unwrap();
    let revision = engine
        .checkpoint_entries(
            None,
            &[EntryChange {
                path: "file".into(),
                content: Some(b"base".to_vec()),
                kind: EntryKind::Executable,
            }],
        )
        .unwrap();
    let event = publish(&engine, &mut authority, revision);
    let error = Reconciler::initialize(
        &data,
        &InitializeRequest {
            git: git(),
            repository: source,
            target: "main".into(),
            git_commit: initial,
            native_target: event.after,
        },
    )
    .err()
    .expect("refusal");
    assert!(matches!(error, agentscm::Error::Conflict(_)), "{error}");
}
