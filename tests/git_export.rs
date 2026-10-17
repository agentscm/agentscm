#![cfg(unix)]
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use agentscm::authority::Authority;
use agentscm::context::ReviewContext;
use agentscm::git_export::{
    CommitMetadata, ExportRecord, ExportRequest, ExportState, ExternalCheckOutcome,
    ExternalCheckRequest, Mirror,
};
use agentscm::local_recovery;
use agentscm::model::{
    Approval, EntryChange, EntryKind, FileChange, IntegrateRequest, IntegrationEvent,
};
use agentscm::store::Engine;
use tempfile::TempDir;

fn git() -> PathBuf {
    let path =
        PathBuf::from(std::env::var("AGENTSCM_TEST_GIT").unwrap_or_else(|_| "/usr/bin/git".into()));
    assert!(
        path.is_absolute() && path.exists(),
        "absolute test Git executable required"
    );
    path
}

fn run(path: &Path, args: &[&str]) -> Vec<u8> {
    let output = Command::new(git())
        .env_clear()
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .arg("-C")
        .arg(path)
        .args(["-c", "core.hooksPath=/dev/null"])
        .args(args)
        .output()
        .unwrap();
    assert!(output.status.success(), "Git {args:?}: {:?}", output.stderr);
    output.stdout
}

fn agentscm(root: &Path, args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_agentscm"))
        .current_dir(root)
        .args(args)
        .output()
        .unwrap()
}

fn seed_repository(path: &Path) -> (String, String) {
    fs::create_dir(path).unwrap();
    run(path, &["init", "--object-format=sha1"]);
    run(path, &["config", "user.name", "Seed Test"]);
    run(path, &["config", "user.email", "seed@example.invalid"]);
    fs::write(path.join("old"), b"old bytes").unwrap();
    fs::write(path.join("binary"), [0, 255, 1]).unwrap();
    fs::write(path.join("empty"), b"").unwrap();
    run(path, &["add", "."]);
    run(path, &["commit", "-m", "seed one"]);
    let first = String::from_utf8(run(path, &["rev-parse", "HEAD"]))
        .unwrap()
        .trim()
        .to_owned();
    fs::write(path.join("second"), b"committed").unwrap();
    run(path, &["add", "second"]);
    run(path, &["commit", "-m", "seed two"]);
    let second = String::from_utf8(run(path, &["rev-parse", "HEAD"]))
        .unwrap()
        .trim()
        .to_owned();
    (first, second)
}

fn publish(
    engine: &Engine,
    authority: &mut Authority,
    changes: Vec<FileChange>,
) -> IntegrationEvent {
    let before = authority.target("main").unwrap();
    let revision = engine
        .checkpoint(before.as_ref().map(|b| b.revision.as_str()), &changes)
        .unwrap();
    integrate_revision(engine, authority, before, revision)
}

fn integrate_revision(
    engine: &Engine,
    authority: &mut Authority,
    before: Option<agentscm::model::TargetState>,
    revision: String,
) -> IntegrationEvent {
    let sequence = before.as_ref().map_or(1, |b| b.version + 1);
    let repository = authority.repository_id().unwrap();
    let context: ReviewContext = serde_json::from_value(serde_json::json!({
        "version":1,"change":{"repository":repository,"id":format!("change-{sequence}")},
        "change_version":1,"revision":revision,"base":before.as_ref().map(|b| serde_json::json!({"revision":b.revision,"target_version":b.version})),
        "target":"main","policy_epoch":0,"intent":"synthetic snapshot export",
        "acceptance_criteria":["exact byte fidelity"],"scope":["source"],"exclusions":[],"omissions":[],"dependencies":[],
        "recipe":{"state":"digest","sha256":"b".repeat(64)},
        "toolchain":{"state":"digest","sha256":"c".repeat(64)},
        "environment":{"state":"digest","sha256":"d".repeat(64)},
        "required_evidence":[],"required_capabilities":["agentscm.review-context/v1"]
    })).unwrap();
    let state = authority.start_change(&context).unwrap();
    let approval = Approval {
        approval_id: format!("approval-{sequence}"),
        target: "main".into(),
        revision: revision.clone(),
        context_digest: state.context_digest.clone(),
        change: Some(state.id.clone()),
        change_version: Some(1),
        policy_epoch: 0,
    };
    authority.approve(&approval).unwrap();
    authority
        .integrate(
            engine,
            &IntegrateRequest {
                operation_id: format!("integrate-{sequence}"),
                target: "main".into(),
                expected_target: before,
                revision,
                context_digest: state.context_digest,
                approval_id: approval.approval_id,
                change: Some(state.id),
                change_version: Some(1),
                policy_epoch: 0,
            },
        )
        .unwrap()
}

fn change(path: &str, bytes: Option<&[u8]>) -> FileChange {
    FileChange {
        path: path.into(),
        content: bytes.map(Vec::from),
    }
}
fn request(event: &IntegrationEvent, expected_oid: Option<String>) -> ExportRequest {
    ExportRequest {
        event_sequence: event.sequence,
        expected_oid,
        metadata: CommitMetadata {
            name: "Snapshot test".into(),
            email: "test@example.invalid".into(),
            timestamp: 1780000000,
            message: "Export native snapshot".into(),
        },
    }
}
fn setup() -> (TempDir, Engine, Authority, PathBuf, Mirror) {
    let root = TempDir::new().unwrap();
    let engine = Engine::open(root.path().join("data")).unwrap();
    let authority = Authority::open(root.path().join("data")).unwrap();
    let path = root.path().join("mirror.git");
    let mirror =
        Mirror::initialize(&git(), &path, &authority.repository_id().unwrap(), "main").unwrap();
    (root, engine, authority, path, mirror)
}

#[test]
fn external_check_binds_historical_mapping_and_never_changes_native_authority() {
    let (root, engine, mut authority, path, mut mirror) = setup();
    let first = publish(&engine, &mut authority, vec![change("file", Some(b"one"))]);
    let one = mirror
        .export(&engine, &authority, &request(&first, None))
        .unwrap();
    let tested_oid = one.record.commit.clone().unwrap();
    let lookup = mirror.lookup_revision(&first.after.revision).unwrap();
    assert_eq!(lookup[0].commit.as_deref(), Some(tested_oid.as_str()));
    run(
        &path,
        &["fsck", "--no-reflogs", "--no-dangling", tested_oid.as_str()],
    );

    let second = publish(&engine, &mut authority, vec![change("file", Some(b"two"))]);
    mirror
        .export(
            &engine,
            &authority,
            &request(&second, one.record.commit.clone()),
        )
        .unwrap();
    let native_events = authority.events(0, 10).unwrap();
    let native_target = authority.target("main").unwrap();
    drop(authority);
    let native_bytes = fs::read(root.path().join("data/state.sqlite")).unwrap();

    let request = ExternalCheckRequest {
        event_sequence: first.sequence,
        result_id: "local-fsck-1".into(),
        tested_oid: tested_oid.clone(),
        outcome: ExternalCheckOutcome::Passed,
        details: Some("git fsck --no-reflogs --no-dangling TESTED_OID".into()),
    };
    let retained = mirror.record_external_check(&request).unwrap();
    assert_eq!(retained.tested_oid, tested_oid);
    assert_eq!(retained.revision, first.after.revision);
    assert_eq!(retained.record_kind, "external_check_result");
    assert_eq!(retained.provenance, "local_operator_unverified");
    assert_eq!(mirror.record_external_check(&request).unwrap(), retained);

    let mut wrong_oid = request.clone();
    wrong_oid.result_id = "wrong-oid".into();
    wrong_oid.tested_oid = "0".repeat(40);
    assert!(mirror.record_external_check(&wrong_oid).is_err());
    let mut changed_identity = request.clone();
    changed_identity.outcome = ExternalCheckOutcome::Failed;
    assert!(mirror.record_external_check(&changed_identity).is_err());
    assert_eq!(
        mirror.external_checks(Some(first.sequence)).unwrap(),
        [retained]
    );
    drop(mirror);

    let check_path = root.path().join("check.json");
    fs::write(&check_path, serde_json::to_vec(&request).unwrap()).unwrap();
    let recorded = agentscm(
        root.path(),
        &[
            "--data-dir",
            root.path().join("data").to_str().unwrap(),
            "git-export",
            "record-check",
            "--git",
            git().to_str().unwrap(),
            "--destination",
            path.to_str().unwrap(),
            check_path.to_str().unwrap(),
        ],
    );
    assert!(recorded.status.success(), "{:?}", recorded.stderr);
    let listed = agentscm(
        root.path(),
        &[
            "--data-dir",
            root.path().join("data").to_str().unwrap(),
            "git-export",
            "checks",
            "--git",
            git().to_str().unwrap(),
            "--destination",
            path.to_str().unwrap(),
            "--event",
            &first.sequence.to_string(),
        ],
    );
    assert!(listed.status.success(), "{:?}", listed.stderr);
    let listed: serde_json::Value = serde_json::from_slice(&listed.stdout).unwrap();
    assert_eq!(listed["checks"][0]["result_id"], "local-fsck-1");

    drop(engine);
    let backup = root.path().join("backup");
    local_recovery::backup(
        &root.path().join("data"),
        &backup,
        &[],
        std::slice::from_ref(&path),
        true,
    )
    .unwrap();
    local_recovery::verify(&backup).unwrap();

    let mirror = Mirror::open(&git(), &path, &one.record.repository).unwrap();
    assert_eq!(mirror.external_checks(None).unwrap().len(), 1);
    assert_eq!(
        fs::read(root.path().join("data/state.sqlite")).unwrap(),
        native_bytes
    );
    let authority = Authority::open_read_only(root.path().join("data")).unwrap();
    assert_eq!(authority.events(0, 10).unwrap(), native_events);
    assert_eq!(authority.target("main").unwrap(), native_target);
    assert!(authority.check_attempt("local-fsck-1").is_err());
    assert!(authority.check_evidence(&"0".repeat(64)).is_err());
}

