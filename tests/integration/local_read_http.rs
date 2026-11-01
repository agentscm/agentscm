#![cfg(unix)]

use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::thread::JoinHandle;
use std::time::Duration;

use agentscm::authority::Authority;
use agentscm::context::ReviewContext;
use agentscm::model::FileChange;
use agentscm::skill_workflow::{advance, compile, library, start};
use agentscm::store::{Engine, digest};
use agentscm::workspace::{SnapshotTrigger, Workspace};
use agentscm::workspace_timeline_service::WorkspaceTimelineService;
use serde_json::{Value, json};
use tempfile::TempDir;

const TOKEN: &str = "local-read-token-0123456789abcdef";
const ORIGIN: &str = "http://127.0.0.1:7332";
const SKILL: &str = "skills/checks/v1/SKILL.md";
const LIBRARY: &str = "skills/checks/v1/workflow.json";
const SOURCE: &str = "src/lib.rs";
const SOURCE_BYTES: &[u8] = b"pub fn answer() -> u8 { 42 }\n";
const RUN_ID: &str = "local-read-run";

fn explorer_checkout(path: &Path) -> String {
    fs::create_dir(path).unwrap();
    for args in [
        vec!["init", "-q", "--initial-branch=main"],
        vec!["config", "user.name", "Example Developer"],
        vec!["config", "user.email", "developer@example.invalid"],
        vec!["config", "commit.gpgsign", "false"],
    ] {
        assert!(
            Command::new("git")
                .arg("-C")
                .arg(path)
                .args(args)
                .status()
                .unwrap()
                .success()
        );
    }
    fs::write(path.join("source.txt"), "first source\n").unwrap();
    assert!(
        Command::new("git")
            .arg("-C")
            .arg(path)
            .args(["add", "source.txt"])
            .status()
            .unwrap()
            .success()
    );
    assert!(
        Command::new("git")
            .arg("-C")
            .arg(path)
            .args([
                "-c",
                "core.hooksPath=/dev/null",
                "commit",
                "-qm",
                "Initial source"
            ])
            .status()
            .unwrap()
            .success()
    );
    String::from_utf8(
        Command::new("git")
            .arg("-C")
            .arg(path)
            .args(["rev-parse", "HEAD"])
            .output()
            .unwrap()
            .stdout,
    )
    .unwrap()
    .trim()
    .into()
}

#[test]
fn explorer_requires_explicit_credentials_and_loopback_even_with_public() {
    let root = TempDir::new().unwrap();
    for (bind, token, public) in [
        ("127.0.0.1:0", None, false),
        ("0.0.0.0:0", Some(TOKEN), true),
    ] {
        let mut command = Command::new(env!("CARGO_BIN_EXE_agentscm"));
        command
            .args(["--no-telemetry", "--data-dir"])
            .arg(root.path().join("data"))
            .args(["serve", "--bind", bind, "--explorer-repo"])
            .arg(root.path())
            .args(["--explorer-repository", "owner/example"])
            .env_remove("AGENTSCM_SERVE_READ_TOKEN");
        if let Some(token) = token {
            command.env("AGENTSCM_SERVE_READ_TOKEN", token);
        }
        if public {
            command.arg("--public");
        }
        let output = command.output().unwrap();
        assert!(!output.status.success());
        let error = String::from_utf8_lossy(&output.stderr);
        assert!(
            error.contains("requires a loopback bind address"),
            "{error}"
        );
        assert!(!root.path().join("data").exists());
    }
}

