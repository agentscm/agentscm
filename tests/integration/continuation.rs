use std::fs;
use std::os::unix::fs::{PermissionsExt, symlink};

use agentscm::change_record::{CoverageV1, SourceBindingV1};
use agentscm::continuation::*;
use agentscm::execution::executable;
use agentscm::hub::work::{ClaimStatus, WorkItem, WorkSource, WorkState};
use serde_json::json;

fn plan(root: &std::path::Path) -> TransferPlan {
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(root.join("src/task.rs"), "pub fn answer() -> i32 { 1 }\n").unwrap();
    let work = WorkItem {
        id: "work-test".into(),
        repository: "synthetic/project".into(),
        owner: "owner".into(),
        owner_login: "synthetic".into(),
        source: WorkSource::Native {
            request_key: "task-1".into(),
        },
        title: "Complete the answer implementation".into(),
        state: WorkState::Active,
        claim_status: ClaimStatus::Active,
        last_checkin_at: 1,
        generation: 1,
        lead_token: "token-a".into(),
        lead_agent: "claude".into(),
        lead_session: "native-outgoing".into(),
        participants: vec![],
        scope: vec!["src".into()],
        branch: Some("task".into()),
        worktree: None,
        next_action: Some("Return 42".into()),
        current: None,
        blocker: None,
        depends_on: vec![],
        related_work: vec![],
        created_at: 1,
        updated_at: 1,
        created_seq: 1,
        updated_seq: 1,
    };
    let mut plan = TransferPlan {
        id: String::new(),
        work,
        root: root.to_owned(),
        authority: json!({"repository":"synthetic/project"}),
        source: SourceBindingV1::Git {
            repository: "synthetic/project".into(),
            worktree_id: "tree-1".into(),
            head: "1".repeat(40),
            tree: "2".repeat(40),
            base: "1".repeat(40),
            dirty: false,
            untracked_count: 0,
            omissions: vec![],
            captured_snapshot: None,
            snapshot_coverage: CoverageV1::Complete,
        },
        index_digest: "0".repeat(64),
        destination: NativeClient::Codex,
        executable: executable::capture(std::path::Path::new("/bin/cat"), &[]).unwrap(),
        files: vec![selected_file(root, "src/task.rs").unwrap()],
        context: json!({"criteria":["answer is 42"],"constraints":["preserve API"]}),
        check: None,
        check_executable: None,
        timeout_seconds: 60,
    };
    plan.seal().unwrap();
    plan
}

fn acceptance(plan: &TransferPlan) -> Acceptance {
    Acceptance {
        transfer_id: plan.id.clone(),
        accepted: true,
        summary: "Implemented the recorded answer criterion".into(),
        edit: Some(ProposedEdit {
            path: "src/task.rs".into(),
            before_sha256: plan.files[0].sha256.clone(),
            content: "pub fn answer() -> i32 { 42 }\n".into(),
        }),
        artifact: None,
        next_action: "Run the approved check".into(),
    }
}

#[test]
fn native_evidence_requires_real_identity_success_and_no_tool_execution() {
    let claude =
        br#"{"subtype":"success","is_error":false,"session_id":"native-claude","result":"{}"}"#;
    assert_eq!(
        parse_native_response(NativeClient::Claude, claude)
            .unwrap()
            .session_id,
        "native-claude"
    );
    assert!(
        parse_native_response(
            NativeClient::Claude,
            br#"{"subtype":"success","result":"{}"}"#
        )
        .is_err()
    );
    let codex=b"{\"type\":\"thread.started\",\"thread_id\":\"native-codex\"}\n{\"type\":\"item.completed\",\"item\":{\"type\":\"agent_message\",\"text\":\"{}\"}}\n{\"type\":\"turn.completed\"}\n";
    assert_eq!(
        parse_native_response(NativeClient::Codex, codex)
            .unwrap()
            .session_id,
        "native-codex"
    );
    let warning=b"{\"type\":\"item.completed\",\"item\":{\"type\":\"error\",\"message\":\"synthetic nonfatal runtime warning\"}}\n";
    let warned = [warning.as_slice(), codex.as_slice()].concat();
    assert!(parse_native_response(NativeClient::Codex, &warned).is_ok());
    assert!(
        parse_native_response(NativeClient::Codex, warning).is_err(),
        "a warning is never completion"
    );
    let too_many = [warning.repeat(17), codex.to_vec()].concat();
    assert!(parse_native_response(NativeClient::Codex, &too_many).is_err());
    let top_error = [codex.as_slice(), b"{\"type\":\"error\"}\n".as_slice()].concat();
    assert!(parse_native_response(NativeClient::Codex, &top_error).is_err());
    let tool_update = [
        codex.as_slice(),
        b"{\"type\":\"item.updated\",\"item\":{\"type\":\"command_execution\"}}\n".as_slice(),
    ]
    .concat();
    assert!(parse_native_response(NativeClient::Codex, &tool_update).is_err());
    let mut tools = codex.to_vec();
    tools.extend_from_slice(
        b"{\"type\":\"item.completed\",\"item\":{\"type\":\"command_execution\"}}\n",
    );
    assert!(parse_native_response(NativeClient::Codex, &tools).is_err());
    assert!(
        parse_native_response(
            NativeClient::Codex,
            b"{\"type\":\"thread.started\",\"thread_id\":\"x\"}\n"
        )
        .is_err()
    );
}

#[test]
fn acceptance_binds_exact_work_source_and_approved_path() {
    let root = tempfile::tempdir().unwrap();
    let plan = plan(root.path());
    plan.validate().unwrap();
    acceptance(&plan).validate(&plan).unwrap();
    let mut changed = plan.clone();
    changed.work.generation += 1;
    assert!(changed.validate().is_err());
    let mut wrong = acceptance(&plan);
    wrong.edit.as_mut().unwrap().path = "outside.rs".into();
    assert!(wrong.validate(&plan).is_err());
    wrong = acceptance(&plan);
    wrong.edit.as_mut().unwrap().before_sha256 = "0".repeat(64);
    assert!(wrong.validate(&plan).is_err());
    wrong = acceptance(&plan);
    wrong.transfer_id = "another".into();
    assert!(wrong.validate(&plan).is_err());
    wrong = acceptance(&plan);
    wrong.accepted = false;
    assert!(wrong.validate(&plan).is_err());
    wrong = acceptance(&plan);
    wrong.next_action = " ".into();
    assert!(wrong.validate(&plan).is_err());
    wrong = acceptance(&plan);
    wrong.summary = "\n".into();
    assert!(wrong.validate(&plan).is_err());
    assert!(plan.prompt().unwrap().contains("preserve API"));
}

#[test]
fn reserved_launch_survives_restart_and_revocation_is_not_overwritten() {
    let root = tempfile::tempdir().unwrap();
    let data = tempfile::tempdir().unwrap();
    let plan = plan(root.path());
    let journal = Journal::open(data.path()).unwrap();
    let mut record = journal.insert(plan.clone()).unwrap();
    let lock = journal.lock_work(&plan.work.id).unwrap();
    assert!(journal.lock_work(&plan.work.id).is_err());
    record.phase = TransferPhase::Reserved;
    journal.save(&record).unwrap();
    drop(lock);
    drop(journal);
    let journal = Journal::open(data.path()).unwrap();
    assert_eq!(
        journal.get(&plan.id).unwrap().phase,
        TransferPhase::Reserved
    );
    assert!(
        journal
            .assert_quiescent(&plan.work.id, "another-transfer")
            .is_err()
    );
    journal.revoke(&plan.id).unwrap();
    journal.save(&record).unwrap();
    assert!(journal.ensure_current(&plan.id).is_err());
    assert_eq!(
        journal.get(&plan.id).unwrap().phase,
        TransferPhase::Reserved
    );
}

#[test]
fn source_effect_reconciliation_observes_success_without_repeating_it() {
    let root = tempfile::tempdir().unwrap();
    let plan = plan(root.path());
    let edit = acceptance(&plan).edit.unwrap();
    assert!(apply_edit(root.path(), &edit, true).is_err());
    assert!(!apply_edit(root.path(), &edit, false).unwrap());
    let inode =
        std::os::unix::fs::MetadataExt::ino(&fs::metadata(root.path().join(&edit.path)).unwrap());
    // Simulate losing the success response after the atomic replacement.
    assert!(apply_edit(root.path(), &edit, true).unwrap());
    assert_eq!(
        std::os::unix::fs::MetadataExt::ino(&fs::metadata(root.path().join(&edit.path)).unwrap()),
        inode
    );
    fs::write(root.path().join(&edit.path), "human change").unwrap();
    assert!(apply_edit(root.path(), &edit, true).is_err());
    assert!(apply_edit(root.path(), &edit, false).is_err());
    assert_eq!(
        fs::read_to_string(root.path().join(&edit.path)).unwrap(),
        "human change"
    );
}

