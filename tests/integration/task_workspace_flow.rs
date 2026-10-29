#![cfg(unix)]

use std::fs;
use std::os::unix::fs::{MetadataExt, PermissionsExt, symlink};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use agentscm::Error;
use agentscm::authority::Authority;
use agentscm::checks::{
    CheckLimits, CheckState, SOURCE_PATHS_CAPABILITY, SOURCE_PATHS_EVIDENCE, SourcePathExpectation,
    SourcePathsRecipe, SourcePathsRequest,
};
use agentscm::context::{
    BaseState, ChangeId, DependencyClaim, DependencyState, InputIdentity, RequiredEvidence,
    ReviewContext,
};
use agentscm::git_export::{CommitMetadata, ExportRequest, Mirror};
use agentscm::git_snapshot::{SnapshotRequest, capture, preflight};
use agentscm::merge::{MergeJournal, MergeOutcome, MergePhase};
use agentscm::model::{
    Approval, IntegrateRequest, LandingPreparation, PrepareLandingRequest, TargetState,
};
use agentscm::store::{Engine, digest};
use agentscm::workspace::{OperationOutcome, Workspace, WorkspacePathState};
use tempfile::TempDir;

const TOOLCHAIN: &str = "cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc";
const ENVIRONMENT: &str = "dddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddd";

#[derive(Clone, Copy, Debug)]
struct TreeStats {
    files: u64,
    logical_bytes: u64,
    allocated_bytes: u64,
}

struct Captured {
    root: TempDir,
    engine: Engine,
    authority: Authority,
    repository: String,
    git_repository: PathBuf,
    git_commit: String,
    base: String,
    capture_elapsed: Duration,
}

impl Captured {
    fn new() -> Self {
        let root = TempDir::new().unwrap();
        let git_repository = root.path().join("origin");
        let git_commit = seed_git_repository(&git_repository);
        let data = root.path().join("native");
        let engine = Engine::open(&data).unwrap();
        let authority = Authority::open(&data).unwrap();
        let repository = authority.repository_id().unwrap();
        let request = SnapshotRequest {
            git: git_path(),
            repository: git_repository.clone(),
            revision: git_commit.clone(),
            exclude: vec![],
        };
        let capture_started = Instant::now();
        let report = preflight(&request).unwrap();
        assert!(report.eligible, "{report:?}");
        let base = capture(&engine, &repository, &request, report)
            .unwrap()
            .revision;
        let capture_elapsed = capture_started.elapsed();
        Self {
            root,
            engine,
            authority,
            repository,
            git_repository,
            git_commit,
            base,
            capture_elapsed,
        }
    }
}

fn git_path() -> PathBuf {
    let path =
        PathBuf::from(std::env::var("AGENTSCM_TEST_GIT").unwrap_or_else(|_| "/usr/bin/git".into()));
    assert!(path.is_absolute() && path.exists());
    path
}

fn git(directory: &Path, arguments: &[&str]) -> String {
    let output = Command::new(git_path())
        .env_clear()
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .arg("-C")
        .arg(directory)
        .args(["-c", "core.hooksPath=/dev/null"])
        .args(arguments)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "git {arguments:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap().trim().to_owned()
}

fn write(root: &Path, relative: &str, bytes: &[u8]) {
    let path = root.join(relative);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, bytes).unwrap();
}

