//! Bounded local prompt dispatch. The installed client keeps its own login and settings.

use std::io::{Read, Write};
#[cfg(target_os = "macos")]
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use agentscm::{Error, Result};

pub(super) const MAX_PROMPT_BYTES: usize = 16 * 1024;
const MAX_OUTPUT_BYTES: u64 = 64 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Agent {
    ClaudeCode,
    Codex,
}

impl Agent {
    pub(super) fn label(self) -> &'static str {
        match self {
            Self::ClaudeCode => "Claude Code",
            Self::Codex => "Codex CLI",
        }
    }

    fn program(self) -> &'static str {
        match self {
            Self::ClaudeCode => "claude",
            Self::Codex => "codex",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum OptionSet {
    NativeDefault,
    Plan,
    ReadOnly,
}

impl OptionSet {
    #[cfg(target_os = "macos")]
    pub(super) fn label(self) -> &'static str {
        match self {
            Self::NativeDefault => "Native defaults",
            Self::Plan => "Plan (Claude Code)",
            Self::ReadOnly => "Read only (Codex)",
        }
    }

    fn supported(self, agent: Agent) -> bool {
        matches!(self, Self::NativeDefault)
            || matches!(
                (agent, self),
                (Agent::ClaudeCode, Self::Plan) | (Agent::Codex, Self::ReadOnly)
            )
    }
}

pub(super) struct Request {
    pub(super) root: PathBuf,
    pub(super) agent: Agent,
    pub(super) option: OptionSet,
    pub(super) prompt: String,
}

#[derive(Debug)]
pub(super) enum Event {
    Started(u32),
    Exited {
        success: bool,
        session: Option<String>,
        preview: Option<String>,
    },
    Failed(String),
}

fn validate(request: &Request) -> Result<()> {
    if request.prompt.trim().is_empty() || request.prompt.len() > MAX_PROMPT_BYTES {
        return Err(Error::Invalid(
            "prompt must contain 1 to 16384 bytes".into(),
        ));
    }
    if !request.option.supported(request.agent) {
        return Err(Error::Invalid(
            "this client does not support that option".into(),
        ));
    }
    let connection = crate::client_config::load()?
        .into_iter()
        .find(|row| row.root == request.root)
        .ok_or_else(|| Error::Invalid("the selected checkout is no longer connected".into()))?;
    if !crate::client_config::is_current(&connection) {
        return Err(Error::Invalid(
            "the selected checkout binding changed".into(),
        ));
    }
    Ok(())
}

fn search_directories() -> Vec<PathBuf> {
    let mut directories: Vec<_> = std::env::var_os("PATH")
        .map(|path| std::env::split_paths(&path).collect())
        .unwrap_or_default();
    directories.retain(|directory| directory.is_absolute());
    if let Some(home) = std::env::var_os("HOME") {
        let home = PathBuf::from(home);
        if home.is_absolute() {
            directories.push(home.join(".local/bin"));
        }
    }
    #[cfg(target_os = "macos")]
    directories.extend([
        PathBuf::from("/opt/homebrew/bin"),
        PathBuf::from("/usr/local/bin"),
    ]);
    directories.dedup();
    directories
}

fn executable(agent: Agent) -> Result<PathBuf> {
    for directory in search_directories() {
        let candidate = directory.join(agent.program());
        let Ok(real) = candidate.canonicalize() else {
            continue;
        };
        let Ok(metadata) = real.metadata() else {
            continue;
        };
        if !metadata.is_file() {
            continue;
        }
        #[cfg(unix)]
        if std::os::unix::fs::PermissionsExt::mode(&metadata.permissions()) & 0o111 == 0 {
            continue;
        }
        return Ok(real);
    }
    Err(Error::Invalid(format!(
        "{} was not found in PATH or standard user install locations",
        agent.label()
    )))
}

fn command(program: &Path, request: &Request) -> Command {
    let mut command = Command::new(program);
    command.current_dir(&request.root);
    #[cfg(target_os = "macos")]
    command.process_group(0);
    // Login items inherit launchd's small PATH. Keep known installed runtimes available to
    // shebang wrappers such as `#!/usr/bin/env node` without invoking a shell.
    if let Ok(path) = std::env::join_paths(search_directories()) {
        command.env("PATH", path);
    }
    match request.agent {
        Agent::ClaudeCode => {
            command.args(["--print", "--output-format", "json"]);
            if request.option == OptionSet::Plan {
                command.args(["--permission-mode", "plan"]);
            }
        }
        Agent::Codex => {
            command.args(["exec", "--json", "-C"]);
            command.arg(&request.root);
            if request.option == OptionSet::ReadOnly {
                command.args(["--sandbox", "read-only"]);
            }
            command.arg("-");
        }
    }
    command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    command
}

