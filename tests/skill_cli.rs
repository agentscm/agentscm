#![cfg(unix)]
//! `agentscm skill` against a temporary Git checkout and a stub Hub.

use std::io::{Read, Write};
use std::net::TcpListener;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{Value, json};
use tempfile::TempDir;

const LOCK: &str = "agentscm-skills.lock.json";

fn git(root: &Path, args: &[&str]) {
    let output = Command::new("git")
        .current_dir(root)
        .args(args)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

type Routes = Vec<(&'static str, u16, String)>;

/// Answers every request from the first route whose text occurs in the request line, else 404.
fn serve(routes: Routes) -> (String, Arc<Mutex<Vec<String>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let seen = Arc::new(Mutex::new(Vec::new()));
    let log = seen.clone();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { return };
            stream
                .set_read_timeout(Some(Duration::from_secs(3)))
                .unwrap();
            let mut request = Vec::new();
            let mut chunk = [0_u8; 4096];
            while let Ok(read) = stream.read(&mut chunk) {
                if read == 0 {
                    break;
                }
                request.extend_from_slice(&chunk[..read]);
                let Some(head) = request.windows(4).position(|w| w == b"\r\n\r\n") else {
                    continue;
                };
                let headers = String::from_utf8_lossy(&request[..head]).to_ascii_lowercase();
                let length = headers
                    .lines()
                    .find_map(|l| {
                        l.strip_prefix("content-length:")?
                            .trim()
                            .parse::<usize>()
                            .ok()
                    })
                    .unwrap_or(0);
                if request.len() >= head + 4 + length {
                    break;
                }
            }
            let text = String::from_utf8_lossy(&request).into_owned();
            let line = text.lines().next().unwrap_or("").to_owned();
            log.lock().unwrap().push(text);
            let (status, body) = routes
                .iter()
                .find(|(needle, _, _)| line.contains(needle))
                .map_or((404, r#"{"error":"not_found"}"#.to_owned()), |(_, s, b)| {
                    (*s, b.clone())
                });
            let _ = write!(
                stream,
                "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
        }
    });
    (url, seen)
}

struct Fixture {
    _temp: TempDir,
    root: PathBuf,
    config: PathBuf,
    data: PathBuf,
    home: PathBuf,
}

/// A checkout on `main` with one commit, connected to `hub`.
fn fixture(hub: &str) -> Fixture {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("repo");
    std::fs::create_dir(&root).unwrap();
    git(&root, &["init", "--quiet", "-b", "main"]);
    git(&root, &["config", "user.email", "t@example.com"]);
    git(&root, &["config", "user.name", "T"]);
    git(
        &root,
        &[
            "remote",
            "add",
            "origin",
            "https://github.com/acme/widgets.git",
        ],
    );
    std::fs::write(root.join("a.txt"), "a").unwrap();
    git(&root, &["add", "."]);
    git(&root, &["commit", "--quiet", "-m", "base"]);
    let root = root.canonicalize().unwrap();
    let config = temp.path().join("config");
    std::fs::create_dir_all(config.join("agentscm")).unwrap();
    let token = temp.path().join("client.token");
    std::fs::write(&token, "synthetic-private-token\n").unwrap();
    std::fs::set_permissions(&token, std::fs::Permissions::from_mode(0o600)).unwrap();
    let saved = format!(
        "{{\"version\":1,\"connections\":[{{\"root\":{},\"origin_host\":\"github.com\",\"hub\":{},\"repo\":\"acme/widgets\",\"token_file\":{}}}]}}",
        serde_json::to_string(&root).unwrap(),
        serde_json::to_string(hub).unwrap(),
        serde_json::to_string(&token).unwrap(),
    );
    std::fs::write(config.join("agentscm").join("connections.json"), saved).unwrap();
    let data = temp.path().join("data");
    std::fs::create_dir(&data).unwrap();
    let home = temp.path().to_path_buf();
    Fixture {
        _temp: temp,
        root,
        config,
        data,
        home,
    }
}

impl Fixture {
    fn run(&self, args: &[&str], stdin: &str) -> Output {
        let mut child = Command::new(env!("CARGO_BIN_EXE_agentscm"))
            .arg("--data-dir")
            .arg(&self.data)
            .arg("skill")
            .args(args)
            .current_dir(&self.root)
            .env_clear()
            .env("XDG_CONFIG_HOME", &self.config)
            .env("HOME", &self.home)
            .env("PATH", std::env::var_os("PATH").unwrap_or_default())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(stdin.as_bytes())
            .unwrap();
        child.wait_with_output().unwrap()
    }

    fn ok(&self, args: &[&str]) -> String {
        let out = self.run(args, "");
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8(out.stdout).unwrap()
    }

    fn stop(&self, active: bool) -> Value {
        let payload = json!({"hook_event_name": "Stop", "session_id": "s1", "cwd": self.root,
            "stop_hook_active": active})
        .to_string();
        let out = self.run(&["stop", "--agent", "claude"], &payload);
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        serde_json::from_slice(&out.stdout).unwrap()
    }

