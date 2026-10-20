//! Shared Stop-hook handling for Claude Code and Codex CLI: payload decoding, a durable
//! bound on consecutive automatic continuations, and the hook output shapes.

use std::fs;
use std::io::{ErrorKind, Read};
use std::path::{Path, PathBuf};

use agentscm::{Error, Result};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

pub const MAX_STOP_ROUNDS: u32 = 8;
const MAX_INPUT_BYTES: u64 = 64 * 1024;
const MAX_SESSION_ID_BYTES: usize = 256;
const MAX_CWD_BYTES: usize = 4 * 1024;
const MAX_STATE_BYTES: u64 = 1024;
const STATE_DIR: &str = "agent-stop";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StopAgent {
    Claude,
    Codex,
}

impl StopAgent {
    pub fn name(self) -> &'static str {
        match self {
            Self::Claude => "claude",
            Self::Codex => "codex",
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
pub struct StopInput {
    pub session_id: Option<String>,
    pub cwd: Option<PathBuf>,
    pub stop_hook_active: bool,
}

impl StopInput {
    pub fn read(reader: impl Read) -> Result<Self> {
        let mut bytes = Vec::new();
        reader.take(MAX_INPUT_BYTES + 1).read_to_end(&mut bytes)?;
        if bytes.len() as u64 > MAX_INPUT_BYTES {
            return Err(Error::Limit("Stop hook input exceeds 64 KiB".into()));
        }
        let payload: Value = serde_json::from_slice(&bytes)?;
        if payload.get("hook_event_name").and_then(Value::as_str) != Some("Stop") {
            return Err(Error::Invalid(
                "Stop workflow hook expects hook_event_name Stop".into(),
            ));
        }
        let session_id = optional_string(&payload, "session_id", MAX_SESSION_ID_BYTES)?;
        let cwd = optional_string(&payload, "cwd", MAX_CWD_BYTES)?.map(PathBuf::from);
        let Some(Value::Bool(stop_hook_active)) = payload.get("stop_hook_active") else {
            return Err(Error::Invalid(
                "Stop hook input requires boolean stop_hook_active".into(),
            ));
        };
        let stop_hook_active = *stop_hook_active;
        Ok(Self {
            session_id,
            cwd,
            stop_hook_active,
        })
    }
}

fn optional_string(payload: &Value, key: &str, maximum: usize) -> Result<Option<String>> {
    match payload.get(key) {
        None => Ok(None),
        Some(Value::String(value)) if value.len() <= maximum => Ok(Some(value.clone())),
        Some(Value::String(_)) => Err(Error::Invalid(format!(
            "Stop hook {key} exceeds {maximum} bytes"
        ))),
        Some(_) => Err(Error::Invalid(format!("Stop hook {key} must be a string"))),
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RoundState {
    version: u32,
    rounds: u32,
}

enum Stored {
    Rounds(u32),
    Unreadable,
}

pub enum Prepared {
    Done(Value),
    Run(Guard),
}

pub struct Guard {
    path: PathBuf,
    rounds: u32,
}

/// Decides whether the check must run. Early outputs never run it or change the counter.
///
/// The counter is keyed by agent, session, workspace and change. Only a new user turn
/// (`stop_hook_active: false`) resets it; a pass leaves it alone, so hook configurations sharing
/// a key cannot alternate pass and reset forever. It needs no lock: concurrent hooks with the
/// same key may lose an update, but each failing invocation still writes at least the round it
/// read plus one, so every block in a continuation chain counts toward the bound.
pub fn prepare(
    data_dir: &Path,
    agent: StopAgent,
    workspace: Option<&Path>,
    change: &str,
    input: impl Read,
) -> Result<Prepared> {
    let input = StopInput::read(input)?;
    if let (Some(cwd), Some(workspace)) = (&input.cwd, workspace)
        && let (Ok(cwd), Ok(workspace)) = (fs::canonicalize(cwd), fs::canonicalize(workspace))
        && !cwd.starts_with(&workspace)
        && !workspace.starts_with(&cwd)
    {
        // Unrelated directory: no blocking feedback, which is not a pass.
        return Ok(Prepared::Done(pass()));
    }
    let relative = Path::new(STATE_DIR).join(format!(
        "{}.json",
        state_key(
            agent,
            input.session_id.as_deref().unwrap_or(""),
            workspace,
            change
        )
    ));
    let path = data_dir.join(&relative);
    if !input.stop_hook_active && remove_state(&path).is_err() {
        return Ok(Prepared::Done(stop(&format!(
            "The AgentSCM Stop-hook attempt counter at {} could not be reset, so automatic attempts are not continued. \
             Stopping is allowed, but nothing was qualified and native landing still requires a current receipt. \
             Inspect the workflow with skill-workflow check or remote-check.",
            relative.display()
        ))));
    }
    let rounds = match load(&path) {
        Stored::Rounds(rounds) if rounds < MAX_STOP_ROUNDS => rounds,
        Stored::Rounds(_) => {
            return Ok(Prepared::Done(stop(&format!(
                "The AgentSCM workflow did not qualify the current bytes after {MAX_STOP_ROUNDS} automatic attempts. \
                 Stopping is allowed, but nothing was qualified and native landing still requires a current receipt. \
                 Inspect the workflow with skill-workflow check or remote-check."
            ))));
        }
        Stored::Unreadable => {
            return Ok(Prepared::Done(stop(&format!(
                "The AgentSCM Stop-hook attempt counter at {} is unreadable, so automatic attempts are not continued. \
                 Stopping is allowed, but nothing was qualified and native landing still requires a current receipt. \
                 Inspect the workflow with skill-workflow check or remote-check.",
                relative.display()
            ))));
        }
    };
    Ok(Prepared::Run(Guard { path, rounds }))
}

impl Guard {
    /// Renders the hook output for a normalised check result and updates the counter.
    pub fn finish(self, result: &Value, default_action: &str, max_reason: usize) -> Result<Value> {
        let passed = result.get("receipt").is_some_and(|value| !value.is_null())
            && result
                .get("workspace_changed_during_run")
                .and_then(Value::as_bool)
                == Some(false);
        if passed {
            return Ok(pass());
        }
        let mut feedback = json!({
            "failures": result.get("failures").cloned().unwrap_or_else(|| json!([])),
            "next_action": result.get("next_action").cloned().unwrap_or_else(|| json!(default_action)),
        });
        if let Some(next) = result.get("next") {
            feedback["next"] = next.clone();
        }
        let state = RoundState {
            version: 1,
            rounds: self.rounds + 1,
        };
        let feedback = serde_json::to_string(&feedback)?;
        if crate::hub_cli::write_private(&self.path, &serde_json::to_string(&state)?).is_err() {
            return Ok(stop(&format!(
                "The AgentSCM Stop-hook attempt counter could not be recorded, so automatic attempts are not continued. \
                 Stopping is allowed, but nothing was qualified and native landing still requires a current receipt. \
                 Feedback: {}",
                bounded(&feedback, 1024)
            )));
        }
        let reason = bounded(&feedback, max_reason);
        Ok(json!({"decision": "block", "reason": reason}))
    }
}

/// Synchronous driver for hooks whose check is not async.
#[allow(clippy::too_many_arguments)] // one call site; a parameter struct would only rename them
pub fn run_stop(
    data_dir: &Path,
    agent: StopAgent,
    workspace: Option<&Path>,
    change: &str,
    input: impl Read,
    default_action: &str,
    max_reason: usize,
    check: impl FnOnce() -> Value,
) -> Result<Value> {
    match prepare(data_dir, agent, workspace, change, input)? {
        Prepared::Done(output) => Ok(output),
        Prepared::Run(guard) => guard.finish(&check(), default_action, max_reason),
    }
}

fn pass() -> Value {
    json!({})
}

fn stop(reason: &str) -> Value {
    json!({"continue": false, "stopReason": bounded(reason, 2 * 1024)})
}

fn state_key(agent: StopAgent, session_id: &str, workspace: Option<&Path>, change: &str) -> String {
    let workspace = workspace.map(|path| fs::canonicalize(path).unwrap_or_else(|_| path.into()));
    let workspace = workspace
        .as_deref()
        .map_or(&[][..], |path| path.as_os_str().as_encoded_bytes());
    let mut hasher = Sha256::new();
    hasher.update(b"agentscm.agent-stop.v2\0");
    for field in [
        agent.name().as_bytes(),
        session_id.as_bytes(),
        workspace,
        change.as_bytes(),
    ] {
        hasher.update((field.len() as u64).to_be_bytes());
        hasher.update(field);
    }
    format!("{:x}", hasher.finalize())
}

fn load(path: &Path) -> Stored {
    let bytes = match fs::File::open(path) {
        Ok(file) => {
            let mut bytes = Vec::new();
            match file.take(MAX_STATE_BYTES + 1).read_to_end(&mut bytes) {
                Ok(_) if bytes.len() as u64 <= MAX_STATE_BYTES => bytes,
                _ => return Stored::Unreadable,
            }
        }
        Err(error) if error.kind() == ErrorKind::NotFound => return Stored::Rounds(0),
        Err(_) => return Stored::Unreadable,
    };
    match serde_json::from_slice::<RoundState>(&bytes) {
        Ok(state) if state.version == 1 => Stored::Rounds(state.rounds),
        _ => Stored::Unreadable,
    }
}

fn remove_state(path: &Path) -> Result<()> {
    match fs::remove_file(path) {
        Err(error) if error.kind() != ErrorKind::NotFound => Err(error.into()),
        _ => Ok(()),
    }
}

fn bounded(value: &str, maximum: usize) -> String {
    if value.len() <= maximum {
        return value.into();
    }
    let mut end = maximum.saturating_sub(3);
    while !value.is_char_boundary(end) {
        end = end.saturating_sub(1);
    }
    format!("{}...", &value[..end])
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    const STOP: &str = r#"{"hook_event_name":"Stop","session_id":"s1","stop_hook_active":true}"#;
    const NEW_TURN: &str =
        r#"{"hook_event_name":"Stop","session_id":"s1","stop_hook_active":false}"#;

    fn passing() -> Value {
        json!({"receipt": "r", "workspace_changed_during_run": false})
    }

    #[test]
    fn typed_next_is_carried_into_the_feedback_when_present() {
        let data = TempDir::new().unwrap();
        let mut ran = false;
        let mut result = failing();
        result["next"] = json!({"operation": "checkpoint"});
        let out = run(data.path(), StopAgent::Codex, STOP, result, &mut ran).unwrap();
        let reason: Value = serde_json::from_str(out["reason"].as_str().unwrap()).unwrap();
        assert_eq!(reason["next"], json!({"operation": "checkpoint"}));
        let out = run(data.path(), StopAgent::Codex, STOP, failing(), &mut ran).unwrap();
        let reason: Value = serde_json::from_str(out["reason"].as_str().unwrap()).unwrap();
        assert!(reason.get("next").is_none());
    }

    fn failing() -> Value {
        json!({"receipt": null, "failures": [{"detail": "bad"}], "next_action": "fix"})
    }

    fn run(
        data: &Path,
        agent: StopAgent,
        input: &str,
        result: Value,
        ran: &mut bool,
    ) -> Result<Value> {
        run_stop(
            data,
            agent,
            None,
            "c1",
            input.as_bytes(),
            "retry",
            1024,
            || {
                *ran = true;
                result
            },
        )
    }

    fn state_files(data: &Path) -> Vec<PathBuf> {
        match fs::read_dir(data.join(STATE_DIR)) {
            Ok(entries) => entries.map(|entry| entry.unwrap().path()).collect(),
            Err(_) => Vec::new(),
        }
    }

    fn rounds(data: &Path) -> u64 {
        let files = state_files(data);
        let value: Value = serde_json::from_slice(&fs::read(&files[0]).unwrap()).unwrap();
        value["rounds"].as_u64().unwrap()
    }

    #[test]
    fn decodes_codex_shaped_payload_and_ignores_unknown_fields() {
        let input = StopInput::read(
            &br#"{"hook_event_name":"Stop","session_id":"s","turn_id":"t","cwd":"/work","stop_hook_active":true,"last_assistant_message":"hi","extra":{"a":1}}"#[..],
        )
        .unwrap();
        assert_eq!(
            input,
            StopInput {
                session_id: Some("s".into()),
                cwd: Some("/work".into()),
                stop_hook_active: true
            }
        );
        let minimal =
            StopInput::read(&br#"{"hook_event_name":"Stop","stop_hook_active":false}"#[..])
                .unwrap();
        assert_eq!(minimal.session_id, None);
        assert!(!minimal.stop_hook_active);
    }

    #[test]
    fn rejects_bad_payloads() {
        let decode = |text: &[u8]| StopInput::read(text);
        assert!(matches!(
            decode(br#"{"hook_event_name":"SessionStart"}"#),
            Err(Error::Invalid(_))
        ));
        assert!(matches!(decode(br#"{}"#), Err(Error::Invalid(_))));
        let missing = decode(br#"{"hook_event_name":"Stop","session_id":"s"}"#);
        assert!(matches!(
            missing,
            Err(Error::Invalid(ref message))
                if message == "Stop hook input requires boolean stop_hook_active"
        ));
        assert!(matches!(
            decode(br#"{"hook_event_name":"Stop","stop_hook_active":"yes"}"#),
            Err(Error::Invalid(_))
        ));
        assert!(matches!(
            decode(br#"{"hook_event_name":"Stop","session_id":7,"stop_hook_active":false}"#),
            Err(Error::Invalid(_))
        ));
        assert!(matches!(
            decode(br#"{"hook_event_name":"Stop","cwd":["x"],"stop_hook_active":false}"#),
            Err(Error::Invalid(_))
        ));
        let long = format!(
            r#"{{"hook_event_name":"Stop","session_id":"{}"}}"#,
            "a".repeat(257)
        );
        assert!(matches!(decode(long.as_bytes()), Err(Error::Invalid(_))));
        let ok = format!(
            r#"{{"hook_event_name":"Stop","session_id":"{}","stop_hook_active":false}}"#,
            "a".repeat(256)
        );
        assert!(decode(ok.as_bytes()).is_ok());
        let huge = format!(
            r#"{{"hook_event_name":"Stop","pad":"{}"}}"#,
            "a".repeat(64 * 1024)
        );
        assert!(matches!(decode(huge.as_bytes()), Err(Error::Limit(_))));
        assert!(decode(b"not json").is_err());
    }

    #[test]
    fn blocks_count_up_and_exhaustion_skips_the_check() {
        let data = TempDir::new().unwrap();
        let mut ran = false;
        let first = run(
            data.path(),
            StopAgent::Claude,
            NEW_TURN,
            failing(),
            &mut ran,
        )
        .unwrap();
        assert!(ran);
        assert_eq!(first["decision"], "block");
        assert_eq!(rounds(data.path()), 1);
        for expected in 2..=u64::from(MAX_STOP_ROUNDS) {
            let output = run(data.path(), StopAgent::Claude, STOP, failing(), &mut ran).unwrap();
            assert_eq!(output["decision"], "block");
            assert_eq!(rounds(data.path()), expected);
        }
        let mut ran = false;
        let output = run(data.path(), StopAgent::Claude, STOP, passing(), &mut ran).unwrap();
        assert!(!ran);
        assert_eq!(output["continue"], false);
        assert!(output["stopReason"].as_str().unwrap().contains("8"));
        assert_eq!(rounds(data.path()), u64::from(MAX_STOP_ROUNDS));
    }

    #[test]
    fn new_turn_resets_and_pass_keeps_state() {
        let data = TempDir::new().unwrap();
        let mut ran = false;
        for _ in 0..3 {
            run(data.path(), StopAgent::Codex, STOP, failing(), &mut ran).unwrap();
        }
        assert_eq!(rounds(data.path()), 3);
        run(data.path(), StopAgent::Codex, NEW_TURN, failing(), &mut ran).unwrap();
        assert_eq!(rounds(data.path()), 1);
        let output = run(data.path(), StopAgent::Codex, STOP, passing(), &mut ran).unwrap();
        assert_eq!(output, json!({}));
        assert_eq!(rounds(data.path()), 1);
    }

    #[test]
    fn interleaved_passes_do_not_reset_the_bound() {
        let data = TempDir::new().unwrap();
        let mut ran = false;
        for round in 1..=MAX_STOP_ROUNDS {
            let output = run(data.path(), StopAgent::Claude, STOP, failing(), &mut ran).unwrap();
            assert_eq!(output["decision"], "block");
            if round < MAX_STOP_ROUNDS {
                let output =
                    run(data.path(), StopAgent::Claude, STOP, passing(), &mut ran).unwrap();
                assert_eq!(output, json!({}));
            }
        }
        assert_eq!(rounds(data.path()), u64::from(MAX_STOP_ROUNDS));
        let mut ran = false;
        let output = run(data.path(), StopAgent::Claude, STOP, failing(), &mut ran).unwrap();
        assert!(!ran);
        assert_eq!(output["continue"], false);
    }

    #[test]
    fn workspace_change_during_run_is_not_a_pass() {
        let data = TempDir::new().unwrap();
        let mut ran = false;
        for changed in [json!(true), Value::Null] {
            let result = json!({"receipt": "r", "workspace_changed_during_run": changed});
            let output = run(data.path(), StopAgent::Claude, NEW_TURN, result, &mut ran).unwrap();
            assert_eq!(output["decision"], "block");
        }
    }

    #[test]
    fn corrupt_state_is_terminal_and_not_reset_while_continuing() {
        let data = TempDir::new().unwrap();
        let mut ran = false;
        run(data.path(), StopAgent::Claude, STOP, failing(), &mut ran).unwrap();
        let path = state_files(data.path()).remove(0);
        for corrupt in [
            &b"garbage"[..],
            br#"{"version":1,"rounds":1,"x":0}"#,
            br#"{"version":2,"rounds":1}"#,
        ] {
            fs::write(&path, corrupt).unwrap();
            let mut ran = false;
            let output = run(data.path(), StopAgent::Claude, STOP, passing(), &mut ran).unwrap();
            assert!(!ran);
            assert_eq!(output["continue"], false);
            let reason = output["stopReason"].as_str().unwrap();
            assert!(reason.contains("unreadable"));
            assert!(reason.contains("agent-stop"));
            assert!(!reason.contains(data.path().to_str().unwrap()));
            assert_eq!(fs::read(&path).unwrap(), corrupt);
        }
        let output = run(
            data.path(),
            StopAgent::Claude,
            NEW_TURN,
            passing(),
            &mut ran,
        )
        .unwrap();
        assert_eq!(output, json!({}));
    }

    #[test]
    fn agents_and_sessions_use_separate_counters() {
        let data = TempDir::new().unwrap();
        let mut ran = false;
        run(data.path(), StopAgent::Claude, STOP, failing(), &mut ran).unwrap();
        run(data.path(), StopAgent::Codex, STOP, failing(), &mut ran).unwrap();
        run(
            data.path(),
            StopAgent::Codex,
            r#"{"hook_event_name":"Stop","session_id":"s2","stop_hook_active":true}"#,
            failing(),
            &mut ran,
        )
        .unwrap();
        assert_eq!(state_files(data.path()).len(), 3);
        let key = |agent, session, workspace: Option<&str>, change| {
            state_key(agent, session, workspace.map(Path::new), change)
        };
        let base = key(StopAgent::Claude, "s1", Some("/w"), "c1");
        assert_ne!(base, key(StopAgent::Codex, "s1", Some("/w"), "c1"));
        assert_ne!(base, key(StopAgent::Claude, "s2", Some("/w"), "c1"));
        assert_ne!(base, key(StopAgent::Claude, "s1", Some("/x"), "c1"));
        assert_ne!(base, key(StopAgent::Claude, "s1", Some("/w"), "c2"));
        assert_ne!(base, key(StopAgent::Claude, "s1", None, "c1"));
        // Length prefixes keep field boundaries distinct.
        assert_ne!(
            key(StopAgent::Claude, "ab", Some("/w"), "c"),
            key(StopAgent::Claude, "a", Some("b/w"), "c")
        );
    }

    #[test]
    fn workspaces_and_changes_in_one_session_do_not_share_a_budget() {
        let data = TempDir::new().unwrap();
        let (a, b) = (TempDir::new().unwrap(), TempDir::new().unwrap());
        let go = |workspace: &Path, change: &str, input: &str| {
            run_stop(
                data.path(),
                StopAgent::Claude,
                Some(workspace),
                change,
                input.as_bytes(),
                "retry",
                1024,
                failing,
            )
            .unwrap()
        };
        let input = |active: bool, cwd: &Path| {
            json!({"hook_event_name": "Stop", "session_id": "s", "cwd": cwd, "stop_hook_active": active})
                .to_string()
        };
        go(a.path(), "c1", &input(true, a.path()));
        go(a.path(), "c1", &input(true, a.path()));
        go(b.path(), "c1", &input(true, b.path()));
        go(a.path(), "c2", &input(true, a.path()));
        assert_eq!(state_files(data.path()).len(), 3);
        // A new turn for one key leaves the others' counts alone.
        go(b.path(), "c1", &input(false, b.path()));
        let mut counts: Vec<u64> = state_files(data.path())
            .iter()
            .map(|file| {
                let value: Value = serde_json::from_slice(&fs::read(file).unwrap()).unwrap();
                value["rounds"].as_u64().unwrap()
            })
            .collect();
        counts.sort_unstable();
        assert_eq!(counts, [1, 1, 2]);
    }

    #[test]
    fn cwd_outside_the_workspace_is_a_no_op() {
        let data = TempDir::new().unwrap();
        let workspace = TempDir::new().unwrap();
        let nested = workspace.path().join("src");
        fs::create_dir(&nested).unwrap();
        let other = TempDir::new().unwrap();
        let payload = |cwd: &Path| {
            json!({"hook_event_name": "Stop", "cwd": cwd, "stop_hook_active": true}).to_string()
        };
        let mut ran = false;
        let mut go = |cwd: &Path| {
            ran = false;
            let output = run_stop(
                data.path(),
                StopAgent::Codex,
                Some(workspace.path()),
                "c1",
                payload(cwd).as_bytes(),
                "retry",
                1024,
                || {
                    ran = true;
                    failing()
                },
            )
            .unwrap();
            (output, ran)
        };
        let (output, ran) = go(other.path());
        assert_eq!(output, json!({}));
        assert!(!ran);
        assert!(state_files(data.path()).is_empty());
        let (output, ran) = go(&nested);
        assert_eq!(output["decision"], "block");
        assert!(ran);
        let (output, ran) = go(workspace.path());
        assert_eq!(output["decision"], "block");
        assert!(ran);
        assert_eq!(rounds(data.path()), 2);
    }

    #[test]
    fn ancestor_cwd_runs_the_check() {
        let data = TempDir::new().unwrap();
        let parent = TempDir::new().unwrap();
        let workspace = parent.path().join("work");
        fs::create_dir(&workspace).unwrap();
        let payload =
            json!({"hook_event_name": "Stop", "cwd": parent.path(), "stop_hook_active": true})
                .to_string();
        let mut ran = false;
        let output = run_stop(
            data.path(),
            StopAgent::Codex,
            Some(&workspace),
            "c1",
            payload.as_bytes(),
            "retry",
            1024,
            || {
                ran = true;
                failing()
            },
        )
        .unwrap();
        assert!(ran);
        assert_eq!(output["decision"], "block");
    }

    fn prepared(data: &Path, input: &str) -> Prepared {
        prepare(data, StopAgent::Claude, None, "c1", input.as_bytes()).unwrap()
    }

    fn break_state_dir(data: &Path) {
        let dir = data.join(STATE_DIR);
        let _ = fs::remove_dir_all(&dir);
        fs::write(dir, b"file").unwrap();
    }

    #[test]
    fn counter_reset_failure_is_terminal_feedback() {
        let data = TempDir::new().unwrap();
        break_state_dir(data.path());
        let Prepared::Done(output) = prepared(data.path(), NEW_TURN) else {
            panic!("expected terminal output");
        };
        assert_eq!(output["continue"], false);
        let reason = output["stopReason"].as_str().unwrap();
        assert!(reason.contains("could not be reset"));
        assert!(reason.contains("agent-stop"));
        assert!(!reason.contains(data.path().to_str().unwrap()));
    }

    #[test]
    fn counter_write_failure_stops_with_the_feedback() {
        let data = TempDir::new().unwrap();
        let Prepared::Run(guard) = prepared(data.path(), NEW_TURN) else {
            panic!("expected a run");
        };
        break_state_dir(data.path());
        let output = guard.finish(&failing(), "retry", 1024).unwrap();
        assert_eq!(output["continue"], false);
        assert!(output.get("decision").is_none());
        let reason = output["stopReason"].as_str().unwrap();
        assert!(reason.contains("could not be recorded"));
        assert!(reason.contains("bad"));
        assert!(reason.len() <= 2 * 1024);
    }

    #[test]
    fn absent_cwd_and_missing_workspace_proceed_to_the_check() {
        let data = TempDir::new().unwrap();
        let missing = data.path().join("missing");
        let mut ran = false;
        let output = run_stop(
            data.path(),
            StopAgent::Claude,
            Some(&missing),
            "c1",
            &br#"{"hook_event_name":"Stop","cwd":"/definitely/elsewhere","stop_hook_active":false}"#[..],
            "retry",
            1024,
            || {
                ran = true;
                failing()
            },
        )
        .unwrap();
        assert!(ran);
        assert_eq!(output["decision"], "block");
    }

    #[test]
    fn block_reason_is_bounded() {
        let data = TempDir::new().unwrap();
        let huge = json!({"failures": [{"detail": "é".repeat(5000)}]});
        let output = run_stop(
            data.path(),
            StopAgent::Claude,
            None,
            "c1",
            NEW_TURN.as_bytes(),
            "retry",
            100,
            || huge,
        )
        .unwrap();
        assert!(output["reason"].as_str().unwrap().len() <= 100);
    }
}
