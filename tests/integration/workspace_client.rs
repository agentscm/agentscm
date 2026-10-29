#![cfg(unix)]

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use serde_json::{Value, json};
use tempfile::TempDir;

struct Fixture {
    _temp: TempDir,
    data: PathBuf,
    workspace: PathBuf,
    forks: PathBuf,
    config: PathBuf,
}

fn command(data: &Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_agentscm"));
    command.arg("--no-telemetry").arg("--data-dir").arg(data);
    command
}

fn cli(data: &Path, args: &[&str]) -> Value {
    let output = command(data).args(args).output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap()
}

fn context(repository: &str, revision: &str) -> Value {
    json!({
        "version":1,"change":{"repository":repository,"id":"work"},"change_version":1,
        "revision":revision,"base":null,"target":"main","policy_epoch":0,
        "intent":"Test workspace capture","acceptance_criteria":["Retain exact bytes"],
        "scope":["a.txt"],"exclusions":[],"omissions":[],"dependencies":[],
        "recipe":{"state":"digest","sha256":"a".repeat(64)},
        "toolchain":{"state":"digest","sha256":"b".repeat(64)},
        "environment":{"state":"digest","sha256":"c".repeat(64)},
        "required_evidence":[],"required_capabilities":["agentscm.review-context/v1"]
    })
}

fn fixture() -> Fixture {
    let temp = tempfile::tempdir().unwrap();
    let data = temp.path().join("data");
    let workspace = temp.path().join("workspace");
    let forks = temp.path().join("forks");
    let config = temp.path().join("config");
    fs::create_dir(&forks).unwrap();
    fs::create_dir(&config).unwrap();
    let repository = cli(&data, &["init"])["repository"]
        .as_str()
        .unwrap()
        .to_owned();
    let checkpoint = temp.path().join("checkpoint.json");
    fs::write(
        &checkpoint,
        r#"{"parent":null,"changes":[{"path":"a.txt","content":[111,108,100]}]}"#,
    )
    .unwrap();
    let revision = cli(&data, &["checkpoint", checkpoint.to_str().unwrap()])["revision"]
        .as_str()
        .unwrap()
        .to_owned();
    let context_path = temp.path().join("context.json");
    fs::write(
        &context_path,
        serde_json::to_vec(&context(&repository, &revision)).unwrap(),
    )
    .unwrap();
    cli(&data, &["start", context_path.to_str().unwrap()]);
    cli(
        &data,
        &[
            "workspace",
            "create",
            workspace.to_str().unwrap(),
            "work",
            &revision,
        ],
    );
    Fixture {
        _temp: temp,
        data,
        workspace,
        forks,
        config,
    }
}

fn rpc(fixture: &Fixture, lines: &[Value], fork: bool) -> Vec<Value> {
    let mut command = command(&fixture.data);
    command
        .args(["workspace-client", "mcp", "--workspace"])
        .arg(&fixture.workspace);
    if fork {
        command.args(["--fork-root"]).arg(&fixture.forks);
    }
    let mut child = command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    {
        let input = child.stdin.as_mut().unwrap();
        for line in lines {
            serde_json::to_writer(&mut *input, line).unwrap();
            input.write_all(b"\n").unwrap();
        }
    }
    drop(child.stdin.take());
    let output = child.wait_with_output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
}

