use std::fs;
use std::path::Path;
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
    assert_eq!(read["content"], json!(b"changed".to_vec()));
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
    assert_eq!(
        success(&data, &["read", &revision, "b.txt"])["content"],
        json!([66])
    );
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