#[test]
fn external_check_requires_mirrored_state_and_enforces_capacity() {
    let (_root, engine, mut authority, path, mut mirror) = setup();
    let event = publish(&engine, &mut authority, vec![change("file", Some(b"one"))]);
    let req = request(&event, None);
    let mut status = mirror.export(&engine, &authority, &req).unwrap();
    let oid = status.record.commit.clone().unwrap();
    status.record.state = ExportState::Error;
    replace_record(&path, &status.record);
    let observation = ExternalCheckRequest {
        event_sequence: event.sequence,
        result_id: "first".into(),
        tested_oid: oid,
        outcome: ExternalCheckOutcome::Passed,
        details: None,
    };
    assert!(mirror.record_external_check(&observation).is_err());
    status.record.state = ExportState::Mirrored;
    replace_record(&path, &status.record);
    mirror.record_external_check(&observation).unwrap();
    let db = rusqlite::Connection::open(path.join("agentscm-export.sqlite")).unwrap();
    db.execute_batch("WITH RECURSIVE n(v) AS (VALUES(1) UNION ALL SELECT v+1 FROM n WHERE v<255) INSERT INTO external_checks SELECT printf('filled-%04d',v),1,x'7b7d' FROM n;").unwrap();
    let mut overflow = observation;
    overflow.result_id = "overflow".into();
    let error = mirror.record_external_check(&overflow).unwrap_err();
    assert!(error.to_string().contains("full at 256"));
    let count: i64 = db
        .query_row("SELECT count(*) FROM external_checks", [], |row| row.get(0))
        .unwrap();
    assert_eq!(count, 256);
}

#[test]
fn seeded_lineage_is_copied_exactly_and_survives_source_removal() {
    let root = TempDir::new().unwrap();
    let source = root.path().join("source");
    let (first_seed, seed) = seed_repository(&source);
    fs::write(source.join("second"), b"dirty worktree bytes").unwrap();
    fs::write(source.join("untracked"), b"never copy").unwrap();
    let source_status = run(&source, &["status", "--porcelain"]);
    let source_config = fs::read(source.join(".git/config")).unwrap();

    let engine = Engine::open(root.path().join("data")).unwrap();
    let mut authority = Authority::open(root.path().join("data")).unwrap();
    let repository = authority.repository_id().unwrap();
    let destination = root.path().join("mirror.git");
    let mirror =
        Mirror::initialize_with_seed(&git(), &destination, &repository, "main", &source, &seed)
            .unwrap();
    assert_eq!(mirror.seed().unwrap().commit, seed);
    assert_eq!(
        run(&destination, &["cat-file", "-p", &first_seed]),
        run(&source, &["cat-file", "-p", &first_seed])
    );
    assert_eq!(
        run(&destination, &["cat-file", "-p", &seed]),
        run(&source, &["cat-file", "-p", &seed])
    );
    assert_eq!(run(&source, &["status", "--porcelain"]), source_status);
    assert_eq!(fs::read(source.join(".git/config")).unwrap(), source_config);

    fs::remove_dir_all(&source).unwrap();
    drop(mirror);
    let mut mirror = Mirror::open(&git(), &destination, &repository).unwrap();
    let event = publish(
        &engine,
        &mut authority,
        vec![
            change("native", Some(&[0, 128, 255])),
            change("empty", Some(b"")),
        ],
    );
    assert!(
        mirror
            .export(&engine, &authority, &request(&event, None))
            .is_err()
    );
    assert!(mirror.status(event.sequence).unwrap().is_none());
    assert_eq!(
        String::from_utf8(run(&destination, &["rev-parse", "refs/heads/agentscm"]))
            .unwrap()
            .trim(),
        seed
    );
    let exported = mirror
        .export(&engine, &authority, &request(&event, Some(seed.clone())))
        .unwrap();
    let commit = exported.record.commit.unwrap();
    assert_eq!(
        String::from_utf8(run(&destination, &["rev-parse", &format!("{commit}^")]))
            .unwrap()
            .trim(),
        seed
    );
    assert_eq!(
        run(&destination, &["show", &format!("{commit}:native")]),
        [0, 128, 255]
    );
    assert_eq!(
        run(&destination, &["show", &format!("{commit}:empty")]),
        b""
    );
    assert_eq!(
        run(&destination, &["ls-tree", "-r", "--name-only", &commit]),
        b"empty\nnative\n"
    );
}

#[test]
fn invalid_seed_inputs_fail_before_destination_creation() {
    let root = TempDir::new().unwrap();
    let source = root.path().join("source");
    let (_first, commit) = seed_repository(&source);
    let tag = {
        run(&source, &["tag", "-a", "seed-tag", "-m", "tag"]);
        String::from_utf8(run(&source, &["rev-parse", "seed-tag"]))
            .unwrap()
            .trim()
            .to_owned()
    };
    for (index, oid) in [
        "HEAD".to_owned(),
        commit[..12].to_owned(),
        "0".repeat(40),
        tag,
    ]
    .into_iter()
    .enumerate()
    {
        let destination = root.path().join(format!("rejected-{index}.git"));
        assert!(
            Mirror::initialize_with_seed(
                &git(),
                &destination,
                &"a".repeat(32),
                "main",
                &source,
                &oid
            )
            .is_err()
        );
        assert!(!destination.exists());
    }
}

#[test]
fn shallow_or_grafted_seed_history_is_rejected_before_creation() {
    for metadata in ["shallow", "info/grafts"] {
        let root = TempDir::new().unwrap();
        let source = root.path().join("source");
        let (_first, commit) = seed_repository(&source);
        let path = source.join(".git").join(metadata);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, format!("{commit}\n")).unwrap();
        let destination = root.path().join("mirror.git");
        assert!(
            Mirror::initialize_with_seed(
                &git(),
                &destination,
                &"a".repeat(32),
                "main",
                &source,
                &commit
            )
            .is_err()
        );
        assert!(!destination.exists());
    }
}

#[test]
fn missing_seed_history_remains_inspectable_and_blocks_export() {
    let root = TempDir::new().unwrap();
    let source = root.path().join("source");
    let (_first, seed) = seed_repository(&source);
    let seed_blob = String::from_utf8(run(&source, &["rev-parse", &format!("{seed}:old")]))
        .unwrap()
        .trim()
        .to_owned();
    let engine = Engine::open(root.path().join("data")).unwrap();
    let mut authority = Authority::open(root.path().join("data")).unwrap();
    let repository = authority.repository_id().unwrap();
    let destination = root.path().join("mirror.git");
    let mut mirror =
        Mirror::initialize_with_seed(&git(), &destination, &repository, "main", &source, &seed)
            .unwrap();
    let event = publish(
        &engine,
        &mut authority,
        vec![change("native", Some(b"one"))],
    );
    let req = request(&event, Some(seed.clone()));
    let original = mirror.export(&engine, &authority, &req).unwrap();
    let current = original.record.commit.clone().unwrap();
    drop(mirror);
    fs::remove_file(
        destination
            .join("objects")
            .join(&seed_blob[..2])
            .join(&seed_blob[2..]),
    )
    .unwrap();

    let mut mirror = Mirror::open(&git(), &destination, &repository).unwrap();
    let observed = mirror.status(event.sequence).unwrap().unwrap();
    assert_eq!(observed.record, original.record);
    assert!(observed.ref_matches);
    assert!(observed.object_error.is_some());
    assert!(mirror.export(&engine, &authority, &req).is_err());
    assert_eq!(
        String::from_utf8(run(&destination, &["rev-parse", "refs/heads/agentscm"]))
            .unwrap()
            .trim(),
        current
    );
}

#[test]
fn cli_requires_paired_seed_flags_and_exports_from_seed_parent() {
    let root = TempDir::new().unwrap();
    let data = root.path().join("data");
    let init = agentscm(root.path(), &["--data-dir", data.to_str().unwrap(), "init"]);
    assert!(init.status.success(), "{:?}", init.stderr);
    let source = root.path().join("source");
    let (_first, seed) = seed_repository(&source);
    let destination = root.path().join("mirror.git");
    let missing_pair = agentscm(
        root.path(),
        &[
            "--data-dir",
            data.to_str().unwrap(),
            "git-export",
            "init",
            "--destination",
            destination.to_str().unwrap(),
            "--seed-repository",
            source.to_str().unwrap(),
        ],
    );
    assert!(!missing_pair.status.success());
    assert!(!destination.exists());
    let seeded = agentscm(
        root.path(),
        &[
            "--data-dir",
            data.to_str().unwrap(),
            "git-export",
            "init",
            "--git",
            git().to_str().unwrap(),
            "--destination",
            destination.to_str().unwrap(),
            "--seed-repository",
            source.to_str().unwrap(),
            "--seed-commit",
            &seed,
        ],
    );
    assert!(seeded.status.success(), "{:?}", seeded.stderr);
    let initialized: serde_json::Value = serde_json::from_slice(&seeded.stdout).unwrap();
    assert_eq!(initialized["seed"]["commit"], seed);

    let engine = Engine::open(&data).unwrap();
    let mut authority = Authority::open(&data).unwrap();
    let event = publish(
        &engine,
        &mut authority,
        vec![change("native", Some(b"cli"))],
    );
    drop(authority);
    let request_path = root.path().join("request.json");
    fs::write(
        &request_path,
        serde_json::to_vec(&request(&event, Some(seed.clone()))).unwrap(),
    )
    .unwrap();
    let exported = agentscm(
        root.path(),
        &[
            "--data-dir",
            data.to_str().unwrap(),
            "git-export",
            "export",
            "--git",
            git().to_str().unwrap(),
            "--destination",
            destination.to_str().unwrap(),
            request_path.to_str().unwrap(),
        ],
    );
    assert!(exported.status.success(), "{:?}", exported.stderr);
    let output: serde_json::Value = serde_json::from_slice(&exported.stdout).unwrap();
    let commit = output["export"]["record"]["commit"].as_str().unwrap();
    assert_eq!(
        String::from_utf8(run(&destination, &["rev-parse", &format!("{commit}^")]))
            .unwrap()
            .trim(),
        seed
    );
}