#[test]
fn paths_cannot_escape_or_replace_tool_configuration() {
    let root = tempfile::tempdir().unwrap();
    let _ = plan(root.path());
    fs::write(root.path().join("secret"), "secret").unwrap();
    symlink(root.path().join("secret"), root.path().join("src/link")).unwrap();
    for path in [
        "../secret",
        "/tmp/secret",
        "src/link",
        ".git/config",
        "AGENTS.md",
        "src/./task.rs",
        "src/../secret",
    ] {
        assert!(selected_file(root.path(), path).is_err(), "{path}");
    }
    fs::set_permissions(
        root.path().join("src/task.rs"),
        fs::Permissions::from_mode(0o755),
    )
    .unwrap();
    let before = selected_file(root.path(), "src/task.rs").unwrap();
    apply_edit(
        root.path(),
        &ProposedEdit {
            path: before.path,
            before_sha256: before.sha256,
            content: "changed".into(),
        },
        false,
    )
    .unwrap();
    assert_eq!(
        fs::metadata(root.path().join("src/task.rs"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o755
    );
}

#[test]
fn result_artifact_is_content_addressed_across_recovery() {
    let data = tempfile::tempdir().unwrap();
    let journal = Journal::open(data.path()).unwrap();
    let first = retain_result(&journal, "useful result").unwrap();
    let second = retain_result(&journal, "useful result").unwrap();
    assert_eq!(first, second);
    assert_eq!(
        journal.spool().unwrap().read_artifact(&first).unwrap(),
        b"useful result"
    );
}

use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::Arc;
use std::sync::atomic::{AtomicU8, Ordering};

use agentscm::backend::LocalBackend;
use agentscm::hub::store::{self, ForgeIdentity, mint_bound_agent_token};
use agentscm::hub::{HubState, router};
use serde_json::Value;
use tempfile::TempDir;

/// Real CLI and real local Hub, with synthetic source. Live mode preserves the caller's existing
/// native subscription auth; provider use requires selecting the ignored test explicitly.
struct ContinuationFixture {
    root: PathBuf,
    data: PathBuf,
    token_file: PathBuf,
    url: String,
    live: bool,
    lose_reply: Arc<AtomicU8>,
    _runtime: tokio::runtime::Runtime,
    directory: TempDir,
}
impl ContinuationFixture {
    fn new(live: bool) -> Self {
        let directory = TempDir::new().unwrap();
        let root = directory.path().join("source");
        let data = directory.path().join("state");
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(root.join("src/task.rs"), "pub fn answer() -> i32 { 1 }\n").unwrap();
        let git = |args: &[&str]| {
            let output = Command::new("git")
                .current_dir(&root)
                .env_clear()
                .env("PATH", std::env::var_os("PATH").unwrap_or_default())
                .env("HOME", directory.path())
                .env("GIT_CONFIG_NOSYSTEM", "1")
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .args(args)
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
        };
        git(&["init", "--quiet"]);
        git(&["config", "user.name", "Synthetic Tester"]);
        git(&["config", "user.email", "tester@example.test"]);
        git(&["add", "src/task.rs"]);
        git(&[
            "-c",
            "commit.gpgsign=false",
            "commit",
            "--quiet",
            "-m",
            "Synthetic fixture",
        ]);
        git(&[
            "remote",
            "add",
            "origin",
            "https://github.com/synthetic/project.git",
        ]);
        let database = format!("sqlite://{}", directory.path().join("hub.sqlite").display());
        let store = store::open(&database).unwrap();
        let user = store
            .upsert_user(
                &ForgeIdentity {
                    provider: "github".into(),
                    host: "github.com".into(),
                    user_id: 42,
                },
                "synthetic",
            )
            .unwrap();
        let (_, secret, _) = mint_bound_agent_token(
            &*store,
            &user.id,
            "continuation-test",
            "synthetic/project",
            101,
            3600,
            false,
        )
        .unwrap();
        let token_file = directory.path().join("agent.token");
        fs::write(&token_file, secret.expose()).unwrap();
        fs::set_permissions(&token_file, fs::Permissions::from_mode(0o600)).unwrap();
        let objects = LocalBackend::open(directory.path().join("objects")).unwrap();
        let lose_reply = Arc::new(AtomicU8::new(0));
        let fault = lose_reply.clone();
        let app = router(HubState::new(store, Arc::new(objects))).layer(axum::middleware::from_fn(move |request:axum::extract::Request,next:axum::middleware::Next| {
            let fault=fault.clone();
            async move {
                let mode=fault.load(Ordering::SeqCst);
                let checkpoint=request.uri().path()=="/v1/hub/standup/checkpoint";
                let takeover=request.uri().path()=="/v1/hub/standup/start";
                let lose=if mode==1&&takeover || (mode==3||mode==4)&&checkpoint {
                    fault.compare_exchange(mode,0,Ordering::SeqCst,Ordering::SeqCst).is_ok()
                }else{
                    if mode==2&&checkpoint {let _=fault.compare_exchange(2,3,Ordering::SeqCst,Ordering::SeqCst);}
                    false
                };
                let response=next.run(request).await;
                if lose&&response.status().is_success() {
                    axum::response::IntoResponse::into_response((axum::http::StatusCode::SERVICE_UNAVAILABLE,axum::Json(json!({"error":"synthetic lost response after committed Hub operation"}))))
                }else{response}
            }
        }));
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap();
        let address = runtime.block_on(async {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let address = listener.local_addr().unwrap();
            tokio::spawn(async move { axum::serve(listener, app).await });
            address
        });
        Self {
            root,
            data,
            token_file,
            url: format!("http://{address}"),
            live,
            lose_reply,
            _runtime: runtime,
            directory,
        }
    }

    fn hub(&self, method: reqwest::Method, path: &str, body: Option<Value>) -> Value {
        self._runtime.block_on(async {
            let client = reqwest::Client::new();
            let mut request = client
                .request(method, format!("{}{path}", self.url))
                .bearer_auth(fs::read_to_string(&self.token_file).unwrap());
            if let Some(body) = body {
                request = request
                    .header(reqwest::header::CONTENT_TYPE, "application/json")
                    .body(serde_json::to_vec(&body).unwrap());
            }
            let response = request.send().await.unwrap();
            let status = response.status();
            let value: Value = serde_json::from_slice(&response.bytes().await.unwrap()).unwrap();
            assert!(status.is_success(), "{value}");
            value
        })
    }
    fn command(&self) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_agentscm"));
        command
            .env_clear()
            .env("HOME", self.directory.path())
            .env("PATH", std::env::var_os("PATH").unwrap_or_default())
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("AGENTSCM_HUB_URL", &self.url)
            .env("AGENTSCM_HUB_TOKEN_FILE", &self.token_file)
            .env("AGENTSCM_TELEMETRY", "off")
            .arg("--data-dir")
            .arg(&self.data)
            .arg("continuation");
        if self.live {
            for name in [
                "HOME",
                "USER",
                "LOGNAME",
                "XDG_CONFIG_HOME",
                "CLAUDE_CONFIG_DIR",
                "CODEX_HOME",
                "TMPDIR",
                "LANG",
                "LC_ALL",
            ] {
                if let Some(value) = std::env::var_os(name) {
                    command.env(name, value);
                }
            }
        }
        command
    }
    fn output(&self, args: &[&str]) -> Output {
        self.command().args(args).output().unwrap()
    }
    fn run(&self, args: &[&str]) -> Value {
        let output = self.output(args);
        assert!(
            output.status.success(),
            "continuation {args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        serde_json::from_slice(&output.stdout)
            .unwrap_or_else(|e| panic!("{e}: {}", String::from_utf8_lossy(&output.stdout)))
    }
    fn enroll(&self) -> String {
        self.run(&["enroll","--root",self.root.to_str().unwrap(),"--request-key","synthetic-task","--title","Increment the integer returned by answer by exactly one per continuation; preserve its public signature","--path","src/task.rs","--next-action","Increment the returned integer by exactly one. Return a brief useful artifact. Set next_action to repeat that same increment once on the next continuation."])["work"]["id"].as_str().unwrap().to_owned()
    }
    fn preview(&self, work: &str, client: &str, executable: &Path) -> Value {
        self.run(&[
            "preview",
            work,
            "--root",
            self.root.to_str().unwrap(),
            "--to",
            client,
            "--executable",
            executable.to_str().unwrap(),
            "--path",
            "src/task.rs",
        ])
    }
    fn execute(&self, preview: &Value) -> Value {
        let id = preview["confirmation"].as_str().unwrap();
        self.run(&["run", id, "--confirm", id])
    }
    fn fake(&self, client: NativeClient, stage: u8, accepted: bool) -> PathBuf {
        let path = self
            .directory
            .path()
            .join(format!("native-{client:?}-{stage}"));
        let before = selected_file(&self.root, "src/task.rs").unwrap();
        let body=json!({"transfer_id":"REPLACE_TRANSFER","accepted":accepted,"summary":"Incremented the implementation once","edit":if accepted {json!({"path":before.path,"before_sha256":before.sha256,"content":format!("pub fn answer() -> i32 {{ {} }}\n",stage+1)})}else{Value::Null},"artifact":if accepted {json!(format!("Progress stage {stage}"))}else{Value::Null},"next_action":"Increment the returned integer by exactly one on the next continuation"}).to_string();
        let session = format!("native-{client:?}-{stage}");
        let output = match client {
            NativeClient::Claude => {
                json!({"subtype":"success","is_error":false,"session_id":session,"result":body})
                    .to_string()
            }
            NativeClient::Codex => [
                json!({"type":"thread.started","thread_id":session}),
                json!({"type":"item.completed","item":{"type":"agent_message","text":body}}),
                json!({"type":"turn.completed"}),
            ]
            .iter()
            .map(Value::to_string)
            .collect::<Vec<_>>()
            .join("\n"),
        };
        let script = format!(
            "#!/bin/sh\npacket=$(/bin/cat)\nid=$(printf '%s\\n' \"$packet\" | /usr/bin/sed -n 's/.*\"transfer_id\":\"\\([^\"]*\\)\".*/\\1/p' | /usr/bin/tail -n 1)\nprintf '%s\\n' '{}' | /usr/bin/sed \"s/REPLACE_TRANSFER/$id/g\"\n",
            output.replace('\'', "'\\''")
        );
        let script = script.replacen(
            "#!/bin/sh\n",
            &format!(
                "#!/bin/sh\nprintf x >> '{}'\n",
                self.directory.path().join("native-count").display()
            ),
            1,
        );
        fs::write(&path, script).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
        path
    }
}

#[test]
fn cli_enrolled_work_continues_both_ways_with_artifact_bodies_and_no_replay() {
    let fixture = ContinuationFixture::new(false);
    let work = fixture.enroll();
    assert_eq!(
        fixture.enroll(),
        work,
        "exact enrollment retry must retain one Work"
    );
    for (index, client) in [
        NativeClient::Claude,
        NativeClient::Codex,
        NativeClient::Claude,
    ]
    .iter()
    .copied()
    .enumerate()
    {
        let executable = fixture.fake(client, index as u8 + 1, true);
        let preview = fixture.preview(
            &work,
            if client == NativeClient::Claude {
                "claude"
            } else {
                "codex"
            },
            &executable,
        );
        if index > 0 {
            assert!(
                !preview["transfer"]["plan"]["context"]["prior_artifacts"]
                    .as_array()
                    .unwrap()
                    .is_empty()
            );
        }
        let result = fixture.execute(&preview);
        assert_eq!(result["phase"], "complete");
        assert_eq!(result["incoming"]["id"], work);
        assert_eq!(result["incoming"]["generation"], index + 2);
        assert!(
            result["response"]["session_id"]
                .as_str()
                .unwrap()
                .starts_with("native-")
        );
        assert_eq!(
            fs::read_to_string(fixture.root.join("src/task.rs")).unwrap(),
            format!("pub fn answer() -> i32 {{ {} }}\n", index + 2)
        );
        let id = preview["confirmation"].as_str().unwrap();
        let inode = std::os::unix::fs::MetadataExt::ino(
            &fs::metadata(fixture.root.join("src/task.rs")).unwrap(),
        );
        assert_eq!(fixture.run(&["recover", id])["phase"], "complete");
        assert_eq!(
            std::os::unix::fs::MetadataExt::ino(
                &fs::metadata(fixture.root.join("src/task.rs")).unwrap()
            ),
            inode
        );
        assert!(
            fixture.run(&["artifact", id])["text"]
                .as_str()
                .unwrap()
                .contains("Progress")
        );
    }
}

#[test]
fn cli_stale_source_and_revocation_block_effects_and_resolve_releases_only_quiescent_work() {
    let fixture = ContinuationFixture::new(false);
    let work = fixture.enroll();
    let executable = fixture.fake(NativeClient::Codex, 1, true);
    let preview = fixture.preview(&work, "codex", &executable);
    let id = preview["confirmation"].as_str().unwrap();
    fs::write(fixture.root.join("src/task.rs"), "human edit\n").unwrap();
    assert!(
        !fixture
            .output(&["run", id, "--confirm", id])
            .status
            .success()
    );
    assert_eq!(
        fs::read_to_string(fixture.root.join("src/task.rs")).unwrap(),
        "human edit\n"
    );
    fixture.run(&["revoke", id]);
    assert!(!fixture.output(&["recover", id]).status.success());
    assert_eq!(fixture.run(&["resolve", id])["phase"], "resolved");
    assert!(fixture.preview(&work, "codex", &executable)["confirmation"].is_string());
}

#[test]
fn cli_declined_response_can_be_observed_and_resolved_without_taking_responsibility() {
    let fixture = ContinuationFixture::new(false);
    let work = fixture.enroll();
    let executable = fixture.fake(NativeClient::Claude, 1, false);
    let preview = fixture.preview(&work, "claude", &executable);
    let id = preview["confirmation"].as_str().unwrap();
    assert!(
        !fixture
            .output(&["run", id, "--confirm", id])
            .status
            .success()
    );
    let result = fixture.run(&["show", id]);
    assert_eq!(result["phase"], "received");
    assert!(result["incoming"].is_null());
    fixture.run(&["revoke", id]);
    let resolved = fixture.run(&["resolve", id]);
    assert_eq!(resolved["phase"], "resolved");
    let next = fixture.preview(&work, "claude", &executable);
    assert_eq!(next["transfer"]["plan"]["work"]["generation"], 1);
}

#[test]
#[ignore = "uses explicitly selected installed Claude and Codex subscription clients"]
fn live_installed_claude_codex_claude_continue_the_same_work() {
    let claude = PathBuf::from(
        std::env::var_os("AGENTSCM_LIVE_CLAUDE")
            .expect("set AGENTSCM_LIVE_CLAUDE to the installed absolute executable"),
    );
    let codex = PathBuf::from(
        std::env::var_os("AGENTSCM_LIVE_CODEX")
            .expect("set AGENTSCM_LIVE_CODEX to the installed absolute executable"),
    );
    assert!(claude.is_absolute() && codex.is_absolute());
    let fixture = ContinuationFixture::new(true);
    let work = fixture.run(&["enroll","--root",fixture.root.to_str().unwrap(),"--request-key","artifact-dependent-live","--title","Continue the single-function task using the retained artifact as the next-answer decision","--path","src/task.rs","--next-action","Use the newest prior_artifacts[0].text JSON next_answer as the answer value; if no prior artifact exists use 2. Replace only the integer in the existing function, preserving formatting/signature. Produce artifact as the JSON string {next_answer: answer*3+5} with valid quoted JSON key. Set next_action to these same instructions so the other provider continues from the artifact decision."])["work"]["id"].as_str().unwrap().to_owned();
    let mut sessions = std::collections::BTreeSet::new();
    for (index, (client, executable)) in
        [("claude", &claude), ("codex", &codex), ("claude", &claude)]
            .into_iter()
            .enumerate()
    {
        let preview = fixture.preview(&work, client, executable);
        let result = fixture.execute(&preview);
        assert_eq!(result["phase"], "complete");
        assert_eq!(result["incoming"]["id"], work);
        assert_eq!(result["incoming"]["generation"], index + 2);
        assert!(
            sessions.insert(
                result["response"]["session_id"]
                    .as_str()
                    .unwrap()
                    .to_owned()
            )
        );
        let source = fs::read_to_string(fixture.root.join("src/task.rs")).unwrap();
        let expected = [2, 11, 38][index];
        assert_eq!(
            source,
            format!("pub fn answer() -> i32 {{ {expected} }}\n"),
            "actual continuation must use the prior artifact decision"
        );
        assert!(
            result["artifact"].is_object(),
            "actual continuation must produce its useful requested artifact"
        );
        let id = preview["confirmation"].as_str().unwrap();
        let artifact = fixture.run(&["artifact", id]);
        let decision: Value = serde_json::from_str(artifact["text"].as_str().unwrap()).unwrap();
        assert_eq!(decision["next_answer"], expected * 3 + 5);
        assert_eq!(fixture.run(&["recover", id])["phase"], "complete");
        eprintln!(
            "qualified {client} generation {} with retained source/artifact checkpoint",
            index + 2
        );
    }
}

#[test]
fn cli_checks_record_actual_success_and_failure_and_recover_lost_receipt_without_replay() {
    for successful in [true, false] {
        let fixture = ContinuationFixture::new(false);
        let work = fixture.enroll();
        let executable = fixture.fake(NativeClient::Codex, 1, true);
        let marker = fixture.directory.path().join("check-count");
        let recipe = fixture.directory.path().join("check.json");
        fs::write(&recipe,serde_json::to_vec(&json!({"id":"synthetic-check","argv":["/bin/sh","-c",format!("printf x >> '{}'; exit {}",marker.display(),if successful {0}else{7})],"coverage":"partial","timeout_seconds":10,"max_output_bytes":4096})).unwrap()).unwrap();
        let preview = fixture.run(&[
            "preview",
            &work,
            "--root",
            fixture.root.to_str().unwrap(),
            "--to",
            "codex",
            "--executable",
            executable.to_str().unwrap(),
            "--path",
            "src/task.rs",
            "--check",
            recipe.to_str().unwrap(),
        ]);
        let result = fixture.execute(&preview);
        let id = preview["confirmation"].as_str().unwrap();
        assert_eq!(result["phase"], "complete");
        assert_eq!(
            result["check"]["result"],
            if successful { "passed" } else { "failed" }
        );
        assert_eq!(fs::read(&marker).unwrap(), b"x");
        // Simulate loss of the local receipt/checkpoint response after the trusted child finished.
        // Preserve the Hub's completed checkpoint as well: exact retry must reconcile that too.
        let journal = Journal::open(&fixture.data).unwrap();
        let mut interrupted = journal.get(id).unwrap();
        interrupted.incoming = serde_json::from_value(result["incoming"].clone()).ok();
        // The result receipt expects the pre-checkpoint incoming sequence; restore it from the
        // stable checkpoint operation's expected sequence retained by the transfer.
        interrupted.incoming.as_mut().unwrap().updated_seq =
            interrupted.handover.as_ref().unwrap().updated_seq + 1;
        interrupted.incoming.as_mut().unwrap().state = WorkState::Active;
        interrupted.phase = TransferPhase::Checking;
        interrupted.check = None;
        journal.save(&interrupted).unwrap();
        // Read the reusable recipe receipt independently. The CLI also must refuse a stale Work
        // sequence instead of treating that retained receipt as new execution authority.
        let incoming = interrupted.incoming.as_ref().unwrap();
        let request = agentscm::change_capture::ConfiguredRecipeRunRequest {
            work_id: work.clone(),
            generation: incoming.generation,
            observed_session_id: incoming.lead_session.clone(),
            session_id: incoming.lead_session.clone(),
            source: agentscm::change_capture::GitSourceRequest {
                repository: "synthetic/project".into(),
                worktree_id: agentscm::store::digest(
                    fixture
                        .root
                        .canonicalize()
                        .unwrap()
                        .as_os_str()
                        .as_encoded_bytes(),
                ),
                repository_path: fixture.root.canonicalize().unwrap(),
                base_ref: "HEAD".into(),
            },
            setup_digest: agentscm::store::digest(id.as_bytes()),
            recipe: interrupted.plan.check.clone().unwrap(),
            managed_invocation: id.into(),
        };
        let operator_identity =
            agentscm::change_capture::operator_recipe_identity(&request).unwrap();
        let observed = agentscm::change_capture::run_operator_recipe(
            request.clone(),
            &journal.spool().unwrap(),
            &operator_identity,
            true,
            true,
            |_, _| panic!("recovery must not spawn"),
        )
        .unwrap()
        .unwrap();
        assert_eq!(
            serde_json::to_value(observed.finish.result).unwrap(),
            if successful { "passed" } else { "failed" }
        );
        assert_eq!(fs::read(&marker).unwrap(), b"x");
        let mut changed_source = request.clone();
        changed_source.source.repository = "synthetic/other".into();
        assert!(
            matches!(
                agentscm::change_capture::run_operator_recipe(
                    changed_source,
                    &journal.spool().unwrap(),
                    &operator_identity,
                    true,
                    true,
                    |_, _| panic!("reconciliation must not spawn")
                ),
                Err(agentscm::change_capture::CaptureError::Conflict(message))
                    if message.contains("operator recipe identity changed")
            ),
            "another source request must conflict with the reserved check identity"
        );

        if successful {
            use agentscm::change_capture::{LocalSpool, run_operator_recipe};
            use agentscm::change_record::CaptureEnvelopeV1;
            let empty =
                LocalSpool::open(fixture.directory.path().join("empty-check-spool")).unwrap();
            assert!(
                run_operator_recipe(
                    request.clone(),
                    &empty,
                    &operator_identity,
                    true,
                    false,
                    |_, _| panic!("observation cannot spawn")
                )
                .unwrap()
                .is_none()
            );
            let unfinished =
                LocalSpool::open(fixture.directory.path().join("unfinished-check-spool")).unwrap();
            let start = CaptureEnvelopeV1::ActionStart(observed.start.clone());
            unfinished.append(&start).unwrap();
            unfinished.acknowledge(&start).unwrap();
            unfinished.compact_acknowledged(16).unwrap();
            assert!(
                unfinished
                    .action_start(&observed.start.operation_id)
                    .unwrap()
                    .is_some(),
                "compaction must retain an unresolved start"
            );
            assert!(
                run_operator_recipe(
                    request.clone(),
                    &unfinished,
                    &operator_identity,
                    true,
                    false,
                    |_, _| panic!("unresolved action cannot spawn")
                )
                .is_err()
            );
            let orphan =
                LocalSpool::open(fixture.directory.path().join("orphan-check-spool")).unwrap();
            orphan
                .append(&CaptureEnvelopeV1::ActionFinish(observed.finish.clone()))
                .unwrap();
            assert!(
                run_operator_recipe(
                    request.clone(),
                    &orphan,
                    &operator_identity,
                    true,
                    false,
                    |_, _| panic!("corrupt receipt cannot spawn")
                )
                .is_err(),
                "an orphan finish cannot be mistaken for no launch"
            );

            // A replaced root must not turn an admitted check into a different empty lookup.
            let original_root = fixture.directory.path().join("original-source");
            fs::rename(&fixture.root, &original_root).unwrap();
            assert!(
                Command::new("cp")
                    .args(["-R"])
                    .arg(&original_root)
                    .arg(&fixture.root)
                    .status()
                    .unwrap()
                    .success()
            );
            assert_ne!(
                agentscm::change_capture::operator_recipe_identity(&request).unwrap(),
                operator_identity
            );
            let mut missing_pid = interrupted.clone();
            missing_pid.check_attempt = Some(NativeAttempt::reserve());
            journal.save(&missing_pid).unwrap();
            let refused = fixture.output(&["resolve", id]);
            assert!(!refused.status.success());
            assert!(
                String::from_utf8_lossy(&refused.stderr)
                    .contains("operator recipe identity changed")
            );
            assert_eq!(fixture.run(&["show", id])["phase"], "checking");
            fs::remove_dir_all(&fixture.root).unwrap();
            fs::rename(&original_root, &fixture.root).unwrap();
            journal.save(&interrupted).unwrap();
            assert_eq!(fs::read(&marker).unwrap(), b"x");
        }
        assert!(
            !fixture.output(&["recover", id]).status.success(),
            "current Work sequence prevents stale check executor"
        );

        // Even with current Work authority, a PID marker contradicts missing start/finish receipts.
        use agentscm::hub::work::{CheckpointKind, WorkCheckpoint};
        let current: WorkItem = serde_json::from_value(fixture.hub(
            reqwest::Method::GET,
            &format!("/v1/hub/standup/work/{work}"),
            None,
        ))
        .unwrap();
        let active = fixture.hub(
            reqwest::Method::POST,
            "/v1/hub/standup/checkpoint",
            Some(
                serde_json::to_value(WorkCheckpoint {
                    work_id: work.clone(),
                    operation_id: "observe-receipt-loss".into(),
                    generation: current.generation,
                    session: current.lead_session.clone(),
                    expected_updated_seq: Some(current.updated_seq),
                    kind: CheckpointKind::Progress,
                    summary: "Observe a synthetic interrupted check".into(),
                    next_action: current.next_action.clone(),
                    scope: None,
                    branch: None,
                    worktree: None,
                    resolve_blocker: false,
                    source_refs: vec![],
                    depends_on: current.depends_on.clone(),
                    related_work: current.related_work.clone(),
                })
                .unwrap(),
            ),
        );
        interrupted.incoming = Some(serde_json::from_value(active["item"].clone()).unwrap());
        assert!(interrupted.check_attempt.as_ref().unwrap().pid.is_some());
        journal.save(&interrupted).unwrap();
        fs::rename(
            journal.spool().unwrap().root(),
            fixture.directory.path().join("preserved-check-spool"),
        )
        .unwrap();
        let refused = fixture.output(&["recover", id]);
        assert!(!refused.status.success());
        assert!(
            String::from_utf8_lossy(&refused.stderr)
                .contains("spawned check lost its durable start")
        );
        assert_eq!(fixture.run(&["show", id])["phase"], "checking");
        assert_eq!(
            fs::read(&marker).unwrap(),
            b"x",
            "contradictory receipt state never relaunches the check"
        );
    }
}

#[test]
fn native_resolution_observes_process_exit_and_never_treats_age_as_quiescence() {
    use std::os::unix::process::CommandExt;
    let mut child = Command::new("/bin/sleep")
        .arg("30")
        .process_group(0)
        .spawn()
        .unwrap();
    let mut attempt = NativeAttempt::reserve();
    attempt.pid = Some(child.id());
    attempt.pgid = Some(child.id());
    assert!(!attempt.quiescent());
    child.kill().unwrap();
    child.wait().unwrap();
    assert!(attempt.quiescent());
    let unknown = NativeAttempt::reserve();
    assert!(!unknown.quiescent());
}

#[test]
fn retained_check_receipt_cannot_release_a_still_running_process_group() {
    use std::os::unix::process::CommandExt;
    let fixture = ContinuationFixture::new(false);
    let work = fixture.enroll();
    let executable = fixture.fake(NativeClient::Codex, 1, true);
    let recipe = fixture.directory.path().join("check.json");
    fs::write(&recipe,serde_json::to_vec(&json!({"id":"quiescent-check","argv":["/usr/bin/true"],"coverage":"partial","timeout_seconds":10,"max_output_bytes":4096})).unwrap()).unwrap();
    let preview = fixture.run(&[
        "preview",
        &work,
        "--root",
        fixture.root.to_str().unwrap(),
        "--to",
        "codex",
        "--executable",
        executable.to_str().unwrap(),
        "--path",
        "src/task.rs",
        "--check",
        recipe.to_str().unwrap(),
    ]);
    fixture.execute(&preview);
    let id = preview["confirmation"].as_str().unwrap();
    let mut child = Command::new("/bin/sleep")
        .arg("30")
        .process_group(0)
        .spawn()
        .unwrap();
    let journal = Journal::open(&fixture.data).unwrap();
    let mut interrupted = journal.get(id).unwrap();
    let receipt = interrupted.check.clone();
    let mut attempt = NativeAttempt::reserve();
    attempt.pid = Some(child.id());
    attempt.pgid = Some(child.id());
    interrupted.phase = TransferPhase::Checking;
    interrupted.check_attempt = Some(attempt);
    journal.save(&interrupted).unwrap();
    fixture.run(&["revoke", id]);
    let blocked = fixture.output(&["resolve", id]);
    assert!(!blocked.status.success());
    assert!(String::from_utf8_lossy(&blocked.stderr).contains("check process/group"));
    assert_eq!(journal.get(id).unwrap().phase, TransferPhase::Checking);
    assert!(child.try_wait().unwrap().is_none());
    child.kill().unwrap();
    child.wait().unwrap();
    let resolved = fixture.run(&["resolve", id]);
    assert_eq!(resolved["phase"], "resolved");
    assert_eq!(journal.get(id).unwrap().check, receipt);
}

#[test]
fn cli_recovers_handover_takeover_and_checkpoint_success_with_lost_hub_reply() {
    for (mode, phase) in [(4, "handing_over"), (1, "received"), (2, "checkpointing")] {
        let fixture = ContinuationFixture::new(false);
        let work = fixture.enroll();
        let executable = fixture.fake(NativeClient::Codex, 1, true);
        let preview = fixture.preview(&work, "codex", &executable);
        let id = preview["confirmation"].as_str().unwrap();
        fixture.lose_reply.store(mode, Ordering::SeqCst);
        assert!(
            !fixture
                .output(&["run", id, "--confirm", id])
                .status
                .success()
        );
        let interrupted = fixture.run(&["show", id]);
        assert_eq!(interrupted["phase"], phase);
        let inode = std::os::unix::fs::MetadataExt::ino(
            &fs::metadata(fixture.root.join("src/task.rs")).unwrap(),
        );
        let recovered = fixture.run(&["recover", id]);
        assert_eq!(recovered["phase"], "complete");
        assert_eq!(recovered["incoming"]["generation"], 2);
        assert_eq!(recovered["incoming"]["id"], work);
        assert_eq!(
            fs::read_to_string(fixture.root.join("src/task.rs")).unwrap(),
            "pub fn answer() -> i32 { 2 }\n"
        );
        assert_eq!(
            fs::read(fixture.directory.path().join("native-count")).unwrap(),
            b"x",
            "a lost response never launches another native call"
        );
        if mode == 2 {
            assert_eq!(
                std::os::unix::fs::MetadataExt::ino(
                    &fs::metadata(fixture.root.join("src/task.rs")).unwrap()
                ),
                inode,
                "checkpoint recovery does not repeat source replacement"
            );
        }
        assert_eq!(
            fixture.run(&["recover", id])["artifact"],
            recovered["artifact"]
        );
    }
}

#[test]
fn explicit_resolution_after_a_lost_reply_uses_the_current_boundary_without_replaying_effects() {
    use agentscm::hub::work::{CheckpointKind, WorkCheckpoint};

    let fixture = ContinuationFixture::new(false);
    let work = fixture.enroll();
    let executable = fixture.fake(NativeClient::Codex, 1, true);
    let marker = fixture.directory.path().join("check-count");
    let recipe = fixture.directory.path().join("check.json");
    fs::write(
        &recipe,
        serde_json::to_vec(&json!({
            "id": "synthetic-check",
            "argv": ["/bin/sh", "-c", format!("printf x >> '{}'", marker.display())],
            "coverage": "partial",
            "timeout_seconds": 10,
            "max_output_bytes": 4096
        }))
        .unwrap(),
    )
    .unwrap();
    let preview = fixture.run(&[
        "preview",
        &work,
        "--root",
        fixture.root.to_str().unwrap(),
        "--to",
        "codex",
        "--executable",
        executable.to_str().unwrap(),
        "--path",
        "src/task.rs",
        "--check",
        recipe.to_str().unwrap(),
    ]);
    let id = preview["confirmation"].as_str().unwrap();
    fixture.lose_reply.store(2, Ordering::SeqCst);
    assert!(
        !fixture
            .output(&["run", id, "--confirm", id])
            .status
            .success()
    );
    assert_eq!(fixture.run(&["show", id])["phase"], "checkpointing");
    let inode = std::os::unix::fs::MetadataExt::ino(
        &fs::metadata(fixture.root.join("src/task.rs")).unwrap(),
    );

    for index in 0..2 {
        let current: WorkItem = serde_json::from_value(fixture.hub(
            reqwest::Method::GET,
            &format!("/v1/hub/standup/work/{work}"),
            None,
        ))
        .unwrap();
        assert_eq!(current.state, WorkState::Handover);
        let progressed = fixture.hub(
            reqwest::Method::POST,
            "/v1/hub/standup/checkpoint",
            Some(
                serde_json::to_value(WorkCheckpoint {
                    work_id: work.clone(),
                    operation_id: format!("same-generation-progress-{index}"),
                    generation: current.generation,
                    session: current.lead_session.clone(),
                    expected_updated_seq: Some(current.updated_seq),
                    kind: CheckpointKind::Progress,
                    summary: "Observed the retained source result".into(),
                    next_action: current.next_action.clone(),
                    scope: None,
                    branch: None,
                    worktree: None,
                    resolve_blocker: false,
                    source_refs: vec![],
                    depends_on: current.depends_on.clone(),
                    related_work: current.related_work.clone(),
                })
                .unwrap(),
            ),
        );
        assert_eq!(progressed["item"]["state"], "active");
        if index == 0 {
            fixture.lose_reply.store(4, Ordering::SeqCst);
            assert!(!fixture.output(&["resolve", id]).status.success());
            assert_eq!(
                fixture.lose_reply.load(Ordering::SeqCst),
                0,
                "resolve checkpoint reached the Hub and consumed the lost-reply fault"
            );
            assert_eq!(fixture.run(&["show", id])["phase"], "checkpointing");
        } else {
            let resolved = fixture.run(&["resolve", id]);
            assert_eq!(resolved["phase"], "resolved");
            assert_eq!(resolved["incoming"]["generation"], 2);
            assert_eq!(resolved["incoming"]["state"], "handover");
        }
    }
    assert_eq!(
        fs::read(fixture.directory.path().join("native-count")).unwrap(),
        b"x"
    );
    assert_eq!(fs::read(&marker).unwrap(), b"x");
    assert_eq!(
        std::os::unix::fs::MetadataExt::ino(
            &fs::metadata(fixture.root.join("src/task.rs")).unwrap()
        ),
        inode,
        "explicit resolution never repeats source replacement"
    );
    assert_eq!(
        fs::read_to_string(fixture.root.join("src/task.rs")).unwrap(),
        "pub fn answer() -> i32 { 2 }\n"
    );
}

#[test]
fn resolution_observes_the_current_handover_and_rejects_changed_scope_branch_or_worktree() {
    use agentscm::hub::work::{CheckpointKind, WorkCheckpoint};

    for changed in ["none", "scope", "branch", "worktree"] {
        let fixture = ContinuationFixture::new(false);
        let work = fixture.enroll();
        let executable = fixture.fake(NativeClient::Codex, 1, true);
        let preview = fixture.preview(&work, "codex", &executable);
        let id = preview["confirmation"].as_str().unwrap();
        fixture.lose_reply.store(2, Ordering::SeqCst);
        assert!(
            !fixture
                .output(&["run", id, "--confirm", id])
                .status
                .success()
        );
        let current: WorkItem = serde_json::from_value(fixture.hub(
            reqwest::Method::GET,
            &format!("/v1/hub/standup/work/{work}"),
            None,
        ))
        .unwrap();
        let observed = fixture.hub(
            reqwest::Method::POST,
            "/v1/hub/standup/checkpoint",
            Some(
                serde_json::to_value(WorkCheckpoint {
                    work_id: work.clone(),
                    operation_id: "observed-later-boundary".into(),
                    generation: current.generation,
                    session: current.lead_session.clone(),
                    expected_updated_seq: Some(current.updated_seq),
                    kind: CheckpointKind::Handover,
                    summary: "Recorded a later same-generation boundary".into(),
                    next_action: current.next_action.clone(),
                    scope: (changed == "scope").then(|| vec!["src/other.rs".into()]),
                    branch: (changed == "branch").then(|| "changed-branch".into()),
                    worktree: (changed == "worktree").then(|| "changed-worktree".into()),
                    resolve_blocker: false,
                    source_refs: vec![],
                    depends_on: current.depends_on.clone(),
                    related_work: current.related_work.clone(),
                })
                .unwrap(),
            ),
        );
        if changed == "none" {
            let resolved = fixture.run(&["resolve", id]);
            assert_eq!(resolved["phase"], "resolved");
            assert_eq!(resolved["incoming"], observed["item"]);
        } else {
            assert!(!fixture.output(&["resolve", id]).status.success());
            assert_eq!(fixture.run(&["show", id])["phase"], "checkpointing");
        }
        assert_eq!(
            fs::read(fixture.directory.path().join("native-count")).unwrap(),
            b"x"
        );
    }
}

#[test]
fn full_journal_fences_and_latest_generation_survive_more_than_sixty_four_transfers() {
    let root = tempfile::tempdir().unwrap();
    let data = tempfile::tempdir().unwrap();
    let base = plan(root.path());
    let journal = Journal::open(data.path()).unwrap();
    journal
        .enroll(&base.work, root.path(), "authority")
        .unwrap();
    let mut current = base.work.clone();
    let mut first = None;
    for index in 0..70 {
        let mut next = base.clone();
        next.work = current.clone();
        next.context["iteration"] = json!(index);
        next.seal().unwrap();
        let mut record = journal.insert(next).unwrap();
        current.generation += 1;
        current.lead_session = format!("native-{index}");
        record.incoming = Some(current.clone());
        record.phase = TransferPhase::Complete;
        journal.save(&record).unwrap();
        if first.is_none() {
            first = Some(record);
        }
    }
    // Unexecuted previews must not conceal the last accepted generation.
    for index in 70..140 {
        let mut next = base.clone();
        next.context["iteration"] = json!(index);
        next.seal().unwrap();
        journal.insert(next).unwrap();
    }
    assert_eq!(journal.history_count(&base.work.id).unwrap(), 140);
    assert_eq!(journal.work_history(&base.work.id).unwrap().len(), 16);
    journal
        .check_enrolled(&current, root.path(), "authority")
        .unwrap();
    assert_eq!(
        journal
            .latest_record(&base.work.id, true)
            .unwrap()
            .unwrap()
            .incoming
            .unwrap()
            .generation,
        71
    );
    journal.assert_quiescent(&base.work.id, "new").unwrap();
    let mut oldest = first.unwrap();
    oldest.phase = TransferPhase::Reserved;
    journal.save(&oldest).unwrap();
    assert!(
        journal.assert_quiescent(&base.work.id, "new").is_err(),
        "a display window cannot hide an old uncertain executor"
    );
    let mut cursor = None;
    let mut ids = std::collections::BTreeSet::new();
    loop {
        let mut page = journal.history_ids(&base.work.id, cursor).unwrap();
        let more = page.len() > 16;
        page.truncate(16);
        for (_, id) in &page {
            assert!(ids.insert(id.clone()));
        }
        if !more {
            break;
        }
        cursor = page.last().map(|(row, _)| *row);
    }
    assert_eq!(ids.len(), 140);
}

#[test]
fn cli_large_artifact_history_stays_bounded_and_earlier_bodies_are_explicitly_selectable() {
    let fixture = ContinuationFixture::new(false);
    let work = fixture.enroll();
    let executable = fixture.fake(NativeClient::Codex, 1, true);
    let preview = fixture.preview(&work, "codex", &executable);
    fixture.execute(&preview);
    let journal = Journal::open(&fixture.data).unwrap();
    let original = journal
        .get(preview["confirmation"].as_str().unwrap())
        .unwrap();
    let mut earlier = String::new();
    for index in 0..70 {
        let mut next = original.plan.clone();
        next.context["synthetic_retained_history_index"] = json!(index);
        next.seal().unwrap();
        let mut record = journal.insert(next).unwrap();
        let prefix = format!("artifact-{index}:");
        let body = format!("{prefix}{}", "x".repeat(MAX_FILE_BYTES - prefix.len()));
        record.artifact = Some(retain_result(&journal, &body).unwrap());
        record.error =
            Some("synthetic retained diagnostic ".repeat(if index == 69 { 1600 } else { 450 }));

        record.incoming = original.incoming.clone();
        record.phase = TransferPhase::Complete;
        journal.save(&record).unwrap();
        if index == 0 {
            earlier = record.plan.id.clone();
        }
    }
    let next = fixture.preview(&work, "codex", &executable);
    let context = &next["transfer"]["plan"]["context"];
    assert_eq!(context["prior_artifacts"].as_array().unwrap().len(), 1);
    assert!(
        context["prior_artifacts"][0]["text"]
            .as_str()
            .unwrap()
            .starts_with("artifact-69:")
    );
    assert!(!context["prior_effects"].as_array().unwrap().is_empty());
    assert!(context["prior_effects"].as_array().unwrap().len() <= 16);
    assert!(serde_json::to_vec(&context["prior_effects"]).unwrap().len() <= 32 * 1024);
    assert_eq!(
        context["history_disclosure"]["compacted_effect_details"],
        true
    );
    assert!(
        context["history_disclosure"]["omitted_transfers"]
            .as_u64()
            .unwrap()
            > 0
    );
    assert!(serde_json::to_vec(&next["transfer"]["plan"]).unwrap().len() < MAX_INPUT_BYTES);
    let selected = fixture.run(&[
        "preview",
        &work,
        "--root",
        fixture.root.to_str().unwrap(),
        "--to",
        "codex",
        "--executable",
        executable.to_str().unwrap(),
        "--artifact",
        &earlier,
    ]);
    assert!(
        selected["transfer"]["plan"]["context"]["prior_artifacts"][0]["text"]
            .as_str()
            .unwrap()
            .starts_with("artifact-0:")
    );
    let history = fixture.run(&["history", &work]);
    assert_eq!(history["records"].as_array().unwrap().len(), 16);
    assert!(history["next_cursor"].as_i64().unwrap() > 0);
    let mut unresolved = journal.get(&earlier).unwrap();
    unresolved.phase = TransferPhase::Reserved;
    journal.save(&unresolved).unwrap();
    assert!(
        !fixture
            .output(&[
                "preview",
                &work,
                "--root",
                fixture.root.to_str().unwrap(),
                "--to",
                "codex",
                "--executable",
                executable.to_str().unwrap(),
                "--path",
                "src/task.rs"
            ])
            .status
            .success()
    );
}

#[test]
fn checkpoint_replay_cannot_adopt_narrowed_scope_or_a_later_external_executor() {
    use agentscm::hub::work::{CheckpointKind, WorkCheckpoint, WorkRole, WorkStart};
    for mode in [4, 2] {
        let fixture = ContinuationFixture::new(false);
        let work = fixture.enroll();
        let executable = fixture.fake(NativeClient::Codex, 1, true);
        let preview = fixture.preview(&work, "codex", &executable);
        let id = preview["confirmation"].as_str().unwrap();
        fixture.lose_reply.store(mode, Ordering::SeqCst);
        assert!(
            !fixture
                .output(&["run", id, "--confirm", id])
                .status
                .success()
        );
        let current: WorkItem = serde_json::from_value(fixture.hub(
            reqwest::Method::GET,
            &format!("/v1/hub/standup/work/{work}"),
            None,
        ))
        .unwrap();
        if mode == 4 {
            fixture.hub(
                reqwest::Method::POST,
                "/v1/hub/standup/checkpoint",
                Some(
                    serde_json::to_value(WorkCheckpoint {
                        work_id: work.clone(),
                        operation_id: "scope-narrowed".into(),
                        generation: current.generation,
                        session: current.lead_session.clone(),
                        expected_updated_seq: Some(current.updated_seq),
                        kind: CheckpointKind::Handover,
                        summary: "Narrowed the task scope".into(),
                        next_action: current.next_action.clone(),
                        scope: Some(vec!["src/other.rs".into()]),
                        branch: None,
                        worktree: None,
                        resolve_blocker: false,
                        source_refs: vec![],
                        depends_on: vec![],
                        related_work: vec![],
                    })
                    .unwrap(),
                ),
            );
        } else {
            fixture.hub(
                reqwest::Method::POST,
                "/v1/hub/standup/start",
                Some(
                    serde_json::to_value(WorkStart {
                        work_id: Some(work.clone()),
                        source: None,
                        request_key: None,
                        title: current.title.clone(),
                        session: "unmanaged-native".into(),
                        role: WorkRole::Lead,
                        scope: current.scope.clone(),
                        branch: current.branch.clone(),
                        worktree: current.worktree.clone(),
                        next_action: current.next_action.clone(),
                        depends_on: vec![],
                        related_work: vec![],
                        takeover_generation: Some(current.generation),
                        preflight: None,
                    })
                    .unwrap(),
                ),
            );
        }
        assert!(!fixture.output(&["recover", id]).status.success());
        let retained = fixture.run(&["show", id]);
        assert_eq!(
            retained["phase"],
            if mode == 4 {
                "handing_over"
            } else {
                "checkpointing"
            }
        );
        if mode == 4 {
            assert!(
                !fixture.directory.path().join("native-count").exists(),
                "narrowed scope prevents even the native disclosure"
            );
        } else {
            assert_ne!(retained["incoming"]["lead_session"], "unmanaged-native");
            assert_eq!(retained["incoming"]["generation"], 2);
        }
    }
}

#[test]
fn failed_native_call_retains_bounded_identity_and_status_without_failed_output_bodies() {
    let root = tempfile::tempdir().unwrap();
    let data = tempfile::tempdir().unwrap();
    let binaries = tempfile::tempdir().unwrap();
    let binary = binaries.path().join("native-failure");
    fs::write(&binary,"#!/bin/sh\n/bin/cat >/dev/null\nprintf '%s\\n' '{\"subtype\":\"error\",\"session_id\":\"native-failed-session\",\"result\":\"synthetic-secret-result\"}'\nprintf '%s\\n' 'synthetic-secret-stderr' >&2\nexit 17\n").unwrap();
    fs::set_permissions(&binary, fs::Permissions::from_mode(0o700)).unwrap();
    let mut plan = plan(root.path());
    plan.destination = NativeClient::Claude;
    plan.executable = executable::capture(&binary, &[root.path(), data.path()]).unwrap();
    plan.seal().unwrap();
    let journal = Journal::open(data.path()).unwrap();
    let mut record = journal.insert(plan).unwrap();
    record.phase = TransferPhase::Reserved;
    record.native = Some(NativeAttempt::reserve());
    journal.save(&record).unwrap();
    assert!(invoke_reserved(&journal, &mut record).is_err());
    let retained = journal.get(&record.plan.id).unwrap();
    let diagnostic = retained.native_diagnostic.as_ref().unwrap();
    assert_eq!(diagnostic.exit_code, Some(17));
    assert_eq!(
        diagnostic.session_id.as_deref(),
        Some("native-failed-session")
    );
    assert_eq!(diagnostic.protocol, "response_unaccepted");
    assert!(retained.response.is_none());
    assert!(
        !serde_json::to_string(&retained)
            .unwrap()
            .contains("synthetic-secret")
    );
}

#[test]
fn resolving_no_effect_transfers_after_first_native_run_preserves_the_current_generation() {
    for lose_handover in [false, true] {
        let fixture = ContinuationFixture::new(false);
        let work = fixture.enroll();
        let first = fixture.fake(NativeClient::Claude, 1, true);
        let preview = fixture.preview(&work, "claude", &first);
        fixture.execute(&preview);
        let next = fixture.fake(NativeClient::Codex, 2, true);
        let preview = fixture.preview(&work, "codex", &next);
        let id = preview["confirmation"].as_str().unwrap();
        if lose_handover {
            fixture.lose_reply.store(4, Ordering::SeqCst);
            assert!(
                !fixture
                    .output(&["run", id, "--confirm", id])
                    .status
                    .success()
            );
        }
        fixture.run(&["revoke", id]);
        let resolved = fixture.run(&["resolve", id]);
        assert_eq!(resolved["phase"], "resolved");
        assert!(resolved["incoming"].is_null());
        assert!(resolved["handover"].is_null());
        let fresh = fixture.preview(&work, "codex", &next);
        assert_eq!(fresh["transfer"]["plan"]["work"]["generation"], 2);
        let result = fixture.execute(&fresh);
        assert_eq!(result["incoming"]["generation"], 3);
        assert_eq!(result["phase"], "complete");
    }
}

#[test]
fn cli_can_restore_dirty_selected_source_to_head_and_recover_lost_checkpoint_reply() {
    for lost_reply in [false, true] {
        let fixture = ContinuationFixture::new(false);
        fs::write(
            fixture.root.join("src/task.rs"),
            "pub fn answer() -> i32 { 2 }\n",
        )
        .unwrap();
        let work = fixture.enroll();
        let executable = fixture.fake(NativeClient::Codex, 0, true);
        let preview = fixture.preview(&work, "codex", &executable);
        let id = preview["confirmation"].as_str().unwrap();
        if lost_reply {
            fixture.lose_reply.store(2, Ordering::SeqCst);
            assert!(
                !fixture
                    .output(&["run", id, "--confirm", id])
                    .status
                    .success()
            );
        } else {
            fixture.execute(&preview);
        }
        let inode = std::os::unix::fs::MetadataExt::ino(
            &fs::metadata(fixture.root.join("src/task.rs")).unwrap(),
        );
        assert_eq!(fixture.run(&["recover", id])["phase"], "complete");
        assert_eq!(
            std::os::unix::fs::MetadataExt::ino(
                &fs::metadata(fixture.root.join("src/task.rs")).unwrap()
            ),
            inode
        );
        assert_eq!(
            fs::read_to_string(fixture.root.join("src/task.rs")).unwrap(),
            "pub fn answer() -> i32 { 1 }\n"
        );
    }
}

#[test]
fn dirty_to_clean_lost_write_reconciliation_preserves_unrelated_source() {
    let fixture = ContinuationFixture::new(false);
    let journal = Journal::open(&fixture.data).unwrap();
    let root = fixture.root.canonicalize().unwrap();
    let base = Command::new("git")
        .current_dir(&root)
        .args(["rev-parse", "HEAD"])
        .output()
        .unwrap();
    assert!(base.status.success());
    let base = String::from_utf8(base.stdout).unwrap().trim().to_owned();
    fs::write(root.join("src/other.rs"), "pub fn unchanged() {}\n").unwrap();
    for args in [
        vec!["add", "src/other.rs"],
        vec![
            "-c",
            "commit.gpgsign=false",
            "commit",
            "--quiet",
            "-m",
            "Retain unrelated synthetic source",
        ],
    ] {
        let output = Command::new("git")
            .current_dir(&root)
            .env_clear()
            .env("PATH", std::env::var_os("PATH").unwrap_or_default())
            .env("HOME", fixture.directory.path())
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .args(args)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    fs::write(root.join("src/task.rs"), "pub fn answer() -> i32 { 2 }\n").unwrap();
    let request = agentscm::change_capture::GitSourceRequest {
        repository: "synthetic/project".into(),
        worktree_id: agentscm::store::digest(root.as_os_str().as_encoded_bytes()),
        repository_path: root.clone(),
        base_ref: base,
    };
    let before =
        agentscm::change_capture::capture_git_source(&request, &journal.spool().unwrap()).unwrap();
    let selected = selected_file(&root, "src/task.rs").unwrap();
    let edit = ProposedEdit {
        path: selected.path,
        before_sha256: selected.sha256,
        content: "pub fn answer() -> i32 { 1 }\n".into(),
    };
    apply_edit(&root, &edit, false).unwrap();
    let inode = std::os::unix::fs::MetadataExt::ino(&fs::metadata(root.join(&edit.path)).unwrap());
    assert!(apply_edit(&root, &edit, true).unwrap());
    let clean =
        agentscm::change_capture::capture_git_source(&request, &journal.spool().unwrap()).unwrap();
    assert!(matches!(
        &clean,
        SourceBindingV1::Git {
            captured_snapshot: None,
            dirty: false,
            ..
        }
    ));
    let materialized = agentscm::change_capture::capture_git_source_materialized(
        &request,
        &journal.spool().unwrap(),
    )
    .unwrap();
    assert!(
        matches!(&materialized, SourceBindingV1::Git { captured_snapshot: Some(_), base, head, .. } if base == &request.base_ref && base != head)
    );
    validate_edit_result(&root, &before, &clean, &edit, &journal.spool().unwrap()).unwrap();
    assert_eq!(
        std::os::unix::fs::MetadataExt::ino(&fs::metadata(root.join(&edit.path)).unwrap()),
        inode
    );
    fs::write(root.join("src/other.rs"), "preserved unrelated work").unwrap();
    let changed =
        agentscm::change_capture::capture_git_source(&request, &journal.spool().unwrap()).unwrap();
    assert!(
        validate_edit_result(&root, &before, &changed, &edit, &journal.spool().unwrap()).is_err()
    );
    assert_eq!(
        fs::read_to_string(root.join("src/other.rs")).unwrap(),
        "preserved unrelated work"
    );
}

#[test]
fn revoked_check_admission_without_durable_start_can_resolve_without_reboot() {
    let fixture = ContinuationFixture::new(false);
    let work = fixture.enroll();
    let executable = fixture.fake(NativeClient::Codex, 1, true);
    let recipe = fixture.directory.path().join("check.json");
    fs::write(&recipe,serde_json::to_vec(&json!({"id":"never-started","argv":["/usr/bin/true"],"coverage":"partial","timeout_seconds":10,"max_output_bytes":4096})).unwrap()).unwrap();
    let preview = fixture.run(&[
        "preview",
        &work,
        "--root",
        fixture.root.to_str().unwrap(),
        "--to",
        "codex",
        "--executable",
        executable.to_str().unwrap(),
        "--path",
        "src/task.rs",
        "--check",
        recipe.to_str().unwrap(),
    ]);
    let id = preview["confirmation"].as_str().unwrap();
    // Inject the durable local admission boundary before the existing runner writes ActionStart.
    let journal = Journal::open(&fixture.data).unwrap();
    let mut record = journal.get(id).unwrap();
    record.incoming = Some(record.plan.work.clone());
    record.phase = TransferPhase::Checking;
    record.check_attempt = Some(NativeAttempt::reserve());
    record.check_identity = Some(
        agentscm::change_capture::operator_recipe_identity(
            &agentscm::change_capture::ConfiguredRecipeRunRequest {
                work_id: work.clone(),
                generation: record.plan.work.generation,
                observed_session_id: record.plan.work.lead_session.clone(),
                session_id: record.plan.work.lead_session.clone(),
                source: agentscm::change_capture::GitSourceRequest {
                    repository: record.plan.work.repository.clone(),
                    worktree_id: agentscm::store::digest(
                        record.plan.root.as_os_str().as_encoded_bytes(),
                    ),
                    repository_path: record.plan.root.clone(),
                    base_ref: "HEAD".into(),
                },
                setup_digest: agentscm::store::digest(id.as_bytes()),
                recipe: record.plan.check.clone().unwrap(),
                managed_invocation: id.into(),
            },
        )
        .unwrap(),
    );
    let identity = record.check_identity.take();
    journal.save(&record).unwrap();
    let missing_identity = fixture.output(&["resolve", id]);
    assert!(!missing_identity.status.success());
    assert!(
        String::from_utf8_lossy(&missing_identity.stderr)
            .contains("check reservation lost its exact identity")
    );
    record.check_identity = identity;
    journal.save(&record).unwrap();
    fixture.run(&["revoke", id]);
    let resolved = fixture.run(&["resolve", id]);
    assert_eq!(resolved["phase"], "resolved");
    assert!(resolved["check"].is_null());
    assert!(
        resolved["error"]
            .as_str()
            .unwrap()
            .contains("never launched")
    );
    assert!(!fixture.directory.path().join("native-count").exists());
    assert_eq!(
        fs::read_to_string(fixture.root.join("src/task.rs")).unwrap(),
        "pub fn answer() -> i32 { 1 }\n"
    );
}

#[test]
fn later_created_cancelled_sibling_cannot_hide_an_earlier_preview_completed_at_new_generation() {
    let fixture = ContinuationFixture::new(false);
    let work = fixture.enroll();
    let first = fixture.fake(NativeClient::Claude, 1, true);
    let initial = fixture.preview(&work, "claude", &first);
    fixture.execute(&initial);
    let next = fixture.fake(NativeClient::Codex, 2, true);
    let earlier = fixture.preview(&work, "codex", &next);
    let later = fixture.preview(&work, "codex", &next);
    let earlier_id = earlier["confirmation"].as_str().unwrap();
    let later_id = later["confirmation"].as_str().unwrap();
    assert_ne!(earlier_id, later_id);
    fixture.run(&["revoke", later_id]);
    fixture.run(&["resolve", later_id]);
    let completed = fixture.execute(&earlier);
    assert_eq!(completed["incoming"]["generation"], 3);
    let latest = fixture.preview(&work, "codex", &next);
    assert_eq!(latest["transfer"]["plan"]["work"]["generation"], 3);
    assert_eq!(
        latest["transfer"]["plan"]["context"]["prior_effects"][0]["transfer_id"],
        earlier_id
    );
    assert_eq!(
        latest["transfer"]["plan"]["context"]["prior_artifacts"][0]["transfer_id"],
        earlier_id
    );
}
