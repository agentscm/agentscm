//! Previewed, operator-local install of the Stop hook into a Claude Code or Codex hook file.

use std::fs::{self, OpenOptions};
use std::io::{Read, Write};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

use agentscm::{Error, Result};
use clap::{Parser, Subcommand};
use serde_json::{Value, json};

use crate::agent_stop::StopAgent;

const SUBCOMMANDS: [&str; 4] = [
    "claude-stop",
    "remote-stop",
    "codex-stop",
    "remote-codex-stop",
];
const TIMEOUT_SECONDS: u32 = 600;
const MAX_SETTINGS_BYTES: u64 = 4 * 1024 * 1024;

#[derive(Debug, Parser)]
struct Stop {
    #[command(subcommand)]
    command: StopCommand,
}

// The variant names are the subcommand names.
#[allow(clippy::enum_variant_names)]
#[derive(Debug, Subcommand)]
enum StopCommand {
    ClaudeStop(crate::skill_workflow_loop::LoopArgs),
    CodexStop(crate::skill_workflow_loop::LoopArgs),
    RemoteStop(crate::hosted_workflow_client::HostedWorkflowArgs),
    RemoteCodexStop(crate::hosted_workflow_client::HostedWorkflowArgs),
}

fn invalid(message: impl Into<String>) -> Error {
    Error::Invalid(message.into())
}

pub fn run(
    data_dir: &Path,
    settings: &Path,
    apply: bool,
    remove: bool,
    workspace_filter: Option<&Path>,
    words: &[String],
) -> Result<Value> {
    if settings.to_str().is_none() {
        return Err(invalid("settings path is not valid UTF-8"));
    }
    if settings
        .symlink_metadata()
        .is_ok_and(|metadata| metadata.file_type().is_symlink())
    {
        return Err(invalid("settings path is a symbolic link; refusing it"));
    }
    let old = read_settings(settings)?;
    if remove {
        if !words.is_empty() {
            return Err(invalid("--remove takes no stop subcommand"));
        }
        if workspace_filter.is_some_and(|filter| filter.to_str().is_none()) {
            return Err(invalid("--workspace is not valid UTF-8"));
        }
        let mut new = old.clone();
        let removed = strip_ours(&mut new, workspace_filter)?;
        let applied = apply && removed > 0;
        if applied {
            write_atomic(settings, &new)?;
        }
        let action = if removed > 0 { "remove" } else { "unchanged" };
        return Ok(
            json!({"settings":settings,"action":action,"removed":removed,"applied":applied}),
        );
    }
    let (agent, workspace) = parse(words)?;
    if workspace_filter.is_some() {
        return Err(invalid("--workspace filters only --remove"));
    }
    refuse_inside(settings, &workspace)?;
    let data_dir = fs::canonicalize(data_dir)
        .map_err(|error| invalid(format!("data directory is not usable: {error}")))?;
    let data_dir = data_dir
        .to_str()
        .ok_or_else(|| invalid("data directory path is not valid UTF-8"))?;
    let exe = fs::canonicalize(std::env::current_exe()?)?;
    if exe.file_name().is_none_or(|name| name != "agentscm") {
        return Err(invalid("the running executable must be named agentscm"));
    }
    let mut args = vec![
        "--data-dir".into(),
        data_dir.into(),
        "skill-workflow".into(),
    ];
    args.extend(words.iter().cloned());
    let exe = exe
        .to_str()
        .ok_or_else(|| invalid("executable path is not valid UTF-8"))?;
    let group = json!({"hooks":[render(agent, exe, &args)]});
    let mut new = old.clone();
    let removed = strip_ours(&mut new, Some(&workspace))?;
    // An identical group that is the only entry for this workspace stays where it is.
    let present = removed == 1
        && old["hooks"]["Stop"]
            .as_array()
            .is_some_and(|stop| stop.contains(&group));
    if present {
        new = old.clone();
    } else {
        append(&mut new, group.clone());
    }
    let action = match (removed, present) {
        (_, true) => "unchanged",
        (0, _) => "add",
        _ => "replace",
    };
    let applied = apply && action != "unchanged";
    if applied {
        write_atomic(settings, &new)?;
    }
    Ok(
        json!({"settings":settings,"agent":agent.name(),"action":action,"applied":applied,"entry":group}),
    )
}

