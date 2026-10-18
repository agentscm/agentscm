use std::io::{BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::thread::JoinHandle;
use std::time::Duration;

use agentscm::authority::Authority;
use agentscm::checks::SourcePathExpectation;
use agentscm::model::FileChange;
use agentscm::skill_workflow::{
    SkillReference, StepAction, StepDefinition, WorkflowDefinition, WorkflowSource, advance,
    compile, start,
};
use agentscm::store::{Engine, digest};
use rusqlite::Connection;
use serde_json::{Value, json};
use tempfile::TempDir;

const SKILL_PATH: &str = "skills/http/SKILL.md";
const SOURCE_PATH: &str = "src/example.rs";
const SOURCE_BYTES: &[u8] = b"pub fn answer() -> u8 { 42 }\n";

struct Server {
    child: Child,
    address: SocketAddr,
    stderr: Option<JoinHandle<()>>,
}

impl Server {
    fn start(data: &Path) -> Self {
        let mut child = Command::new(env!("CARGO_BIN_EXE_agentscm"))
            .arg("--data-dir")
            .arg(data)
            .args(["serve", "--bind", "127.0.0.1:0", "--max-concurrency", "2"])
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let stderr = child.stderr.take().unwrap();
        let (tx, rx) = std::sync::mpsc::sync_channel(1);
        let reader = std::thread::spawn(move || {
            for line in BufReader::new(stderr).lines() {
                let Ok(line) = line else { break };
                if line.starts_with("listening on ") {
                    let _ = tx.try_send(line);
                }
            }
        });
        let line = match rx.recv_timeout(Duration::from_secs(10)) {
            Ok(line) => line,
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                let _ = reader.join();
                panic!("server did not report its listening address: {error}");
            }
        };
        Self {
            child,
            address: line.strip_prefix("listening on ").unwrap().parse().unwrap(),
            stderr: Some(reader),
        }
    }

    fn get(&self, path: &str) -> String {
        let mut stream = TcpStream::connect(self.address).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(10)))
            .unwrap();
        write!(
            stream,
            "GET {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n"
        )
        .unwrap();
        let mut response = String::new();
        stream
            .take(5 * 1024 * 1024)
            .read_to_string(&mut response)
            .unwrap();
        response
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        if let Some(reader) = self.stderr.take() {
            let _ = reader.join();
        }
    }
}

fn response_json(response: &str, status: &str) -> Value {
    assert!(response.starts_with(status), "{response}");
    let (headers, body) = response.split_once("\r\n\r\n").unwrap();
    let headers = headers.to_ascii_lowercase();
    assert!(headers.contains("cache-control: no-store"));
    assert!(headers.contains("x-content-type-options: nosniff"));
    serde_json::from_str(body).unwrap()
}

fn put(path: &str, content: &[u8]) -> FileChange {
    FileChange {
        path: path.into(),
        content: Some(content.to_vec()),
    }
}

fn initialized_native() -> (TempDir, String, String) {
    let data = TempDir::new().unwrap();
    let engine = Engine::open(data.path()).unwrap();
    let revision = engine
        .checkpoint(
            None,
            &[
                put(SKILL_PATH, b"# HTTP workflow\n"),
                put(SOURCE_PATH, SOURCE_BYTES),
            ],
        )
        .unwrap();
    let authority = Authority::open(data.path()).unwrap();
    let repository = authority.repository_id().unwrap();
    drop(authority);
    drop(engine);
    (data, repository, revision)
}

fn definition(
    repository: &str,
    revision: &str,
    name: &str,
    step: StepDefinition,
) -> WorkflowDefinition {
    WorkflowDefinition {
        version: 1,
        name: name.into(),
        source: WorkflowSource {
            repository: repository.into(),
            revision: revision.into(),
            policy_epoch: 0,
        },
        skill: SkillReference {
            entrypoint: SKILL_PATH.into(),
            resources: vec![],
        },
        steps: vec![step],
    }
}

fn source_step() -> StepDefinition {
    StepDefinition {
        id: "source".into(),
        needs: vec![],
        required: true,
        action: StepAction::SourcePaths {
            expectations: vec![SourcePathExpectation {
                path: SOURCE_PATH.into(),
                sha256: digest(SOURCE_BYTES),
            }],
        },
    }
}

fn capability_step(required: bool) -> StepDefinition {
    StepDefinition {
        id: "review".into(),
        needs: vec![],
        required,
        action: StepAction::RequireCapability {
            capability: "model.review".into(),
        },
    }
}

fn retain(data: &Path, run_id: &str, definition: &WorkflowDefinition) {
    let plan = compile(data, definition).unwrap();
    start(data, run_id, &plan).unwrap();
    advance(data, run_id, 32).unwrap();
}