/// Starts one client process and reports only process facts. Exit zero does not prove the task was
/// completed or that a tool action was approved. The caller prevents duplicate dispatch.
pub(super) fn run(request: Request, report: impl Fn(Event) + Send + 'static) {
    std::thread::spawn(move || {
        if let Err(error) = validate(&request) {
            report(Event::Failed(error.to_string()));
            return;
        }
        let program = match executable(request.agent) {
            Ok(program) => program,
            Err(error) => {
                report(Event::Failed(error.to_string()));
                return;
            }
        };
        let mut child = match command(&program, &request).spawn() {
            Ok(child) => child,
            Err(error) => {
                report(Event::Failed(format!("client could not start: {error}")));
                return;
            }
        };
        report(Event::Started(child.id()));
        let written = child
            .stdin
            .take()
            .ok_or_else(|| std::io::Error::other("client input unavailable"))
            .and_then(|mut stdin| stdin.write_all(request.prompt.as_bytes()));
        if let Err(error) = written {
            let _ = child.kill();
            let _ = child.wait();
            report(Event::Failed(format!(
                "could not send prompt to client: {error}"
            )));
            return;
        }
        // Drain the native client's output without retaining prompts, tool logs or unbounded
        // buffers. Only a bounded final response and native session ID may enter local UI state.
        let captured = child.stdout.take().map_or(Ok((None, None)), |stdout| {
            capture_output(request.agent, stdout)
        });
        let (session, preview) = match captured {
            Ok(captured) => captured,
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                report(Event::Failed(format!(
                    "could not read client output: {error}"
                )));
                return;
            }
        };
        match child.wait() {
            Ok(status) => report(Event::Exited {
                success: status.success(),
                session,
                preview,
            }),
            Err(error) => report(Event::Failed(format!("client status is unknown: {error}"))),
        }
    });
}

fn capture_output(
    agent: Agent,
    mut stdout: impl Read,
) -> std::io::Result<(Option<String>, Option<String>)> {
    if agent == Agent::ClaudeCode {
        let mut output = Vec::new();
        stdout
            .by_ref()
            .take(MAX_OUTPUT_BYTES + 1)
            .read_to_end(&mut output)?;
        std::io::copy(&mut stdout, &mut std::io::sink())?;
        if output.len() as u64 > MAX_OUTPUT_BYTES {
            return Ok((
                None,
                Some("Client output exceeded the preview limit; inspect it in Claude Code.".into()),
            ));
        }
        return Ok(parse_output(agent, &output));
    }
    // Codex emits JSONL. Keep a bounded record, not a bounded prefix of the stream: the
    // final agent message can follow arbitrarily large tool output.
    let mut session = None;
    let mut preview = None;
    let mut line = Vec::new();
    let mut overlong = false;
    let mut chunk = [0_u8; 8192];
    loop {
        let size = match stdout.read(&mut chunk) {
            Ok(size) => size,
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error),
        };
        if size == 0 {
            break;
        }
        for &byte in &chunk[..size] {
            if byte == b'\n' {
                if !overlong {
                    parse_codex_line(&line, &mut session, &mut preview);
                } else {
                    preview = Some(
                        "Some client output exceeded the preview limit; inspect it in Codex."
                            .into(),
                    );
                }
                line.clear();
                overlong = false;
            } else if line.len() < MAX_OUTPUT_BYTES as usize {
                line.push(byte);
            } else {
                overlong = true;
            }
        }
    }
    if overlong {
        preview =
            Some("Some client output exceeded the preview limit; inspect it in Codex.".into());
    } else if !line.is_empty() {
        parse_codex_line(&line, &mut session, &mut preview);
    }
    Ok((session, preview))
}

fn parse_codex_line(line: &[u8], session: &mut Option<String>, preview: &mut Option<String>) {
    let Ok(value) = serde_json::from_slice::<serde_json::Value>(line) else {
        return;
    };
    if value["type"] == "thread.started" {
        *session = value["thread_id"].as_str().and_then(safe_id);
    }
    if value["type"] == "item.completed" && value["item"]["type"] == "agent_message" {
        *preview = value["item"]["text"].as_str().map(short_preview);
    }
}

