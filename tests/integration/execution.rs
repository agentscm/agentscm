#![cfg(unix)]

use crate::fake_docker;

use std::collections::BTreeMap;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixListener;
use std::path::PathBuf;
use std::thread;
use std::time::{Duration, Instant};

use agentscm::checks::CheckSubject;
use agentscm::context::ChangeId;
use agentscm::execution::{
    Cancellation, CommandRecipe, ExecutionLimits, ExecutionOutcome, ExecutionRequest, OciBackend,
    Snapshot, SyscallPolicy, environment_digest,
};
use agentscm::model::{EntryChange, EntryKind, FileChange};
use agentscm::store::Engine;
use fake_docker::FAKE_DOCKER;
use tempfile::TempDir;

const DIGEST_1: &str = "1111111111111111111111111111111111111111111111111111111111111111";
const DIGEST_2: &str = "2222222222222222222222222222222222222222222222222222222222222222";
const DIGEST_3: &str = "3333333333333333333333333333333333333333333333333333333333333333";

struct Fixture {
    _temporary: TempDir,
    _socket: UnixListener,
    engine: Engine,
    backend: OciBackend,
    request: ExecutionRequest,
    snapshot_path: PathBuf,
    config: PathBuf,
}

impl Fixture {
    fn fake() -> Self {
        let temporary = TempDir::new().expect("temporary fixture");
        fs::set_permissions(temporary.path(), fs::Permissions::from_mode(0o700))
            .expect("private snapshot parent");
        let executable = temporary.path().join("fake-docker.py");
        fs::write(&executable, FAKE_DOCKER).expect("write fake Docker");
        fs::set_permissions(&executable, fs::Permissions::from_mode(0o700))
            .expect("make fake Docker executable");
        let socket_path = temporary.path().join("docker.sock");
        let socket = UnixListener::bind(&socket_path).expect("private Unix socket");
        let config = temporary.path().join("config");
        fs::create_dir(&config).expect("config directory");
        fs::set_permissions(&config, fs::Permissions::from_mode(0o700))
            .expect("private config directory");
        let backend = OciBackend::new(&executable, &socket_path, &config).expect("backend");
        let engine = Engine::open(temporary.path().join("store")).expect("engine");
        let revision = engine
            .checkpoint(
                None,
                &[FileChange {
                    path: "README.md".into(),
                    content: Some(b"trusted source\n".to_vec()),
                }],
            )
            .expect("revision");
        let request = request(
            &revision,
            format!("sha256:{DIGEST_1}"),
            vec!["/bin/true".into()],
        );
        Self {
            snapshot_path: temporary.path().join("snapshot"),
            _temporary: temporary,
            _socket: socket,
            engine,
            backend,
            request,
            config,
        }
    }

    fn scenario(&self, value: &str) {
        fs::write(self.config.join("scenario"), value).expect("write scenario");
    }

    fn log(&self) -> String {
        fs::read_to_string(self.config.join("calls.jsonl")).unwrap_or_default()
    }

    fn observed(&self) -> String {
        fs::read_to_string(self.config.join("observed.json")).unwrap_or_default()
    }

    fn snapshot(&self) -> Snapshot {
        Snapshot::materialize(
            &self.engine,
            &self.request.subject.revision,
            &self.snapshot_path,
            32,
            1024 * 1024,
        )
        .expect("snapshot")
    }
}

#[test]
fn fake_runtime_exercises_create_start_inspect_and_cleanup() {
    let fixture = Fixture::fake();
    let snapshot = fixture.snapshot();
    let record = fixture
        .backend
        .collect(&fixture.request, &snapshot, &Cancellation::default())
        .expect("collect execution");

    assert_eq!(
        record.outcome,
        ExecutionOutcome::Success,
        "{record:?}\n{}",
        fixture.observed()
    );
    assert_eq!(record.exit_code, Some(0));
    assert_eq!(record.stdout, b"fake candidate output\n");
    assert!(record.stderr.is_empty());
    assert!(record.isolation_observed);
    assert!(record.cleanup_confirmed);
    assert!(
        record
            .runtime_identity
            .as_deref()
            .is_some_and(|id| id.starts_with("docker:fake-1:"))
    );
    record.validate().expect("valid record");
    let log = fixture.log();
    assert!(log.contains("\"container\", \"create\""));
    assert!(log.contains("\"container\", \"start\""));
    assert!(log.contains("\"container\", \"rm\""));
}