/// The agent and workspace of a trailing command that is exactly one valid Stop subcommand.
fn parse(words: &[String]) -> Result<(StopAgent, PathBuf)> {
    let named = words
        .first()
        .is_some_and(|w| SUBCOMMANDS.contains(&w.as_str()));
    if !named {
        return Err(invalid(format!(
            "expected one of {} after --",
            SUBCOMMANDS.join(", ")
        )));
    }
    let stop =
        Stop::try_parse_from(std::iter::once("agentscm").chain(words.iter().map(String::as_str)))
            .map_err(|error| invalid(error.to_string()))?;
    let local = |agent, args: crate::skill_workflow_loop::LoopArgs| {
        let absolute = vec![
            ("--docker", args.docker),
            ("--docker-socket", args.docker_socket),
            ("--claude", args.claude),
        ];
        (agent, args.workspace, absolute)
    };
    let (agent, workspace, absolute) = match stop.command {
        StopCommand::ClaudeStop(args) => local(StopAgent::Claude, args),
        StopCommand::CodexStop(args) => local(StopAgent::Codex, args),
        StopCommand::RemoteStop(args) => (
            StopAgent::Claude,
            Some(args.workspace),
            vec![("--token-file", args.token_file)],
        ),
        StopCommand::RemoteCodexStop(args) => (
            StopAgent::Codex,
            Some(args.workspace),
            vec![("--token-file", args.token_file)],
        ),
    };
    let workspace = workspace
        .filter(|path| path.is_absolute())
        .ok_or_else(|| invalid("--workspace is required and must be an absolute path"))?;
    for (flag, path) in absolute {
        if !path.is_absolute() {
            return Err(invalid(format!("{flag} must be an absolute path")));
        }
    }
    if workspace.to_str().is_none() {
        return Err(invalid("--workspace is not valid UTF-8"));
    }
    Ok((agent, workspace))
}

/// Canonical path of `path` through its deepest existing ancestor.
fn resolve(path: &Path) -> Result<PathBuf> {
    let mut rest = Vec::new();
    let mut existing = path;
    while existing.symlink_metadata().is_err() {
        rest.push(
            existing
                .file_name()
                .ok_or_else(|| invalid("unusable settings path"))?,
        );
        existing = existing
            .parent()
            .ok_or_else(|| invalid("unusable settings path"))?;
    }
    let mut real = fs::canonicalize(if existing.as_os_str().is_empty() {
        Path::new(".")
    } else {
        existing
    })?;
    real.extend(rest.iter().rev());
    Ok(real)
}

/// Local hook files hold absolute paths, so they must never be part of checkpointed source.
fn refuse_inside(settings: &Path, workspace: &Path) -> Result<()> {
    let workspace = fs::canonicalize(workspace)
        .map_err(|error| invalid(format!("workspace is not usable: {error}")))?;
    let absolute = std::path::absolute(settings)?;
    let parent = absolute
        .parent()
        .ok_or_else(|| invalid("unusable settings path"))?;
    if resolve(parent)?.starts_with(&workspace) {
        return Err(invalid(
            "settings file is inside the workspace; keep hook config outside candidate source",
        ));
    }
    Ok(())
}

fn read_settings(path: &Path) -> Result<Value> {
    let mut bytes = Vec::new();
    match fs::File::open(path) {
        Ok(file) => file.take(MAX_SETTINGS_BYTES + 1).read_to_end(&mut bytes)?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(json!({})),
        Err(error) => return Err(error.into()),
    };
    if bytes.len() as u64 > MAX_SETTINGS_BYTES {
        return Err(Error::Limit("settings file exceeds 4 MiB".into()));
    }
    let value: Value = serde_json::from_slice(&bytes)
        .map_err(|error| invalid(format!("settings file is not plain JSON: {error}")))?;
    check_shape(&value)?;
    Ok(value)
}

fn check_shape(root: &Value) -> Result<()> {
    let object = root
        .as_object()
        .ok_or_else(|| invalid("settings root must be an object"))?;
    let Some(hooks) = object.get("hooks") else {
        return Ok(());
    };
    let hooks = hooks
        .as_object()
        .ok_or_else(|| invalid("hooks must be an object"))?;
    if hooks.get("Stop").is_some_and(|stop| !stop.is_array()) {
        return Err(invalid("hooks.Stop must be an array"));
    }
    Ok(())
}