#[test]
fn mcp_lists_strict_tools_and_dispatches_without_paths() {
    let fixture = fixture();
    fs::write(fixture.workspace.join("a.txt"), b"new").unwrap();
    let replies = rpc(
        &fixture,
        &[
            json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-11-25","capabilities":{}}}),
            json!({"jsonrpc":"2.0","id":2,"method":"tools/list"}),
            json!({"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"agentscm_snapshot_capture","arguments":{},"_meta":{"progressToken":"capture-1"}}}),
            json!({"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"agentscm_snapshot_timeline","arguments":{"limit":10}}}),
            json!({"jsonrpc":"2.0","id":5,"method":"ping"}),
        ],
        true,
    );
    assert_eq!(
        replies[0]["result"]["serverInfo"]["name"],
        "agentscm-workspace"
    );
    let tools = replies[1]["result"]["tools"].as_array().unwrap();
    assert_eq!(tools.len(), 5);
    assert_eq!(tools[0]["annotations"]["idempotentHint"], false);
    assert_eq!(tools[4]["annotations"]["idempotentHint"], false);
    assert_eq!(replies[2]["result"]["isError"], false);
    assert_eq!(replies[3]["result"]["isError"], false);
    assert_eq!(
        replies[3]["result"]["structuredContent"],
        cli(
            &fixture.data,
            &[
                "workspace",
                "timeline",
                fixture.workspace.to_str().unwrap(),
                "--limit",
                "10"
            ]
        )
    );
    assert_eq!(replies[4]["result"], json!({}));
    let schema = &tools[4]["inputSchema"]["properties"];
    for forbidden in [
        "path",
        "root",
        "workspace",
        "data_dir",
        "command",
        "model",
        "executable",
    ] {
        assert!(schema.get(forbidden).is_none());
    }
    assert_eq!(
        rpc(
            &fixture,
            &[json!({"jsonrpc":"2.0","id":1,"method":"tools/list"})],
            false,
        )[0]["result"]["tools"]
            .as_array()
            .unwrap()
            .len(),
        4
    );
}

#[test]
fn mcp_rejects_unknown_arguments_and_arbitrary_fork_paths() {
    let fixture = fixture();
    let replies = rpc(
        &fixture,
        &[
            json!({"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"agentscm_snapshot_capture","arguments":{"workspace":"/tmp/other"}}}),
            json!({"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"agentscm_snapshot_fork","arguments":{"id":1,"child_basename":"../escape","change_id":"c"}}}),
            json!({"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"agentscm_snapshot_fork","arguments":{"id":1,"child_basename":"/absolute","change_id":"c"}}}),
        ],
        true,
    );
    assert!(replies.iter().all(|reply| reply["error"]["code"] == -32602));
}

fn hook(fixture: &Fixture, agent: &str, payload: Value) -> std::process::Output {
    let mut child = command(&fixture.data)
        .args(["workspace-client", "hook", "--workspace"])
        .arg(&fixture.workspace)
        .args(["--agent", agent])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    serde_json::to_writer(child.stdin.as_mut().unwrap(), &payload).unwrap();
    drop(child.stdin.take());
    child.wait_with_output().unwrap()
}

#[test]
fn all_five_completion_payloads_capture_and_unrelated_root_is_ignored() {
    let fixture = fixture();
    let cwd = fixture.workspace.to_string_lossy();
    let payloads = [
        (
            "claude",
            json!({"hook_event_name":"Stop","cwd":cwd,"stop_hook_active":false}),
        ),
        (
            "codex",
            json!({"hook_event_name":"Stop","cwd":cwd,"stop_hook_active":false}),
        ),
        (
            "cursor",
            json!({"hook_event_name":"stop","status":"completed","loop_count":0,"workspace_roots":[cwd],"summary":"not retained"}),
        ),
        (
            "copilot",
            json!({"sessionId":"private","cwd":cwd,"stopReason":"end_turn","stop_hook_active":false,"transcriptPath":"/private"}),
        ),
        (
            "gemini",
            json!({"hook_event_name":"AfterAgent","cwd":cwd,"stop_hook_active":false,"prompt":"private","prompt_response":"private"}),
        ),
    ];
    for (index, (agent, payload)) in payloads.into_iter().enumerate() {
        fs::write(fixture.workspace.join("a.txt"), format!("turn-end-{index}")).unwrap();
        let output = hook(&fixture, agent, payload);
        assert!(output.status.success());
        assert_eq!(output.stdout, b"{}\n");
        assert!(output.stderr.is_empty());
        let timeline = cli(
            &fixture.data,
            &[
                "workspace",
                "timeline",
                fixture.workspace.to_str().unwrap(),
                "--limit",
                "100",
            ],
        );
        let entries = timeline["timeline"]["entries"].as_array().unwrap();
        assert_eq!(entries.len(), index + 1);
        assert_eq!(entries[0]["trigger"], "turn_end");
    }
    let elsewhere = tempfile::tempdir().unwrap();
    let output = hook(
        &fixture,
        "claude",
        json!({"hook_event_name":"Stop","cwd":elsewhere.path(),"stop_hook_active":false}),
    );
    assert!(output.status.success());
    assert_eq!(output.stdout, b"{}\n");
    assert!(output.stderr.is_empty());
}

#[test]
fn hook_limits_and_multi_root_never_block() {
    let fixture = fixture();
    let mut child = command(&fixture.data)
        .args(["workspace-client", "hook", "--workspace"])
        .arg(&fixture.workspace)
        .args(["--agent", "gemini"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .as_mut()
        .unwrap()
        .write_all(&vec![b'x'; 64 * 1024 + 1])
        .unwrap();
    drop(child.stdin.take());
    let output = child.wait_with_output().unwrap();
    assert!(output.status.success());
    assert_eq!(output.stdout, b"{}\n");
    assert!(!output.stderr.is_empty());

    let output = hook(
        &fixture,
        "cursor",
        json!({"hook_event_name":"stop","status":"completed","loop_count":0,"workspace_roots":[fixture.workspace,"/tmp/other"]}),
    );
    assert!(output.status.success());
    assert_eq!(output.stdout, b"{}\n");
    assert!(!output.stderr.is_empty());
}

#[test]
fn hook_startup_failure_is_visible_but_nonblocking() {
    let fixture = fixture();
    let missing = fixture.config.join("missing-workspace");
    let mut child = command(&fixture.data)
        .args(["workspace-client", "hook", "--workspace"])
        .arg(&missing)
        .args(["--agent", "claude"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    serde_json::to_writer(
        child.stdin.as_mut().unwrap(),
        &json!({"hook_event_name":"Stop","cwd":missing,"stop_hook_active":false}),
    )
    .unwrap();
    drop(child.stdin.take());
    let output = child.wait_with_output().unwrap();
    assert!(output.status.success());
    assert_eq!(output.stdout, b"{}\n");
    assert!(String::from_utf8_lossy(&output.stderr).contains("capture failed"));
}

fn install(fixture: &Fixture, agent: &str, hook: &Path, mcp: &Path, apply: bool) -> Value {
    let mut command = command(&fixture.data);
    command
        .args(["workspace-client", "install", "--workspace"])
        .arg(&fixture.workspace)
        .args(["--fork-root"])
        .arg(&fixture.forks)
        .args(["--agent", agent, "--hook-settings"])
        .arg(hook)
        .args(["--mcp-settings"])
        .arg(mcp);
    if apply {
        command.arg("--apply");
    }
    let output = command.output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap()
}

#[test]
fn installers_preserve_existing_config_for_all_five_clients() {
    for agent in ["claude", "codex", "cursor", "copilot", "gemini"] {
        let fixture = fixture();
        let hook = fixture.config.join(format!("{agent}-hooks.json"));
        let mcp = fixture.config.join(if agent == "codex" {
            "codex.toml".to_owned()
        } else {
            format!("{agent}-mcp.json")
        });
        fs::write(
            &hook,
            "{\"keep\":true,\"hooks\":{\"other\":[{\"command\":\"mine\"}]}}\n",
        )
        .unwrap();
        fs::write(
            &mcp,
            if agent == "codex" {
                "[features]\n# retained comment\nkeep = true\n"
            } else {
                "{\"keep\":true}\n"
            },
        )
        .unwrap();
        let preview = install(&fixture, agent, &hook, &mcp, false);
        assert_eq!(preview["applied"], false);
        assert!(
            preview["files"]
                .as_array()
                .unwrap()
                .iter()
                .all(|file| file["after"].as_str().unwrap().contains("keep"))
        );
        let applied = install(&fixture, agent, &hook, &mcp, true);
        assert_eq!(applied["applied"], true);
        assert!(fs::read_to_string(&hook).unwrap().contains("mine"));
        let mcp_text = fs::read_to_string(&mcp).unwrap();
        assert!(mcp_text.contains("agentscm-workspace"));
        assert!(mcp_text.contains("--no-telemetry"));
        if agent == "codex" {
            assert!(mcp_text.contains("[features]"));
            assert!(mcp_text.contains("# retained comment"));
            let retry = install(&fixture, agent, &hook, &mcp, false);
            assert!(
                retry["files"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .all(|file| file["action"] == "unchanged")
            );
        }
        if agent == "copilot" {
            let settings: Value = serde_json::from_str(&mcp_text).unwrap();
            assert!(settings["mcpServers"]["agentscm-workspace"].is_object());
            assert_eq!(
                settings["mcpServers"]["agentscm-workspace"]["type"],
                "local"
            );
            assert!(settings.get("servers").is_none());
            let hooks: Value = serde_json::from_str(&fs::read_to_string(&hook).unwrap()).unwrap();
            assert!(hooks["hooks"]["agentStop"][0]["exec"].is_string());
            assert!(hooks["hooks"]["agentStop"][0].get("command").is_none());
        }
    }
}

#[test]
fn codex_toml_rejects_quoted_inline_and_escaped_conflicts() {
    let variants = [
        "[mcp_servers.agentscm-workspace]\ncommand = \"other\"\n",
        "[mcp_servers.\"agentscm-workspace\"]\ncommand = \"other\"\n",
        "mcp_servers = { \"agentscm-workspace\" = { command = \"other\" } }\n",
        "[mcp_servers.\"agentscm\\u002dworkspace\"]\ncommand = \"other\"\n",
    ];
    for (index, contents) in variants.into_iter().enumerate() {
        let fixture = fixture();
        let hook = fixture.config.join(format!("hooks-{index}.json"));
        let mcp = fixture.config.join(format!("config-{index}.toml"));
        fs::write(&hook, "{}").unwrap();
        fs::write(&mcp, contents).unwrap();
        let output = command(&fixture.data)
            .args(["workspace-client", "install", "--workspace"])
            .arg(&fixture.workspace)
            .args(["--agent", "codex", "--hook-settings"])
            .arg(&hook)
            .args(["--mcp-settings"])
            .arg(&mcp)
            .output()
            .unwrap();
        assert!(!output.status.success(), "variant {index} was accepted");
        assert!(String::from_utf8_lossy(&output.stderr).contains("entry differs"));
    }
}

#[test]
fn installer_normalizes_aliases_and_only_combines_gemini_settings() {
    let fixture = fixture();
    let settings = fixture.config.join("settings.json");
    fs::write(&settings, "{}").unwrap();
    let alias = fixture.config.join("..").join("config/settings.json");
    let preview = install(&fixture, "gemini", &settings, &alias, false);
    assert_eq!(preview["files"].as_array().unwrap().len(), 1);

    for agent in ["claude", "codex", "cursor", "copilot"] {
        let output = command(&fixture.data)
            .args(["workspace-client", "install", "--workspace"])
            .arg(&fixture.workspace)
            .args(["--agent", agent, "--hook-settings"])
            .arg(&settings)
            .args(["--mcp-settings"])
            .arg(&alias)
            .output()
            .unwrap();
        assert!(!output.status.success(), "{agent}");
        assert!(String::from_utf8_lossy(&output.stderr).contains("different settings files"));
        assert_eq!(fs::read_to_string(&settings).unwrap(), "{}");
    }
}

#[test]
fn installer_refuses_case_aliases_before_writing_existing_or_new_settings() {
    for existing in [false, true] {
        for agent in ["cursor", "gemini", "codex"] {
            let fixture = fixture();
            let settings = fixture.config.join("settings.json");
            let alias = fixture.config.join("SETTINGS.json");
            if existing {
                fs::write(&settings, "{}").unwrap();
            }
            let output = command(&fixture.data)
                .args(["workspace-client", "install", "--workspace"])
                .arg(&fixture.workspace)
                .args(["--agent", agent, "--hook-settings"])
                .arg(&settings)
                .args(["--mcp-settings"])
                .arg(&alias)
                .arg("--apply")
                .output()
                .unwrap();
            assert!(!output.status.success(), "{agent}, existing={existing}");
            assert!(String::from_utf8_lossy(&output.stderr).contains("letter case"));
            if existing {
                assert_eq!(fs::read_to_string(&settings).unwrap(), "{}");
            } else {
                assert!(!settings.exists());
                assert!(!alias.exists());
            }
        }
    }
}

#[test]
fn installer_refuses_parallel_agentscm_stop_and_workspace_settings() {
    let fixture = fixture();
    let hook = fixture.config.join("hooks.json");
    let mcp = fixture.config.join("mcp.json");
    fs::write(
        &hook,
        r#"{"hooks":{"Stop":[{"hooks":[{"command":"agentscm skill-workflow claude-stop --workspace /w"}]}]}}"#,
    )
    .unwrap();
    fs::write(&mcp, "{}").unwrap();
    let output = command(&fixture.data)
        .args(["workspace-client", "install", "--workspace"])
        .arg(&fixture.workspace)
        .args(["--agent", "claude", "--hook-settings"])
        .arg(&hook)
        .args(["--mcp-settings"])
        .arg(&mcp)
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("timeline check links"));

    let output = command(&fixture.data)
        .args(["workspace-client", "install", "--workspace"])
        .arg(&fixture.workspace)
        .args(["--agent", "cursor", "--hook-settings"])
        .arg(fixture.workspace.join("hooks.json"))
        .args(["--mcp-settings"])
        .arg(&mcp)
        .output()
        .unwrap();
    assert!(!output.status.success());
}

#[test]
fn invalid_capture_hook_options_never_block_but_other_commands_still_fail() {
    let temp = TempDir::new().unwrap();
    for args in [
        vec![
            "workspace-client",
            "hook",
            "--workspace",
            "/missing",
            "--agent",
            "invalid",
        ],
        vec!["workspace-client", "hook", "--unknown-option"],
    ] {
        let output = command(temp.path()).args(args).output().unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(
            serde_json::from_slice::<Value>(&output.stdout).unwrap(),
            json!({})
        );
    }
    let output = command(temp.path())
        .args(["workspace-client", "install", "--unknown-option"])
        .output()
        .unwrap();
    assert!(!output.status.success());
}
