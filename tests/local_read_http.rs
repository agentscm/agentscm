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

struct Server {
    child: Child,
    address: SocketAddr,
    stderr: Option<JoinHandle<()>>,
}

impl Server {
    fn start(data: &Path, token: Option<&str>, extra: &[&str]) -> Self {
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