fn plain(c: char) -> bool {
    c.is_ascii_alphanumeric() || "_./=:@%+,-".contains(c)
}

/// Quotes a word for a POSIX shell unless it is plainly safe.
fn quote(word: &str) -> String {
    if !word.is_empty() && word.chars().all(plain) {
        word.into()
    } else {
        format!("'{}'", word.replace('\'', "'\\''"))
    }
}

/// Claude takes exec form; Codex has no `args`, so its command is one shell string.
fn render(agent: StopAgent, exe: &str, args: &[String]) -> Value {
    match agent {
        StopAgent::Claude => {
            json!({"type":"command","command":exe,"args":args,"timeout":TIMEOUT_SECONDS})
        }
        StopAgent::Codex => {
            let words = std::iter::once(exe).chain(args.iter().map(String::as_str));
            let command = words.map(quote).collect::<Vec<_>>().join(" ");
            json!({"type":"command","command":command,"timeout":TIMEOUT_SECONDS})
        }
    }
}

fn is_agentscm(program: &str) -> bool {
    Path::new(program)
        .file_name()
        .is_some_and(|name| name == "agentscm")
}

/// Decodes exactly the grammar `quote` writes: single-space separated words, each bare (safe
/// characters only) or wholly single-quoted with `'\''` as the only escape. Anything else is None.
fn decode(command: &str) -> Option<Vec<String>> {
    let chars: Vec<char> = command.chars().collect();
    let mut words = Vec::new();
    let mut at = 0;
    loop {
        let mut word = String::new();
        if chars.get(at) == Some(&'\'') {
            at += 1;
            loop {
                match chars.get(at)? {
                    '\'' if chars[at + 1..].starts_with(&['\\', '\'', '\'']) => {
                        word.push('\'');
                        at += 4;
                    }
                    '\'' => {
                        at += 1;
                        break;
                    }
                    c => {
                        word.push(*c);
                        at += 1;
                    }
                }
            }
        } else {
            while let Some(c) = chars.get(at).filter(|c| **c != ' ') {
                if !plain(*c) {
                    return None;
                }
                word.push(*c);
                at += 1;
            }
            if word.is_empty() {
                return None;
            }
        }
        words.push(word);
        match chars.get(at) {
            None => return Some(words),
            Some(' ') => at += 1,
            Some(_) => return None,
        }
    }
}

/// The arguments of `hook` when install-stop wrote it: an `agentscm` program running
/// `--data-dir D skill-workflow <stop subcommand> ...`, in exec form or the decoded string form.
fn ours_args(hook: &Value) -> Option<Vec<String>> {
    let command = hook["command"].as_str()?;
    let (program, args) = if let Some(args) = hook.get("args") {
        let args = args
            .as_array()?
            .iter()
            .map(|arg| arg.as_str().map(String::from))
            .collect::<Option<Vec<_>>>()?;
        (command.to_owned(), args)
    } else {
        let mut words = decode(command)?;
        (words.remove(0), words)
    };
    (is_agentscm(&program)
        && args.len() >= 4
        && args[0] == "--data-dir"
        && args[2] == "skill-workflow"
        && SUBCOMMANDS.contains(&args[3].as_str()))
    .then_some(args)
}

/// The `--workspace` value among the arguments after the subcommand.
fn workspace_of(args: &[String]) -> Option<&str> {
    let mut rest = args.iter().skip(4);
    while let Some(arg) = rest.next() {
        if arg == "--workspace" {
            return rest.next().map(String::as_str);
        }
        if let Some(value) = arg.strip_prefix("--workspace=") {
            return Some(value);
        }
    }
    None
}

/// Equal after canonicalization when both resolve, else as raw strings.
fn same_workspace(a: &str, b: &str) -> bool {
    match (fs::canonicalize(a), fs::canonicalize(b)) {
        (Ok(a), Ok(b)) => a == b,
        _ => a == b,
    }
}

fn is_selected(hook: &Value, workspace: Option<&Path>) -> bool {
    let Some(args) = ours_args(hook) else {
        return false;
    };
    workspace.is_none_or(|filter| {
        workspace_of(&args)
            .zip(filter.to_str())
            .is_some_and(|(ours, filter)| same_workspace(ours, filter))
    })
}

