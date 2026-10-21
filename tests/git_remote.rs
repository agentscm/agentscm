#![cfg(unix)]
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;

use agentscm::authority::Authority;
use agentscm::context::ReviewContext;
use agentscm::git_export::remote::{RejectionKind, Request, State};
use agentscm::git_export::{CommitMetadata, ExportRequest, Mirror};
use agentscm::git_snapshot::{self, SnapshotRequest};
use agentscm::model::{Approval, FileChange, IntegrateRequest, IntegrationEvent};
use agentscm::store::Engine;
use tempfile::TempDir;

// Each test runs the product code only in a child with synthetic operator
// config sources. This avoids mutating process environment in concurrent tests
// or reading the developer's real global/system policy and accounts.
fn isolated_test(name: &str) -> bool {
    isolated_test_with(name, &[])
}

fn isolated_test_with(name: &str, extra_environment: &[(&str, &str)]) -> bool {
    isolated_policy_test(name, extra_environment, &[])
}

fn isolated_policy_test(name: &str, extra_environment: &[(&str, &str)], omitted: &[&str]) -> bool {
    if std::env::var("AGENTSCM_REMOTE_TEST_ISOLATED").as_deref() == Ok("active") {
        return false;
    }
    let policy = TempDir::new().unwrap();
    let home = policy.path().join("home");
    let xdg = policy.path().join("xdg");
    fs::create_dir(&home).unwrap();
    fs::create_dir(&xdg).unwrap();
    let global = policy.path().join("global-config");
    let system = policy.path().join("system-config");
    fs::write(&global, "").unwrap();
    fs::write(&system, "").unwrap();
    let mut command = Command::new(std::env::current_exe().unwrap());
    command
        .current_dir(policy.path())
        .env_clear()
        .env("HOME", home)
        .env("XDG_CONFIG_HOME", xdg)
        .env("PATH", "/usr/bin:/bin")
        .env("GIT_CONFIG_GLOBAL", global)
        .env("GIT_CONFIG_SYSTEM", system)
        .env("AGENTSCM_REMOTE_TEST_ISOLATED", "active")
        .env("AGENTSCM_TEST_GIT", git())
        .envs(extra_environment.iter().copied())
        .args(["--exact", name, "--nocapture"]);
    for key in omitted {
        command.env_remove(key);
    }
    let status = command.status().unwrap();
    assert!(status.success(), "isolated test {name} failed");
    true
}

fn git() -> PathBuf {
    PathBuf::from(std::env::var("AGENTSCM_TEST_GIT").unwrap_or_else(|_| "/usr/bin/git".into()))
}

fn run(path: &Path, args: &[&str]) -> String {
    let output = Command::new(git())
        .env_clear()
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .arg("-C")
        .arg(path)
        .args([
            "-c",
            "core.hooksPath=/dev/null",
            "-c",
            "commit.gpgSign=false",
        ])
        .args(args)
        .output()
        .unwrap();
    assert!(output.status.success(), "Git {args:?}: {:?}", output.stderr);
    String::from_utf8(output.stdout).unwrap().trim().into()
}