#[test]
fn complete_trees_binary_bytes_deletions_and_two_integrations() {
    let (_root, engine, mut authority, path, mut mirror) = setup();
    let first = publish(
        &engine,
        &mut authority,
        vec![
            change("gone", Some(b"remove me")),
            change("dir/binary", Some(&[0, 255, 10, 13])),
            change("empty", Some(b"")),
            change("dir.z", Some(b"order")),
        ],
    );
    let original_event = authority.events(0, 10).unwrap();
    let one = mirror
        .export(&engine, &authority, &request(&first, None))
        .unwrap();
    assert_eq!(one.record.state, ExportState::Mirrored);
    assert!(one.ref_matches);
    let commit = one.record.commit.clone().unwrap();
    assert_eq!(
        run(&path, &["show", &format!("{commit}:dir/binary")]),
        [0, 255, 10, 13]
    );
    assert_eq!(
        run(&path, &["ls-tree", "-r", "--name-only", &commit]),
        b"dir.z\ndir/binary\nempty\ngone\n"
    );
    assert_eq!(authority.events(0, 10).unwrap(), original_event);
    let second = publish(
        &engine,
        &mut authority,
        vec![
            change("gone", None),
            change("added", Some(b"new")),
            change("dir/binary", Some(&[128, 0, 1])),
        ],
    );
    let two = mirror
        .export(&engine, &authority, &request(&second, Some(commit.clone())))
        .unwrap();
    let next = two.record.commit.clone().unwrap();
    assert_eq!(
        run(&path, &["ls-tree", "-r", "--name-only", &next]),
        b"added\ndir.z\ndir/binary\nempty\n"
    );
    assert_eq!(
        run(&path, &["show", &format!("{next}:dir/binary")]),
        [128, 0, 1]
    );
    assert_eq!(
        String::from_utf8(run(&path, &["rev-parse", &format!("{next}^")]))
            .unwrap()
            .trim(),
        commit
    );
    for line in String::from_utf8(run(&path, &["ls-tree", "-r", &next]))
        .unwrap()
        .lines()
    {
        assert!(line.starts_with("100644 blob "));
    }
    let mapping = mirror.lookup_revision(&second.after.revision).unwrap();
    assert_eq!(mapping.len(), 1);
    assert_eq!(mapping[0].commit, Some(next));
}

#[test]
fn fixed_request_retry_survives_reopen_and_rejects_metadata_change() {
    let (_root, engine, mut authority, path, mut mirror) = setup();
    let event = publish(&engine, &mut authority, vec![change("file", Some(b"one"))]);
    let req = request(&event, None);
    let one = mirror.export(&engine, &authority, &req).unwrap();
    drop(mirror);
    let mut mirror = Mirror::open(&git(), &path, &authority.repository_id().unwrap()).unwrap();
    assert_eq!(mirror.export(&engine, &authority, &req).unwrap(), one);
    let mut changed = req;
    changed.metadata.timestamp += 1;
    assert!(mirror.export(&engine, &authority, &changed).is_err());
    assert_eq!(mirror.status(event.sequence).unwrap().unwrap(), one);
}

fn replace_record(path: &Path, record: &ExportRecord) {
    let db = rusqlite::Connection::open(path.join("agentscm-export.sqlite")).unwrap();
    db.execute(
        "UPDATE exports SET record=?1 WHERE sequence=?2",
        rusqlite::params![serde_json::to_vec(record).unwrap(), record.event.sequence],
    )
    .unwrap();
}

#[test]
fn synthetic_interruption_before_cas_and_lost_reply_after_cas_reconcile_exact_commit() {
    let (_root, engine, mut authority, path, mut mirror) = setup();
    let event = publish(&engine, &mut authority, vec![change("file", Some(b"one"))]);
    let req = request(&event, None);
    let outcome = mirror.export(&engine, &authority, &req).unwrap();
    let commit = outcome.record.commit.clone().unwrap();
    // Model a retained verified record after ref CAS but before the journal acknowledgement.
    let mut interrupted = outcome.record.clone();
    interrupted.state = ExportState::Verified;
    replace_record(&path, &interrupted);
    assert_eq!(mirror.export(&engine, &authority, &req).unwrap(), outcome);
    // Model durable request saved before any objects/mapping or ref were published.
    run(&path, &["update-ref", "-d", "refs/heads/agentscm", &commit]);
    interrupted.state = ExportState::Pending;
    interrupted.tree = None;
    interrupted.commit = None;
    // A pending retry must not trust a retained owner/profile just because its
    // fixed request and freshly fetched Authority event still match.
    for (pointer, value) in [
        ("/profile", serde_json::json!("wrong-profile")),
        ("/repository", serde_json::json!("f".repeat(32))),
    ] {
        let mut changed = serde_json::to_value(&interrupted).unwrap();
        *changed.pointer_mut(pointer).unwrap() = value;
        let db = rusqlite::Connection::open(path.join("agentscm-export.sqlite")).unwrap();
        set_export_payload(&db, event.sequence, &serde_json::to_vec(&changed).unwrap());
        let before = fs::read(path.join("agentscm-export.sqlite")).unwrap();
        assert!(
            mirror.export(&engine, &authority, &req).is_err(),
            "{pointer}"
        );
        assert_eq!(
            fs::read(path.join("agentscm-export.sqlite")).unwrap(),
            before
        );
        assert!(
            run(
                &path,
                &[
                    "for-each-ref",
                    "--format=%(objectname)",
                    "refs/heads/agentscm"
                ]
            )
            .is_empty()
        );
    }
    replace_record(&path, &interrupted);
    drop(mirror);
    let mut mirror = Mirror::open(&git(), &path, &authority.repository_id().unwrap()).unwrap();
    assert_eq!(mirror.export(&engine, &authority, &req).unwrap(), outcome);
    assert_eq!(
        String::from_utf8(run(&path, &["rev-list", "--count", "refs/heads/agentscm"]))
            .unwrap()
            .trim(),
        "1"
    );
}

#[test]
fn unexpected_ref_movement_and_unfinished_export_cannot_be_overtaken() {
    let (_root, engine, mut authority, path, mut mirror) = setup();
    let first = publish(&engine, &mut authority, vec![change("file", Some(b"one"))]);
    let req = request(&first, None);
    let outcome = mirror.export(&engine, &authority, &req).unwrap();
    let commit = outcome.record.commit.clone().unwrap();
    run(&path, &["update-ref", "-d", "refs/heads/agentscm", &commit]);
    assert!(mirror.export(&engine, &authority, &req).is_err());
    assert!(!mirror.status(first.sequence).unwrap().unwrap().ref_matches);
    let second = publish(&engine, &mut authority, vec![change("file", Some(b"two"))]);
    assert!(
        mirror
            .export(&engine, &authority, &request(&second, None))
            .is_err()
    );
    assert!(mirror.status(second.sequence).unwrap().is_none());
    let mut pending = outcome.record;
    pending.state = ExportState::Verified;
    replace_record(&path, &pending);
    assert!(
        mirror
            .export(&engine, &authority, &request(&second, Some(commit)))
            .is_err()
    );
}

#[test]
fn unowned_or_reconfigured_destination_is_rejected_without_running_hooks_or_helpers() {
    let (root, engine, mut authority, path, mut mirror) = setup();
    assert!(
        Mirror::initialize(&git(), &path, &authority.repository_id().unwrap(), "main").is_err()
    );
    let unowned = root.path().join("unowned");
    fs::create_dir(&unowned).unwrap();
    assert!(Mirror::open(&git(), &unowned, &authority.repository_id().unwrap()).is_err());
    let event = publish(
        &engine,
        &mut authority,
        vec![
            change(".gitattributes", Some(b"* filter=evil")),
            change("file", Some(b"bytes")),
        ],
    );
    let marker = root.path().join("executed");
    let hooks = path.join("hooks");
    fs::create_dir(&hooks).unwrap();
    fs::write(
        hooks.join("reference-transaction"),
        format!("#!/bin/sh\ntouch '{}'\n", marker.display()),
    )
    .unwrap();
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(
        hooks.join("reference-transaction"),
        fs::Permissions::from_mode(0o755),
    )
    .unwrap();
    mirror
        .export(&engine, &authority, &request(&event, None))
        .unwrap();
    assert!(!marker.exists());
    let mut config = fs::read_to_string(path.join("config")).unwrap();
    config.push_str(&format!(
        "\n[filter \"evil\"]\n clean = touch '{}'\n[gpg]\n program = touch '{}'\n",
        marker.display(),
        marker.display()
    ));
    fs::write(path.join("config"), config).unwrap();
    assert!(
        mirror
            .export(&engine, &authority, &request(&event, None))
            .is_err()
    );
    assert!(!marker.exists());
}