#[test]
fn fake_runtime_reports_nonzero_candidate_as_failed() {
    let fixture = Fixture::fake();
    fixture.scenario("nonzero");
    let record = fixture
        .backend
        .collect(
            &fixture.request,
            &fixture.snapshot(),
            &Cancellation::default(),
        )
        .expect("collect failed candidate");
    assert_eq!(record.outcome, ExecutionOutcome::Failed);
    assert_eq!(record.exit_code, Some(7));
    assert!(record.cleanup_confirmed);
}

#[test]
fn sigkill_exit_without_an_oom_flag_is_incomplete_not_failed() {
    let fixture = Fixture::fake();
    fixture.scenario("killed");
    let record = fixture
        .backend
        .collect(
            &fixture.request,
            &fixture.snapshot(),
            &Cancellation::default(),
        )
        .expect("collect killed candidate");
    assert_eq!(record.outcome, ExecutionOutcome::Incomplete, "{record:?}");
    assert_eq!(record.exit_code, Some(137));
    assert!(record.cleanup_confirmed);
    record.validate().unwrap();
}

#[test]
fn create_warning_or_rejected_effective_configuration_never_starts() {
    for scenario in ["create_warning", "bad_cpu", "bad_health"] {
        let fixture = Fixture::fake();
        fixture.scenario(scenario);
        let record = fixture
            .backend
            .collect(
                &fixture.request,
                &fixture.snapshot(),
                &Cancellation::default(),
            )
            .expect("collect rejected creation");
        assert_eq!(record.outcome, ExecutionOutcome::Incomplete);
        assert!(!record.isolation_observed);
        assert!(record.cleanup_confirmed);
        assert!(!fixture.log().contains("\"container\", \"start\""));
    }
}

#[test]
fn cancellation_interrupts_probe_and_image_acquisition_before_create() {
    for (scenario, phase) in [
        ("hanging_probe", "\"info\""),
        ("hanging_image", "\"image\", \"inspect\""),
    ] {
        let fixture = Fixture::fake();
        fixture.scenario(scenario);
        let cancellation = Cancellation::default();
        let trigger = cancellation.clone();
        let log = fixture.config.join("calls.jsonl");
        let phase = phase.to_owned();
        let (send, receive) = std::sync::mpsc::sync_channel(1);
        let canceller = thread::spawn(move || {
            wait_for_log(&log, &phase);
            let cancelled_at = Instant::now();
            trigger.cancel();
            send.send(cancelled_at).unwrap();
        });
        let record = fixture
            .backend
            .collect(&fixture.request, &fixture.snapshot(), &cancellation)
            .expect("cancel bounded acquisition");
        canceller.join().expect("phase canceller");
        // Half the fake runtime's 10 s hang: cancellation, not the hang ending, stopped it.
        assert!(receive.recv().unwrap().elapsed() < Duration::from_secs(5));
        assert_eq!(record.outcome, ExecutionOutcome::Cancelled);
        assert!(record.cleanup_confirmed);
        assert!(!record.isolation_observed);
        assert!(!fixture.log().contains("\"container\", \"create\""));
        assert!(!fixture.log().contains("\"container\", \"start\""));
    }
}

#[test]
fn cleanup_failure_is_indeterminate() {
    let fixture = Fixture::fake();
    fixture.scenario("cleanup_failure");
    let record = fixture
        .backend
        .collect(
            &fixture.request,
            &fixture.snapshot(),
            &Cancellation::default(),
        )
        .expect("collect uncertain cleanup");
    assert_eq!(record.outcome, ExecutionOutcome::Indeterminate);
    assert!(!record.cleanup_confirmed);
    assert!(record.isolation_observed);
}

#[test]
fn runtime_rollover_cannot_receive_success_for_an_old_identity() {
    for (scenario, started) in [
        ("rollover_before_start", false),
        ("rollover_after_start", true),
    ] {
        let fixture = Fixture::fake();
        fixture.scenario(scenario);
        let record = fixture
            .backend
            .collect(
                &fixture.request,
                &fixture.snapshot(),
                &Cancellation::default(),
            )
            .unwrap();
        assert_eq!(
            record.outcome,
            ExecutionOutcome::Indeterminate,
            "{record:?}"
        );
        assert_eq!(record.isolation_observed, started);
        assert!(!record.cleanup_confirmed);
        assert_eq!(fixture.log().contains("\"container\", \"start\""), started);
    }
}