    fn feature_branch(&self) {
        git(&self.root, &["checkout", "--quiet", "-b", "feat/x"]);
        std::fs::write(self.root.join("b.txt"), "b").unwrap();
        git(&self.root, &["add", "."]);
        git(&self.root, &["commit", "--quiet", "-m", "work"]);
    }
}

fn run_json(current: &str, blocks: bool) -> String {
    let gate = |id: &str, state: &str, assurance: Value| {
        json!({"id": id, "state": state, "by": "author", "enforce": ["stop"],
            "min_assurance": "self_reported", "assurance": assurance, "evidence": []})
    };
    json!({"run_id": "r1", "repo": "acme/widgets", "branch": "feat/x", "pull_request": 41,
        "head_sha": "a".repeat(40), "state": "open",
        "skill": {"name": "pr-gate", "workflow_version": 1, "sha256": "0".repeat(64)},
        "params": {"ci_label": "ci-extended"}, "current_gate": current,
        "next_action": format!("Do the {current} step."), "blocks_finish": blocks,
        "gates": [gate("branch", "done", json!("self_reported")), gate(current, "current", Value::Null)],
        "stale_evidence": [], "updated_at": 1})
    .to_string()
}

#[test]
fn list_show_add_remove_round_trip() {
    let fx = fixture("http://127.0.0.1:9");
    let text = fx.ok(&["list"]);
    assert!(
        text.contains("pr-gate v1 [not adopted]") && text.contains("Follow when"),
        "{text}"
    );

    let out = fx.run(&["add", "pr-gate", "--param", "nope=1"], "");
    assert!(!out.status.success());
    assert!(!fx.root.join(LOCK).exists());
    assert!(
        !fx.run(&["add", "pr-gate", "--version", "9"], "")
            .status
            .success()
    );
    assert!(!fx.run(&["add", "other"], "").status.success());
    assert!(
        !fx.run(&["add", "pr-gate", "--param", "merge_method=octopus"], "")
            .status
            .success()
    );

    let printed = fx.ok(&[
        "add",
        "pr-gate",
        "--param",
        "ci_label=ci-extended",
        "--param",
        "retarget_stacked=false",
    ]);
    assert!(
        printed.contains("+ ") && printed.contains("Commit"),
        "{printed}"
    );
    let lock: Value = serde_json::from_slice(&std::fs::read(fx.root.join(LOCK)).unwrap()).unwrap();
    let pin = &lock["skills"][0];
    assert_eq!(
        pin["params"],
        json!({"ci_label": "ci-extended", "retarget_stacked": false})
    );
    assert_eq!(pin["source"]["kind"], "first_party");
    assert!(!fx.root.join(".agentscm").exists());

    let listed: Value = serde_json::from_str(&fx.ok(&["list", "--json"])).unwrap();
    assert_eq!(listed["skills"][0]["status"], "adopted");
    let shown = fx.ok(&["show", "pr-gate"]);
    assert!(
        shown.contains("[adopted]") && shown.contains("post_merge by author"),
        "{shown}"
    );

    fx.ok(&["add", "pr-gate", "--param", "ci_label=null"]);
    let lock: Value = serde_json::from_slice(&std::fs::read(fx.root.join(LOCK)).unwrap()).unwrap();
    assert_eq!(
        lock["skills"][0]["params"],
        json!({"retarget_stacked": false})
    );

    fx.ok(&["remove", "pr-gate"]);
    let lock: Value = serde_json::from_slice(&std::fs::read(fx.root.join(LOCK)).unwrap()).unwrap();
    assert_eq!(lock["skills"], json!([]));
    assert!(!fx.run(&["remove", "pr-gate"], "").status.success());
}

#[test]
fn status_labels_self_reported_gates_and_handles_no_run() {
    let (hub, seen) = serve(vec![(
        "by-branch?branch=feat%2Fx",
        200,
        run_json("review", true),
    )]);
    let fx = fixture(&hub);
    fx.feature_branch();
    let text = fx.ok(&["status"]);
    assert!(
        text.contains("branch") && text.contains("done [self-reported]"),
        "{text}"
    );
    assert!(
        text.contains("Next: Do the review step.") && text.contains("Finishing blocked: yes"),
        "{text}"
    );
    assert!(seen.lock().unwrap()[0].contains("acme%2Fwidgets/skills/runs/by-branch"));
    let json: Value = serde_json::from_str(&fx.ok(&["status", "--json"])).unwrap();
    assert_eq!(json["current_gate"], "review");
    let none = fx.ok(&["status", "--branch", "other"]);
    assert!(
        none.contains("No pr-gate run for branch other yet") && none.contains("--gate branch"),
        "{none}"
    );
}