#[test]
fn every_shared_reserved_namespace_is_rejected_and_retains_request() {
    for namespace in [".GIT", ".AgentSCM", ".AGENTSCM-workspace", "AgentsCM-Data"] {
        let (_root, engine, mut authority, _path, mut mirror) = setup();
        let event = publish(
            &engine,
            &mut authority,
            vec![change(&format!("outer/{namespace}/secret"), Some(b"no"))],
        );
        assert!(
            mirror
                .export(&engine, &authority, &request(&event, None))
                .is_err(),
            "reserved namespace {namespace}"
        );
        let state = mirror.status(event.sequence).unwrap().unwrap();
        assert_eq!(state.record.state, ExportState::Error, "{namespace}");
        assert!(state.current_oid.is_none(), "{namespace}");
        assert!(state.record.commit.is_none(), "{namespace}");
        assert!(
            state
                .record
                .error
                .as_deref()
                .is_some_and(|error| error.contains("reserved namespace")),
            "{namespace}: {:?}",
            state.record.error
        );
    }
}

#[test]
fn writer_lock_and_unknown_or_wrong_authority_event_fail_closed() {
    let (root, engine, mut authority, path, mut mirror) = setup();
    let event = publish(&engine, &mut authority, vec![change("file", Some(b"one"))]);
    let lock = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(path.join("agentscm-export.lock"))
        .unwrap();
    lock.try_lock().unwrap();
    assert!(
        mirror
            .export(&engine, &authority, &request(&event, None))
            .is_err()
    );
    drop(lock);
    let other = Authority::open(root.path().join("other-data")).unwrap();
    assert!(
        mirror
            .export(&engine, &other, &request(&event, None))
            .is_err()
    );
    let mut absent = request(&event, None);
    absent.event_sequence += 10;
    assert!(mirror.export(&engine, &authority, &absent).is_err());
    assert!(mirror.status(event.sequence).unwrap().is_none());
}

#[test]
fn deleting_last_file_exports_an_empty_complete_tree() {
    let (_root, engine, mut authority, path, mut mirror) = setup();
    let first = publish(
        &engine,
        &mut authority,
        vec![change("only/file", Some(b"one"))],
    );
    let one = mirror
        .export(&engine, &authority, &request(&first, None))
        .unwrap();
    let second = publish(&engine, &mut authority, vec![change("only/file", None)]);
    let two = mirror
        .export(&engine, &authority, &request(&second, one.record.commit))
        .unwrap();
    assert!(
        run(
            &path,
            &[
                "ls-tree",
                "-r",
                "--name-only",
                two.record.commit.as_ref().unwrap()
            ]
        )
        .is_empty()
    );
    assert_eq!(
        two.record.tree.as_deref(),
        Some("4b825dc642cb6eb9a060e54bf8d69288fbee4904")
    );
}

#[test]
fn nested_destination_is_rejected_before_creating_it() {
    let root = TempDir::new().unwrap();
    let checkout = root.path().join("checkout");
    fs::create_dir(&checkout).unwrap();
    // A .git directory or linked-worktree .git file is sufficient for rejection;
    // no Git discovery command or source-controlled helper is needed.
    fs::write(checkout.join(".git"), b"gitdir: /not/read").unwrap();
    let destination = checkout.join("mirror.git");
    assert!(Mirror::initialize(&git(), &destination, &"a".repeat(32), "main").is_err());
    assert!(!destination.exists());
    assert_eq!(
        fs::read(checkout.join(".git")).unwrap(),
        b"gitdir: /not/read"
    );
}

#[test]
fn missing_snapshot_objects_are_not_false_success_and_can_be_rebuilt() {
    let (_root, engine, mut authority, path, mut mirror) = setup();
    let event = publish(&engine, &mut authority, vec![change("file", Some(b"one"))]);
    let req = request(&event, None);
    let original = mirror.export(&engine, &authority, &req).unwrap();
    let commit = original.record.commit.as_ref().unwrap();
    let blob = String::from_utf8(run(&path, &["rev-parse", &format!("{commit}:file")]))
        .unwrap()
        .trim()
        .to_owned();
    let object = path.join("objects").join(&blob[..2]).join(&blob[2..]);
    fs::remove_file(&object).unwrap();
    let observed = mirror.status(event.sequence).unwrap().unwrap();
    assert!(observed.ref_matches);
    assert!(observed.object_error.is_some());
    assert_eq!(mirror.export(&engine, &authority, &req).unwrap(), original);
    assert!(object.exists());
}

#[test]
fn missing_historical_blob_blocks_new_export_without_moving_ref() {
    let (_root, engine, mut authority, path, mut mirror) = setup();
    let first = publish(&engine, &mut authority, vec![change("file", Some(b"old"))]);
    let original = mirror
        .export(&engine, &authority, &request(&first, None))
        .unwrap();
    let commit = original.record.commit.unwrap();
    let blob = String::from_utf8(run(&path, &["rev-parse", &format!("{commit}:file")]))
        .unwrap()
        .trim()
        .to_owned();
    let historical_object = path.join("objects").join(&blob[..2]).join(&blob[2..]);
    let historical_bytes = fs::read(&historical_object).unwrap();
    fs::remove_file(&historical_object).unwrap();
    let second = publish(&engine, &mut authority, vec![change("file", Some(b"new"))]);
    assert!(
        mirror
            .export(&engine, &authority, &request(&second, Some(commit.clone())))
            .is_err()
    );
    let mappings = mirror.lookup_revision(&second.after.revision).unwrap();
    assert_eq!(mappings.len(), 1);
    assert_eq!(mappings[0].state, ExportState::Error);
    assert_eq!(authority.target("main").unwrap().unwrap(), second.after);
    assert_eq!(
        String::from_utf8(run(&path, &["rev-parse", "refs/heads/agentscm"]))
            .unwrap()
            .trim(),
        commit
    );
    // This is a real retained Error from failed parent verification, before
    // either new OID was assigned. Owner/profile corruption must refuse retry.
    let failed = mappings[0].clone();
    for (pointer, value) in [
        ("/profile", serde_json::json!("wrong-profile")),
        ("/repository", serde_json::json!("f".repeat(32))),
    ] {
        let mut changed = serde_json::to_value(&failed).unwrap();
        *changed.pointer_mut(pointer).unwrap() = value;
        let db = rusqlite::Connection::open(path.join("agentscm-export.sqlite")).unwrap();
        set_export_payload(&db, second.sequence, &serde_json::to_vec(&changed).unwrap());
        let before = fs::read(path.join("agentscm-export.sqlite")).unwrap();
        assert!(
            mirror
                .export(&engine, &authority, &request(&second, Some(commit.clone())))
                .is_err(),
            "{pointer}"
        );
        assert_eq!(
            fs::read(path.join("agentscm-export.sqlite")).unwrap(),
            before
        );
    }
    replace_record(&path, &failed);
    fs::write(historical_object, historical_bytes).unwrap();
    let retried = mirror
        .export(&engine, &authority, &request(&second, Some(commit)))
        .unwrap();
    assert_eq!(retried.record.state, ExportState::Mirrored);
    assert!(retried.ref_matches);
}

#[test]
fn invalid_native_target_is_rejected_before_destination_creation() {
    let root = TempDir::new().unwrap();
    for (index, target) in [
        "bad target".to_owned(),
        "x".repeat(129),
        "".to_owned(),
        "tärgét".to_owned(),
    ]
    .into_iter()
    .enumerate()
    {
        let destination = root.path().join(format!("invalid-{index}"));
        assert!(Mirror::initialize(&git(), &destination, &"a".repeat(32), &target).is_err());
        assert!(!destination.exists());
    }
}

#[test]
fn ref_movement_in_the_update_window_is_a_retained_cas_failure() {
    let (root, engine, mut authority, path, mut mirror) = setup();
    let first = publish(&engine, &mut authority, vec![change("file", Some(b"one"))]);
    let original = mirror
        .export(&engine, &authority, &request(&first, None))
        .unwrap();
    let commit = original.record.commit.unwrap();
    let second = publish(&engine, &mut authority, vec![change("file", Some(b"two"))]);
    // The trusted operator-selected test Git wrapper moves the ref exactly when
    // the exporter submits update-ref. Real Git then enforces the old-OID CAS.
    let wrapper = root.path().join("race-git");
    fn quote(path: &Path) -> String {
        format!("'{}'", path.to_str().unwrap().replace('\'', "'\\''"))
    }
    fs::write(&wrapper,format!("#!/bin/sh\nfor arg in \"$@\"; do\n if [ \"$arg\" = update-ref ]; then\n  {} -C {} -c core.hooksPath=/dev/null update-ref -d refs/heads/agentscm {} || exit 90\n fi\ndone\nexec {} \"$@\"\n",quote(&git()),quote(&path),commit,quote(&git()))).unwrap();
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(&wrapper, fs::Permissions::from_mode(0o755)).unwrap();
    drop(mirror);
    let mut mirror = Mirror::open(&wrapper, &path, &authority.repository_id().unwrap()).unwrap();
    assert!(
        mirror
            .export(&engine, &authority, &request(&second, Some(commit)))
            .is_err()
    );
    let status = mirror.status(second.sequence).unwrap().unwrap();
    assert_eq!(status.record.state, ExportState::Error);
    assert!(status.record.tree.is_some() && status.record.commit.is_some());
    assert!(status.current_oid.is_none());
    assert!(!status.ref_matches);
    assert_eq!(authority.target("main").unwrap().unwrap(), second.after);
}