#[test]
fn cleanup_never_accepts_absence_from_a_replacement_daemon() {
    let fixture = Fixture::fake();
    fixture.scenario("rollover_absent");
    let record = fixture
        .backend
        .collect(
            &fixture.request,
            &fixture.snapshot(),
            &Cancellation::default(),
        )
        .unwrap();
    assert_eq!(record.outcome, ExecutionOutcome::Indeterminate);
    assert!(!record.cleanup_confirmed);
    let mut handle = record.cleanup_handle.clone().unwrap();
    assert!(
        !fixture
            .backend
            .reconcile(&fixture.request, &mut handle)
            .unwrap()
    );
    assert!(!fixture.log().contains("\"container\", \"rm\""));
    assert!(
        fixture.config.join("state.json").exists(),
        "old daemon's container must remain visible as unresolved"
    );
    record.validate().unwrap();
}

#[test]
fn cleanup_deletes_by_immutable_id_and_preserves_a_name_replacement() {
    let fixture = Fixture::fake();
    fixture.scenario("replace_on_remove");
    let record = fixture
        .backend
        .collect(
            &fixture.request,
            &fixture.snapshot(),
            &Cancellation::default(),
        )
        .unwrap();
    assert_eq!(record.outcome, ExecutionOutcome::Indeterminate);
    let handle = record.cleanup_handle.clone().unwrap();
    let state: serde_json::Value =
        serde_json::from_slice(&fs::read(fixture.config.join("state.json")).unwrap()).unwrap();
    assert_eq!(state["Id"], "f".repeat(64));
    assert_eq!(
        state["Config"]["Labels"]["io.agentscm.execution"],
        "replacement-owner"
    );
    let calls: Vec<Vec<String>> = fixture
        .log()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    for args in calls
        .iter()
        .filter(|args| args.get(1).is_some_and(|s| s == "start" || s == "rm"))
    {
        assert_eq!(args.last(), handle.container_id.as_ref());
    }
}

#[test]
fn cleanup_reconciliation_checks_ownership_and_uncertain_create() {
    let fixture = Fixture::fake();
    fixture.scenario("cleanup_failure");
    let record = fixture
        .backend
        .collect(
            &fixture.request,
            &fixture.snapshot(),
            &Cancellation::default(),
        )
        .unwrap();
    let mut handle = record.cleanup_handle.clone().unwrap();
    let state_path = fixture.config.join("state.json");
    let original = fs::read(&state_path).unwrap();
    let mut wrong: serde_json::Value = serde_json::from_slice(&original).unwrap();
    wrong["Config"]["Labels"]["io.agentscm.execution"] = "another-owner".into();
    fs::write(&state_path, serde_json::to_vec(&wrong).unwrap()).unwrap();
    assert!(
        !fixture
            .backend
            .reconcile(&fixture.request, &mut handle)
            .unwrap()
    );
    fs::write(&state_path, original).unwrap();
    fixture.scenario("inspect_unavailable");
    assert!(
        !fixture
            .backend
            .reconcile(&fixture.request, &mut handle)
            .unwrap()
    );
    fixture.scenario("success");
    assert!(
        fixture
            .backend
            .reconcile(&fixture.request, &mut handle)
            .unwrap()
    );
    assert!(
        fixture
            .backend
            .reconcile(&fixture.request, &mut handle)
            .unwrap()
    );

    let unknown = Fixture::fake();
    unknown.scenario("uncertain_create");
    let record = unknown
        .backend
        .collect(
            &unknown.request,
            &unknown.snapshot(),
            &Cancellation::default(),
        )
        .unwrap();
    assert_eq!(record.outcome, ExecutionOutcome::Indeterminate);
    let mut handle = record.cleanup_handle.clone().unwrap();
    assert!(!handle.creation_confirmed);
    assert!(
        !unknown
            .backend
            .reconcile(&unknown.request, &mut handle)
            .unwrap()
    );
    record.validate().unwrap();
}