#[test]
fn explorer_http_pins_source_and_enforces_private_read_boundary() {
    let root = TempDir::new().unwrap();
    let repo = root.path().join("repo");
    let head = explorer_checkout(&repo);
    let server = Server::start(
        &root.path().join("data"),
        Some(TOKEN),
        &[
            "--explorer-repo",
            repo.to_str().unwrap(),
            "--explorer-repository",
            "owner/example",
            "--browser-origin",
            ORIGIN,
        ],
    );
    let summary = "/v1/local/explorer/summary";
    assert_eq!(server.request("GET", summary, &[]).0, 401);
    let auth = format!("Bearer {TOKEN}");
    let headers = [("Authorization", auth.as_str())];
    assert_eq!(
        server
            .request_to("untrusted.invalid", "GET", summary, &headers)
            .0,
        421
    );
    assert_eq!(
        server
            .request(
                "GET",
                summary,
                &[
                    ("Authorization", &auth),
                    ("Origin", "https://untrusted.invalid")
                ]
            )
            .0,
        403
    );
    assert_eq!(
        server.request("OPTIONS", summary, &[("Origin", ORIGIN)]).0,
        204
    );
    assert_eq!(server.request("POST", summary, &headers).0, 405);
    let (status, response) = server.get(summary);
    assert_eq!(status, 200, "{response}");
    assert_eq!(response["commit"], head);
    assert_eq!(response["repository_id"], "owner/example");
    assert!(response["dirty"].is_null());
    fs::write(repo.join("source.txt"), "uncommitted replacement\n").unwrap();
    let (status, response) = server.get(&format!(
        "/v1/local/explorer/file?commit={head}&path=source.txt"
    ));
    assert_eq!(status, 200, "{response}");
    assert_eq!(response["content"], "first source\n");
    assert!(server.get(summary).1["dirty"].is_null());
    fs::write(repo.join("untracked.txt"), "local-only source\n").unwrap();
    assert_eq!(server.get(summary).1["dirty"], true);
    assert_eq!(
        server.get("/v1/local/explorer/summary?ref=HEAD&ref=main").0,
        400
    );
    assert_eq!(server.get("/v1/local/explorer/summary?root=/etc").0, 400);
    assert_eq!(
        server
            .get("/v1/local/explorer/file?commit=HEAD&path=source.txt")
            .0,
        400
    );
    assert_eq!(
        server
            .get(&format!(
                "/v1/local/explorer/file?commit={head}&path=..%2Foutside"
            ))
            .0,
        400
    );
    let (status, response) = server.get(&format!("/v1/local/explorer/evidence?commit={head}"));
    assert_eq!(status, 404);
    assert!(
        response["error"]
            .as_str()
            .unwrap()
            .contains("--explorer-spool")
    );
    assert!(!response.to_string().contains(root.path().to_str().unwrap()));
    assert_eq!(
        fs::read_to_string(repo.join("source.txt")).unwrap(),
        "uncommitted replacement\n"
    );
}

#[test]
fn explorer_conversation_reads_only_retained_visible_capture_after_authorization() {
    use agentscm::change_capture::claude_code::{CaptureRequest, import_jsonl};
    use agentscm::change_capture::{LocalSpool, SpoolNamespaceV1};
    use agentscm::change_record::CaptureModeV1;

    let root = TempDir::new().unwrap();
    let repo = root.path().join("repo");
    explorer_checkout(&repo);
    let spool_path = root.path().join("private-spool");
    let spool = LocalSpool::open(&spool_path).unwrap();
    spool
        .bind_namespace(SpoolNamespaceV1 {
            version: 1,
            hub: "https://hub.example.invalid".into(),
            repository: "owner/example".into(),
            worktree_id: "synthetic-checkout".into(),
        })
        .unwrap();
    let request = CaptureRequest {
        operation_prefix: "http-visible-capture".into(),
        work_id: "wrk_example".into(),
        generation: 1,
        observed_session_id: "session_example".into(),
        canonical_session_id: "session_example".into(),
        parent_session_id: None,
        capture_mode: CaptureModeV1::LocalConversation,
        live_capture_mode: CaptureModeV1::LocalConversation,
    };
    let transcript = concat!(
        "{\"type\":\"user\",\"uuid\":\"u1\",\"sessionId\":\"session_example\",\"timestamp\":\"2026-01-01T00:00:00Z\",\"message\":{\"content\":\"Review the fixture source\"}}\n",
        "{\"type\":\"assistant\",\"uuid\":\"a1\",\"sessionId\":\"session_example\",\"timestamp\":\"2026-01-01T00:00:01Z\",\"message\":{\"content\":[{\"type\":\"thinking\",\"thinking\":\"hidden-fixture-marker\"},{\"type\":\"text\",\"text\":\"Visible fixture response\"}]}}\n"
    );
    import_jsonl(std::io::Cursor::new(transcript), &request, &spool).unwrap();
    let server = Server::start(
        &root.path().join("data"),
        Some(TOKEN),
        &[
            "--explorer-repo",
            repo.to_str().unwrap(),
            "--explorer-repository",
            "owner/example",
            "--explorer-spool",
            spool_path.to_str().unwrap(),
        ],
    );
    let path = "/v1/local/explorer/conversation?work_id=wrk_example&session_id=session_example";
    let (status, _, denied) = server.request("GET", path, &[]);
    assert_eq!(status, 401);
    assert!(!denied.contains("Visible fixture response"));
    let (status, response) = server.get(path);
    assert_eq!(status, 200, "{response}");
    let response = response.to_string();
    assert!(response.contains("Review the fixture source"));
    assert!(response.contains("Visible fixture response"));
    assert!(!response.contains("hidden-fixture-marker"));
    assert!(!response.contains(spool_path.to_str().unwrap()));
    let (status, response) = server.get(&format!("{path}&artifact=other"));
    assert_eq!(status, 400);
    assert!(!response.to_string().contains("Visible fixture response"));
}