#[test]
fn full_journal_rejects_new_event_before_inserting_the_100001st_row() {
    let (_root, engine, mut authority, path, mut mirror) = setup();
    let event = publish(&engine, &mut authority, vec![change("file", Some(b"one"))]);
    let db = rusqlite::Connection::open(path.join("agentscm-export.sqlite")).unwrap();
    // Minimal opaque row payloads keep this capacity fixture small. The limit
    // must be enforced before parsing earlier records or inserting a new row.
    db.execute_batch("WITH RECURSIVE n(v) AS (VALUES(1000) UNION ALL SELECT v+1 FROM n WHERE v<100999) INSERT INTO exports SELECT v,x'' FROM n;").unwrap();
    let error = mirror
        .export(&engine, &authority, &request(&event, None))
        .unwrap_err();
    assert!(error.to_string().contains("journal is full"));
    let count: i64 = db
        .query_row("SELECT count(*) FROM exports", [], |r| r.get(0))
        .unwrap();
    assert_eq!(count, 100000);
    assert!(mirror.status(event.sequence).unwrap().is_none());
}

#[test]
fn mirror_handle_holds_recovery_lease_until_journal_is_closed() {
    let (_root, _engine, _authority, path, mirror) = setup();
    assert!(agentscm::local_recovery::AccessGuard::exclusive(&path).is_err());
    drop(mirror);
    assert!(agentscm::local_recovery::AccessGuard::exclusive(&path).is_ok());
}

#[test]
fn recovery_validates_seeded_mirror_ownership_lease_and_journal() {
    let root = TempDir::new().unwrap();
    let source = root.path().join("source");
    let (_first_seed, seed) = seed_repository(&source);
    let source_status = run(&source, &["status", "--porcelain"]);
    let source_config = fs::read(source.join(".git/config")).unwrap();
    let data = root.path().join("data");
    let engine = Engine::open(&data).unwrap();
    let mut authority = Authority::open(&data).unwrap();
    let repository = authority.repository_id().unwrap();
    let path = root.path().join("mirror.git");
    let mut mirror =
        Mirror::initialize_with_seed(&git(), &path, &repository, "main", &source, &seed).unwrap();
    let event = publish(
        &engine,
        &mut authority,
        vec![change("native", Some(b"one"))],
    );
    let exported = mirror
        .export(&engine, &authority, &request(&event, Some(seed)))
        .unwrap();
    let commit = exported.record.commit.clone().unwrap();
    let native_events = authority.events(0, 10).unwrap();
    let native_target = authority.target("main").unwrap();
    let owner = fs::read(path.join("agentscm-mirror.json")).unwrap();
    let mirror_config = fs::read(path.join("config")).unwrap();
    let owned_ref = fs::read(path.join("refs/heads/agentscm")).unwrap();
    drop(authority);
    drop(engine);

    let active_backup = root.path().join("active-backup");
    assert!(
        local_recovery::backup(
            &data,
            &active_backup,
            &[],
            std::slice::from_ref(&path),
            true,
        )
        .is_err()
    );
    assert!(!active_backup.exists());
    drop(mirror);

    let version_one_backup = root.path().join("version-one-backup");
    local_recovery::backup(
        &data,
        &version_one_backup,
        &[],
        std::slice::from_ref(&path),
        true,
    )
    .unwrap();
    local_recovery::verify(&version_one_backup).unwrap();

    let mut mirror = Mirror::open(&git(), &path, &repository).unwrap();
    mirror
        .record_external_check(&ExternalCheckRequest {
            event_sequence: event.sequence,
            result_id: "seeded-recovery-check".into(),
            tested_oid: commit,
            outcome: ExternalCheckOutcome::Passed,
            details: None,
        })
        .unwrap();
    drop(mirror);
    let version_two_backup = root.path().join("version-two-backup");
    local_recovery::backup(
        &data,
        &version_two_backup,
        &[],
        std::slice::from_ref(&path),
        true,
    )
    .unwrap();
    local_recovery::verify(&version_two_backup).unwrap();

    // Qualify the copied schema-v2 journal through restore and activation too.
    // Ownership stays bound to the original path; restoration does not silently
    // authorize the copied mirror as a new writable Git destination.
    let restored = root.path().join("restored-version-two");
    local_recovery::restore(&version_two_backup, &restored).unwrap();
    let restored_data = restored.join("data");
    assert!(Engine::open(&restored_data).is_err());
    assert!(Authority::open(&restored_data).is_err());
    assert_eq!(local_recovery::activate(&restored, true, true).unwrap(), 1);
    let restored_authority = Authority::open_read_only(&restored_data).unwrap();
    assert_eq!(restored_authority.events(0, 10).unwrap(), native_events);
    assert_eq!(restored_authority.target("main").unwrap(), native_target);
    let restored_mirror = restored.join("attachments/0");
    assert_eq!(
        fs::read(restored_mirror.join("agentscm-mirror.json")).unwrap(),
        owner
    );
    assert_eq!(
        fs::read(restored_mirror.join("config")).unwrap(),
        mirror_config
    );
    assert_eq!(
        fs::read(restored_mirror.join("refs/heads/agentscm")).unwrap(),
        owned_ref
    );
    assert!(Mirror::open(&git(), &restored_mirror, &repository).is_err());
    run(&restored_mirror, &["fsck", "--no-reflogs", "--no-dangling"]);
    drop(restored_authority);

    fs::OpenOptions::new()
        .write(true)
        .open(version_two_backup.join("payload/attachments/0/agentscm-export.sqlite"))
        .unwrap()
        .set_len(1)
        .unwrap();
    assert!(local_recovery::verify(&version_two_backup).is_err());

    let authority = Authority::open_read_only(&data).unwrap();
    assert_eq!(authority.events(0, 10).unwrap(), native_events);
    assert_eq!(authority.target("main").unwrap(), native_target);
    drop(authority);
    assert_eq!(fs::read(path.join("agentscm-mirror.json")).unwrap(), owner);
    assert_eq!(fs::read(path.join("config")).unwrap(), mirror_config);
    assert_eq!(
        fs::read(path.join("refs/heads/agentscm")).unwrap(),
        owned_ref
    );
    assert_eq!(run(&source, &["status", "--porcelain"]), source_status);
    assert_eq!(fs::read(source.join(".git/config")).unwrap(), source_config);

    let original_owner: serde_json::Value = serde_json::from_slice(&owner).unwrap();
    for (index, changed) in [
        {
            let mut value = original_owner.clone();
            value["version"] = 3.into();
            value
        },
        {
            let mut value = original_owner.clone();
            value.as_object_mut().unwrap().remove("seed");
            value
        },
        {
            let mut value = original_owner.clone();
            value["seed"]["commit"] = "A".repeat(40).into();
            value
        },
    ]
    .into_iter()
    .enumerate()
    {
        fs::write(
            path.join("agentscm-mirror.json"),
            serde_json::to_vec(&changed).unwrap(),
        )
        .unwrap();
        let backup = root.path().join(format!("invalid-owner-{index}"));
        assert!(
            local_recovery::backup(&data, &backup, &[], std::slice::from_ref(&path), true,)
                .is_err()
        );
        assert!(!backup.exists());
    }
    fs::write(path.join("agentscm-mirror.json"), &owner).unwrap();
    let journal = fs::read(path.join("agentscm-export.sqlite")).unwrap();
    // Physically valid schema-v2 databases still fail backup before publication
    // when a required product relation or column is absent/incompatible.
    for (index, mutation) in [
        "DROP TABLE external_checks",
        "ALTER TABLE external_checks RENAME COLUMN result_id TO wrong_identity",
        "ALTER TABLE external_checks RENAME COLUMN event_sequence TO wrong_event",
        "ALTER TABLE external_checks RENAME COLUMN record TO wrong_record",
        "DROP TABLE exports",
        "ALTER TABLE exports RENAME COLUMN sequence TO wrong_sequence",
        "ALTER TABLE exports RENAME COLUMN record TO wrong_record",
    ]
    .into_iter()
    .enumerate()
    {
        fs::write(path.join("agentscm-export.sqlite"), &journal).unwrap();
        {
            let db = rusqlite::Connection::open(path.join("agentscm-export.sqlite")).unwrap();
            db.execute_batch(mutation).unwrap();
            let integrity: String = db
                .query_row("PRAGMA integrity_check", [], |row| row.get(0))
                .unwrap();
            assert_eq!(integrity, "ok");
            assert_eq!(
                db.pragma_query_value::<i64, _>(None, "user_version", |row| row.get(0))
                    .unwrap(),
                2
            );
        }
        let backup = root.path().join(format!("wrong-export-schema-{index}"));
        assert!(
            local_recovery::backup(&data, &backup, &[], std::slice::from_ref(&path), true).is_err(),
            "{mutation}"
        );
        assert!(!backup.exists(), "{mutation}");
    }
    fs::write(path.join("agentscm-export.sqlite"), &journal).unwrap();
    fs::remove_file(path.join("agentscm-export.sqlite")).unwrap();
    let missing_journal_backup = root.path().join("missing-journal-backup");
    assert!(
        local_recovery::backup(
            &data,
            &missing_journal_backup,
            &[],
            std::slice::from_ref(&path),
            true,
        )
        .is_err()
    );
    assert!(!missing_journal_backup.exists());
    fs::write(path.join("agentscm-export.sqlite"), &journal[..1]).unwrap();
    let truncated_journal_backup = root.path().join("truncated-journal-backup");
    assert!(
        local_recovery::backup(
            &data,
            &truncated_journal_backup,
            &[],
            std::slice::from_ref(&path),
            true,
        )
        .is_err()
    );
    assert!(!truncated_journal_backup.exists());
}

