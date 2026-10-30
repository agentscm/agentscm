use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tempfile::TempDir;

fn run(data: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_agentscm"))
        .arg("--data-dir")
        .arg(data)
        .args(args)
        .env("PATH", "") // The native developer workflow must not invoke Git.
        .output()
        .unwrap()
}

fn success(data: &Path, args: &[&str]) -> Value {
    let output = run(data, args);
    assert!(
        output.status.success(),
        "{args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap()
}

fn failure(data: &Path, args: &[&str]) -> Value {
    let output = run(data, args);
    assert!(!output.status.success(), "{args:?}");
    serde_json::from_slice(&output.stderr).unwrap()
}

fn input(path: &Path, value: &Value) {
    fs::write(path, serde_json::to_vec(value).unwrap()).unwrap();
}

fn context(repository: &str, revision: &str) -> Value {
    json!({
        "version":1,"change":{"repository":repository,"id":"work"},"change_version":1,
        "revision":revision,"base":null,"target":"main","policy_epoch":0,
        "intent":"Verify the native workflow","acceptance_criteria":["Source stays readable"],
        "scope":["a.txt"],"exclusions":[],"omissions":[],"dependencies":[],
        "recipe":{"state":"digest","sha256":"a".repeat(64)},
        "toolchain":{"state":"digest","sha256":"b".repeat(64)},
        "environment":{"state":"digest","sha256":"c".repeat(64)},
        "required_evidence":[],"required_capabilities":["agentscm.review-context/v1"]
    })
}

fn initialize(root: &Path) -> (String, String) {
    let repository = success(root, &["init"])["repository"]
        .as_str()
        .unwrap()
        .to_owned();
    let request = root.join("fixture.json");
    input(
        &request,
        &json!({"parent":null,"changes":[{"path":"a.txt","content":[97]}]}),
    );
    let revision = success(root, &["checkpoint", request.to_str().unwrap()])["revision"]
        .as_str()
        .unwrap()
        .to_owned();
    (repository, revision)
}

#[test]
fn workspace_checkpoint_and_offline_exchange_preserve_source_without_git() {
    let temp = TempDir::new().unwrap();
    let sender = temp.path().join("sender");
    let receiver = temp.path().join("receiver");
    let workspace = temp.path().join("work");
    let (repository, revision) = initialize(&sender);
    let context_path = temp.path().join("context.json");
    input(&context_path, &context(&repository, &revision));
    success(&sender, &["start", context_path.to_str().unwrap()]);
    success(
        &sender,
        &[
            "workspace",
            "create",
            workspace.to_str().unwrap(),
            "work",
            &revision,
        ],
    );
    fs::write(workspace.join("a.txt"), b"changed").unwrap();
    let status = success(
        &sender,
        &["workspace", "status", workspace.to_str().unwrap()],
    );
    assert_eq!(status["status"]["complete"], true);
    assert_eq!(status["status"]["changes"][0]["kind"], "modified");
    let checkpoint_args = [
        "workspace",
        "checkpoint",
        workspace.to_str().unwrap(),
        "checkpoint-1",
        "0",
    ];
    let result = success(&sender, &checkpoint_args);
    assert_eq!(result, success(&sender, &checkpoint_args));
    let next = result["outcome"]["revision"].as_str().unwrap();
    fs::write(workspace.join("a.txt"), b"unexpected edit").unwrap();
    // A failed or explicitly blocked undo must preserve the user's bytes.
    let _ = run(
        &sender,
        &[
            "workspace",
            "undo",
            workspace.to_str().unwrap(),
            "undo-1",
            "1",
            "checkpoint-1",
        ],
    );
    assert_eq!(
        fs::read(workspace.join("a.txt")).unwrap(),
        b"unexpected edit"
    );

    let receiver_id = success(&receiver, &["init"])["repository"]
        .as_str()
        .unwrap()
        .to_owned();
    let offer = temp.path().join("offer.json");
    let request = temp.path().join("request.json");
    let bundle = temp.path().join("source.bundle");
    let staging = temp.path().join("staging");
    success(&sender, &["sync", "offer", offer.to_str().unwrap(), next]);
    success(
        &receiver,
        &[
            "sync",
            "request",
            offer.to_str().unwrap(),
            request.to_str().unwrap(),
        ],
    );
    success(
        &sender,
        &[
            "sync",
            "bundle",
            offer.to_str().unwrap(),
            request.to_str().unwrap(),
            bundle.to_str().unwrap(),
        ],
    );
    success(
        &receiver,
        &[
            "sync",
            "import",
            bundle.to_str().unwrap(),
            staging.to_str().unwrap(),
        ],
    );
    success(
        &receiver,
        &[
            "sync",
            "import",
            bundle.to_str().unwrap(),
            staging.to_str().unwrap(),
        ],
    );
    let read = success(&receiver, &["read", next, "a.txt"]);
    assert_eq!(read["text"], "changed");
    assert_eq!(success(&receiver, &["init"])["repository"], receiver_id);
    assert!(success(&receiver, &["target", "main"])["target"].is_null());
    failure(
        &receiver,
        &["workspace", "state", workspace.to_str().unwrap()],
    );
    let nested = workspace.join("repo");
    success(&nested, &["init"]);
    let error = failure(
        &nested,
        &["workspace", "status", workspace.to_str().unwrap()],
    );
    assert!(error["error"].as_str().unwrap().contains("non-nested"));
}

#[test]
fn controller_check_cli_blocks_approval_until_current_success() {
    let temp = TempDir::new().unwrap();
    let data = temp.path().join("repo");
    let (repository, revision) = initialize(&data);
    let recipe = json!({"version":1,"expectations":[{"path":"a.txt","sha256":format!("{:x}", Sha256::digest(b"a"))}],
        "limits":{"max_paths":1,"max_total_bytes":1024,"max_output_bytes":4096,"timeout_ms":1000}});
    let recipe_path = temp.path().join("recipe.json");
    input(&recipe_path, &recipe);
    let digest =
        success(&data, &["check-recipe", recipe_path.to_str().unwrap()])["recipe_sha256"].clone();
    let mut context = context(&repository, &revision);
    context["recipe"]["sha256"] = digest.clone();
    context["required_capabilities"] = json!([
        "agentscm.review-context/v1",
        "agentscm.checks.source-paths/v1"
    ]);
    context["required_evidence"] =
        json!([{"identity":"source_paths/v1","digest":{"state":"digest","sha256":digest}}]);
    let context_path = temp.path().join("context.json");
    input(&context_path, &context);
    let change = success(&data, &["start", context_path.to_str().unwrap()])["change"].clone();
    let approval_path = temp.path().join("approval.json");
    input(
        &approval_path,
        &json!({"approval_id":"approved","target":"main","revision":revision,
        "context_digest":change["context_digest"],"change":change["id"],"change_version":1,"policy_epoch":0}),
    );
    failure(&data, &["approve", approval_path.to_str().unwrap()]);
    let attempt_path = temp.path().join("attempt.json");
    input(
        &attempt_path,
        &json!({"attempt_id":"attempt-1","change":change["id"],"change_version":1,
        "context_digest":change["context_digest"],"revision":revision,"recipe":recipe}),
    );
    let attempt = success(
        &data,
        &["check-source-paths", attempt_path.to_str().unwrap()],
    );
    let generation = attempt["attempt"]["run_generation"]
        .as_u64()
        .unwrap()
        .to_string();
    let completed = success(&data, &["check-run", "attempt-1", &generation]);
    assert_eq!(completed["attempt"]["state"], "success");
    let evidence = completed["attempt"]["evidence_digest"].as_str().unwrap();
    assert_eq!(
        success(&data, &["check-evidence", evidence])["evidence"]["execution_basis"],
        "controller_builtin"
    );
    success(&data, &["approve", approval_path.to_str().unwrap()]);
}

#[test]
fn target_initialization_and_landing_preparation_work_without_git() {
    let temp = TempDir::new().unwrap();
    let data = temp.path().join("repo");
    let (repository, s0) = initialize(&data);
    let path = temp.path().join("request.json");
    let checkpoint = |parent: Option<&str>, name: &str, content: u8| -> String {
        input(
            &path,
            &json!({"parent":parent,"changes":[{"path":name,"content":[content]}]}),
        );
        success(&data, &["checkpoint", path.to_str().unwrap()])["revision"]
            .as_str()
            .unwrap()
            .to_owned()
    };
    let s1 = checkpoint(Some(&s0), "a.txt", 98);
    let start = |context: &Value| -> Value {
        input(&path, context);
        success(&data, &["start", path.to_str().unwrap()])["change"].clone()
    };
    let approve = |id: &str, revision: &str, change: &Value| {
        input(
            &path,
            &json!({"approval_id":id,"target":"main","revision":revision,
            "context_digest":change["context_digest"],"change":change["id"],
            "change_version":change["version"],"policy_epoch":0}),
        );
        success(&data, &["approve", path.to_str().unwrap()]);
    };

    // An imported head with a parent initializes the absent target.
    let change = start(&context(&repository, &s1));
    approve("approve-init", &s1, &change);
    let initialize_request = |operation: &str, approval: &str| {
        json!({"operation_id":operation,"target":"main","expected_target":null,
        "revision":s1,"context_digest":change["context_digest"],"approval_id":approval,
        "change":change["id"],"change_version":change["version"],"policy_epoch":0})
    };
    input(&path, &initialize_request("init-1", "approve-init"));
    let event = success(&data, &["initialize-target", path.to_str().unwrap()])["event"].clone();
    assert_eq!(event["kind"], "initialize");
    assert_eq!(event["after"], json!({"revision":s1,"version":1}));
    assert_eq!(
        success(&data, &["target", "main"])["target"],
        event["after"]
    );
    input(&path, &initialize_request("init-2", "approve-init"));
    let error = failure(&data, &["initialize-target", path.to_str().unwrap()]);
    assert!(error["error"].as_str().unwrap().starts_with("conflict:"));

    // Offline work on top of the target is prepared as an exact landing revision.
    let a = checkpoint(Some(&s1), "b.txt", 65);
    let b = checkpoint(Some(&a), "b.txt", 66);
    let mut work = context(&repository, &b);
    work["change"]["id"] = json!("landing");
    work["base"] = json!({"revision":s1,"target_version":1});
    let old = start(&work);
    approve("approve-old", &b, &old);
    let mut prepared = work.clone();
    prepared["change_version"] = json!(2);
    let request = json!({"operation_id":"prepare-1","target":"main",
        "expected_target":{"revision":s1,"version":1},"source_tip":b,
        "change_version":1,"context":prepared});
    input(&path, &request);
    let first = success(&data, &["prepare-landing", path.to_str().unwrap()]);
    let landing = &first["landing"];
    assert_eq!(landing["outcome"], "prepared");
    let revision = landing["revision"].as_str().unwrap().to_owned();
    assert_ne!(revision, b);
    assert_eq!(success(&data, &["read", &revision, "b.txt"])["text"], "B");
    assert_eq!(
        success(&data, &["target", "main"])["target"],
        event["after"]
    );
    let retry = success(&data, &["prepare-landing", path.to_str().unwrap()]);
    assert_eq!(retry, first);

    // The approval for the source tip cannot authorize the landing revision.
    let change = landing["change"].clone();
    assert_eq!(change["version"], 2);
    let integrate_request = |revision: &str, approval: &str| {
        json!({"operation_id":format!("land-{approval}"),"target":"main",
        "expected_target":{"revision":s1,"version":1},"revision":revision,
        "context_digest":change["context_digest"],"approval_id":approval,
        "change":change["id"],"change_version":change["version"],"policy_epoch":0})
    };
    input(&path, &integrate_request(&revision, "approve-old"));
    failure(&data, &["integrate", path.to_str().unwrap()]);
    approve("approve-landing", &revision, &change);
    input(&path, &integrate_request(&revision, "approve-landing"));
    let landed = success(&data, &["integrate", path.to_str().unwrap()])["event"].clone();
    assert_eq!(landed["kind"], "integrate");
    assert_eq!(landed["after"], json!({"revision":revision,"version":2}));
}

/// The Git these tests drive, resolved as in tests/integration/git_snapshot.rs. A missing Git fails the test.
fn test_git() -> PathBuf {
    let path = PathBuf::from(
        std::env::var("AGENTSCM_TEST_GIT").unwrap_or_else(|_| "/usr/bin/git".to_owned()),
    );
    assert!(
        path.exists(),
        "git not found at {}; set AGENTSCM_TEST_GIT to an absolute git path",
        path.display()
    );
    path
}

/// Git with a cleared environment and an isolated, test-controlled configuration.
fn isolated_git(directory: &Path, home: &Path, args: &[&str]) {
    let git = test_git();
    let output = Command::new(&git)
        .env_clear()
        .env("PATH", git.parent().unwrap())
        .env("HOME", home)
        .env("XDG_CONFIG_HOME", home.join("xdg"))
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .args(args)
        .current_dir(directory)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn create_workspace(temp: &Path, data: &Path, name: &str) -> PathBuf {
    let (repository, revision) = initialize(data);
    let context_path = temp.join("context.json");
    input(&context_path, &context(&repository, &revision));
    success(data, &["start", context_path.to_str().unwrap()]);
    let workspace = temp.join(name);
    success(
        data,
        &[
            "workspace",
            "create",
            workspace.to_str().unwrap(),
            "work",
            &revision,
        ],
    );
    workspace
}

/// `workspace preview` with a cleared environment; Git configuration is test-controlled.
fn preview_output(
    data: &Path,
    workspace: &Path,
    home: &Path,
    gitconfig: &Path,
    path_env: &std::ffi::OsStr,
) -> Output {
    Command::new(env!("CARGO_BIN_EXE_agentscm"))
        .arg("--data-dir")
        .arg(data)
        .args(["workspace", "preview", workspace.to_str().unwrap()])
        .env_clear()
        .env("PATH", path_env)
        .env("HOME", home)
        .env("XDG_CONFIG_HOME", home.join("xdg"))
        .env("GIT_CONFIG_GLOBAL", gitconfig)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .output()
        .unwrap()
}

#[test]
fn workspace_preview_resolves_global_git_excludes_through_git_only_with_a_root_git() {
    let git = test_git();
    let temp = TempDir::new().unwrap();
    let data = temp.path().join("repo");
    let workspace = create_workspace(temp.path(), &data, "work");
    // Isolated Git configuration: nothing here reads the real user's.
    let home = temp.path().join("home");
    fs::create_dir_all(home.join("dir with space")).unwrap();
    fs::write(home.join("dir with space/ignore"), "*.globalonly\n").unwrap();
    let included = home.join("included-config");
    fs::write(&included, "[core]\n\tignorecase = true\n").unwrap();
    let gitconfig = home.join("gitconfig");
    fs::write(
        &gitconfig,
        format!(
            "[core]\n\texcludesFile = \"{}\"\n[include]\n\tpath = {}\n",
            home.join("dir with space/ignore").display(),
            included.display()
        ),
    )
    .unwrap();
    let preview = |path_env: &std::ffi::OsStr| -> Value {
        let output = preview_output(&data, &workspace, &home, &gitconfig, path_env);
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        serde_json::from_slice(&output.stdout).unwrap()
    };
    fs::write(workspace.join("x.globalonly"), b"x").unwrap();
    // No root .git: Git is never started (PATH is empty) and global excludes do not apply.
    let without_git = preview(std::ffi::OsStr::new(""));
    assert!(
        without_git["preview"]["excluded"]
            .as_array()
            .unwrap()
            .is_empty()
    );

    isolated_git(&workspace, &home, &["init", "-q"]);
    let with_git = preview(git.parent().unwrap().as_os_str());
    let excluded = with_git["preview"]["excluded"].as_array().unwrap();
    assert_eq!(excluded.len(), 1, "{with_git}");
    assert_eq!(excluded[0]["path"], "x.globalonly");
    assert_eq!(excluded[0]["rule"]["source"], "global-excludes");
    assert_eq!(excluded[0]["rule"]["pattern"], "*.globalonly");
    assert_eq!(with_git["ok"], true);

    // A configuration Git cannot parse is a typed error, not a silent fallback.
    let broken = home.join("broken-config");
    fs::write(&broken, "[core\nexcludesFile = \n").unwrap();
    let output = preview_output(
        &data,
        &workspace,
        &home,
        &broken,
        git.parent().unwrap().as_os_str(),
    );
    assert!(!output.status.success());
    let error: Value = serde_json::from_slice(&output.stderr).unwrap();
    assert!(error["error"].as_str().unwrap().contains("Git refused"));
}

#[test]
fn a_root_git_that_is_not_the_workspace_repository_is_refused() {
    let git = test_git();
    let path_env = git.parent().unwrap().as_os_str().to_owned();
    let temp = TempDir::new().unwrap();
    let home = temp.path().join("home");
    fs::create_dir(&home).unwrap();
    let gitconfig = home.join("gitconfig");
    fs::write(&gitconfig, "").unwrap();
    let error_of = |data: &Path, workspace: &Path| -> String {
        let output = preview_output(data, workspace, &home, &gitconfig, &path_env);
        assert!(!output.status.success());
        let error: Value = serde_json::from_slice(&output.stderr).unwrap();
        error["error"].as_str().unwrap().to_owned()
    };

    // An empty `.git` directory is not a repository: Git refuses it.
    let data = temp.path().join("repo-a");
    let workspace = create_workspace(temp.path(), &data, "work-a");
    fs::create_dir(workspace.join(".git")).unwrap();
    assert!(error_of(&data, &workspace).contains("Git refused"));

    // Inside another repository, Git finds that one instead: the `.git` does not own the root.
    let outer = temp.path().join("outer");
    fs::create_dir(&outer).unwrap();
    isolated_git(&outer, &home, &["init", "-q"]);
    let data = temp.path().join("repo-b");
    let workspace = create_workspace(&outer, &data, "work-b");
    fs::create_dir(workspace.join(".git")).unwrap();
    assert!(error_of(&data, &workspace).contains("does not own"));
}

#[test]
fn info_exclude_is_found_through_git_for_separate_and_linked_git_directories() {
    let git = test_git();
    let path_env = git.parent().unwrap().as_os_str().to_owned();
    let temp = TempDir::new().unwrap();
    let home = temp.path().join("home");
    fs::create_dir(&home).unwrap();
    let gitconfig = home.join("gitconfig");
    fs::write(&gitconfig, "").unwrap();
    let excluded_paths = |data: &Path, workspace: &Path| -> Vec<(String, String)> {
        let output = preview_output(data, workspace, &home, &gitconfig, &path_env);
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let value: Value = serde_json::from_slice(&output.stdout).unwrap();
        value["preview"]["excluded"]
            .as_array()
            .unwrap()
            .iter()
            .map(|entry| {
                (
                    entry["path"].as_str().unwrap().to_owned(),
                    entry["rule"]["source"].as_str().unwrap().to_owned(),
                )
            })
            .collect()
    };

    // A separate git directory whose name ends in a space, reached through a `.git` pointer.
    let data = temp.path().join("repo-a");
    let workspace = create_workspace(temp.path(), &data, "work-a");
    let separate = temp.path().join("metadata ");
    isolated_git(
        &workspace,
        &home,
        &[
            "init",
            "-q",
            "--separate-git-dir",
            separate.to_str().unwrap(),
            workspace.to_str().unwrap(),
        ],
    );
    assert!(workspace.join(".git").is_file());
    fs::create_dir_all(separate.join("info")).unwrap();
    fs::write(separate.join("info/exclude"), "from-info.txt\n").unwrap();
    fs::write(workspace.join("from-info.txt"), b"x").unwrap();
    assert_eq!(
        excluded_paths(&data, &workspace),
        [("from-info.txt".to_owned(), "info/exclude".to_owned())]
    );

    // A linked worktree resolves the main repository's info/exclude through the common dir.
    let main = temp.path().join("main");
    fs::create_dir(&main).unwrap();
    isolated_git(&main, &home, &["init", "-q"]);
    fs::write(main.join("tracked.txt"), b"x").unwrap();
    isolated_git(&main, &home, &["add", "tracked.txt"]);
    isolated_git(
        &main,
        &home,
        &[
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@example.invalid",
            "commit",
            "-q",
            "-m",
            "init",
        ],
    );
    let linked = temp.path().join("linked");
    isolated_git(
        &main,
        &home,
        &["worktree", "add", "-q", linked.to_str().unwrap()],
    );
    fs::write(main.join(".git/info/exclude"), "from-main.txt\n").unwrap();
    let data = temp.path().join("repo-b");
    let workspace = create_workspace(temp.path(), &data, "work-b");
    fs::copy(linked.join(".git"), workspace.join(".git")).unwrap();
    fs::write(workspace.join("from-main.txt"), b"x").unwrap();
    assert_eq!(
        excluded_paths(&data, &workspace),
        [("from-main.txt".to_owned(), "info/exclude".to_owned())]
    );
}

#[test]
fn an_empty_core_excludes_file_means_no_global_file() {
    let git = test_git();
    let temp = TempDir::new().unwrap();
    let data = temp.path().join("repo");
    let workspace = create_workspace(temp.path(), &data, "work");
    let home = temp.path().join("home");
    fs::create_dir_all(home.join("xdg/git")).unwrap();
    // Git's default location would exclude this file if the empty setting were ignored.
    fs::write(home.join("xdg/git/ignore"), "x.default\n").unwrap();
    fs::write(workspace.join("x.default"), b"x").unwrap();
    isolated_git(&workspace, &home, &["init", "-q"]);
    let gitconfig = home.join("gitconfig");
    let excluded = |config: &str| -> usize {
        fs::write(&gitconfig, config).unwrap();
        let output = preview_output(
            &data,
            &workspace,
            &home,
            &gitconfig,
            git.parent().unwrap().as_os_str(),
        );
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let value: Value = serde_json::from_slice(&output.stdout).unwrap();
        value["preview"]["excluded"].as_array().unwrap().len()
    };
    assert_eq!(excluded(""), 1);
    assert_eq!(excluded("[core]\n\texcludesFile =\n"), 0);
}

#[test]
fn timeline_capture_is_nonadvancing_and_fork_requires_exact_new_change() {
    let temp = TempDir::new().unwrap();
    let data = temp.path().join("source");
    let root = temp.path().join("work");
    let (repository, revision) = initialize(&data);
    let context_path = temp.path().join("context.json");
    input(&context_path, &context(&repository, &revision));
    success(&data, &["start", context_path.to_str().unwrap()]);
    success(
        &data,
        &[
            "workspace",
            "create",
            root.to_str().unwrap(),
            "work",
            &revision,
        ],
    );
    let original = success(&data, &["workspace", "state", root.to_str().unwrap()]);
    fs::write(root.join("a.txt"), b"captured").unwrap();
    let capture = success(&data, &["workspace", "capture", root.to_str().unwrap()]);
    assert_eq!(capture["outcome"]["outcome"], "captured");
    let id = capture["outcome"]["entry"]["id"]
        .as_u64()
        .unwrap()
        .to_string();
    let captured_revision = capture["outcome"]["entry"]["revision"].as_str().unwrap();
    assert_ne!(captured_revision, revision);
    assert_eq!(
        success(&data, &["workspace", "state", root.to_str().unwrap()]),
        original
    );
    assert_eq!(
        success(&data, &["workspace", "capture", root.to_str().unwrap()])["outcome"]["outcome"],
        "unchanged"
    );
    let entry = success(&data, &["workspace", "entry", root.to_str().unwrap(), &id]);
    assert_eq!(entry["evidence"]["state"], "not_run");
    assert_eq!(entry["entry"]["revision"], captured_revision);
    let list = success(
        &data,
        &[
            "workspace",
            "timeline",
            root.to_str().unwrap(),
            "--limit",
            "1",
        ],
    );
    assert_eq!(list["timeline"]["entries"].as_array().unwrap().len(), 1);
    assert_eq!(
        list["timeline"]["coverage"]["includes_pre_feature_history"],
        false
    );
    fs::write(root.join("a.txt"), b"later").unwrap();
    let second = success(&data, &["workspace", "capture", root.to_str().unwrap()]);
    let second_id = second["outcome"]["entry"]["id"]
        .as_u64()
        .unwrap()
        .to_string();
    let diff = success(
        &data,
        &["workspace", "diff", root.to_str().unwrap(), &id, &second_id],
    );
    assert_eq!(diff["diff"]["entries"][0]["path"], "a.txt");
    let mut fork_context = context(&repository, captured_revision);
    fork_context["change"]["id"] = json!("forked");
    input(&context_path, &fork_context);
    success(&data, &["start", context_path.to_str().unwrap()]);
    let parent = temp.path().to_str().unwrap();
    let fork = success(
        &data,
        &[
            "workspace",
            "fork",
            root.to_str().unwrap(),
            &id,
            "fork-work",
            "forked",
            "--fork-root",
            parent,
        ],
    );
    assert_eq!(fork["workspace"]["base_revision"], captured_revision);
    assert_eq!(
        fs::read(temp.path().join("fork-work/a.txt")).unwrap(),
        b"captured"
    );
    assert_eq!(fs::read(root.join("a.txt")).unwrap(), b"later");
    assert!(
        !run(
            &data,
            &[
                "workspace",
                "fork",
                root.to_str().unwrap(),
                &id,
                "../escape",
                "forked",
                "--fork-root",
                parent
            ]
        )
        .status
        .success()
    );
    assert!(
        !run(
            &data,
            &[
                "workspace",
                "fork",
                root.to_str().unwrap(),
                &id,
                "another",
                "work",
                "--fork-root",
                parent
            ]
        )
        .status
        .success()
    );
    assert!(
        !run(
            &data,
            &[
                "workspace",
                "fork",
                root.to_str().unwrap(),
                &id,
                "fork-work",
                "forked",
                "--fork-root",
                parent
            ]
        )
        .status
        .success()
    );
}

#[test]
fn timeline_capture_enrolls_existing_v4_workspace_without_mutating_read() {
    let temp = TempDir::new().unwrap();
    let data = temp.path().join("source");
    let root = temp.path().join("work");
    let (repository, revision) = initialize(&data);
    let context_path = temp.path().join("context.json");
    input(&context_path, &context(&repository, &revision));
    success(&data, &["start", context_path.to_str().unwrap()]);
    success(
        &data,
        &[
            "workspace",
            "create",
            root.to_str().unwrap(),
            "work",
            &revision,
        ],
    );
    let database = root.join(".agentscm-workspace/state.sqlite");
    let db = rusqlite::Connection::open(&database).unwrap();
    db.execute_batch("DROP INDEX timeline_check_link_identity; DROP TABLE timeline_entries; PRAGMA user_version=4;").unwrap();
    drop(db);
    assert!(
        success(&data, &["workspace", "timeline", root.to_str().unwrap()])["timeline"]["entries"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    let db = rusqlite::Connection::open(&database).unwrap();
    assert_eq!(
        db.pragma_query_value(None, "user_version", |row| row.get::<_, i64>(0))
            .unwrap(),
        4
    );
    drop(db);
    fs::write(root.join("a.txt"), b"new observation").unwrap();
    let result = success(&data, &["workspace", "capture", root.to_str().unwrap()]);
    assert_eq!(result["outcome"]["outcome"], "captured");
    let state = success(&data, &["workspace", "state", root.to_str().unwrap()]);
    assert_eq!(state["workspace"]["base_revision"], revision);
    assert_eq!(state["workspace"]["generation"], 0);
}