fn seed_git_repository(root: &Path) -> String {
    fs::create_dir(root).unwrap();
    git(root, &["init", "--object-format=sha1"]);
    git(root, &["config", "user.name", "Task Workspace Test"]);
    git(
        root,
        &["config", "user.email", "task-workspace@example.invalid"],
    );
    write(root, "app/input.txt", b"application\n");
    write(root, "app/main.txt", b"before\n");
    write(root, "app/delete-me.txt", b"delete me\n");
    write(root, "app/rename-me.txt", b"rename me\n");
    write(root, "app/mode.sh", b"#!/bin/sh\nexit 0\n");
    write(root, "config/settings.txt", b"profile=local\n");
    write(root, "deps/value.txt", b"dependency\n");
    write(root, "docs/other.txt", b"other branch\n");
    write(root, "unseen/keep.txt", b"must survive\n");
    for index in 0..96 {
        write(
            root,
            &format!("large/{index:03}.txt"),
            &[u8::try_from(index % 8).unwrap() + b'a'; 4096],
        );
    }
    git(root, &["add", "."]);
    git(
        root,
        &[
            "-c",
            "user.name=Task Workspace Test",
            "-c",
            "user.email=task-workspace@example.invalid",
            "-c",
            "commit.gpgsign=false",
            "commit",
            "-m",
            "synthetic base",
        ],
    );
    git(root, &["rev-parse", "HEAD"])
}

fn clone_repository(source: &Path, destination: &Path, sparse: bool) -> Duration {
    let started = Instant::now();
    let parent = destination.parent().unwrap();
    let source = source.to_str().unwrap();
    let destination = destination.to_str().unwrap();
    if sparse {
        git(
            parent,
            &["clone", "--shared", "--no-checkout", source, destination],
        );
        let destination = Path::new(destination);
        git(
            destination,
            &[
                "sparse-checkout",
                "set",
                "--no-cone",
                "/app/",
                "/config/settings.txt",
            ],
        );
        git(destination, &["checkout", "--quiet", "HEAD"]);
    } else {
        git(parent, &["clone", "--shared", source, destination]);
    }
    started.elapsed()
}

fn dependency_command(root: &Path) -> bool {
    Command::new("/bin/sh")
        .arg("-c")
        .arg(
            "test -f app/input.txt && grep -q '^profile=local$' config/settings.txt && grep -q '^dependency$' deps/value.txt",
        )
        .current_dir(root)
        .status()
        .unwrap()
        .success()
}

fn stats(root: &Path) -> TreeStats {
    fn walk(path: &Path, result: &mut TreeStats) {
        for entry in fs::read_dir(path).unwrap() {
            let entry = entry.unwrap();
            if matches!(
                entry.file_name().to_str(),
                Some(".git" | ".agentscm-workspace")
            ) {
                continue;
            }
            let path = entry.path();
            let metadata = fs::symlink_metadata(&path).unwrap();
            if metadata.is_dir() {
                walk(&path, result);
            } else {
                result.files += 1;
                result.logical_bytes += if metadata.file_type().is_symlink() {
                    fs::read_link(&path).unwrap().as_os_str().len() as u64
                } else {
                    metadata.len()
                };
                result.allocated_bytes += metadata.blocks().saturating_mul(512);
            }
        }
    }
    let mut result = TreeStats {
        files: 0,
        logical_bytes: 0,
        allocated_bytes: 0,
    };
    walk(root, &mut result);
    result
}

fn object_stats(root: &Path) -> TreeStats {
    stats(&root.join("native/objects"))
}

fn identity(byte: u8) -> InputIdentity {
    InputIdentity::Digest {
        sha256: std::iter::repeat_n(char::from(byte), 64).collect(),
    }
}