#[test]
fn rollback_to_an_older_mapped_commit_does_not_make_a_stale_retry_succeed() {
    let (_root, engine, mut authority, path, mut mirror) = setup();
    let first = publish(&engine, &mut authority, vec![change("file", Some(b"one"))]);
    let first_request = request(&first, None);
    let one = mirror.export(&engine, &authority, &first_request).unwrap();
    let c1 = one.record.commit.clone().unwrap();
    let second = publish(&engine, &mut authority, vec![change("file", Some(b"two"))]);
    let two = mirror
        .export(&engine, &authority, &request(&second, Some(c1.clone())))
        .unwrap();
    let c2 = two.record.commit.as_ref().unwrap();
    run(
        &path,
        &["update-ref", "--no-deref", "refs/heads/agentscm", &c1, c2],
    );
    assert!(mirror.export(&engine, &authority, &first_request).is_err());
    assert_eq!(
        mirror.lookup_revision(&first.after.revision).unwrap()[0],
        one.record
    );
    assert_eq!(
        mirror.lookup_revision(&second.after.revision).unwrap()[0],
        two.record
    );
    assert_eq!(
        String::from_utf8(run(&path, &["rev-parse", "refs/heads/agentscm"]))
            .unwrap()
            .trim(),
        c1
    );
}

#[test]
fn failed_native_reverification_preserves_historical_mirrored_record() {
    let (root, engine, mut authority, path, mut mirror) = setup();
    let event = publish(&engine, &mut authority, vec![change("file", Some(b"one"))]);
    let req = request(&event, None);
    let original = mirror.export(&engine, &authority, &req).unwrap();
    let journal = fs::read(path.join("agentscm-export.sqlite")).unwrap();
    // An empty independent native source store cannot read this revision. Git
    // remains healthy; failure to collect fresh evidence must not alter history.
    let empty = Engine::open(root.path().join("empty-source")).unwrap();
    assert!(mirror.export(&empty, &authority, &req).is_err());
    assert_eq!(mirror.status(event.sequence).unwrap().unwrap(), original);
    assert_eq!(
        fs::read(path.join("agentscm-export.sqlite")).unwrap(),
        journal
    );
    assert_eq!(mirror.export(&engine, &authority, &req).unwrap(), original);
    assert_eq!(
        fs::read(path.join("agentscm-export.sqlite")).unwrap(),
        journal
    );
}

#[test]
fn dangling_symbolic_owned_ref_is_rejected_and_left_untouched() {
    let (_root, engine, mut authority, path, mut mirror) = setup();
    let event = publish(&engine, &mut authority, vec![change("file", Some(b"one"))]);
    run(
        &path,
        &["symbolic-ref", "refs/heads/agentscm", "refs/heads/missing"],
    );
    let before = fs::read(path.join("refs/heads/agentscm")).unwrap();
    assert!(
        mirror
            .export(&engine, &authority, &request(&event, None))
            .is_err()
    );
    assert_eq!(fs::read(path.join("refs/heads/agentscm")).unwrap(), before);
    assert!(!path.join("refs/heads/missing").exists());
    assert!(
        mirror
            .lookup_revision(&event.after.revision)
            .unwrap()
            .is_empty()
    );
}

#[test]
fn quarantined_restore_rejects_mirror_initialization_before_mkdir() {
    let root = TempDir::new().unwrap();
    let restored = root.path().join("restored");
    fs::create_dir_all(restored.join(".agentscm-recovery")).unwrap();
    fs::write(restored.join(".agentscm-recovery/quarantine.json"), b"{}").unwrap();
    let destination = restored.join("new-mirror.git");
    assert!(Mirror::initialize(&git(), &destination, &"a".repeat(32), "main").is_err());
    assert!(!destination.exists());
    assert_eq!(
        fs::read(restored.join(".agentscm-recovery/quarantine.json")).unwrap(),
        b"{}"
    );
}

#[test]
fn repository_redirection_metadata_cannot_write_into_another_owned_mirror() {
    fn inventory(root: &Path) -> Vec<(PathBuf, Option<Vec<u8>>)> {
        let mut pending = vec![root.to_path_buf()];
        let mut entries = Vec::new();
        while let Some(path) = pending.pop() {
            let metadata = fs::symlink_metadata(&path).unwrap();
            assert!(!metadata.file_type().is_symlink());
            let bytes = if metadata.is_dir() {
                pending.extend(
                    fs::read_dir(&path)
                        .unwrap()
                        .map(|entry| entry.unwrap().path()),
                );
                None
            } else {
                Some(fs::read(&path).unwrap())
            };
            entries.push((path.strip_prefix(root).unwrap().to_path_buf(), bytes));
        }
        entries.sort();
        entries
    }
    let (root, engine, mut authority, path, mut mirror) = setup();
    let repository = authority.repository_id().unwrap();
    let other_path = root.path().join("other.git");
    let _other = Mirror::initialize(&git(), &other_path, &repository, "main").unwrap();
    let event = publish(
        &engine,
        &mut authority,
        vec![change("file", Some(b"must stay native"))],
    );
    let req = request(&event, None);
    // Both mirrors are initially empty with identical configs. Without the
    // discovery check, commondir could satisfy bare/config/ref checks using B
    // while the exporter holds only A's journal and locks.
    assert_eq!(
        fs::read(path.join("config")).unwrap(),
        fs::read(other_path.join("config")).unwrap()
    );
    let before: Vec<_> = [&path, &other_path]
        .into_iter()
        .map(|p| {
            (
                fs::read(p.join("agentscm-export.sqlite")).unwrap(),
                inventory(&p.join("objects")),
                inventory(&p.join("refs")),
            )
        })
        .collect();
    for name in ["commondir", ".git", "gitdir"] {
        for symlink in [false, true] {
            let marker = path.join(name);
            if symlink {
                std::os::unix::fs::symlink(root.path().join("missing"), &marker).unwrap();
            } else {
                let prefix = if name == ".git" { "gitdir: " } else { "" };
                fs::write(&marker, format!("{prefix}{}\n", other_path.display())).unwrap();
            }
            for error in [
                mirror.export(&engine, &authority, &req).err().unwrap(),
                Mirror::open(&git(), &path, &repository).err().unwrap(),
            ] {
                assert!(
                    error
                        .to_string()
                        .contains(&format!("unsupported mirror metadata: {name}")),
                    "{error}"
                );
            }
            for (p, expected) in [&path, &other_path].into_iter().zip(&before) {
                assert_eq!(
                    &(
                        fs::read(p.join("agentscm-export.sqlite")).unwrap(),
                        inventory(&p.join("objects")),
                        inventory(&p.join("refs"))
                    ),
                    expected
                );
                assert!(!p.join("refs/heads/agentscm").exists());
            }
            fs::remove_file(marker).unwrap();
        }
    }
}

fn stored_result_request(event: &IntegrationEvent, mapping: &ExportRecord) -> ExternalCheckRequest {
    ExternalCheckRequest {
        event_sequence: event.sequence,
        result_id: "retained-result".into(),
        tested_oid: mapping.commit.clone().unwrap(),
        outcome: ExternalCheckOutcome::Failed,
        details: Some("synthetic operator declaration; no CI execution".into()),
    }
}

fn set_export_payload(db: &rusqlite::Connection, sequence: u64, bytes: &[u8]) {
    db.execute(
        "UPDATE exports SET record=?1 WHERE sequence=?2",
        rusqlite::params![bytes, sequence],
    )
    .unwrap();
}