fn bare(root: &Path) -> PathBuf {
    let path = root.join("remote with 'quote.git");
    fs::create_dir(&path).unwrap();
    run(
        &path,
        &["init", "--bare", "--template=", "--object-format=sha1"],
    );
    path
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
fn native_export_publishes_fetches_and_imports_without_moving_native_targets() {
    if isolated_test("native_export_publishes_fetches_and_imports_without_moving_native_targets") {
        return;
    }
    let (root, engine, mut authority, path, mut mirror) = setup();
    let remote = bare(root.path());
    let binding = mirror.remote_bind(&remote, "main").unwrap();
    assert_eq!(mirror.remote_bind(&remote, "main").unwrap(), binding);
    assert!(mirror.remote_bind(&remote, "other").is_err());
    assert_eq!(mirror.remote_observe().unwrap(), None);
    let event = publish(
        &engine,
        &mut authority,
        vec![change("hello.txt", Some(b"native bytes\n"))],
    );
    let export = mirror
        .export(&engine, &authority, &request(&event, None))
        .unwrap();
    let commit = export.record.commit.unwrap();
    let native_before = authority.target("main").unwrap();
    let events_before = authority.events(0, 10).unwrap();
    let operation = Request::Publish {
        operation_id: "publish-one".into(),
        event_sequence: event.sequence,
        expected_oid: None,
    };
    let published = mirror.remote_run(&operation).unwrap();
    assert_eq!(published.state, State::Published);
    assert_eq!(published.observed_oid.as_deref(), Some(commit.as_str()));
    assert_eq!(mirror.remote_run(&operation).unwrap(), published);
    assert_eq!(run(&remote, &["rev-parse", "refs/heads/main"]), commit);

    // A second owned mirror receives the exact commit. Ordinary native capture remains
    // the import authority: remote receipts cannot mint a target or approval.
    let receiving_data = root.path().join("receiving-data");
    let receiving_engine = Engine::open(&receiving_data).unwrap();
    let receiving_authority = Authority::open(&receiving_data).unwrap();
    let receiving_path = root.path().join("receiving.git");
    let receiving = Mirror::initialize(
        &git(),
        &receiving_path,
        &receiving_authority.repository_id().unwrap(),
        "main",
    )
    .unwrap();
    receiving.remote_bind(&remote, "main").unwrap();
    let fetched = receiving
        .remote_run(&Request::Fetch {
            operation_id: "incoming-one".into(),
            expected_oid: commit.clone(),
        })
        .unwrap();
    assert_eq!(fetched.state, State::Fetched);
    assert_eq!(
        run(
            &receiving_path,
            &["rev-parse", fetched.local_ref.as_deref().unwrap()]
        ),
        commit
    );
    let checkout = root.path().join("incoming-checkout");
    fs::create_dir(&checkout).unwrap();
    run(&checkout, &["init", "--template=", "--object-format=sha1"]);
    run(
        &checkout,
        &[
            "fetch",
            "--no-tags",
            receiving_path.to_str().unwrap(),
            fetched.local_ref.as_deref().unwrap(),
        ],
    );
    run(&checkout, &["checkout", "--detach", &commit]);
    let capture_request = SnapshotRequest {
        git: git(),
        repository: checkout,
        revision: commit,
        exclude: vec![],
    };
    let report = git_snapshot::preflight(&capture_request).unwrap();
    assert!(report.eligible);
    let imported = git_snapshot::capture(
        &receiving_engine,
        &receiving_authority.repository_id().unwrap(),
        &capture_request,
        report,
    )
    .unwrap();
    assert_eq!(
        receiving_engine
            .read_file(&imported.revision, "hello.txt")
            .unwrap(),
        b"native bytes\n"
    );
    assert_eq!(receiving_authority.target("main").unwrap(), None);
    assert_eq!(authority.target("main").unwrap(), native_before);
    assert_eq!(authority.events(0, 10).unwrap(), events_before);
    assert_eq!(
        run(&path, &["rev-parse", "refs/heads/agentscm"]),
        published.commit
    );
}

#[test]
fn stale_preimage_and_non_fast_forward_preserve_the_remote_and_fence_retry() {
    if isolated_test("stale_preimage_and_non_fast_forward_preserve_the_remote_and_fence_retry") {
        return;
    }
    let (root, engine, mut authority, _, mut mirror) = setup();
    let remote = bare(root.path());
    mirror.remote_bind(&remote, "main").unwrap();
    let one = publish(&engine, &mut authority, vec![change("file", Some(b"one"))]);
    let first = mirror
        .export(&engine, &authority, &request(&one, None))
        .unwrap()
        .record
        .commit
        .unwrap();
    let initial = Request::Publish {
        operation_id: "first".into(),
        event_sequence: one.sequence,
        expected_oid: None,
    };
    assert_eq!(mirror.remote_run(&initial).unwrap().state, State::Published);
    let two = publish(&engine, &mut authority, vec![change("file", Some(b"two"))]);
    let second = mirror
        .export(&engine, &authority, &request(&two, Some(first.clone())))
        .unwrap()
        .record
        .commit
        .unwrap();
    let stale = Request::Publish {
        operation_id: "stale".into(),
        event_sequence: two.sequence,
        expected_oid: None,
    };
    assert_eq!(mirror.remote_run(&stale).unwrap().state, State::Unknown);
    assert_eq!(run(&remote, &["rev-parse", "refs/heads/main"]), first);
    let good = Request::Publish {
        operation_id: "second".into(),
        event_sequence: two.sequence,
        expected_oid: Some(first.clone()),
    };
    assert!(mirror.remote_run(&good).is_err());
    assert_eq!(
        mirror.remote_recover("stale").unwrap().state,
        State::Diverged
    );
    assert_eq!(mirror.remote_run(&good).unwrap().state, State::Published);
    let rewind = Request::Publish {
        operation_id: "rewind".into(),
        event_sequence: one.sequence,
        expected_oid: Some(second.clone()),
    };
    assert_eq!(mirror.remote_run(&rewind).unwrap().state, State::Unknown);
    assert_eq!(
        mirror.remote_recover("rewind").unwrap().state,
        State::ObservedExpected
    );
    assert_eq!(run(&remote, &["rev-parse", "refs/heads/main"]), second);
    assert!(
        mirror
            .remote_run(&Request::Publish {
                operation_id: "first".into(),
                event_sequence: two.sequence,
                expected_oid: None
            })
            .is_err()
    );
}

#[test]
fn lost_push_acknowledgement_is_observed_without_replaying_the_write() {
    if isolated_test("lost_push_acknowledgement_is_observed_without_replaying_the_write") {
        return;
    }
    let (root, engine, mut authority, path, mut mirror) = setup();
    let remote = bare(root.path());
    mirror.remote_bind(&remote, "main").unwrap();
    let event = publish(&engine, &mut authority, vec![change("file", Some(b"one"))]);
    let commit = mirror
        .export(&engine, &authority, &request(&event, None))
        .unwrap()
        .record
        .commit
        .unwrap();
    let repository = authority.repository_id().unwrap();
    drop(mirror);
    let wrapper = root.path().join("git lost 'ack");
    let binary = git().to_str().unwrap().replace('\'', "'\\''");
    fs::write(&wrapper, format!("#!/bin/sh\npush=0\nfor arg in \"$@\"; do [ \"$arg\" = push ] && push=1; done\n'{binary}' \"$@\"\nresult=$?\n[ \"$push\" = 1 ] && exit 42\nexit \"$result\"\n")).unwrap();
    fs::set_permissions(&wrapper, fs::Permissions::from_mode(0o700)).unwrap();
    let mirror = Mirror::open(&wrapper, &path, &repository).unwrap();
    let operation = Request::Publish {
        operation_id: "lost-ack".into(),
        event_sequence: event.sequence,
        expected_oid: None,
    };
    let uncertain = mirror.remote_run(&operation).unwrap();
    assert_eq!(uncertain.state, State::Unknown);
    assert_eq!(run(&remote, &["rev-parse", "refs/heads/main"]), commit);
    // Retrying is historical and cannot turn Unknown into a second write.
    assert_eq!(mirror.remote_run(&operation).unwrap(), uncertain);
    drop(mirror);
    let mirror = Mirror::open(&git(), &path, &repository).unwrap();
    assert_eq!(
        mirror.remote_recover("lost-ack").unwrap().state,
        State::ObservedDesired
    );
    // Simulate a process interruption at the durable reservation boundary.
    let db = rusqlite::Connection::open(path.join("agentscm-remote.sqlite")).unwrap();
    let mut interrupted = mirror.remote_lookup("lost-ack").unwrap().unwrap();
    interrupted.request = Request::Publish {
        operation_id: "interrupted".into(),
        event_sequence: event.sequence,
        expected_oid: None,
    };
    interrupted.state = State::Pending;
    interrupted.observed_oid = None;
    db.execute(
        "INSERT INTO operations(id,record) VALUES (?1,?2)",
        rusqlite::params!["interrupted", serde_json::to_vec(&interrupted).unwrap()],
    )
    .unwrap();
    assert_eq!(
        mirror.remote_run(&interrupted.request).unwrap().state,
        State::Pending
    );
    assert_eq!(
        mirror.remote_recover("interrupted").unwrap().state,
        State::ObservedDesired
    );
}

#[test]
fn invalid_remote_profiles_and_symbolic_refs_are_rejected() {
    if isolated_test("invalid_remote_profiles_and_symbolic_refs_are_rejected") {
        return;
    }
    let (root, _, _, _, mirror) = setup();
    assert!(
        mirror
            .remote_bind(Path::new("https://example.invalid/repo.git"), "main")
            .is_err()
    );
    assert!(
        mirror
            .remote_bind(Path::new("relative.git"), "main")
            .is_err()
    );
    let remote = bare(root.path());
    assert!(mirror.remote_bind(&remote, "../main").is_err());
    mirror.remote_bind(&remote, "main").unwrap();
    run(
        &remote,
        &["symbolic-ref", "refs/heads/main", "refs/heads/other"],
    );
    assert!(mirror.remote_observe().is_err());
}

#[test]
fn existing_export_lock_serializes_remote_work_and_offline_lookup_survives() {
    if isolated_test("existing_export_lock_serializes_remote_work_and_offline_lookup_survives") {
        return;
    }
    let (root, engine, mut authority, path, mut mirror) = setup();
    let remote = bare(root.path());
    mirror.remote_bind(&remote, "main").unwrap();
    let lock = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(path.join("agentscm-export.lock"))
        .unwrap();
    let lease = agentscm::lease::Lease::try_exclusive(lock).unwrap();
    assert!(mirror.remote_observe().is_err());
    assert!(mirror.remote_lookup("missing").is_err());
    drop(lease);
    let event = publish(&engine, &mut authority, vec![change("file", Some(b"one"))]);
    mirror
        .export(&engine, &authority, &request(&event, None))
        .unwrap();
    let operation = Request::Publish {
        operation_id: "first".into(),
        event_sequence: event.sequence,
        expected_oid: None,
    };
    let receipt = mirror.remote_run(&operation).unwrap();
    assert_eq!(receipt.state, State::Published);
    fs::rename(&remote, root.path().join("unavailable.git")).unwrap();
    assert_eq!(mirror.remote_lookup("first").unwrap(), Some(receipt));
    assert!(mirror.remote_observe().is_err());
}

#[test]
fn exact_lease_refuses_a_branch_move_between_observation_and_push() {
    if isolated_test("exact_lease_refuses_a_branch_move_between_observation_and_push") {
        return;
    }
    let (root, engine, mut authority, path, mut mirror) = setup();
    let remote = bare(root.path());
    mirror.remote_bind(&remote, "main").unwrap();
    let one = publish(&engine, &mut authority, vec![change("file", Some(b"one"))]);
    let first = mirror
        .export(&engine, &authority, &request(&one, None))
        .unwrap()
        .record
        .commit
        .unwrap();
    assert_eq!(
        mirror
            .remote_run(&Request::Publish {
                operation_id: "one".into(),
                event_sequence: one.sequence,
                expected_oid: None
            })
            .unwrap()
            .state,
        State::Published
    );
    let two = publish(&engine, &mut authority, vec![change("file", Some(b"two"))]);
    let second = mirror
        .export(&engine, &authority, &request(&two, Some(first.clone())))
        .unwrap()
        .record
        .commit
        .unwrap();
    let three = publish(
        &engine,
        &mut authority,
        vec![change("file", Some(b"three"))],
    );
    let third = mirror
        .export(&engine, &authority, &request(&three, Some(second)))
        .unwrap()
        .record
        .commit
        .unwrap();
    // Supply a competing actor's commit, but leave the branch at the expected first commit.
    run(
        &remote,
        &[
            "fetch",
            "--no-tags",
            "--no-write-fetch-head",
            path.to_str().unwrap(),
            &third,
        ],
    );
    let repository = authority.repository_id().unwrap();
    drop(mirror);
    let quote = |text: &str| format!("'{}'", text.replace('\'', "'\\''"));
    let wrapper = root.path().join("git-race");
    fs::write(&wrapper, format!(
        "#!/bin/sh\nfor arg in \"$@\"; do\n if [ \"$arg\" = push ]; then\n {} -C {} update-ref refs/heads/main {} {} || exit 43\n fi\ndone\nexec {} \"$@\"\n",
        quote(git().to_str().unwrap()), quote(remote.to_str().unwrap()), third, first, quote(git().to_str().unwrap())
    )).unwrap();
    fs::set_permissions(&wrapper, fs::Permissions::from_mode(0o700)).unwrap();
    let mirror = Mirror::open(&wrapper, &path, &repository).unwrap();
    let result = mirror
        .remote_run(&Request::Publish {
            operation_id: "race".into(),
            event_sequence: two.sequence,
            expected_oid: Some(first),
        })
        .unwrap();
    assert_eq!(result.state, State::Diverged);
    assert_eq!(result.observed_oid.as_deref(), Some(third.as_str()));
    assert_eq!(result.rejection, None);
    assert_eq!(run(&remote, &["rev-parse", "refs/heads/main"]), third);
    assert_eq!(
        mirror.remote_recover("race").unwrap().state,
        State::Diverged
    );
}

#[test]
fn remote_pre_receive_hook_is_honored_despite_owned_mirror_hook_isolation() {
    if isolated_test("remote_pre_receive_hook_is_honored_despite_owned_mirror_hook_isolation") {
        return;
    }
    let (root, engine, mut authority, _, mut mirror) = setup();
    let remote = bare(root.path());
    mirror.remote_bind(&remote, "main").unwrap();
    let first_event = publish(&engine, &mut authority, vec![change("file", Some(b"one"))]);
    let first = mirror
        .export(&engine, &authority, &request(&first_event, None))
        .unwrap()
        .record
        .commit
        .unwrap();
    assert_eq!(
        mirror
            .remote_run(&Request::Publish {
                operation_id: "allowed".into(),
                event_sequence: first_event.sequence,
                expected_oid: None
            })
            .unwrap()
            .state,
        State::Published
    );
    let second_event = publish(&engine, &mut authority, vec![change("file", Some(b"two"))]);
    mirror
        .export(
            &engine,
            &authority,
            &request(&second_event, Some(first.clone())),
        )
        .unwrap();
    let hooks = remote.join("policy-hooks");
    fs::create_dir(&hooks).unwrap();
    run(
        &remote,
        &["config", "core.hooksPath", hooks.to_str().unwrap()],
    );
    let hook = hooks.join("pre-receive");
    fs::write(&hook, "#!/bin/sh\nexit 1\n").unwrap();
    fs::set_permissions(&hook, fs::Permissions::from_mode(0o700)).unwrap();
    let rejected = mirror
        .remote_run(&Request::Publish {
            operation_id: "rejected".into(),
            event_sequence: second_event.sequence,
            expected_oid: Some(first.clone()),
        })
        .unwrap();
    assert_eq!(rejected.state, State::Rejected);
    assert_eq!(rejected.rejection, Some(RejectionKind::HookDeclined));
    assert_eq!(rejected.observed_oid.as_deref(), Some(first.as_str()));
    assert_eq!(run(&remote, &["rev-parse", "refs/heads/main"]), first);
    assert_eq!(fs::read_to_string(&hook).unwrap(), "#!/bin/sh\nexit 1\n");
    assert_eq!(mirror.remote_recover("rejected").unwrap(), rejected);
    // A rejection is terminal and does not fence new operations.
    fs::remove_file(&hook).unwrap();
    assert_eq!(
        mirror
            .remote_run(&Request::Publish {
                operation_id: "retry".into(),
                event_sequence: second_event.sequence,
                expected_oid: Some(first),
            })
            .unwrap()
            .state,
        State::Published
    );
}

#[test]
fn hidden_branch_cannot_be_observed_as_absent_or_clear_an_unknown_fence() {
    if isolated_test("hidden_branch_cannot_be_observed_as_absent_or_clear_an_unknown_fence") {
        return;
    }
    let (root, engine, mut authority, _, mut mirror) = setup();
    let remote = bare(root.path());
    mirror.remote_bind(&remote, "main").unwrap();
    let one = publish(&engine, &mut authority, vec![change("file", Some(b"one"))]);
    let first = mirror
        .export(&engine, &authority, &request(&one, None))
        .unwrap()
        .record
        .commit
        .unwrap();
    assert_eq!(
        mirror
            .remote_run(&Request::Publish {
                operation_id: "one".into(),
                event_sequence: one.sequence,
                expected_oid: None
            })
            .unwrap()
            .state,
        State::Published
    );
    let two = publish(&engine, &mut authority, vec![change("file", Some(b"two"))]);
    mirror
        .export(&engine, &authority, &request(&two, Some(first.clone())))
        .unwrap();
    run(
        &remote,
        &["config", "uploadpack.hideRefs", "refs/heads/main"],
    );
    assert!(mirror.remote_observe().is_err());
    let hidden = Request::Publish {
        operation_id: "hidden".into(),
        event_sequence: two.sequence,
        expected_oid: None,
    };
    assert_eq!(mirror.remote_run(&hidden).unwrap().state, State::Unknown);
    assert!(mirror.remote_recover("hidden").is_err());
    assert_eq!(
        mirror.remote_lookup("hidden").unwrap().unwrap().state,
        State::Unknown
    );
    assert_eq!(run(&remote, &["rev-parse", "refs/heads/main"]), first);
    run(&remote, &["config", "--unset", "uploadpack.hideRefs"]);
    assert_eq!(
        mirror.remote_recover("hidden").unwrap().state,
        State::Diverged
    );
}

#[test]
fn oversized_corrupt_journal_key_is_rejected_before_new_reservation() {
    if isolated_test("oversized_corrupt_journal_key_is_rejected_before_new_reservation") {
        return;
    }
    let (root, _, _, path, mirror) = setup();
    let remote = bare(root.path());
    mirror.remote_bind(&remote, "main").unwrap();
    let db = rusqlite::Connection::open(path.join("agentscm-remote.sqlite")).unwrap();
    db.execute(
        "INSERT INTO operations(id,record) VALUES (?1,?2)",
        rusqlite::params!["x".repeat(100_000), b"{}".as_slice()],
    )
    .unwrap();
    assert!(
        mirror
            .remote_run(&Request::Fetch {
                operation_id: "new".into(),
                expected_oid: "1".repeat(40)
            })
            .is_err()
    );
    assert_eq!(
        db.query_row("SELECT count(*) FROM operations", [], |r| r
            .get::<_, i64>(0))
            .unwrap(),
        1
    );
    assert_eq!(mirror.remote_lookup("new").unwrap(), None);
}

#[test]
fn branch_racing_during_advertisement_fails_closed() {
    if isolated_test("branch_racing_during_advertisement_fails_closed") {
        return;
    }
    let (root, engine, mut authority, path, mut mirror) = setup();
    let remote = bare(root.path());
    mirror.remote_bind(&remote, "main").unwrap();
    let one = publish(&engine, &mut authority, vec![change("file", Some(b"one"))]);
    let first = mirror
        .export(&engine, &authority, &request(&one, None))
        .unwrap()
        .record
        .commit
        .unwrap();
    assert_eq!(
        mirror
            .remote_run(&Request::Publish {
                operation_id: "one".into(),
                event_sequence: one.sequence,
                expected_oid: None
            })
            .unwrap()
            .state,
        State::Published
    );
    let two = publish(&engine, &mut authority, vec![change("file", Some(b"two"))]);
    let second = mirror
        .export(&engine, &authority, &request(&two, Some(first.clone())))
        .unwrap()
        .record
        .commit
        .unwrap();
    run(
        &remote,
        &[
            "fetch",
            "--no-tags",
            "--no-write-fetch-head",
            path.to_str().unwrap(),
            &second,
        ],
    );
    let repository = authority.repository_id().unwrap();
    drop(mirror);
    let quote = |text: &str| format!("'{}'", text.replace('\'', "'\\''"));
    let wrapper = root.path().join("git-advertisement-race");
    fs::write(&wrapper, format!(
        "#!/bin/sh\nfor arg in \"$@\"; do\n if [ \"$arg\" = upload-pack ]; then\n {} -C {} update-ref refs/heads/main {} {} || exit 43\n fi\ndone\nexec {} \"$@\"\n",
        quote(git().to_str().unwrap()), quote(remote.to_str().unwrap()), second, first, quote(git().to_str().unwrap())
    )).unwrap();
    fs::set_permissions(&wrapper, fs::Permissions::from_mode(0o700)).unwrap();
    let mirror = Mirror::open(&wrapper, &path, &repository).unwrap();
    assert!(mirror.remote_observe().is_err());
    assert_eq!(run(&remote, &["rev-parse", "refs/heads/main"]), second);
}

fn rejecting_operator_hook(config_key: &str) {
    rejecting_operator_hook_at(PathBuf::from(std::env::var_os(config_key).unwrap()));
}

fn rejecting_operator_hook_at(config: PathBuf) {
    let (root, engine, mut authority, _, mut mirror) = setup();
    let remote = bare(root.path());
    mirror.remote_bind(&remote, "main").unwrap();
    let event = publish(&engine, &mut authority, vec![change("file", Some(b"one"))]);
    mirror
        .export(&engine, &authority, &request(&event, None))
        .unwrap();
    let hooks = root.path().join("operator-policy-hooks");
    fs::create_dir(&hooks).unwrap();
    let hook = hooks.join("pre-receive");
    fs::write(&hook, "#!/bin/sh\nexit 1\n").unwrap();
    fs::set_permissions(&hook, fs::Permissions::from_mode(0o700)).unwrap();
    fs::write(
        &config,
        format!("[core]\n hooksPath = {}\n", hooks.display()),
    )
    .unwrap();
    let rejected = mirror
        .remote_run(&Request::Publish {
            operation_id: "operator-rejected".into(),
            event_sequence: event.sequence,
            expected_oid: None,
        })
        .unwrap();
    assert_eq!(rejected.state, State::Rejected);
    assert_eq!(rejected.rejection, Some(RejectionKind::HookDeclined));
    assert_eq!(rejected.observed_oid, None);
    assert_eq!(run(&remote, &["for-each-ref", "refs/heads/main"]), "");
    assert_eq!(
        mirror.remote_recover("operator-rejected").unwrap(),
        rejected
    );
}

#[test]
fn remote_global_pre_receive_policy_is_preserved() {
    if isolated_test("remote_global_pre_receive_policy_is_preserved") {
        return;
    }
    rejecting_operator_hook("GIT_CONFIG_GLOBAL");
}

#[test]
fn remote_system_pre_receive_policy_is_preserved() {
    if isolated_test("remote_system_pre_receive_policy_is_preserved") {
        return;
    }
    rejecting_operator_hook("GIT_CONFIG_SYSTEM");
}

#[test]
fn original_numbered_policy_is_refused_before_binding() {
    if isolated_test_with(
        "original_numbered_policy_is_refused_before_binding",
        &[
            ("GIT_CONFIG_COUNT", "1"),
            ("GIT_CONFIG_KEY_0", "core.hooksPath"),
            ("GIT_CONFIG_VALUE_0", "/dev/null"),
        ],
    ) {
        return;
    }
    let (root, _, _, path, mirror) = setup();
    let remote = bare(root.path());
    assert!(mirror.remote_bind(&remote, "main").is_err());
    assert!(!path.join("agentscm-remote.sqlite").exists());
}

#[test]
fn original_parameter_policy_is_refused_before_binding() {
    if isolated_test_with(
        "original_parameter_policy_is_refused_before_binding",
        &[("GIT_CONFIG_PARAMETERS", "'core.hooksPath'='/dev/null'")],
    ) {
        return;
    }
    let (root, _, _, path, mirror) = setup();
    let remote = bare(root.path());
    assert!(mirror.remote_bind(&remote, "main").is_err());
    assert!(!path.join("agentscm-remote.sqlite").exists());
}

#[test]
fn remote_default_home_policy_is_preserved_without_explicit_global_source() {
    if isolated_policy_test(
        "remote_default_home_policy_is_preserved_without_explicit_global_source",
        &[],
        &["GIT_CONFIG_GLOBAL"],
    ) {
        return;
    }
    rejecting_operator_hook_at(PathBuf::from(std::env::var_os("HOME").unwrap()).join(".gitconfig"));
}

#[test]
fn remote_default_xdg_policy_is_preserved_without_explicit_global_source() {
    if isolated_policy_test(
        "remote_default_xdg_policy_is_preserved_without_explicit_global_source",
        &[],
        &["GIT_CONFIG_GLOBAL"],
    ) {
        return;
    }
    let directory = PathBuf::from(std::env::var_os("XDG_CONFIG_HOME").unwrap()).join("git");
    fs::create_dir(&directory).unwrap();
    rejecting_operator_hook_at(directory.join("config"));
}

#[test]
fn relative_operator_policy_source_keeps_its_invoking_directory() {
    if isolated_test_with(
        "relative_operator_policy_source_keeps_its_invoking_directory",
        &[("GIT_CONFIG_GLOBAL", "global-config")],
    ) {
        return;
    }
    rejecting_operator_hook("GIT_CONFIG_GLOBAL");
}

#[path = "git_remote/network.rs"]
mod network_tests;