fn context(
    repository: &str,
    id: &str,
    version: u64,
    revision: &str,
    base: Option<TargetState>,
    recipe: Option<&SourcePathsRecipe>,
) -> ReviewContext {
    let required = recipe.map(|recipe| recipe.digest().unwrap());
    ReviewContext {
        version: 1,
        change: ChangeId {
            repository: repository.to_owned(),
            id: id.to_owned(),
        },
        change_version: version,
        revision: revision.to_owned(),
        base: base.map(|state| BaseState {
            revision: state.revision,
            target_version: state.version,
        }),
        target: "main".into(),
        policy_epoch: 0,
        intent: "qualify a task-scoped source edit".into(),
        acceptance_criteria: vec!["exact checked bytes land and export".into()],
        scope: vec!["app".into(), "deps/value.txt".into()],
        exclusions: vec!["large is not materialized for the task".into()],
        omissions: vec![],
        dependencies: vec![DependencyClaim {
            identity: "local-source-dependency".into(),
            state: DependencyState::Resolved {
                revision: revision.to_owned(),
            },
        }],
        recipe: required.as_ref().map_or_else(
            || identity(b'b'),
            |sha256| InputIdentity::Digest {
                sha256: sha256.clone(),
            },
        ),
        toolchain: InputIdentity::Digest {
            sha256: TOOLCHAIN.into(),
        },
        environment: InputIdentity::Digest {
            sha256: ENVIRONMENT.into(),
        },
        required_evidence: required
            .map(|sha256| {
                vec![RequiredEvidence {
                    identity: SOURCE_PATHS_EVIDENCE.into(),
                    digest: InputIdentity::Digest { sha256 },
                }]
            })
            .unwrap_or_default(),
        required_capabilities: if recipe.is_some() {
            vec![
                "agentscm.review-context/v1".into(),
                SOURCE_PATHS_CAPABILITY.into(),
            ]
        } else {
            vec!["agentscm.review-context/v1".into()]
        },
    }
}

fn approval(id: &str, state: &agentscm::context::ChangeState, revision: &str) -> Approval {
    Approval {
        approval_id: id.into(),
        target: "main".into(),
        revision: revision.into(),
        context_digest: state.context_digest.clone(),
        change: Some(state.id.clone()),
        change_version: Some(state.version),
        policy_epoch: 0,
    }
}

fn integrate_request(
    operation: &str,
    state: &agentscm::context::ChangeState,
    expected: Option<TargetState>,
    revision: &str,
    approval_id: &str,
) -> IntegrateRequest {
    IntegrateRequest {
        operation_id: operation.into(),
        target: "main".into(),
        expected_target: expected,
        revision: revision.into(),
        context_digest: state.context_digest.clone(),
        approval_id: approval_id.into(),
        change: Some(state.id.clone()),
        change_version: Some(state.version),
        policy_epoch: 0,
    }
}

fn initialize_target(captured: &mut Captured) -> TargetState {
    let initial = context(
        &captured.repository,
        "initialize",
        1,
        &captured.base,
        None,
        None,
    );
    let state = captured.authority.start_change(&initial).unwrap();
    captured
        .authority
        .approve(&approval("initialize", &state, &captured.base))
        .unwrap();
    captured
        .authority
        .integrate(
            &captured.engine,
            &integrate_request("initialize", &state, None, &captured.base, "initialize"),
        )
        .unwrap()
        .after
}

fn apply_task_edits(root: &Path) {
    write(root, "app/main.txt", b"after\n");
    write(root, "app/added.txt", b"added\n");
    fs::remove_file(root.join("app/delete-me.txt")).unwrap();
    fs::rename(root.join("app/rename-me.txt"), root.join("app/renamed.txt")).unwrap();
    let mode = root.join("app/mode.sh");
    let mut permissions = fs::metadata(&mode).unwrap().permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(mode, permissions).unwrap();
    symlink("../deps/value.txt", root.join("app/value-link")).unwrap();
}

fn checkpointed(outcome: &OperationOutcome) -> (&str, u64) {
    match outcome {
        OperationOutcome::Checkpointed {
            revision,
            generation,
        } => (revision, *generation),
        other => panic!("expected checkpoint, got {other:?}"),
    }
}