/// Removes our hooks (only those for `workspace` when given) from `hooks.Stop`, dropping only
/// containers that removal emptied.
fn strip_ours(root: &mut Value, workspace: Option<&Path>) -> Result<usize> {
    check_shape(root)?;
    let mut removed = 0;
    let Some(hooks) = root.get_mut("hooks").and_then(Value::as_object_mut) else {
        return Ok(0);
    };
    if let Some(stop) = hooks.get_mut("Stop").and_then(Value::as_array_mut) {
        stop.retain_mut(|group| {
            let Some(inner) = group.get_mut("hooks").and_then(Value::as_array_mut) else {
                return true;
            };
            let before = inner.len();
            inner.retain(|hook| !is_selected(hook, workspace));
            removed += before - inner.len();
            before == inner.len() || !inner.is_empty()
        });
        if removed > 0 && stop.is_empty() {
            hooks.remove("Stop");
        }
    }
    if removed > 0 && hooks.is_empty() {
        root.as_object_mut().map(|object| object.remove("hooks"));
    }
    Ok(removed)
}

fn append(root: &mut Value, group: Value) {
    if let Some(object) = root.as_object_mut() {
        let hooks = object.entry("hooks").or_insert_with(|| json!({}));
        if let Some(stop) = hooks
            .as_object_mut()
            .map(|hooks| hooks.entry("Stop").or_insert_with(|| json!([])))
            .and_then(Value::as_array_mut)
        {
            stop.push(group);
        }
    }
}