#[test]
fn newly_discovered_id_is_retained_before_uncertain_removal() {
    let fixture = Fixture::fake();
    fixture.scenario("uncertain_create");
    let record = fixture
        .backend
        .collect(
            &fixture.request,
            &fixture.snapshot(),
            &Cancellation::default(),
        )
        .unwrap();
    let mut handle = record.cleanup_handle.clone().unwrap();
    assert!(handle.container_id.is_none());
    let late = serde_json::json!({"Id":"e".repeat(64),"name":handle.container_name,
        "Config":{"Labels":{"io.agentscm.execution":handle.owner_label}},"State":{"Status":"running"}});
    fs::write(
        fixture.config.join("state.json"),
        serde_json::to_vec(&late).unwrap(),
    )
    .unwrap();
    fixture.scenario("success");
    assert!(
        !fixture
            .backend
            .reconcile(&fixture.request, &mut handle)
            .unwrap()
    );
    assert_eq!(
        handle.container_id.as_deref(),
        Some("eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee")
    );
    assert!(
        !fixture.log().contains("\"container\", \"rm\""),
        "discovery must return before destructive cleanup"
    );
    let persisted = serde_json::to_vec(&handle).unwrap();
    let mut recovered = serde_json::from_slice(&persisted).unwrap();
    fixture.scenario("lost_remove_reply");
    assert!(
        !fixture
            .backend
            .reconcile(&fixture.request, &mut recovered)
            .unwrap()
    );
    assert_eq!(recovered.container_id, handle.container_id);
    fixture.scenario("success");
    assert!(
        fixture
            .backend
            .reconcile(&fixture.request, &mut recovered)
            .unwrap()
    );
}

#[test]
fn lost_remove_reply_can_be_reconciled_from_structured_handle() {
    let fixture = Fixture::fake();
    fixture.scenario("lost_remove_reply");
    let record = fixture
        .backend
        .collect(
            &fixture.request,
            &fixture.snapshot(),
            &Cancellation::default(),
        )
        .unwrap();
    assert_eq!(record.outcome, ExecutionOutcome::Indeterminate);
    let mut handle = record.cleanup_handle.clone().unwrap();
    assert!(handle.creation_confirmed);
    assert!(
        fixture
            .backend
            .reconcile(&fixture.request, &mut handle)
            .unwrap()
    );
}

#[test]
fn unavailable_probe_stays_unavailable() {
    let fixture = Fixture::fake();
    fixture.scenario("unavailable");
    let record = fixture
        .backend
        .collect(
            &fixture.request,
            &fixture.snapshot(),
            &Cancellation::default(),
        )
        .expect("collect unavailable runtime");
    assert_eq!(record.outcome, ExecutionOutcome::Unavailable);
    assert!(record.cleanup_confirmed);
    assert!(!record.isolation_observed);
    assert!(!fixture.log().contains("\"container\", \"create\""));
}

#[test]
fn timeout_output_limit_and_active_cancellation_are_distinct() {
    let fixture = Fixture::fake();
    fixture.scenario("timeout");
    let mut timed_request = fixture.request.clone();
    timed_request.recipe.limits.timeout_ms = 100;
    timed_request.subject.recipe_sha256 = timed_request.recipe.digest().expect("recipe digest");
    let timed = fixture
        .backend
        .collect(
            &timed_request,
            &fixture.snapshot(),
            &Cancellation::default(),
        )
        .expect("timed execution");
    assert_eq!(timed.outcome, ExecutionOutcome::TimedOut);
    assert!(timed.cleanup_confirmed);

    let fixture = Fixture::fake();
    fixture.scenario("flood");
    let flooded = fixture
        .backend
        .collect(
            &fixture.request,
            &fixture.snapshot(),
            &Cancellation::default(),
        )
        .expect("output-limited execution");
    assert_eq!(flooded.outcome, ExecutionOutcome::OutputLimit);
    assert!(flooded.output_truncated);
    assert!(flooded.cleanup_confirmed);

    let fixture = Fixture::fake();
    fixture.scenario("cancel");
    let cancellation = Cancellation::default();
    let trigger = cancellation.clone();
    let log = fixture.config.join("calls.jsonl");
    let canceller = thread::spawn(move || {
        wait_for_log(&log, "\"container\", \"start\"");
        trigger.cancel();
    });
    let cancelled = fixture
        .backend
        .collect(&fixture.request, &fixture.snapshot(), &cancellation)
        .expect("actively cancelled execution");
    canceller.join().expect("canceller");
    assert_eq!(cancelled.outcome, ExecutionOutcome::Cancelled);
    assert!(cancelled.cleanup_confirmed);
}