#[test]
fn malformed_mapping_is_refused_before_result_schema_migration_and_on_result_reads() {
    let (_root, engine, mut authority, path, mut mirror) = setup();
    let event = publish(&engine, &mut authority, vec![change("file", Some(b"one"))]);
    let original = mirror
        .export(&engine, &authority, &request(&event, None))
        .unwrap()
        .record;
    let declaration = stored_result_request(&event, &original);
    let db = rusqlite::Connection::open(path.join("agentscm-export.sqlite")).unwrap();
    for (pointer, value) in [
        ("/profile", serde_json::json!("other-profile")),
        ("/repository", serde_json::json!("f".repeat(32))),
        ("/event/target", serde_json::json!("other-target")),
        ("/event/sequence", serde_json::json!(event.sequence + 1)),
        (
            "/request/event_sequence",
            serde_json::json!(event.sequence + 1),
        ),
        ("/event/after/revision", serde_json::json!("bad")),
        ("/event/after/version", serde_json::json!(0)),
        ("/event/after/version", serde_json::json!(u64::MAX)),
        ("/event/semantics_version", serde_json::json!(0)),
        ("/event/semantics_version", serde_json::json!(3)),
        ("/event/operation_id", serde_json::json!("")),
        ("/event/approval_id", serde_json::json!("bad identifier")),
        ("/event/context_digest", serde_json::json!("A".repeat(64))),
        ("/event/policy_epoch", serde_json::json!(u64::MAX)),
        ("/event/change", serde_json::Value::Null),
        ("/event/change_version", serde_json::Value::Null),
        ("/event/change_version", serde_json::json!(0)),
        (
            "/event/change/repository",
            serde_json::json!("f".repeat(32)),
        ),
        (
            "/event/before",
            serde_json::json!({"revision":"bad","version":1}),
        ),
        (
            "/event/before",
            serde_json::json!({"revision":"a".repeat(64),"version":0}),
        ),
        (
            "/event/before",
            serde_json::json!({"revision":"a".repeat(64),"version":1}),
        ),
        ("/state", serde_json::json!("pending")),
        ("/state", serde_json::json!("verified")),
        ("/state", serde_json::json!("error")),
        ("/error", serde_json::json!("unfinished")),
        ("/commit", serde_json::Value::Null),
        ("/commit", serde_json::json!("f".repeat(40))),
        ("/tree", serde_json::Value::Null),
        ("/tree", serde_json::json!("BAD")),
        ("/request/metadata/message", serde_json::json!("")),
    ] {
        let mut changed = serde_json::to_value(&original).unwrap();
        *changed.pointer_mut(pointer).unwrap() = value;
        set_export_payload(&db, event.sequence, &serde_json::to_vec(&changed).unwrap());
        assert!(
            mirror.record_external_check(&declaration).is_err(),
            "{pointer}"
        );
        let version: i64 = db
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .unwrap();
        assert_eq!(version, 1);
        let tables: i64 = db
            .query_row(
                "SELECT count(*) FROM sqlite_schema WHERE name='external_checks'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(tables, 0);
    }
    replace_record(&path, &original);
    let retained = mirror.record_external_check(&declaration).unwrap();
    let mut changed = original.clone();
    changed.repository = "f".repeat(32);
    replace_record(&path, &changed);
    assert!(mirror.external_checks(None).is_err());
    assert!(mirror.record_external_check(&declaration).is_err());
    replace_record(&path, &original);
    assert_eq!(mirror.external_checks(None).unwrap(), [retained]);
}

#[test]
fn persisted_results_validate_sql_keys_envelopes_and_historical_mapping_on_retry() {
    let (_root, engine, mut authority, path, mut mirror) = setup();
    let first = publish(&engine, &mut authority, vec![change("file", Some(b"one"))]);
    let mapping = mirror
        .export(&engine, &authority, &request(&first, None))
        .unwrap()
        .record;
    let declaration = stored_result_request(&first, &mapping);
    let retained = mirror.record_external_check(&declaration).unwrap();
    let bytes = serde_json::to_vec(&retained).unwrap();
    let db = rusqlite::Connection::open(path.join("agentscm-export.sqlite")).unwrap();
    for (pointer, value) in [
        ("/result_id", serde_json::json!("other-result")),
        ("/event_sequence", serde_json::json!(first.sequence + 1)),
        ("/repository", serde_json::json!("f".repeat(32))),
        ("/revision", serde_json::json!("f".repeat(64))),
        ("/tree", serde_json::json!("f".repeat(40))),
        ("/tested_oid", serde_json::json!("f".repeat(40))),
        ("/record_kind", serde_json::json!("native_check_evidence")),
        ("/provenance", serde_json::json!("authenticated")),
        ("/outcome", serde_json::json!("unknown")),
        ("/details", serde_json::json!("x".repeat(4097))),
        ("/details", serde_json::json!("nul\0")),
    ] {
        let mut changed = serde_json::to_value(&retained).unwrap();
        *changed.pointer_mut(pointer).unwrap() = value;
        db.execute(
            "UPDATE external_checks SET record=?1 WHERE result_id=?2",
            rusqlite::params![serde_json::to_vec(&changed).unwrap(), declaration.result_id],
        )
        .unwrap();
        assert!(mirror.external_checks(None).is_err(), "{pointer}");
        assert!(
            mirror.external_checks(Some(first.sequence)).is_err(),
            "{pointer}"
        );
        assert!(
            mirror.record_external_check(&declaration).is_err(),
            "{pointer}"
        );
    }
    db.execute("UPDATE external_checks SET record=?1", [&bytes])
        .unwrap();
    db.execute(
        "UPDATE external_checks SET event_sequence=?1",
        [first.sequence + 1],
    )
    .unwrap();
    assert!(mirror.external_checks(None).is_err());
    assert!(mirror.record_external_check(&declaration).is_err());
    db.execute(
        "UPDATE external_checks SET event_sequence=?1",
        [first.sequence],
    )
    .unwrap();
    db.execute("UPDATE external_checks SET result_id='other-key'", [])
        .unwrap();
    assert!(mirror.external_checks(None).is_err());
    let mut renamed = declaration.clone();
    renamed.result_id = "other-key".into();
    assert!(mirror.record_external_check(&renamed).is_err());
    db.execute(
        "UPDATE external_checks SET result_id=?1",
        [&declaration.result_id],
    )
    .unwrap();
    let second = publish(&engine, &mut authority, vec![change("file", Some(b"two"))]);
    let newer = mirror
        .export(
            &engine,
            &authority,
            &request(&second, mapping.commit.clone()),
        )
        .unwrap();
    let before = fs::read(path.join("agentscm-export.sqlite")).unwrap();
    assert_eq!(
        mirror.record_external_check(&declaration).unwrap(),
        retained
    );
    assert_eq!(
        mirror.external_checks(Some(first.sequence)).unwrap(),
        [retained]
    );
    assert!(
        mirror
            .external_checks(Some(second.sequence))
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        fs::read(path.join("agentscm-export.sqlite")).unwrap(),
        before
    );
    assert_eq!(
        mirror.status(second.sequence).unwrap().unwrap().record,
        newer.record
    );
    assert!(!mirror.status(first.sequence).unwrap().unwrap().ref_matches);
}

#[test]
fn oversized_stored_mapping_result_and_sql_key_fail_with_limits_before_decoding() {
    let (_root, engine, mut authority, path, mut mirror) = setup();
    let event = publish(&engine, &mut authority, vec![change("file", Some(b"one"))]);
    let mapping = mirror
        .export(&engine, &authority, &request(&event, None))
        .unwrap()
        .record;
    let declaration = stored_result_request(&event, &mapping);
    let db = rusqlite::Connection::open(path.join("agentscm-export.sqlite")).unwrap();
    db.execute("UPDATE exports SET record=zeroblob(1048576)", [])
        .unwrap();
    assert!(matches!(
        mirror.record_external_check(&declaration),
        Err(agentscm::Error::Limit(_))
    ));
    assert!(matches!(
        mirror.status(event.sequence),
        Err(agentscm::Error::Limit(_))
    ));
    let version: i64 = db
        .pragma_query_value(None, "user_version", |row| row.get(0))
        .unwrap();
    assert_eq!(version, 1);
    replace_record(&path, &mapping);
    let retained = mirror.record_external_check(&declaration).unwrap();
    db.execute("UPDATE external_checks SET record=zeroblob(1048576)", [])
        .unwrap();
    assert!(matches!(
        mirror.external_checks(None),
        Err(agentscm::Error::Limit(_))
    ));
    assert!(matches!(
        mirror.record_external_check(&declaration),
        Err(agentscm::Error::Limit(_))
    ));
    db.execute(
        "UPDATE external_checks SET record=?1, result_id=?2",
        rusqlite::params![serde_json::to_vec(&retained).unwrap(), "x".repeat(129)],
    )
    .unwrap();
    assert!(matches!(
        mirror.external_checks(None),
        Err(agentscm::Error::Limit(_))
    ));
}

#[test]
fn result_listing_charges_mapping_rereads_to_one_cumulative_payload_budget() {
    let (_root, engine, mut authority, path, mut mirror) = setup();
    let event = publish(&engine, &mut authority, vec![change("file", Some(b"one"))]);
    let mapping = mirror
        .export(&engine, &authority, &request(&event, None))
        .unwrap()
        .record;
    let mut declaration = stored_result_request(&event, &mapping);
    declaration.result_id = "budget-000".into();
    let retained = mirror.record_external_check(&declaration).unwrap();
    let mut db = rusqlite::Connection::open(path.join("agentscm-export.sqlite")).unwrap();
    // One small stored mapping is reread for each result. Trailing JSON
    // whitespace exercises the inclusive64KiB bound without a large database.
    let mut padded = serde_json::to_vec(&mapping).unwrap();
    padded.resize(64 * 1024, b' ');
    set_export_payload(&db, event.sequence, &padded);
    for (start, end) in [(1, 120), (120, 136)] {
        let tx = db.transaction().unwrap();
        for index in start..end {
            let mut record = retained.clone();
            record.result_id = format!("budget-{index:03}");
            tx.execute(
                "INSERT INTO external_checks(result_id,event_sequence,record) VALUES(?1,?2,?3)",
                rusqlite::params![
                    record.result_id,
                    event.sequence,
                    serde_json::to_vec(&record).unwrap()
                ],
            )
            .unwrap();
        }
        tx.commit().unwrap();
        if end == 120 {
            assert_eq!(mirror.external_checks(None).unwrap().len(), 120);
        }
    }
    assert!(matches!(
        mirror.external_checks(None),
        Err(agentscm::Error::Limit(_))
    ));
    assert!(matches!(
        mirror.external_checks(Some(event.sequence)),
        Err(agentscm::Error::Limit(_))
    ));
    let count: i64 = db
        .query_row("SELECT count(*) FROM external_checks", [], |row| row.get(0))
        .unwrap();
    assert_eq!(count, 136); // Failure did not remove records or return a partial list.
}

#[test]
fn retained_legacy_and_current_event_shapes_remain_historical_declarations() {
    let (_root, engine, mut authority, path, mut mirror) = setup();
    let first = publish(&engine, &mut authority, vec![change("file", Some(b"one"))]);
    let one = mirror
        .export(&engine, &authority, &request(&first, None))
        .unwrap()
        .record;
    let second = publish(&engine, &mut authority, vec![change("file", Some(b"two"))]);
    let two = mirror
        .export(&engine, &authority, &request(&second, one.commit.clone()))
        .unwrap()
        .record;
    let declaration = stored_result_request(&second, &two);
    let retained = mirror.record_external_check(&declaration).unwrap();
    // Model a retained semantics1 shape; this does not produce a new native event.
    let mut legacy = two.clone();
    legacy.event.semantics_version = 1;
    legacy.event.change = None;
    legacy.event.change_version = None;
    replace_record(&path, &legacy);
    assert_eq!(
        mirror.record_external_check(&declaration).unwrap(),
        retained
    );
    assert_eq!(mirror.external_checks(None).unwrap(), [retained]);
    legacy.event.kind = agentscm::model::EventKind::Initialize;
    replace_record(&path, &legacy);
    assert!(mirror.external_checks(None).is_err());
    replace_record(&path, &two);
    // Model current Initialize shape without rewriting an Authority event.
    let mut initialized = one.clone();
    initialized.event.kind = agentscm::model::EventKind::Initialize;
    replace_record(&path, &initialized);
    assert!(
        mirror
            .record_external_check(&stored_result_request(&first, &initialized))
            .is_err()
    );
    let mut first_declaration = stored_result_request(&first, &initialized);
    first_declaration.result_id = "modeled-initialize".into();
    assert_eq!(
        mirror
            .record_external_check(&first_declaration)
            .unwrap()
            .event_sequence,
        first.sequence
    );
}

#[test]
fn corrupt_retained_export_cannot_restart_lineage_with_an_absent_ref() {
    let (_root, engine, mut authority, path, mut mirror) = setup();
    let first = publish(&engine, &mut authority, vec![change("file", Some(b"one"))]);
    let original = mirror
        .export(&engine, &authority, &request(&first, None))
        .unwrap()
        .record;
    let second = publish(&engine, &mut authority, vec![change("file", Some(b"two"))]);
    run(
        &path,
        &[
            "update-ref",
            "-d",
            "refs/heads/agentscm",
            original.commit.as_ref().unwrap(),
        ],
    );
    let db = rusqlite::Connection::open(path.join("agentscm-export.sqlite")).unwrap();
    for (pointer, value) in [
        ("/commit", serde_json::Value::Null),
        ("/tree", serde_json::Value::Null),
        (
            "/error",
            serde_json::json!("unexpected error on mirrored row"),
        ),
        ("/profile", serde_json::json!("wrong-profile")),
        ("/repository", serde_json::json!("f".repeat(32))),
        ("/event/target", serde_json::json!("wrong-target")),
        ("/event/sequence", serde_json::json!(second.sequence)),
        (
            "/request/event_sequence",
            serde_json::json!(second.sequence),
        ),
        ("/event/context_digest", serde_json::json!("invalid")),
        (
            "/request/metadata/name",
            serde_json::json!("invalid\nidentity"),
        ),
    ] {
        let mut changed = serde_json::to_value(&original).unwrap();
        *changed.pointer_mut(pointer).unwrap() = value;
        set_export_payload(&db, first.sequence, &serde_json::to_vec(&changed).unwrap());
        let before = fs::read(path.join("agentscm-export.sqlite")).unwrap();
        assert!(mirror.status(first.sequence).is_err(), "{pointer}");
        assert!(
            mirror.lookup_revision(&first.after.revision).is_err(),
            "{pointer}"
        );
        assert!(
            mirror
                .export(&engine, &authority, &request(&first, None))
                .is_err(),
            "{pointer}"
        );
        assert!(
            mirror
                .export(&engine, &authority, &request(&second, None))
                .is_err(),
            "{pointer}"
        );
        assert_eq!(
            fs::read(path.join("agentscm-export.sqlite")).unwrap(),
            before
        );
        assert!(mirror.status(second.sequence).unwrap().is_none());
        assert!(
            run(
                &path,
                &[
                    "for-each-ref",
                    "--format=%(objectname)",
                    "refs/heads/agentscm"
                ]
            )
            .is_empty()
        );
    }
    replace_record(&path, &original);
    // A changed SQL key is rejected even if both embedded sequences still
    // agree with each other. No allocation or implicit row repair is allowed.
    db.execute("UPDATE exports SET sequence=99", []).unwrap();
    assert!(mirror.status(99).is_err());
    assert!(mirror.lookup_revision(&first.after.revision).is_err());
    assert!(
        mirror
            .export(&engine, &authority, &request(&second, None))
            .is_err()
    );
    assert!(mirror.status(second.sequence).unwrap().is_none());
    let count: i64 = db
        .query_row("SELECT count(*) FROM exports", [], |row| row.get(0))
        .unwrap();
    assert_eq!(count, 1);
    assert!(
        run(
            &path,
            &[
                "for-each-ref",
                "--format=%(objectname)",
                "refs/heads/agentscm"
            ]
        )
        .is_empty()
    );
}

#[test]
fn every_export_mapping_consumer_checks_payload_size_before_decoding() {
    let (_root, engine, mut authority, path, mut mirror) = setup();
    let first = publish(&engine, &mut authority, vec![change("file", Some(b"one"))]);
    let original = mirror
        .export(&engine, &authority, &request(&first, None))
        .unwrap();
    let second = publish(&engine, &mut authority, vec![change("file", Some(b"two"))]);
    let owned_ref = fs::read(path.join("refs/heads/agentscm")).unwrap();
    let db = rusqlite::Connection::open(path.join("agentscm-export.sqlite")).unwrap();
    db.execute("UPDATE exports SET record=zeroblob(65537)", [])
        .unwrap();
    let before = fs::read(path.join("agentscm-export.sqlite")).unwrap();
    assert!(matches!(
        mirror.status(first.sequence),
        Err(agentscm::Error::Limit(_))
    ));
    assert!(matches!(
        mirror.lookup_revision(&first.after.revision),
        Err(agentscm::Error::Limit(_))
    ));
    assert!(matches!(
        mirror.export(&engine, &authority, &request(&first, None)),
        Err(agentscm::Error::Limit(_))
    ));
    assert!(matches!(
        mirror.export(
            &engine,
            &authority,
            &request(&second, original.record.commit.clone())
        ),
        Err(agentscm::Error::Limit(_))
    ));
    assert_eq!(
        fs::read(path.join("agentscm-export.sqlite")).unwrap(),
        before
    );
    assert_eq!(
        fs::read(path.join("refs/heads/agentscm")).unwrap(),
        owned_ref
    );
    assert!(mirror.status(second.sequence).unwrap().is_none());
    let mut padded = serde_json::to_vec(&original.record).unwrap();
    padded.resize(64 * 1024, b' ');
    set_export_payload(&db, first.sequence, &padded);
    assert_eq!(mirror.status(first.sequence).unwrap().unwrap(), original);
    assert_eq!(
        mirror
            .lookup_revision(&first.after.revision)
            .unwrap()
            .as_slice(),
        std::slice::from_ref(&original.record)
    );
    assert_eq!(
        mirror
            .export(&engine, &authority, &request(&first, None))
            .unwrap(),
        original
    );
}

#[test]
fn export_history_and_lookup_share_a_finite_cumulative_payload_budget() {
    let (_root, engine, mut authority, path, mut mirror) = setup();
    let first = publish(&engine, &mut authority, vec![change("file", Some(b"one"))]);
    let original = mirror
        .export(&engine, &authority, &request(&first, None))
        .unwrap()
        .record;
    let mut events = vec![first.clone()];
    for _ in 1..130 {
        events.push(publish(
            &engine,
            &mut authority,
            vec![change("file", Some(b"one"))],
        ));
    }
    let owned_ref = fs::read(path.join("refs/heads/agentscm")).unwrap();
    let mut db = rusqlite::Connection::open(path.join("agentscm-export.sqlite")).unwrap();
    let tx = db.transaction().unwrap();
    // Model structurally valid retained records using actual Authority events,
    // not executed Git exports. Each row has an exact coherent SQL/event/request
    // key and parent; padding reaches the inclusive per-record byte ceiling.
    for (index, event) in events.iter().take(129).enumerate() {
        let mut record = original.clone();
        record.event = event.clone();
        record.request = request(
            event,
            if index == 0 {
                None
            } else {
                original.commit.clone()
            },
        );
        let mut bytes = serde_json::to_vec(&record).unwrap();
        bytes.resize(64 * 1024, b' ');
        tx.execute(
            "INSERT OR REPLACE INTO exports(sequence,record) VALUES(?1,?2)",
            rusqlite::params![event.sequence, bytes],
        )
        .unwrap();
    }
    tx.commit().unwrap();
    let before = fs::read(path.join("agentscm-export.sqlite")).unwrap();
    assert!(matches!(
        mirror.lookup_revision(&first.after.revision),
        Err(agentscm::Error::Limit(_))
    ));
    let next = events.last().unwrap();
    assert!(matches!(
        mirror.export(&engine, &authority, &request(next, original.commit.clone())),
        Err(agentscm::Error::Limit(_))
    ));
    assert!(mirror.status(next.sequence).unwrap().is_none());
    assert_eq!(
        mirror.status(first.sequence).unwrap().unwrap().record,
        original
    );
    assert_eq!(
        fs::read(path.join("agentscm-export.sqlite")).unwrap(),
        before
    );
    assert_eq!(
        fs::read(path.join("refs/heads/agentscm")).unwrap(),
        owned_ref
    );
    let count: i64 = db
        .query_row("SELECT count(*) FROM exports", [], |row| row.get(0))
        .unwrap();
    assert_eq!(count, 129);
}

#[test]
fn export_refuses_executable_and_symlink_entries_instead_of_flattening() {
    for kind in [EntryKind::Executable, EntryKind::Symlink] {
        let (_root, engine, mut authority, _path, mut mirror) = setup();
        let revision = engine
            .checkpoint_entries(
                None,
                &[
                    EntryChange {
                        path: "plain".into(),
                        content: Some(b"text".to_vec()),
                        kind: EntryKind::Regular,
                    },
                    EntryChange {
                        path: "special".into(),
                        content: Some(b"plain".to_vec()),
                        kind,
                    },
                ],
            )
            .unwrap();
        let event = integrate_revision(&engine, &mut authority, None, revision);
        let error = mirror
            .export(&engine, &authority, &request(&event, None))
            .unwrap_err();
        let message = error.to_string();
        assert!(
            message.contains("unsupported entry kind: special") && message.contains("Git export"),
            "{message}"
        );
    }
}

#[test]
fn export_profile_string_is_fixed() {
    assert_eq!(agentscm::git_export::PROFILE, "agentscm-git-snapshot/v1");
}