/// Temp file plus rename in the same directory; keeps an existing file's mode, else 0600.
fn write_atomic(path: &Path, value: &Value) -> Result<()> {
    let mode = fs::metadata(path).map_or(0o600, |metadata| metadata.permissions().mode() & 0o7777);
    let path = std::path::absolute(path)?;
    let dir = path
        .parent()
        .ok_or_else(|| invalid("unusable settings path"))?;
    fs::create_dir_all(dir)?;
    let name = path
        .file_name()
        .ok_or_else(|| invalid("unusable settings path"))?;
    let temp = dir.join(format!(
        ".{}.agentscm-{}.tmp",
        name.to_string_lossy(),
        std::process::id()
    ));
    let text = serde_json::to_string_pretty(value)? + "\n";
    let written = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(mode)
        .open(&temp)
        .and_then(|mut file| {
            file.write_all(text.as_bytes())?;
            file.sync_all()?;
            fs::set_permissions(&temp, fs::Permissions::from_mode(mode))?;
            fs::rename(&temp, &path)
        });
    if written.is_err() {
        let _ = fs::remove_file(&temp);
    }
    Ok(written?)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(words: &[&str]) -> Vec<String> {
        words.iter().map(|w| (*w).into()).collect()
    }

    #[test]
    fn renders_exec_form_for_claude_and_quoted_string_for_codex() {
        let words = args(&["--data-dir", "/d x/it's", "skill-workflow", ""]);
        let claude = render(StopAgent::Claude, "/bin/agentscm", &words);
        assert_eq!(claude["command"], "/bin/agentscm");
        assert_eq!(claude["args"][1], "/d x/it's");
        assert_eq!(claude["timeout"], 600);
        let codex = render(StopAgent::Codex, "/a b/agentscm", &words);
        assert_eq!(
            codex["command"],
            r"'/a b/agentscm' --data-dir '/d x/it'\''s' skill-workflow ''"
        );
        assert!(codex.get("args").is_none());
    }

    fn is_ours(hook: &Value) -> bool {
        ours_args(hook).is_some()
    }

    fn stop_args(sub: &str) -> Vec<String> {
        args(&[
            "--data-dir",
            "/d",
            "skill-workflow",
            sub,
            "c",
            "--workspace",
            "/w",
        ])
    }

    #[test]
    fn recognizes_only_our_stop_hooks() {
        let claude = render(StopAgent::Claude, "/x/agentscm", &stop_args("remote-stop"));
        let codex = render(StopAgent::Codex, "/x y/agentscm", &stop_args("codex-stop"));
        assert!(is_ours(&claude) && is_ours(&codex));
        assert_eq!(workspace_of(&ours_args(&codex).unwrap()), Some("/w"));
        for command in [
            "agentscm hook start",
            "agentscm intent start --hook claude",
            "agentscm skill-workflow check c",
            "agentscm skill-workflow claude-stop",
            "other --data-dir /d skill-workflow claude-stop",
            "agentscm --data-dir /d skill-workflow claude-stopper",
            // A data directory that merely contains the words is not a subcommand.
            "agentscm --data-dir '/tmp/ skill-workflow codex-stop data' hook generic",
            r#""agentscm" --data-dir /d skill-workflow codex-stop c"#,
            r"agentscm --data-dir /d\ x skill-workflow codex-stop c",
            "FOO=1 agentscm --data-dir /d skill-workflow codex-stop c",
            "agentscm  --data-dir /d skill-workflow codex-stop c",
            "agentscm --data-dir /d skill-workflow codex-stop c ",
            "'agentscm'x --data-dir /d skill-workflow codex-stop c",
            "'agentscm --data-dir /d skill-workflow codex-stop c",
            "",
        ] {
            assert!(
                !is_ours(&json!({"type":"command","command":command})),
                "{command}"
            );
        }
        assert!(!is_ours(
            &json!({"command":"/x/other","args":["--data-dir","/d","skill-workflow","claude-stop"]})
        ));
        assert!(!is_ours(
            &json!({"command":"/x/agentscm","args":["skill-workflow","claude-stop"]})
        ));
        assert!(!is_ours(
            &json!({"command":"/x/agentscm","args":["--data-dir","/d","skill-workflow","claude-stop",1]})
        ));
        assert!(!is_ours(
            &json!({"type":"http","url":"https://h/v1/hub/hook/x"})
        ));
    }

    #[test]
    fn decodes_exactly_the_quote_grammar() {
        for word in ["", "a b", "it's", "'", "''", "x'", "$(y) \\ \"z\"", "é"] {
            let line = format!("{} {} tail", quote(word), quote(word));
            assert_eq!(decode(&line), Some(args(&[word, word, "tail"])), "{line}");
        }
        for bad in [
            "a  b", " a", "a ", "a\\b", "\"a\"", "a'b'", "'a'b", "'a", "a$b",
        ] {
            assert_eq!(decode(bad), None, "{bad}");
        }
    }

    fn ours() -> Value {
        json!({"hooks":[render(StopAgent::Claude, "/x/agentscm", &stop_args("claude-stop"))]})
    }

    #[test]
    fn strip_keeps_foreign_hooks_and_drops_only_emptied_containers() {
        let foreign = json!({"hooks":[{"type":"command","command":"keep"}]});
        let mixed = json!({"matcher":"m","hooks":[{"type":"command","command":"keep"}, ours()["hooks"][0]]});
        let mut root = json!({"x":1,"hooks":{"Stop":[foreign, ours(), mixed],"Other":[ours()]}});
        assert_eq!(strip_ours(&mut root, None).unwrap(), 2);
        assert_eq!(root["hooks"]["Stop"].as_array().unwrap().len(), 2);
        assert_eq!(
            root["hooks"]["Stop"][1]["hooks"].as_array().unwrap().len(),
            1
        );
        assert_eq!(root["hooks"]["Other"], json!([ours()]));
        let mut only = json!({"hooks":{"Stop":[ours()]}});
        append(&mut only, ours());
        assert_eq!(strip_ours(&mut only, None).unwrap(), 2);
        assert_eq!(only, json!({}));
        let mut empty_group = json!({"hooks":{"Stop":[{"hooks":[]}]}});
        assert_eq!(strip_ours(&mut empty_group, None).unwrap(), 0);
        assert_eq!(empty_group, json!({"hooks":{"Stop":[{"hooks":[]}]}}));
    }

    #[test]
    fn rejects_wrong_shapes_and_other_subcommands() {
        for bad in [json!([]), json!({"hooks":[]}), json!({"hooks":{"Stop":{}}})] {
            assert!(strip_ours(&mut bad.clone(), None).is_err());
        }
        for words in [args(&[]), args(&["check", "c"]), args(&["claude-stop"])] {
            assert!(parse(&words).is_err());
        }
    }
}