#[test]
fn cancellation_before_runtime_does_not_create_candidate() {
    let fixture = Fixture::fake();
    let snapshot = fixture.snapshot();
    let cancellation = Cancellation::default();
    cancellation.cancel();
    let record = fixture
        .backend
        .collect(&fixture.request, &snapshot, &cancellation)
        .expect("cancelled execution");
    assert_eq!(record.outcome, ExecutionOutcome::Cancelled);
    assert!(record.cleanup_confirmed);
    assert!(!record.isolation_observed);
}

#[test]
fn failed_custody_persist_starts_nothing() {
    for fail_on in [1, 2] {
        let fixture = Fixture::fake();
        let mut calls = 0;
        let result = fixture.backend.collect_with(
            &fixture.request,
            &fixture.snapshot(),
            &Cancellation::default(),
            &mut |_| {
                calls += 1;
                if calls == fail_on {
                    Err(agentscm::Error::Conflict("fenced".into()))
                } else {
                    Ok(())
                }
            },
        );
        assert!(!fixture.log().contains("\"container\", \"start\""));
        if fail_on == 1 {
            assert!(result.is_err());
            assert!(!fixture.log().contains("\"container\", \"create\""));
        } else {
            let record = result.expect("record");
            assert_eq!(record.outcome, ExecutionOutcome::Incomplete);
            assert!(record.cleanup_confirmed);
        }
    }
}

#[test]
fn snapshot_refuses_reserved_metadata_paths() {
    for path in [
        ".git/config",
        "src/.Git/hooks/pre-commit",
        ".agentscm/state",
    ] {
        let temporary = TempDir::new().expect("temporary fixture");
        fs::set_permissions(temporary.path(), fs::Permissions::from_mode(0o700))
            .expect("private snapshot parent");
        let engine = Engine::open(temporary.path().join("store")).expect("engine");
        let revision = engine
            .checkpoint(
                None,
                &[FileChange {
                    path: path.into(),
                    content: Some(b"planted".to_vec()),
                }],
            )
            .expect("revision");
        let destination = temporary.path().join("snapshot");
        assert!(
            matches!(
                Snapshot::materialize(&engine, &revision, &destination, 32, 1024 * 1024),
                Err(agentscm::Error::Conflict(_))
            ),
            "{path}"
        );
        assert!(!destination.exists(), "{path}");
    }
}

#[test]
fn slow_create_has_its_own_longer_deadline() {
    let fixture = Fixture::fake();
    fixture.scenario("slow_create");
    let record = fixture
        .backend
        .collect(
            &fixture.request,
            &fixture.snapshot(),
            &Cancellation::default(),
        )
        .unwrap();
    assert_eq!(record.outcome, ExecutionOutcome::Success, "{record:?}");
}

#[test]
fn late_create_is_persisted_and_removed_in_the_same_run() {
    let fixture = Fixture::fake();
    fixture.scenario("late_create");
    let mut persisted = Vec::new();
    let record = fixture
        .backend
        .collect_with(
            &fixture.request,
            &fixture.snapshot(),
            &Cancellation::default(),
            &mut |handle| {
                persisted.push(handle.clone());
                Ok(())
            },
        )
        .unwrap();
    assert_eq!(record.outcome, ExecutionOutcome::Incomplete, "{record:?}");
    assert!(record.cleanup_confirmed && record.cleanup_handle.is_none());
    // The discovered ID was persisted before the removal that used it.
    assert!(persisted.iter().any(|handle| handle.container_id.is_some()));
    let log = fixture.log();
    assert!(log.contains("\"container\", \"rm\""));
    assert!(!log.contains("\"container\", \"start\""));
    assert!(!fixture.config.join("state.json").exists());
    record.validate().unwrap();
}