#[test]
fn projects_completed_blocked_and_omitted_runs_without_admission_claims() {
    let (data, repository, revision) = initialized_native();
    retain(
        data.path(),
        "completed",
        &definition(&repository, &revision, "completed", source_step()),
    );
    retain(
        data.path(),
        "blocked",
        &definition(&repository, &revision, "blocked", capability_step(true)),
    );
    retain(
        data.path(),
        "omitted",
        &definition(&repository, &revision, "omitted", capability_step(false)),
    );

    let server = Server::start(data.path());
    for (run_id, state, step_state) in [
        ("completed", "succeeded", "succeeded"),
        ("blocked", "blocked", "blocked"),
        ("omitted", "completed_with_omissions", "blocked"),
    ] {
        let view = response_json(
            &server.get(&format!("/v1/skill-workflows/{run_id}")),
            "HTTP/1.1 200",
        );
        assert_eq!(view["version"], 1);
        assert_eq!(view["run"]["run_id"], run_id);
        assert_eq!(view["run"]["state"], state);
        assert_eq!(view["run"]["steps"][0]["state"], step_state);
        assert_eq!(view["run"]["admission"], "not_assessed");
        assert_eq!(
            view["present_applicability"]["basis"],
            "current_authority_repository_policy_epoch_and_immutable_revision"
        );
        assert_eq!(view["present_applicability"]["state"], "current");
        assert_eq!(view["present_applicability"]["reason"], Value::Null);
    }
}

#[test]
fn unavailable_current_authority_does_not_rewrite_historical_success() {
    let (data, repository, revision) = initialized_native();
    retain(
        data.path(),
        "unavailable",
        &definition(&repository, &revision, "unavailable", source_step()),
    );
    let server = Server::start(data.path());
    std::fs::remove_file(data.path().join("state.sqlite")).unwrap();

    let view = response_json(
        &server.get("/v1/skill-workflows/unavailable"),
        "HTTP/1.1 200",
    );
    assert_eq!(view["run"]["state"], "succeeded");
    assert_eq!(view["run"]["admission"], "not_assessed");
    assert_eq!(view["present_applicability"]["state"], "unavailable");
    assert_eq!(
        view["present_applicability"]["reason"],
        "current native authority is unavailable"
    );
}

#[test]
fn redacts_retained_operating_system_errors_without_omitting_the_detail() {
    let (data, repository, revision) = initialized_native();
    retain(
        data.path(),
        "redacted",
        &definition(&repository, &revision, "redacted", source_step()),
    );
    Connection::open(data.path().join("skill-workflows/state.sqlite"))
        .unwrap()
        .execute(
            "UPDATE workflow_steps SET detail=?1 WHERE run_id='redacted'",
            ["could not read src/example.rs: /private/objects/secret: permission denied"],
        )
        .unwrap();

    let server = Server::start(data.path());
    let response = server.get("/v1/skill-workflows/redacted");
    assert!(!response.contains("/private/objects/secret"));
    let view = response_json(&response, "HTTP/1.1 200");
    assert_eq!(
        view["run"]["steps"][0]["detail"],
        "source path could not be read; internal error detail was redacted"
    );
}

#[test]
fn stale_policy_keeps_historical_success_separate_from_present_applicability() {
    let (data, repository, revision) = initialized_native();
    retain(
        data.path(),
        "historical",
        &definition(&repository, &revision, "historical", source_step()),
    );
    Authority::open(data.path())
        .unwrap()
        .advance_policy(0)
        .unwrap();

    let server = Server::start(data.path());
    let view = response_json(
        &server.get("/v1/skill-workflows/historical"),
        "HTTP/1.1 200",
    );
    assert_eq!(view["run"]["state"], "succeeded");
    assert_eq!(view["run"]["admission"], "not_assessed");
    assert_eq!(view["present_applicability"]["state"], "stale");
    assert_eq!(
        view["present_applicability"]["reason"],
        "policy epoch changed after this run was recorded"
    );
}

#[test]
fn absent_journal_invalid_id_and_unknown_query_are_bounded_read_failures() {
    let (data, _, _) = initialized_native();
    assert!(!data.path().join("skill-workflows").exists());
    let server = Server::start(data.path());

    let absent = response_json(&server.get("/v1/skill-workflows/missing"), "HTTP/1.1 404");
    assert_eq!(absent, json!({"error":"not found: skill workflow run"}));
    assert!(!data.path().join("skill-workflows").exists());

    let invalid = response_json(&server.get("/v1/skill-workflows/bad%20id"), "HTTP/1.1 400");
    assert!(
        invalid["error"]
            .as_str()
            .unwrap()
            .contains("canonical ASCII")
    );
    assert!(!data.path().join("skill-workflows").exists());

    let query = response_json(
        &server.get("/v1/skill-workflows/missing?limit=1"),
        "HTTP/1.1 400",
    );
    assert_eq!(
        query,
        json!({"error":"skill workflow reads accept no query fields"})
    );
    assert!(!data.path().join("skill-workflows").exists());
}