#[test]
fn qualification_compares_full_and_scoped_git_and_native_workspaces() {
    let captured = Captured::new();
    let full_git = captured.root.path().join("full-git");
    let sparse_git = captured.root.path().join("sparse-git");
    let full_git_elapsed = clone_repository(&captured.git_repository, &full_git, false);
    let sparse_git_elapsed = clone_repository(&captured.git_repository, &sparse_git, true);
    let full_git_checkout = stats(&full_git);
    let sparse_git_initial_checkout = stats(&sparse_git);
    assert_eq!(
        captured
            .engine
            .read_file(&captured.base, "app/input.txt")
            .unwrap(),
        b"application\n"
    );
    assert!(dependency_command(&full_git));
    assert!(!dependency_command(&sparse_git));
    let sparse_expand = Instant::now();
    git(&sparse_git, &["sparse-checkout", "add", "/deps/value.txt"]);
    let sparse_expand_elapsed = sparse_expand.elapsed();
    assert!(dependency_command(&sparse_git));
    let sparse_git_useful_checkout = stats(&sparse_git);

    let full_native = captured.root.path().join("full-native");
    let full_native_started = Instant::now();
    Workspace::create(
        &captured.engine,
        &full_native,
        ChangeId {
            repository: captured.repository.clone(),
            id: "full-native".into(),
        },
        &captured.base,
    )
    .unwrap();
    let full_native_elapsed = full_native_started.elapsed();
    let full_native_checkout = stats(&full_native);
    assert!(dependency_command(&full_native));

    let scoped_native = captured.root.path().join("scoped-native");
    let scoped_started = Instant::now();
    let mut scoped = Workspace::create_scoped(
        &captured.engine,
        &scoped_native,
        ChangeId {
            repository: captured.repository.clone(),
            id: "scoped-native".into(),
        },
        &captured.base,
        &["app".into(), "config/settings.txt".into()],
    )
    .unwrap();
    let scoped_elapsed = scoped_started.elapsed();
    let scoped_native_initial_checkout = stats(&scoped_native);
    assert!(!scoped_native.join("large").exists());
    assert!(!scoped_native.join("deps").exists());
    assert!(!dependency_command(&scoped_native));
    assert_eq!(
        scoped
            .path_state(&captured.engine, "app/input.txt")
            .unwrap(),
        WorkspacePathState::Materialized
    );
    assert_eq!(
        scoped
            .path_state(&captured.engine, "deps/value.txt")
            .unwrap(),
        WorkspacePathState::Unmaterialized
    );
    assert_eq!(
        scoped.path_state(&captured.engine, "absent.txt").unwrap(),
        WorkspacePathState::Nonexistent
    );
    assert!(
        captured
            .engine
            .stat_file(&captured.base, "deps/value.txt")
            .unwrap()
            .is_some()
    );
    assert!(
        scoped
            .state()
            .unwrap()
            .scope
            .as_ref()
            .unwrap()
            .iter()
            .any(|root| root == "app")
    );
    assert!(
        scoped
            .state()
            .unwrap()
            .scope
            .as_ref()
            .unwrap()
            .iter()
            .any(|root| root == "config/settings.txt")
    );
    let expand_started = Instant::now();
    assert_eq!(
        scoped
            .expand(
                &captured.engine,
                "dependency-expansion",
                0,
                &["deps/value.txt".into()],
            )
            .unwrap(),
        OperationOutcome::Expanded { generation: 1 }
    );
    let scoped_expand_elapsed = expand_started.elapsed();
    assert!(dependency_command(&scoped_native));
    assert_eq!(
        scoped
            .path_state(&captured.engine, "deps/value.txt")
            .unwrap(),
        WorkspacePathState::Materialized
    );
    let scoped_native_useful_checkout = stats(&scoped_native);

    let store_before_reuse = object_stats(captured.root.path());
    let fallback = captured.root.path().join("full-fallback");
    let mut fallback_workspace = Workspace::create_scoped(
        &captured.engine,
        &fallback,
        ChangeId {
            repository: captured.repository.clone(),
            id: "full-fallback".into(),
        },
        &captured.base,
        &["app".into()],
    )
    .unwrap();
    assert_eq!(
        fallback_workspace
            .expand(&captured.engine, "full-fallback", 0, &[".".into()])
            .unwrap(),
        OperationOutcome::Expanded { generation: 1 }
    );
    assert!(fallback_workspace.state().unwrap().scope.is_none());
    assert!(fallback.join("unseen/keep.txt").is_file());
    let store_after_reuse = object_stats(captured.root.path());
    assert_eq!(store_after_reuse.files, store_before_reuse.files);
    assert_eq!(
        store_after_reuse.logical_bytes,
        store_before_reuse.logical_bytes
    );
    assert_eq!(
        store_after_reuse.allocated_bytes,
        store_before_reuse.allocated_bytes
    );

    let full_git_control = stats(&full_git.join(".git"));
    let sparse_git_control = stats(&sparse_git.join(".git"));
    let shared_git_objects = stats(&captured.git_repository.join(".git/objects"));
    assert!(sparse_git_initial_checkout.files < full_git_checkout.files);
    assert!(sparse_git_useful_checkout.files < full_git_checkout.files);
    assert!(scoped_native_initial_checkout.files < full_native_checkout.files);
    assert!(scoped_native_useful_checkout.files < full_native_checkout.files);
    eprintln!(
        "task-workspace qualification (local filesystem only; no network/model metric): full_git_shared={{first_useful:{full_git_elapsed:?},checkout:{full_git_checkout:?},control:{full_git_control:?}}} sparse_git_shared_no_cone={{initial:{sparse_git_elapsed:?},first_useful:{:?},initial_checkout:{sparse_git_initial_checkout:?},useful_checkout:{sparse_git_useful_checkout:?},control:{sparse_git_control:?}}} shared_git_objects={shared_git_objects:?} native_git_import={{preflight_and_capture:{:?}}} full_native={{first_useful:{full_native_elapsed:?},checkout:{full_native_checkout:?}}} scoped_native={{initial:{scoped_elapsed:?},first_useful:{:?},initial_checkout:{scoped_native_initial_checkout:?},useful_checkout:{scoped_native_useful_checkout:?}}} native_store={{objects:{},bytes:{},allocated:{},reused_without_growth:true}}",
        sparse_git_elapsed + sparse_expand_elapsed,
        captured.capture_elapsed,
        scoped_elapsed + scoped_expand_elapsed,
        store_after_reuse.files,
        store_after_reuse.logical_bytes,
        store_after_reuse.allocated_bytes,
    );
}