#[test]
fn late_create_stays_indeterminate_when_its_id_cannot_be_persisted() {
    let fixture = Fixture::fake();
    fixture.scenario("late_create");
    let record = fixture
        .backend
        .collect_with(
            &fixture.request,
            &fixture.snapshot(),
            &Cancellation::default(),
            &mut |handle| {
                if handle.container_id.is_some() {
                    Err(agentscm::Error::Conflict("fenced".into()))
                } else {
                    Ok(())
                }
            },
        )
        .unwrap();
    assert_eq!(
        record.outcome,
        ExecutionOutcome::Indeterminate,
        "{record:?}"
    );
    assert!(record.cleanup_handle.unwrap().container_id.is_some());
    assert!(!fixture.log().contains("\"container\", \"rm\""));
}

#[test]
#[ignore = "requires explicitly selected local Docker socket and immutable image"]
fn real_runtime_smoke_checks_source_scratch_identity_and_socket_absence() {
    let executable = required_absolute_env("AGENTSCM_DOCKER");
    let socket = required_absolute_env("AGENTSCM_DOCKER_SOCKET");
    let image = std::env::var("AGENTSCM_OCI_IMAGE")
        .expect("set AGENTSCM_OCI_IMAGE to sha256:<64 lowercase hex>");
    let temporary = TempDir::new().expect("temporary fixture");
    fs::set_permissions(temporary.path(), fs::Permissions::from_mode(0o700))
        .expect("private snapshot parent");
    let config = temporary.path().join("config");
    fs::create_dir(&config).expect("config directory");
    fs::set_permissions(&config, fs::Permissions::from_mode(0o700))
        .expect("private config directory");
    let backend = OciBackend::new(&executable, &socket, &config).expect("real backend");
    let engine = Engine::open(temporary.path().join("store")).expect("engine");
    let revision = engine
        .checkpoint(
            None,
            &[FileChange {
                path: "canary.txt".into(),
                content: Some(b"immutable\n".to_vec()),
            }],
        )
        .expect("revision");
    let script = "set -eu; test \"$(/bin/busybox id -u)\" = 65532; test \"$HOME\" = /scratch; test \"$TMPDIR\" = /scratch; test -z \"${DOCKER_HOST+x}\"; test -r /source/canary.txt; ! /bin/busybox touch /source/mutation; ! /bin/busybox touch /tmp/rootfs-mutation; /bin/busybox touch /scratch/writable; test ! -e /authority; test ! -e /var/run/docker.sock; test -z \"$(/bin/busybox ip route 2>/dev/null)\"";
    let mut request = request(
        &revision,
        image,
        vec!["/bin/sh".into(), "-c".into(), script.into()],
    );
    request.recipe.environment.insert(
        "PATH".into(),
        "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin".into(),
    );
    request.recipe.environment_sha256 =
        environment_digest(&request.recipe.environment).expect("environment digest");
    request.subject.environment_sha256 = request.recipe.environment_sha256.clone();
    request.subject.recipe_sha256 = request.recipe.digest().expect("recipe digest");
    let snapshot = Snapshot::materialize(
        &engine,
        &revision,
        &temporary.path().join("snapshot"),
        32,
        1024 * 1024,
    )
    .expect("snapshot");
    let record = backend
        .collect(&request, &snapshot, &Cancellation::default())
        .expect("real collection");
    assert_eq!(record.outcome, ExecutionOutcome::Success, "{record:?}");
    assert!(record.isolation_observed);
    assert!(record.cleanup_confirmed);
    record.validate().expect("valid real record");
}

#[test]
#[ignore = "requires explicitly selected local Docker executable and socket"]
fn live_runtime_probe_reports_observed_availability() {
    let executable = required_absolute_env("AGENTSCM_DOCKER");
    let socket = required_absolute_env("AGENTSCM_DOCKER_SOCKET");
    let temporary = TempDir::new().expect("temporary fixture");
    fs::set_permissions(temporary.path(), fs::Permissions::from_mode(0o700))
        .expect("private snapshot parent");
    let config = temporary.path().join("config");
    fs::create_dir(&config).expect("config directory");
    fs::set_permissions(&config, fs::Permissions::from_mode(0o700))
        .expect("private config directory");
    let probe = OciBackend::new(&executable, &socket, &config)
        .expect("real backend")
        .probe();
    eprintln!("runtime probe: {probe:?}");
    assert!(probe.available, "{probe:?}");
}