#[test]
fn evidence_posts_self_reported_head_and_shows_refusals() {
    let (hub, seen) = serve(vec![("POST", 200, run_json("tests", true))]);
    let fx = fixture(&hub);
    fx.feature_branch();
    let out = fx.ok(&[
        "evidence",
        "--gate",
        "branch",
        "--kind",
        "task_branch",
        "--summary",
        "s",
        "--pr",
        "41",
    ]);
    assert!(out.contains("Evidence recorded as self-reported"), "{out}");
    let request = seen.lock().unwrap()[0].clone();
    let body: Value = serde_json::from_str(request.split("\r\n\r\n").nth(1).unwrap()).unwrap();
    assert_eq!(body["assurance"], "self_reported");
    assert_eq!(body["branch"], "feat/x");
    assert_eq!(body["pull_request"], 41);
    assert_eq!(body["head_sha"].as_str().unwrap().len(), 40);
    assert!(
        !fx.run(
            &[
                "evidence",
                "--gate",
                "g",
                "--kind",
                "bogus",
                "--summary",
                "s"
            ],
            ""
        )
        .status
        .success()
    );

    let (hub, _) = serve(vec![(
        "POST",
        409,
        r#"{"error":"stale_head","message":"old head"}"#.into(),
    )]);
    let fx = fixture(&hub);
    let out = fx.run(
        &[
            "evidence",
            "--gate",
            "review",
            "--kind",
            "independent_review",
            "--summary",
            "s",
        ],
        "",
    );
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(
        !out.status.success() && err.contains("stale_head") && err.contains("old head"),
        "{err}"
    );
}

#[test]
fn report_adoption_puts_the_lock_and_prints_the_source() {
    let (hub, seen) = serve(vec![("PUT", 200, r#"{"source":"default_branch"}"#.into())]);
    let fx = fixture(&hub);
    fx.ok(&["add", "pr-gate"]);
    let out = fx.ok(&["report-adoption"]);
    assert!(out.contains("default_branch"), "{out}");
    assert!(
        seen.lock().unwrap()[0].starts_with("PUT /v1/hub/repos/acme%2Fwidgets/skills/adoption")
    );
}

#[test]
fn stop_fails_open() {
    let fx = fixture("http://127.0.0.1:9");
    // No lock file.
    fx.feature_branch();
    assert_eq!(fx.stop(false), json!({}));
    // Adopted but the Hub is unreachable.
    fx.ok(&["add", "pr-gate"]);
    assert_eq!(fx.stop(false), json!({}));
    // Not a Git repository.
    let elsewhere = TempDir::new().unwrap();
    let payload =
        json!({"hook_event_name": "Stop", "cwd": elsewhere.path(), "stop_hook_active": false});
    let out = fx.run(&["stop", "--agent", "codex"], &payload.to_string());
    assert_eq!(
        serde_json::from_slice::<Value>(&out.stdout).unwrap(),
        json!({})
    );
    // Garbage input.
    let out = fx.run(&["stop", "--agent", "codex"], "not json");
    assert!(out.status.success());
    assert_eq!(
        serde_json::from_slice::<Value>(&out.stdout).unwrap(),
        json!({})
    );
}

#[test]
fn stop_blocks_on_blocks_finish_and_allows_waiting_gates() {
    let (hub, _) = serve(vec![("by-branch", 200, run_json("review", true))]);
    let fx = fixture(&hub);
    fx.feature_branch();
    fx.ok(&["add", "pr-gate"]);
    let out = fx.stop(false);
    assert_eq!(out["decision"], "block");
    assert_eq!(out["reason"], "Do the review step.");

    let (hub, _) = serve(vec![("by-branch", 200, run_json("ci", false))]);
    let fx = fixture(&hub);
    fx.feature_branch();
    fx.ok(&["add", "pr-gate"]);
    assert_eq!(fx.stop(false), json!({}));
}

#[test]
fn stop_blocks_when_commits_exist_without_a_run_but_not_on_the_default_branch() {
    let (hub, _) = serve(vec![]);
    let fx = fixture(&hub);
    fx.ok(&["add", "pr-gate"]);
    assert_eq!(fx.stop(false), json!({}), "default branch");
    git(&fx.root, &["checkout", "--quiet", "-b", "feat/empty"]);
    assert_eq!(fx.stop(false), json!({}), "no commits ahead");
    fx.feature_branch();
    let out = fx.stop(false);
    assert_eq!(out["decision"], "block");
    assert!(
        out["reason"]
            .as_str()
            .unwrap()
            .contains("--gate branch --kind task_branch")
    );
}

#[test]
fn stop_gives_up_after_eight_consecutive_blocks() {
    let (hub, _) = serve(vec![("by-branch", 200, run_json("tests", true))]);
    let fx = fixture(&hub);
    fx.feature_branch();
    fx.ok(&["add", "pr-gate"]);
    assert_eq!(fx.stop(false)["decision"], "block");
    for _ in 1..8 {
        assert_eq!(fx.stop(true)["decision"], "block");
    }
    let out = fx.stop(true);
    assert_eq!(out["continue"], false);
    assert!(out.get("decision").is_none());
}