fn parse_output(agent: Agent, output: &[u8]) -> (Option<String>, Option<String>) {
    let text = String::from_utf8_lossy(output);
    match agent {
        Agent::ClaudeCode => {
            let Ok(value) = serde_json::from_str::<serde_json::Value>(&text) else {
                return (None, None);
            };
            let session = value["session_id"].as_str().and_then(safe_id);
            let preview = value["result"].as_str().map(short_preview);
            (session, preview)
        }
        Agent::Codex => {
            let mut session = None;
            let mut preview = None;
            for line in text.lines() {
                parse_codex_line(line.as_bytes(), &mut session, &mut preview);
            }
            (session, preview)
        }
    }
}

fn safe_id(value: &str) -> Option<String> {
    (value.len() <= 80
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-'))
    .then(|| value.to_owned())
}

fn short_preview(value: &str) -> String {
    value
        .chars()
        .filter(|character| !character.is_control())
        .take(500)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn options_do_not_cross_clients() {
        assert!(OptionSet::Plan.supported(Agent::ClaudeCode));
        assert!(!OptionSet::Plan.supported(Agent::Codex));
        assert!(OptionSet::ReadOnly.supported(Agent::Codex));
        assert!(!OptionSet::ReadOnly.supported(Agent::ClaudeCode));
    }

    #[test]
    fn prompt_and_checkout_are_never_shell_arguments() {
        let request = Request {
            root: PathBuf::from("/tmp/a'; touch /tmp/owned; echo '"),
            agent: Agent::Codex,
            option: OptionSet::NativeDefault,
            prompt: "$(touch /tmp/owned)\n`echo hello`".into(),
        };
        let command = command(Path::new("/usr/bin/codex"), &request);
        assert!(
            !command
                .get_args()
                .any(|argument| argument.to_string_lossy() == request.prompt)
        );
        assert_eq!(command.get_current_dir(), Some(request.root.as_path()));
    }

    #[test]
    fn native_session_handoff_is_bounded_and_validated() {
        let claude = br#"{"session_id":"abcd-1234","result":"done"}"#;
        assert_eq!(
            parse_output(Agent::ClaudeCode, claude),
            (Some("abcd-1234".into()), Some("done".into()))
        );
        let codex = b"{\"type\":\"thread.started\",\"thread_id\":\"t-1\"}\n{\"type\":\"item.completed\",\"item\":{\"type\":\"agent_message\",\"text\":\"ok\"}}\n";
        assert_eq!(
            parse_output(Agent::Codex, codex),
            (Some("t-1".into()), Some("ok".into()))
        );
        assert!(safe_id("a b; rm").is_none());
        assert_eq!(short_preview(&"x".repeat(501)).len(), 500);
    }

    #[test]
    fn response_preview_does_not_emit_terminal_controls() {
        assert_eq!(
            short_preview("ok\u{1b}]0;hidden\u{7}\nnext"),
            "ok]0;hiddennext"
        );
    }

    #[test]
    fn codex_final_message_survives_large_tool_output() {
        let mut output = b"{\"type\":\"thread.started\",\"thread_id\":\"t-1\"}\n".to_vec();
        output.extend_from_slice(format!("{{\"tool\":\"{}\"}}\n", "x".repeat(80_000)).as_bytes());
        output.extend_from_slice(b"{\"type\":\"item.completed\",\"item\":{\"type\":\"agent_message\",\"text\":\"finished\"}}\n");
        assert_eq!(
            capture_output(Agent::Codex, output.as_slice()).unwrap(),
            (Some("t-1".into()), Some("finished".into()))
        );
    }

    #[test]
    fn oversized_codex_final_does_not_show_earlier_message_as_final() {
        let mut output = b"{\"type\":\"item.completed\",\"item\":{\"type\":\"agent_message\",\"text\":\"working\"}}\n".to_vec();
        output.extend_from_slice(format!("{{\"type\":\"item.completed\",\"item\":{{\"type\":\"agent_message\",\"text\":\"{}\"}}}}\n", "x".repeat(80_000)).as_bytes());
        let (_, preview) = capture_output(Agent::Codex, output.as_slice()).unwrap();
        assert!(preview.unwrap().contains("exceeded the preview limit"));
    }
}