#[test]
fn scoped_edits_recover_undo_check_land_and_export_the_complete_tree() {
    let mut captured = Captured::new();
    let target = initialize_target(&mut captured);
    let work_context = context(
        &captured.repository,
        "task-work",
        1,
        &captured.base,
        Some(target.clone()),
        None,
    );
    let work = captured.authority.start_change(&work_context).unwrap();
    let workspace_root = captured.root.path().join("task-workspace");
    let mut workspace = Workspace::create_scoped(
        &captured.engine,
        &workspace_root,
        work.id.clone(),
        &captured.base,
        &["app".into()],
    )
    .unwrap();
    let mut stale = Workspace::open(&workspace_root).unwrap();
    workspace
        .expand(
            &captured.engine,
            "expand-dependency",
            0,
            &["deps/value.txt".into()],
        )
        .unwrap();
    assert!(matches!(
        stale.checkpoint(&captured.engine, "stale-checkpoint", 0),
        Err(Error::Conflict(_))
    ));
    apply_task_edits(&workspace_root);
    let first = workspace
        .checkpoint(&captured.engine, "task-checkpoint", 1)
        .unwrap();
    let (tip, generation) = checkpointed(&first);
    assert_eq!(generation, 2);
    assert_eq!(
        captured.engine.read_file(tip, "unseen/keep.txt").unwrap(),
        b"must survive\n"
    );
    assert!(
        captured
            .engine
            .stat_file(tip, "app/delete-me.txt")
            .unwrap()
            .is_none()
    );
    assert!(
        captured
            .engine
            .stat_file(tip, "app/renamed.txt")
            .unwrap()
            .is_some()
    );

    // Lost-reply/restart recovery returns the durable checkpoint, then undo restores the
    // exact scoped before-state without touching source that was never materialized.
    drop(workspace);
    let mut workspace = Workspace::open(&workspace_root).unwrap();
    assert_eq!(
        workspace
            .checkpoint(&captured.engine, "task-checkpoint", 1)
            .unwrap(),
        first
    );
    assert!(matches!(
        workspace
            .undo(&captured.engine, "undo-task", 2, "task-checkpoint")
            .unwrap(),
        OperationOutcome::Undone { generation: 3, .. }
    ));
    assert_eq!(
        fs::read(workspace_root.join("app/main.txt")).unwrap(),
        b"before\n"
    );
    assert!(!workspace_root.join("app/added.txt").exists());
    apply_task_edits(&workspace_root);
    let final_checkpoint = workspace
        .checkpoint(&captured.engine, "task-checkpoint-final", 3)
        .unwrap();
    let (source_tip, generation) = checkpointed(&final_checkpoint);
    assert_eq!(generation, 4);
    assert_eq!(
        source_tip, tip,
        "identical bytes must recover the same revision"
    );

    let checked_bytes = b"after\n";
    let recipe = SourcePathsRecipe {
        version: 1,
        expectations: vec![SourcePathExpectation {
            path: "app/main.txt".into(),
            sha256: digest(checked_bytes),
        }],
        limits: CheckLimits {
            max_paths: 8,
            max_total_bytes: 1024 * 1024,
            max_output_bytes: 4096,
            timeout_ms: 30_000,
        },
    };
    let next_context = context(
        &captured.repository,
        "task-work",
        2,
        source_tip,
        Some(target.clone()),
        Some(&recipe),
    );
    let prepared = captured
        .authority
        .prepare_landing(
            &captured.engine,
            &PrepareLandingRequest {
                operation_id: "prepare-task".into(),
                target: "main".into(),
                expected_target: target.clone(),
                source_tip: source_tip.into(),
                change_version: work.version,
                context: next_context,
            },
        )
        .unwrap();
    let (landing, prepared_change) = match prepared {
        LandingPreparation::Prepared {
            revision, change, ..
        } => (revision, change),
        other => panic!("expected prepared landing, got {other:?}"),
    };
    let landing_context = captured
        .authority
        .review_context(&prepared_change.context_digest)
        .unwrap();
    let attempt = SourcePathsRequest {
        attempt_id: "task-source-check".into(),
        change: prepared_change.id.clone(),
        change_version: prepared_change.version,
        context_digest: prepared_change.context_digest.clone(),
        revision: landing.clone(),
        recipe: recipe.clone(),
    };
    captured
        .authority
        .create_source_paths_attempt(&attempt)
        .unwrap();
    let checked = captured
        .authority
        .run_source_paths_attempt(&captured.engine, "task-source-check", 0)
        .unwrap();
    assert_eq!(checked.state, CheckState::Success);
    assert_eq!(landing_context.revision, landing);
    captured
        .authority
        .approve(&approval("task-approval", &prepared_change, &landing))
        .unwrap();
    let event = captured
        .authority
        .integrate(
            &captured.engine,
            &integrate_request(
                "integrate-task",
                &prepared_change,
                Some(target),
                &landing,
                "task-approval",
            ),
        )
        .unwrap();

    let reference = captured.root.path().join("reference");
    clone_repository(&captured.git_repository, &reference, false);
    apply_task_edits(&reference);
    git(&reference, &["add", "-A"]);
    let expected_tree = git(&reference, &["write-tree"]);

    let mirror_path = captured.root.path().join("mirror.git");
    let mut mirror = Mirror::initialize_with_seed(
        &git_path(),
        &mirror_path,
        &captured.repository,
        "main",
        &captured.git_repository,
        &captured.git_commit,
    )
    .unwrap();
    let mirror_before_export = stats(&mirror_path);
    let export_started = Instant::now();
    let exported = mirror
        .export(
            &captured.engine,
            &captured.authority,
            &ExportRequest {
                event_sequence: event.sequence,
                expected_oid: Some(captured.git_commit.clone()),
                metadata: CommitMetadata {
                    name: "Task Workspace Test".into(),
                    email: "task-workspace@example.invalid".into(),
                    timestamp: 1_800_000_000,
                    message: "Export task-scoped synthetic change".into(),
                },
            },
        )
        .unwrap();
    let export_elapsed = export_started.elapsed();
    let mirror_after_export = stats(&mirror_path);
    eprintln!(
        "task-workspace native-to-git export (local filesystem only): elapsed={export_elapsed:?} mirror_before={mirror_before_export:?} mirror_after={mirror_after_export:?}"
    );
    assert_eq!(
        exported.record.tree.as_deref(),
        Some(expected_tree.as_str())
    );
    assert_eq!(
        captured
            .engine
            .read_file(&event.after.revision, "unseen/keep.txt")
            .unwrap(),
        b"must survive\n"
    );
    git(
        &mirror_path,
        &[
            "fsck",
            "--no-reflogs",
            "--no-dangling",
            exported.record.commit.as_deref().unwrap(),
        ],
    );
}