struct Server {
    child: Child,
    address: SocketAddr,
    stderr: Option<JoinHandle<()>>,
}

impl Server {
    fn start(data: &Path, token: Option<&str>, extra: &[&str]) -> Self {
        Self::start_with_data_home(data, token, extra, None)
    }

    fn start_with_data_home(
        data: &Path,
        token: Option<&str>,
        extra: &[&str],
        home: Option<&Path>,
    ) -> Self {
        let mut command = Command::new(env!("CARGO_BIN_EXE_agentscm"));
        command
            .arg("--no-telemetry")
            .arg("--data-dir")
            .arg(data)
            .args(["serve", "--bind", "127.0.0.1:0", "--max-concurrency", "2"])
            .args(extra)
            .env_remove("AGENTSCM_SERVE_READ_TOKEN")
            .stdout(Stdio::null())
            .stderr(Stdio::piped());
        if let Some(token) = token {
            command.env("AGENTSCM_SERVE_READ_TOKEN", token);
        }
        if let Some(home) = home {
            command.env("XDG_DATA_HOME", home);
        }
        let mut child = command.spawn().unwrap();
        let stderr = child.stderr.take().unwrap();
        let (tx, rx) = std::sync::mpsc::sync_channel(1);
        let reader = std::thread::spawn(move || {
            for line in BufReader::new(stderr).lines() {
                let Ok(line) = line else { break };
                if line.starts_with("listening on ") {
                    let _ = tx.try_send(line);
                } else {
                    eprintln!("serve: {line}");
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

    fn request(&self, method: &str, path: &str, headers: &[(&str, &str)]) -> (u16, String, String) {
        self.request_to("localhost", method, path, headers)
    }

    fn request_to(
        &self,
        host: &str,
        method: &str,
        path: &str,
        headers: &[(&str, &str)],
    ) -> (u16, String, String) {
        let mut stream = TcpStream::connect(self.address).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(10)))
            .unwrap();
        let mut request =
            format!("{method} {path} HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\n");
        for (name, value) in headers {
            request.push_str(&format!("{name}: {value}\r\n"));
        }
        request.push_str("Content-Length: 0\r\n\r\n");
        stream.write_all(request.as_bytes()).unwrap();
        let mut response = String::new();
        stream
            .take(5 * 1024 * 1024)
            .read_to_string(&mut response)
            .unwrap();
        let (head, body) = response.split_once("\r\n\r\n").unwrap();
        let status = head[9..12].parse().unwrap();
        (status, head.to_ascii_lowercase(), body.to_owned())
    }

    fn get(&self, path: &str) -> (u16, Value) {
        let (status, head, body) = self.request(
            "GET",
            path,
            &[("Authorization", &format!("Bearer {TOKEN}"))],
        );
        assert!(head.contains("cache-control: no-store"), "{head}");
        assert!(head.contains("x-content-type-options: nosniff"), "{head}");
        (status, serde_json::from_str(&body).unwrap())
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

fn put(path: &str, bytes: &[u8]) -> FileChange {
    FileChange {
        path: path.into(),
        content: Some(bytes.into()),
    }
}

/// A native store with one Change, a template, a retained run and three snapshots.
fn seeded() -> (TempDir, ReviewContext) {
    let temp = TempDir::new().unwrap();
    let data = temp.path().join("data");
    let root = temp.path().join("work");
    let manifest = json!({
        "version": 1, "name": "verify", "workflow_version": 1,
        "skill": {"entrypoint": SKILL, "resources": []},
        "steps": [{"id": "source", "needs": [], "required": true, "action": {
            "kind": "source_paths",
            "expectations": [{"path": SOURCE, "sha256": digest(SOURCE_BYTES)}]
        }}]
    });
    let engine = Engine::open(&data).unwrap();
    let revision = engine
        .checkpoint(
            None,
            &[
                put(SKILL, b"# Checks\n"),
                put(SOURCE, SOURCE_BYTES),
                put(LIBRARY, &serde_json::to_vec(&manifest).unwrap()),
            ],
        )
        .unwrap();
    let mut authority = Authority::open(&data).unwrap();
    let repository = authority.repository_id().unwrap();
    let context: ReviewContext = serde_json::from_value(json!({
        "version":1,"change":{"repository":repository,"id":"demo"},"change_version":1,
        "revision":revision,"base":null,"target":"main","policy_epoch":0,
        "intent":"Check source","acceptance_criteria":["Source matches"],
        "scope":["src"],"exclusions":[],"omissions":[],"dependencies":[],
        "recipe":{"state":"digest","sha256":"a".repeat(64)},
        "toolchain":{"state":"digest","sha256":"b".repeat(64)},
        "environment":{"state":"digest","sha256":"c".repeat(64)},
        "required_evidence":[],"required_capabilities":["agentscm.review-context/v1"]
    }))
    .unwrap();
    authority.start_change(&context).unwrap();
    drop(authority);

    let definition = library::load(&data, &revision, LIBRARY)
        .unwrap()
        .instantiate(&context)
        .unwrap();
    start(&data, RUN_ID, &compile(&data, &definition).unwrap()).unwrap();
    advance(&data, RUN_ID, 32).unwrap();

    Workspace::create(&engine, &root, context.change.clone(), &revision).unwrap();
    drop(engine);
    let service = WorkspaceTimelineService::new(&data, &root, None).unwrap();
    for (index, path) in ["a.txt", "b.txt", "c.txt"].iter().enumerate() {
        fs::write(root.join(path), format!("file {index}\n")).unwrap();
        service.capture(SnapshotTrigger::Manual).unwrap();
    }
    (temp, context)
}

fn timeline_flag(temp: &TempDir) -> String {
    temp.path().join("work").to_str().unwrap().to_owned()
}

#[test]
fn local_reads_match_the_cli_views_and_page_to_completion() {
    let (temp, context) = seeded();
    let data = temp.path().join("data");
    let root = timeline_flag(&temp);
    let server = Server::start(&data, Some(TOKEN), &["--timeline-workspace", &root]);
    let service = WorkspaceTimelineService::new(&data, Path::new(&root), None).unwrap();

    let (status, first) = server.get("/v1/local/workspace/timeline?limit=2");
    assert_eq!(status, 200);
    assert_eq!(first, service.timeline(2, None).unwrap());
    let ids: Vec<_> = first["timeline"]["entries"]
        .as_array()
        .unwrap()
        .iter()
        .map(|entry| entry["id"].as_u64().unwrap())
        .collect();
    assert_eq!(ids, [3, 2]);
    assert_eq!(first["timeline"]["next_before"], 2);
    let (_, rest) = server.get("/v1/local/workspace/timeline?limit=2&before=2");
    assert_eq!(rest["timeline"]["entries"][0]["id"], 1);
    assert!(rest["timeline"].get("next_before").is_none());
    assert_eq!(
        rest["timeline"]["coverage"]["includes_pre_feature_history"],
        false
    );
    let (_, default_page) = server.get("/v1/local/workspace/timeline");
    assert_eq!(default_page, service.timeline(25, None).unwrap());

    let (status, entry) = server.get("/v1/local/workspace/timeline/2");
    assert_eq!(status, 200);
    assert_eq!(entry, service.entry(2).unwrap());
    assert_eq!(entry["evidence"]["state"], "not_run");

    let mut paths = Vec::new();
    let mut after: Option<String> = None;
    for _ in 0..10 {
        let query = match &after {
            Some(cursor) => format!("left=1&right=3&limit=1&after={cursor}"),
            None => "left=1&right=3&limit=1".into(),
        };
        let (status, page) = server.get(&format!("/v1/local/workspace/diff?{query}"));
        assert_eq!(status, 200);
        assert_eq!(page, service.diff(1, 3, 1, after.clone()).unwrap());
        assert_eq!(page["diff"]["needed_objects"], json!([]));
        for entry in page["diff"]["entries"].as_array().unwrap() {
            paths.push(entry["path"].as_str().unwrap().to_owned());
        }
        match page["diff"]["next"].as_str() {
            Some(cursor) => after = Some(cursor.to_owned()),
            None => break,
        }
    }
    // Diff order follows the source traversal, not path order.
    paths.sort();
    assert_eq!(paths, ["b.txt", "c.txt"]);

    let (status, inspect) = server.get(&format!(
        "/v1/local/skill-workflow-library?change=demo&library={}",
        LIBRARY.replace('/', "%2F")
    ));
    assert_eq!(status, 200);
    let snapshot = library::load(&data, &context.revision, LIBRARY).unwrap();
    assert_eq!(
        inspect["reference"],
        serde_json::to_value(snapshot.reference()).unwrap()
    );
    assert_eq!(
        inspect["definition"],
        serde_json::to_value(snapshot.instantiate(&context).unwrap()).unwrap()
    );
    assert_eq!(inspect["admission"], "not_assessed");

    let (status, handoff) = server.get(&format!("/v1/local/skill-workflows/{RUN_ID}/handoff"));
    assert_eq!(status, 200);
    assert_eq!(handoff["run"]["run_id"], RUN_ID);
    assert_eq!(handoff["run"]["state"], "succeeded");
    assert_eq!(handoff["subject_scope"], "source_revision_only");
    assert_eq!(handoff["admission"], "not_assessed");

    // Reads record nothing: the timeline is unchanged and the workspace is untouched.
    assert_eq!(server.get("/v1/local/workspace/timeline").1, default_page);
}

#[test]
fn local_reads_enforce_bounds_and_not_found() {
    let (temp, _) = seeded();
    let data = temp.path().join("data");
    let server = Server::start(
        &data,
        Some(TOKEN),
        &["--timeline-workspace", &timeline_flag(&temp)],
    );
    for path in [
        "/v1/local/workspace/timeline?limit=0",
        "/v1/local/workspace/timeline?limit=101",
        "/v1/local/workspace/timeline?limit=-1",
        "/v1/local/workspace/timeline?before=0",
        "/v1/local/workspace/timeline?limit=1&limit=2",
        "/v1/local/workspace/timeline?path=/etc",
        "/v1/local/workspace/timeline/x",
        "/v1/local/workspace/timeline/1?limit=1",
        "/v1/local/workspace/diff?left=1",
        "/v1/local/workspace/diff?left=1&right=2&limit=101",
        "/v1/local/workspace/diff?left=1&right=2&limit=0",
        "/v1/local/skill-workflow-library?change=demo",
        "/v1/local/skill-workflow-library?change=demo&library=../x&data_dir=/tmp",
        "/v1/local/skill-workflows/local-read-run/handoff?x=1",
    ] {
        let (status, body) = server.get(path);
        assert_eq!(status, 400, "{path}: {body}");
        assert!(body["error"].is_string(), "{path}");
    }
    for path in [
        "/v1/local/workspace/timeline/99",
        "/v1/local/workspace/diff?left=1&right=99",
        "/v1/local/skill-workflow-library?change=missing&library=skills%2Fchecks%2Fv1%2Fworkflow.json",
        "/v1/local/skill-workflows/missing-run/handoff",
    ] {
        assert_eq!(server.get(path).0, 404, "{path}");
    }
}

#[test]
fn local_pickers_page_search_and_reject_bad_input() {
    let (temp, _) = seeded();
    let data = temp.path().join("data");
    let server = Server::start(&data, Some(TOKEN), &[]);

    let (status, body) = server.get("/v1/local/changes");
    assert_eq!(status, 200, "{body}");
    assert_eq!(
        body,
        json!({"changes": [{"id": "demo", "version": 1}], "next_after": null})
    );
    let (_, body) = server.get("/v1/local/changes?limit=1");
    assert_eq!(
        body["next_after"],
        Value::Null,
        "an exact page is complete: {body}"
    );
    let (_, body) = server.get("/v1/local/changes?after=demo");
    assert_eq!(body["changes"], json!([]));

    let (status, body) = server.get("/v1/local/skill-workflows?q=LOCAL-READ");
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["runs"][0]["run_id"], RUN_ID);
    assert_eq!(body["next_cursor"], Value::Null);
    let (_, body) = server.get("/v1/local/skill-workflows?q=%25");
    assert_eq!(
        body["runs"],
        json!([]),
        "LIKE wildcards are literal: {body}"
    );
    let (_, body) = server.get("/v1/local/skill-workflows");
    assert_eq!(body["runs"].as_array().unwrap().len(), 1);

    let long = "a".repeat(201);
    for path in [
        "/v1/local/changes?limit=0",
        "/v1/local/changes?limit=101",
        "/v1/local/changes?q=x",
        "/v1/local/skill-workflows?before=0",
        "/v1/local/skill-workflows?q=a%01b",
        &format!("/v1/local/skill-workflows?q={long}"),
        "/v1/local/skill-workflows?x=1",
    ] {
        assert_eq!(server.get(path).0, 400, "{path}");
    }
    let (status, ..) = server.request("GET", "/v1/local/changes", &[]);
    assert_eq!(status, 401);
}

#[test]
fn local_reads_require_the_token_refuse_writes_and_allow_only_configured_origins() {
    let (temp, _) = seeded();
    let data = temp.path().join("data");
    let server = Server::start(
        &data,
        Some(TOKEN),
        &[
            "--timeline-workspace",
            &timeline_flag(&temp),
            "--browser-origin",
            ORIGIN,
        ],
    );
    let path = "/v1/local/workspace/timeline";
    for headers in [
        vec![],
        vec![("Authorization", "Bearer wrong")],
        vec![("Authorization", TOKEN)],
    ] {
        let (status, head, body) = server.request("GET", path, &headers);
        assert_eq!(status, 401, "{headers:?}");
        assert!(head.contains("www-authenticate: bearer"));
        assert!(body.contains("AGENTSCM_SERVE_READ_TOKEN"));
        assert!(!body.contains(TOKEN));
    }
    let bearer = format!("Bearer {TOKEN}");
    for method in ["POST", "PUT", "PATCH", "DELETE"] {
        let (status, _, _) = server.request(method, path, &[("Authorization", &bearer)]);
        assert_eq!(status, 405, "{method}");
    }
    let (status, _, _) = server.request("POST", path, &[]);
    assert_eq!(status, 401);

    let (status, head, _) = server.request(
        "OPTIONS",
        path,
        &[
            ("Origin", ORIGIN),
            ("Access-Control-Request-Method", "GET"),
            ("Access-Control-Request-Headers", "authorization"),
        ],
    );
    assert_eq!(status, 204);
    assert!(head.contains(&format!("access-control-allow-origin: {ORIGIN}")));
    assert!(head.contains("access-control-allow-methods: get"));
    assert!(head.contains("access-control-allow-headers: authorization"));
    assert!(!head.contains("access-control-allow-credentials"));

    let (status, head, _) = server.request(
        "GET",
        path,
        &[("Origin", ORIGIN), ("Authorization", &bearer)],
    );
    assert_eq!(status, 200);
    assert!(head.contains(&format!("access-control-allow-origin: {ORIGIN}")));

    for origin in ["http://evil.example", "http://127.0.0.1:7333", "null"] {
        let (status, head, _) = server.request("OPTIONS", path, &[("Origin", origin)]);
        assert_eq!(status, 403, "{origin}");
        assert!(!head.contains("access-control-allow-origin"));
        let (status, head, _) = server.request(
            "GET",
            path,
            &[("Origin", origin), ("Authorization", &bearer)],
        );
        assert_eq!(status, 200);
        assert!(!head.contains("access-control-allow-origin"), "{origin}");
    }
    // The existing routes stay as they were: no CORS and no token.
    let (status, head, _) = server.request("GET", "/health", &[("Origin", ORIGIN)]);
    assert_eq!(status, 200);
    assert!(!head.contains("access-control-allow-origin"));
}

#[test]
fn local_reads_without_a_token_or_timeline_flag() {
    let (temp, _) = seeded();
    let server = Server::start(&temp.path().join("data"), None, &[]);
    let (status, _, body) = server.request("GET", "/v1/local/workspace/timeline", &[]);
    assert_eq!(status, 404);
    assert!(body.contains("--timeline-workspace"));
    let (status, _, _) = server.request(
        "GET",
        &format!("/v1/local/skill-workflows/{RUN_ID}/handoff"),
        &[],
    );
    assert_eq!(status, 200);
    let (status, head, _) = server.request(
        "OPTIONS",
        "/v1/local/workspace/timeline",
        &[("Origin", ORIGIN)],
    );
    assert_eq!(status, 403);
    assert!(!head.contains("access-control-allow-origin"));

    // A page whose DNS name was rebound to loopback sends its own Host.
    let handoff = format!("/v1/local/skill-workflows/{RUN_ID}/handoff");
    for host in [
        "127.0.0.1:7331",
        "localhost",
        "[::1]:7331",
        "localhost:5184",
    ] {
        assert_eq!(
            server.request_to(host, "GET", &handoff, &[]).0,
            200,
            "{host}"
        );
    }
    for host in [
        "evil.example",
        "evil.example:7331",
        "127.0.0.1.evil.example",
        "[::1].x",
    ] {
        let (status, _, body) = server.request_to(host, "GET", &handoff, &[]);
        assert_eq!(status, 421, "{host}");
        assert!(body.contains("loopback Host"));
    }
}

#[test]
fn serve_refuses_unsafe_local_read_configuration() {
    let (temp, _) = seeded();
    let data = temp.path().join("data");
    let missing = temp.path().join("missing");
    for (token, args) in [
        (None, vec!["--browser-origin", ORIGIN]),
        (Some("short"), vec![]),
        (Some(TOKEN), vec!["--browser-origin", "*"]),
        (
            Some(TOKEN),
            vec!["--browser-origin", "http://127.0.0.1:7332/ui"],
        ),
        (Some(TOKEN), vec!["--browser-origin", "file://x"]),
        (
            Some(TOKEN),
            vec!["--timeline-workspace", missing.to_str().unwrap()],
        ),
        (
            Some(TOKEN),
            vec!["--timeline-workspace", data.to_str().unwrap()],
        ),
    ] {
        let mut command = Command::new(env!("CARGO_BIN_EXE_agentscm"));
        command
            .arg("--no-telemetry")
            .arg("--data-dir")
            .arg(&data)
            .args(["serve", "--bind", "127.0.0.1:0"])
            .args(&args)
            .env_remove("AGENTSCM_SERVE_READ_TOKEN")
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        if let Some(token) = token {
            command.env("AGENTSCM_SERVE_READ_TOKEN", token);
        }
        let status = command.status().unwrap();
        assert!(!status.success(), "{token:?} {args:?}");
    }
}

#[test]
fn walkthrough_cli_and_authenticated_explorer_share_immutable_local_records() {
    let temp = TempDir::new().unwrap();
    let root = fs::canonicalize(temp.path()).unwrap();
    let repo = root.join("repo");
    let head = explorer_checkout(&repo);
    let home = root.join("operator-data");
    let server = Server::start_with_data_home(
        &root.join("authority"),
        Some(TOKEN),
        &[
            "--explorer-repo",
            repo.to_str().unwrap(),
            "--explorer-repository",
            "owner/example",
            "--browser-origin",
            ORIGIN,
        ],
        Some(&home),
    );
    let list = "/v1/local/explorer/walkthroughs?limit=100";
    assert_eq!(server.request("GET", list, &[]).0, 401);
    assert_eq!(server.get(list).0, 200);
    assert!(
        !home.exists(),
        "HTTP reads must not create a walkthrough store"
    );
    let draft = root.join("draft.json");
    fs::write(&draft,serde_json::to_vec(&json!({"title":"Inspect source", "author":"Synthetic author", "purpose":"Review exact committed source", "steps":[{"id":"read","title":"Read source","body":"An authored explanation <script> is data", "anchors":[{"id":"source","side":"head","path":"source.txt","start_line":1,"end_line":1}]}]})).unwrap()).unwrap();
    let cli = |args: &[&str]| {
        let output = Command::new(env!("CARGO_BIN_EXE_agentscm"))
            .args(["--no-telemetry", "walkthrough"])
            .args(args)
            .args([
                "--root",
                repo.to_str().unwrap(),
                "--repository",
                "owner/example",
            ])
            .env("XDG_DATA_HOME", &home)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        serde_json::from_slice::<Value>(&output.stdout).unwrap()
    };
    let created = cli(&[
        "create",
        "--change-id",
        "case",
        "--base",
        &head,
        "--head",
        &head,
        "--draft",
        draft.to_str().unwrap(),
    ]);
    let id = created["id"].as_str().unwrap();
    assert_eq!(cli(&["list"])["walkthroughs"][0]["id"], id);
    let detail_path = format!("/v1/local/explorer/walkthrough?id={id}&current_head={head}");
    let (status, detail) = server.get(&detail_path);
    assert_eq!(status, 200, "{detail}");
    assert_eq!(detail["record"], created["record"]);
    assert_eq!(detail["applicability"], "same_source");
    assert_eq!(detail["evidence"]["state"], "not_connected");
    assert_eq!(cli(&["show", "--id", id])["record"], detail["record"]);
    let context_path = format!("/v1/local/explorer/walkthrough-context?id={id}");
    let (status, context) = server.get(&context_path);
    assert_eq!(status, 200, "{context}");
    assert_eq!(context, cli(&["context", "--id", id]));
    assert!(serde_json::to_vec(&context).unwrap().len() <= 32 * 1024);
    let source_path = format!("/v1/local/explorer/walkthrough-source?id={id}&anchor=source");
    let (status, source) = server.get(&source_path);
    assert_eq!(status, 200, "{source}");
    assert_eq!(source["content"], "first source\n");
    assert_eq!(source, cli(&["source", "--id", id, "--anchor", "source"]));
    assert_eq!(server.get(list).1["walkthroughs"][0]["id"], id);
    assert_eq!(server.get(&format!("{detail_path}&id={id}")).0, 400);
    assert_eq!(server.get(&format!("{detail_path}&store=/etc")).0, 400);
    assert_eq!(server.get(&format!("{source_path}&root=/etc")).0, 400);
    let auth = format!("Bearer {TOKEN}");
    assert_eq!(
        server
            .request("POST", &detail_path, &[("Authorization", &auth)])
            .0,
        405
    );
    assert_eq!(
        server
            .request(
                "GET",
                &detail_path,
                &[
                    ("Authorization", &auth),
                    ("Origin", "https://untrusted.invalid")
                ]
            )
            .0,
        403
    );
    for path in [&detail_path, &context_path, &source_path] {
        let (_, _, body) = server.request("GET", path, &[]);
        assert!(!body.contains("An authored explanation"));
        assert!(!body.contains("first source"));
    }
    // A second namespace in the same checkout cannot read the first namespace's store.
    let wrong = Server::start_with_data_home(
        &root.join("other-authority"),
        Some(TOKEN),
        &[
            "--explorer-repo",
            repo.to_str().unwrap(),
            "--explorer-repository",
            "other/project",
        ],
        Some(&home),
    );
    assert_eq!(wrong.get(&detail_path).0, 404);
    assert_eq!(wrong.get(list).1["walkthroughs"], json!([]));
    assert!(
        !serde_json::to_string(&detail)
            .unwrap()
            .contains(root.to_str().unwrap())
    );
    // Supplied boundary drafts must remain authored content; CLI adds no 33rd question.
    fs::write(&draft,serde_json::to_vec(&json!({"title":"Boundary", "open_questions":(0..32).map(|n|format!("Question {n}")).collect::<Vec<_>>()})).unwrap()).unwrap();
    let boundary = cli(&[
        "create",
        "--change-id",
        "boundary",
        "--base",
        &head,
        "--head",
        &head,
        "--draft",
        draft.to_str().unwrap(),
    ]);
    assert_eq!(
        boundary["record"]["draft"]["open_questions"]
            .as_array()
            .unwrap()
            .len(),
        32
    );
    // WAL is unsupported: HTTP rejects it before SQLite can create read bookkeeping.
    let data = home.join("agentscm-walkthroughs");
    let store_dir = fs::read_dir(&data)
        .unwrap()
        .map(|e| e.unwrap().path())
        .find(|p| {
            let connection = rusqlite::Connection::open(p.join("store.sqlite")).unwrap();
            connection
                .query_row("SELECT repository FROM metadata", [], |r| {
                    r.get::<_, String>(0)
                })
                .unwrap()
                == "owner/example"
        })
        .unwrap();
    let connection = rusqlite::Connection::open(store_dir.join("store.sqlite")).unwrap();
    connection
        .pragma_update(None, "journal_mode", "WAL")
        .unwrap();
    connection.close().unwrap();
    let inventory = || {
        fs::read_dir(&store_dir)
            .unwrap()
            .map(|e| {
                let e = e.unwrap();
                (e.file_name(), fs::read(e.path()).unwrap())
            })
            .collect::<std::collections::BTreeMap<_, _>>()
    };
    let before = inventory();
    assert_eq!(server.get(list).0, 400);
    assert_eq!(before, inventory());
}

#[test]
fn walkthrough_map_cli_is_bounded_stateless_and_requires_full_source_identities() {
    let temporary = TempDir::new().unwrap();
    let root = temporary.path().join("repo");
    let head = explorer_checkout(&root);
    let home = temporary.path().join("home");
    let data = temporary.path().join("data");
    let native = temporary.path().join("native");
    let invoke = |base: &str, current: Option<&str>| {
        let mut command = Command::new(env!("CARGO_BIN_EXE_agentscm"));
        command
            .args(["--no-telemetry", "--data-dir"])
            .arg(&native)
            .args(["walkthrough", "map", "--root"])
            .arg(&root)
            .args([
                "--repository",
                "synthetic/project",
                "--base",
                base,
                "--head",
                &head,
            ])
            .env("HOME", &home)
            .env("XDG_DATA_HOME", &data)
            .env_remove("AGENTSCM_VERIFIED_INSTALL");
        if let Some(current) = current {
            command.args(["--current-head", current]);
        }
        command.output().unwrap()
    };
    for current in [None, Some(head.as_str())] {
        let output = invoke(&head, current);
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(output.stdout.len() <= 8192);
        assert_eq!(output.stdout.last(), Some(&b'\n'));
        let packet: Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(
            packet["applicability"],
            if current.is_some() {
                "same_source"
            } else {
                "unknown"
            }
        );
        assert_eq!(packet["coverage"]["discovered_manifest_complete"], true);
        assert_eq!(packet["evidence"]["state"], "not_loaded");
        assert_eq!(packet["evidence"]["verification"], "unassessed");
        assert!(!home.exists());
        assert!(!data.exists());
        assert!(!native.exists());
        assert!(!root.join(".agentscm").exists());
    }
    let output = invoke("HEAD", None);
    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("full lowercase Git object ID required")
    );
    let output = invoke(&head, Some("HEAD"));
    assert!(!output.status.success());
    assert!(!home.exists());
    assert!(!data.exists());
    assert!(!native.exists());
}