fn request(revision: &str, image: String, argv: Vec<String>) -> ExecutionRequest {
    let environment = BTreeMap::new();
    let environment_sha256 = environment_digest(&environment).expect("environment digest");
    let recipe = CommandRecipe {
        version: 1,
        image,
        argv,
        environment,
        toolchain_sha256: DIGEST_2.into(),
        environment_sha256: environment_sha256.clone(),
        limits: ExecutionLimits {
            timeout_ms: 5_000,
            max_output_bytes: 16 * 1024,
            memory_bytes: 64 * 1024 * 1024,
            scratch_bytes: 4 * 1024 * 1024,
            pids: 16,
            cpu_millis: 1_000,
        },
        syscall_policy: SyscallPolicy::DockerBuiltin,
        no_new_privileges: true,
    };
    let recipe_sha256 = recipe.digest().expect("recipe digest");
    ExecutionRequest {
        attempt_id: "execution-fixture".into(),
        subject: CheckSubject {
            repository: "repository".into(),
            change: ChangeId {
                repository: "repository".into(),
                id: "change".into(),
            },
            change_version: 1,
            context_digest: DIGEST_3.into(),
            revision: revision.into(),
            recipe_sha256,
            toolchain_sha256: DIGEST_2.into(),
            environment_sha256,
        },
        run_generation: 1,
        recipe,
    }
}

fn required_absolute_env(name: &str) -> PathBuf {
    let path = PathBuf::from(std::env::var_os(name).unwrap_or_else(|| panic!("set {name}")));
    assert!(path.is_absolute(), "{name} must be absolute");
    path
}

fn wait_for_log(path: &PathBuf, needle: &str) {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if fs::read_to_string(path)
            .unwrap_or_default()
            .contains(needle)
        {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "fake Docker phase {needle} was not observed; log={}",
            fs::read_to_string(path).unwrap_or_default()
        );
        thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn snapshot_refuses_non_regular_entries_instead_of_flattening() {
    for kind in [EntryKind::Executable, EntryKind::Symlink] {
        let temporary = TempDir::new().expect("temporary fixture");
        fs::set_permissions(temporary.path(), fs::Permissions::from_mode(0o700))
            .expect("private snapshot parent");
        let engine = Engine::open(temporary.path().join("store")).expect("engine");
        let revision = engine
            .checkpoint_entries(
                None,
                &[EntryChange {
                    path: "special".into(),
                    content: Some(b"target".to_vec()),
                    kind,
                }],
            )
            .expect("revision");
        let destination = temporary.path().join("snapshot");
        let Err(error) = Snapshot::materialize(&engine, &revision, &destination, 32, 1024 * 1024)
        else {
            panic!("expected refusal");
        };
        let message = error.to_string();
        assert!(
            matches!(error, agentscm::Error::Invalid(_))
                && message.contains("unsupported entry kind: special"),
            "{message}"
        );
        assert!(!destination.exists());
    }
}

#[test]
fn candidate_change_to_a_symlink_path_conflicts_and_executables_keep_their_kind() {
    use agentscm::execution::CandidateOutput;
    let temporary = TempDir::new().expect("temporary fixture");
    let engine = Engine::open(temporary.path().join("store")).expect("engine");
    let base = engine
        .checkpoint_entries(
            None,
            &[
                EntryChange {
                    path: "link".into(),
                    content: Some(b"target".to_vec()),
                    kind: EntryKind::Symlink,
                },
                EntryChange {
                    path: "tool".into(),
                    content: Some(b"old".to_vec()),
                    kind: EntryKind::Executable,
                },
            ],
        )
        .expect("base");
    let to_link = CandidateOutput::parse_changes(
        br#"{"version":1,"changes":[{"path":"link","hex":"6162"}]}"#,
    )
    .expect("candidate");
    assert!(matches!(
        engine.checkpoint(Some(&base), &to_link),
        Err(agentscm::Error::Conflict(_))
    ));
    let to_tool = CandidateOutput::parse_changes(
        br#"{"version":1,"changes":[{"path":"tool","hex":"6e6577"}]}"#,
    )
    .expect("candidate");
    let revision = engine.checkpoint(Some(&base), &to_tool).expect("edit");
    assert_eq!(
        engine.read_entry(&revision, "tool").unwrap(),
        (EntryKind::Executable, b"new".to_vec())
    );
}