#[test]
fn disjoint_scoped_checkpoints_merge_without_losing_unmaterialized_source() {
    let root = TempDir::new().unwrap();
    let engine = Engine::open(root.path().join("source")).unwrap();
    let base = engine
        .checkpoint(
            None,
            &[
                agentscm::model::FileChange {
                    path: "left.txt".into(),
                    content: Some(b"left before\n".to_vec()),
                },
                agentscm::model::FileChange {
                    path: "right.txt".into(),
                    content: Some(b"right before\n".to_vec()),
                },
                agentscm::model::FileChange {
                    path: "unseen.txt".into(),
                    content: Some(b"preserved\n".to_vec()),
                },
            ],
        )
        .unwrap();
    let mut left = Workspace::create_scoped(
        &engine,
        root.path().join("left"),
        ChangeId {
            repository: "repository".into(),
            id: "left".into(),
        },
        &base,
        &["left.txt".into()],
    )
    .unwrap();
    let mut right = Workspace::create_scoped(
        &engine,
        root.path().join("right"),
        ChangeId {
            repository: "repository".into(),
            id: "right".into(),
        },
        &base,
        &["right.txt".into()],
    )
    .unwrap();
    write(&root.path().join("left"), "left.txt", b"left after\n");
    write(&root.path().join("right"), "right.txt", b"right after\n");
    let left_revision = left.checkpoint(&engine, "left-checkpoint", 0).unwrap();
    let right_revision = right.checkpoint(&engine, "right-checkpoint", 0).unwrap();
    let (left_revision, _) = checkpointed(&left_revision);
    let (right_revision, _) = checkpointed(&right_revision);
    let mut journal = MergeJournal::open(root.path().join("source"), "repository").unwrap();
    let (session, generation) = match journal
        .begin(&engine, "merge-disjoint", left_revision, right_revision)
        .unwrap()
    {
        MergeOutcome::Prepared {
            session,
            generation,
            phase: MergePhase::Ready,
        } => (session, generation),
        other => panic!("expected ready disjoint merge, got {other:?}"),
    };
    let merged = match journal
        .complete(&engine, &session, "complete-disjoint", generation)
        .unwrap()
    {
        MergeOutcome::Completed { revision, .. } => revision,
        other => panic!("expected completed disjoint merge, got {other:?}"),
    };
    assert_eq!(
        engine.read_file(&merged, "left.txt").unwrap(),
        b"left after\n"
    );
    assert_eq!(
        engine.read_file(&merged, "right.txt").unwrap(),
        b"right after\n"
    );
    assert_eq!(
        engine.read_file(&merged, "unseen.txt").unwrap(),
        b"preserved\n"
    );
}
