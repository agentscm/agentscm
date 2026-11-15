//! Which agent conversation a process belongs to, found without the model's help.
//!
//! An agent host (Claude Code, Codex and the like) starts AgentSCM twice: its hooks, which are
//! given the native session ID, and the checkout's MCP server, which is not. Both descend from
//! the same host process, so the start hook records the session under that process and the MCP
//! server looks through its own ancestors for it. Reading the process table needs `ps`, so this
//! is Unix only: elsewhere, and where `ps` is missing, nothing is recorded or found.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use serde_json::{Value, json};

/// How far up the MCP server looks: a launcher or two (`npx`, a shell) sit above the host.
const MAX_ANCESTORS: usize = 8;
const MAX_RECORDS: usize = 512;
const RECORD_BYTES: u64 = 4096;
const SHELLS: [&str; 8] = ["sh", "bash", "zsh", "dash", "fish", "ksh", "csh", "tcsh"];

/// A running process. The start time tells it from a later process that reuses its id.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Process {
    pub(crate) pid: u32,
    pub(crate) started: String,
    pub(crate) name: String,
}

/// Each process by id, with its parent's id.
type Table = HashMap<u32, (u32, Process)>;

/// One `pid ppid lstart comm` line per process. `lstart` is five words in the C locale, and
/// `comm` may hold spaces.
#[cfg(any(unix, test))]
fn parse(text: &str) -> Table {
    text.lines()
        .filter_map(|line| {
            let mut words = line.split_whitespace();
            let pid = words.next()?.parse().ok()?;
            let parent = words.next()?.parse().ok()?;
            let started = words.by_ref().take(5).collect::<Vec<_>>();
            let name = words.collect::<Vec<_>>().join(" ");
            (started.len() == 5 && !name.is_empty()).then(|| {
                let started = started.join(" ");
                (pid, (parent, Process { pid, started, name }))
            })
        })
        .collect()
}

#[cfg(unix)]
fn table() -> Option<Table> {
    let output = std::process::Command::new("ps")
        .args(["-axo", "pid=,ppid=,lstart=,comm="])
        .env("LC_ALL", "C")
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .output()
        .ok()?;
    output
        .status
        .success()
        .then(|| parse(&String::from_utf8_lossy(&output.stdout)))
}

#[cfg(not(unix))]
fn table() -> Option<Table> {
    None
}

/// The ancestors of `pid`, nearest first.
fn chain(table: &Table, pid: u32) -> Vec<Process> {
    let mut found = Vec::new();
    let mut next = table.get(&pid).map(|(parent, _)| *parent);
    while let Some((parent, process)) = next.and_then(|pid| table.get(&pid)) {
        found.push(process.clone());
        if found.len() == MAX_ANCESTORS {
            break;
        }
        next = Some(*parent);
    }
    found
}

/// The ancestors of this process, nearest first; none when the process table cannot be read.
pub(crate) fn ancestors() -> Vec<Process> {
    table().map_or_else(Vec::new, |table| chain(&table, std::process::id()))
}

/// The agent host among a hook's ancestors: the nearest one that is not a shell running the
/// hook command. A process further up may host several agents, so it is never recorded.
fn host(ancestors: &[Process]) -> Option<&Process> {
    ancestors.iter().find(|process| {
        let name = process.name.rsplit('/').next().unwrap_or_default();
        !SHELLS.contains(&name.trim_start_matches('-'))
    })
}

/// Claude configuration can be inherited by another host; it does not identify that host.
pub(crate) async fn hook_agent<'a>(configured: &'a str, payload: &Value) -> Option<&'a str> {
    if configured != "claude" {
        return Some(configured);
    }
    let foreign_environment = ["CURSOR_VERSION", "CURSOR_PROJECT_DIR", "DEVIN_PROJECT_DIR"]
        .iter()
        .any(|key| std::env::var_os(key).is_some_and(|value| !value.is_empty()));
    let ancestors = tokio::time::timeout(
        std::time::Duration::from_millis(200),
        tokio::task::spawn_blocking(ancestors),
    )
    .await
    .ok()
    .and_then(Result::ok)
    .unwrap_or_default();
    let versions = std::env::var_os("HOME")
        .map(|home| PathBuf::from(home).join(".local/share/claude/versions"))
        .and_then(|path| path.canonicalize().ok());
    let process =
        host(&ancestors).and_then(|process| hook_process_name(&process.name, versions.as_deref()));
    inherited_hook_agent(payload, process, foreign_environment)
}

fn hook_process_name<'a>(process: &'a str, versions: Option<&Path>) -> Option<&'a str> {
    let path = Path::new(process);
    if versions.is_some_and(|versions| path.parent() == Some(versions)) {
        return Some("claude");
    }
    path.file_stem()?.to_str()
}

fn inherited_hook_agent(
    payload: &Value,
    process: Option<&str>,
    foreign_environment: bool,
) -> Option<&'static str> {
    let cursor = payload.get("cursor_version").is_some()
        || payload.get("conversation_id").is_some()
        || payload.get("workspace_roots").is_some();
    let copilot = payload.get("sessionId").is_some();
    let text = |key| {
        payload[key].as_str().filter(|text| {
            !text.is_empty() && text.len() <= 1024 && !text.chars().any(char::is_control)
        })
    };
    if cursor && (text("cursor_version").is_none() || text("conversation_id").is_none())
        || copilot && text("sessionId").is_none()
    {
        return None;
    }
    let mut sessions = ["session_id", "conversation_id", "sessionId"]
        .into_iter()
        .filter(|key| payload.get(*key).is_some());
    if let Some(first) = sessions.next() {
        let first = text(first)?;
        if sessions.any(|key| text(key) != Some(first)) {
            return None;
        }
    }
    let shaped = match (cursor, copilot) {
        (true, false) => Some("cursor"),
        (false, true) => Some("copilot"),
        (true, true) => return None,
        (false, false) => None,
    };
    // Only the nearest non-shell process counts: a host may itself launch another host.
    let observed = match process {
        Some("claude") => Some("claude"),
        Some("cursor" | "cursor-agent") => Some("cursor"),
        Some("copilot") => Some("copilot"),
        Some("devin") => return None,
        _ => None,
    };
    match (shaped, observed) {
        (Some(a), Some(b)) if a != b => None,
        (Some(agent), _) | (_, Some(agent)) => Some(agent),
        (None, None) if foreign_environment => None,
        // shortcut: identical PascalCase payloads have no host discriminator; use the
        // configured adapter until the host supplies one or its executable is recognizable.
        (None, None) => Some("claude"),
    }
}

pub(crate) fn directory(config: &Path) -> PathBuf {
    config.join("agentscm").join("agent-hosts")
}

/// The file-name part for `session`: a hash, so any session text makes a safe name.
fn tag(session: &str) -> String {
    use sha2::{Digest, Sha256};
    format!("{:x}", Sha256::digest(session))[..16].to_owned()
}

struct Record {
    pid: u32,
    started: String,
    session: String,
    file: PathBuf,
}

/// Every readable record. One file per host and session, so adding or removing a session
/// never rewrites another's file.
fn records(directory: &Path) -> Vec<Record> {
    use std::io::Read;
    std::fs::read_dir(directory)
        .into_iter()
        .flatten()
        .flatten()
        .take(MAX_RECORDS)
        .filter_map(|entry| {
            let file = entry.path();
            let pid = file
                .file_name()?
                .to_str()?
                .split('.')
                .next()?
                .parse()
                .ok()?;
            let mut text = String::new();
            std::fs::File::open(&file)
                .ok()?
                .take(RECORD_BYTES)
                .read_to_string(&mut text)
                .ok()?;
            let record: Value = serde_json::from_str(&text).ok()?;
            Some(Record {
                pid,
                started: record["started"].as_str()?.to_owned(),
                session: record["session"].as_str()?.to_owned(),
                file,
            })
        })
        .collect()
}

/// Records that `host` has `session` open.
pub(crate) fn record_under(directory: &Path, host: &Process, session: &str) -> std::io::Result<()> {
    use std::io::Write;
    let mut builder = std::fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    std::os::unix::fs::DirBuilderExt::mode(&mut builder, 0o700);
    builder.create(directory)?;
    // Written whole and renamed, so the MCP server never reads half a record.
    let mut file = tempfile::NamedTempFile::new_in(directory)?;
    let record = json!({"started": host.started, "session": session});
    file.write_all(record.to_string().as_bytes())?;
    file.persist(directory.join(format!("{}.{}", host.pid, tag(session))))?;
    Ok(())
}

/// Records that the agent host this hook runs under has `session` open, and drops the records
/// of hosts that have exited. Best effort: a failure only leaves the session unrecorded.
pub(crate) fn record(config: &Path, session: &str) {
    let Some(table) = table() else {
        return;
    };
    let directory = directory(config);
    for record in records(&directory) {
        let running = table
            .get(&record.pid)
            .is_some_and(|(_, process)| process.started == record.started);
        if !running {
            let _ = std::fs::remove_file(record.file);
        }
    }
    if let Some(host) = host(&chain(&table, std::process::id())) {
        let _ = record_under(&directory, host, session);
    }
}

fn routes(config: &Path) -> PathBuf {
    config.join("agentscm").join("session-routes")
}

/// How long a route record that no hook rewrote survives a later write.
const ROUTE_KEEP: std::time::Duration = std::time::Duration::from_secs(30 * 24 * 60 * 60);

/// The file-name part for `session`'s route in one repository.
fn route_tag(session: &str, hub: &str, repo: &str) -> String {
    // Repository names compare without case elsewhere (`same_repository`).
    tag(&format!("{session}\0{hub}\0{}", repo.to_ascii_lowercase()))
}

/// The text of one route record, when it is readable.
fn route_record(file: &Path) -> Option<Value> {
    use std::io::Read;
    let mut text = String::new();
    std::fs::File::open(file)
        .ok()?
        .take(RECORD_BYTES)
        .read_to_string(&mut text)
        .ok()?;
    serde_json::from_str(&text).ok()
}

/// The root of the connection `session`'s hooks route to in `hub`'s `repo`, if one is
/// recorded. The record is data a hook wrote, so the caller checks it against the saved
/// connections.
pub(crate) fn route_of(config: &Path, session: &str, hub: &str, repo: &str) -> Option<PathBuf> {
    let record = route_record(&routes(config).join(route_tag(session, hub, repo)))?;
    let root = PathBuf::from(record["root"].as_str()?);
    (record["session"].as_str()? == session
        && record["hub"].as_str()? == hub
        && record["repo"].as_str()?.eq_ignore_ascii_case(repo)
        && root.is_absolute())
    .then_some(root)
}

/// Records that `session`'s hooks route to the connection at `root` for `hub`'s `repo`, so the
/// MCP server names the same agent. Only the root is stored, never a token path. Nothing is
/// written when the record already says so; a write also drops the oldest records beyond the
/// bound.
pub(crate) fn record_route(
    config: &Path,
    session: &str,
    hub: &str,
    repo: &str,
    root: &Path,
) -> std::io::Result<()> {
    use std::io::Write;
    if route_of(config, session, hub, repo).as_deref() == Some(root) {
        return Ok(());
    }
    let directory = routes(config);
    let mut builder = std::fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    std::os::unix::fs::DirBuilderExt::mode(&mut builder, 0o700);
    builder.create(&directory)?;
    let mut file = tempfile::NamedTempFile::new_in(&directory)?;
    let record = json!({"session": session, "hub": hub, "repo": repo, "root": root});
    file.write_all(record.to_string().as_bytes())?;
    file.persist(directory.join(route_tag(session, hub, repo)))?;
    let mut kept: Vec<_> = std::fs::read_dir(&directory)?
        .flatten()
        .filter_map(|entry| {
            let modified = entry.metadata().ok()?.modified().ok()?;
            Some((modified, entry.path()))
        })
        .collect();
    kept.sort();
    let excess = kept.len().saturating_sub(MAX_RECORDS);
    for (index, (modified, file)) in kept.into_iter().enumerate() {
        let old = modified.elapsed().is_ok_and(|age| age > ROUTE_KEEP);
        if index < excess || old {
            let _ = std::fs::remove_file(file);
        }
    }
    Ok(())
}

/// A successful explicit Work start, separate from whichever repository a tool last touched.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct CaptureAssociation {
    pub session: String,
    pub harness: agentscm::change_record::AgentHarnessV1,
    pub hub: String,
    pub repo: String,
    pub checkout: PathBuf,
    pub work_id: String,
    pub generation: i64,
    #[serde(default)]
    pub native_start_verified: bool,
}

impl CaptureAssociation {
    fn valid(&self) -> bool {
        [&self.session, &self.work_id].iter().all(|value| {
            !value.is_empty() && value.len() <= 128 && !value.chars().any(char::is_control)
        }) && self.generation > 0
            && self.checkout.is_absolute()
            && !self.hub.is_empty()
            && !self.repo.is_empty()
    }
}

fn capture_directory(config: &Path, session: &str) -> PathBuf {
    config
        .join("agentscm")
        .join("session-work")
        .join(tag(session))
}

pub(crate) fn capture_associations(config: &Path, session: &str) -> Vec<CaptureAssociation> {
    let mut associations: Vec<CaptureAssociation> =
        std::fs::read_dir(capture_directory(config, session))
            .into_iter()
            .flatten()
            .flatten()
            .take(MAX_RECORDS)
            .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_file()))
            .filter_map(|entry| {
                serde_json::from_value::<CaptureAssociation>(route_record(&entry.path())?).ok()
            })
            .filter(|record| record.session == session && record.valid())
            .collect();
    associations.sort_by(|a, b| {
        (&a.hub, &a.repo, &a.work_id, a.generation, &a.checkout).cmp(&(
            &b.hub,
            &b.repo,
            &b.work_id,
            b.generation,
            &b.checkout,
        ))
    });
    associations
}

pub(crate) fn record_capture(
    config: &Path,
    association: &CaptureAssociation,
) -> std::io::Result<()> {
    use std::io::Write;
    if !association.valid() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "invalid capture association",
        ));
    }
    let directory = capture_directory(config, &association.session);
    let name = tag(&format!(
        "{}\0{}\0{}\0{}",
        association.session,
        association.hub,
        association.repo.to_ascii_lowercase(),
        association.work_id
    ));
    let path = directory.join(name);
    let mut association = association.clone();
    if let Some(mut previous) = route_record(&path)
        .and_then(|value| serde_json::from_value::<CaptureAssociation>(value).ok())
        .filter(|previous| previous.native_start_verified)
    {
        previous.native_start_verified = association.native_start_verified;
        if previous == association {
            association.native_start_verified = true;
        }
    }
    if route_record(&path)
        .and_then(|value| serde_json::from_value::<CaptureAssociation>(value).ok())
        .as_ref()
        == Some(&association)
    {
        return Ok(());
    }
    let mut builder = std::fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    std::os::unix::fs::DirBuilderExt::mode(&mut builder, 0o700);
    builder.create(&directory)?;
    // Never evict another live association to admit a new one.
    if !path.exists() && std::fs::read_dir(&directory)?.take(MAX_RECORDS).count() >= MAX_RECORDS {
        return Err(std::io::Error::other("capture association limit reached"));
    }
    let bytes = serde_json::to_vec(&association)?;
    if bytes.len() > RECORD_BYTES as usize {
        return Err(std::io::Error::other("capture association is too large"));
    }
    let mut file = tempfile::NamedTempFile::new_in(&directory)?;
    file.write_all(&bytes)?;
    file.persist(path)?;
    Ok(())
}

/// Drops live host routes at session end. Explicit Work associations survive native resume.
pub(crate) fn forget(config: &Path, session: &str) {
    for entry in std::fs::read_dir(routes(config))
        .into_iter()
        .flatten()
        .take(MAX_RECORDS)
        .flatten()
    {
        let file = entry.path();
        if route_record(&file).is_some_and(|record| record["session"].as_str() == Some(session)) {
            let _ = std::fs::remove_file(file);
        }
    }
    for record in records(&directory(config)) {
        if record.session == session {
            let _ = std::fs::remove_file(record.file);
        }
    }
}

/// What the start hooks recorded for the nearest of `ancestors` that hosts an agent.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Recorded {
    /// The host has exactly this session open.
    One(String),
    /// The host has several conversations open, so the caller's cannot be told from the others.
    Several,
    /// No ancestor is a recorded host: no start hook ran under this process tree.
    None,
}

pub(crate) fn recorded(config: &Path, ancestors: &[Process]) -> Recorded {
    let records = records(&directory(config));
    for process in ancestors {
        let mut sessions = records
            .iter()
            .filter(|record| record.pid == process.pid && record.started == process.started)
            .map(|record| &record.session);
        match (sessions.next(), sessions.next()) {
            (Some(session), None) => return Recorded::One(session.clone()),
            (Some(_), Some(_)) => return Recorded::Several,
            _ => {}
        }
    }
    Recorded::None
}

#[cfg(test)]
mod tests {
    #[test]
    fn native_claude_version_executables_are_recognized_only_in_the_install_directory() {
        let versions = Path::new("/home/example/.local/share/claude/versions");
        let process = versions.join("2.1.37");
        assert_eq!(
            super::hook_process_name(process.to_str().unwrap(), Some(versions)),
            Some("claude")
        );
        assert_ne!(
            super::hook_process_name("/other/versions/2.1.37", Some(versions)),
            Some("claude")
        );
        assert_ne!(
            super::hook_process_name("/usr/bin/node", Some(versions)),
            Some("claude")
        );
    }
    #[test]
    fn inherited_hook_hosts_use_payload_and_nearest_process_not_config_origin() {
        let cursor = serde_json::json!({"hook_event_name":"preToolUse", "conversation_id":"s", "cursor_version":"1"});
        assert_eq!(
            super::inherited_hook_agent(&cursor, None, true),
            Some("cursor")
        );
        assert_eq!(
            super::inherited_hook_agent(&cursor, Some("claude"), true),
            None
        );
        let pascal = serde_json::json!({"hook_event_name":"PreToolUse", "session_id":"s"});
        assert_eq!(
            super::inherited_hook_agent(&pascal, Some("copilot"), false),
            Some("copilot")
        );
        assert_eq!(
            super::inherited_hook_agent(&pascal, Some("devin"), false),
            None
        );
        assert_eq!(super::inherited_hook_agent(&pascal, None, true), None);
        assert_eq!(
            super::inherited_hook_agent(&pascal, Some("claude"), true),
            Some("claude")
        );
        assert_eq!(
            super::inherited_hook_agent(&pascal, None, false),
            Some("claude")
        );
        let conflict = serde_json::json!({"conversation_id":"s", "sessionId":"s"});
        assert_eq!(super::inherited_hook_agent(&conflict, None, false), None);
        for bad in [
            serde_json::json!({"cursor_version":"1"}),
            serde_json::json!({"cursor_version":null, "conversation_id":"s"}),
            serde_json::json!({"cursor_version":"1", "conversation_id":"s", "session_id":"other"}),
            serde_json::json!({"sessionId":5}),
        ] {
            assert_eq!(super::inherited_hook_agent(&bad, None, false), None);
        }
    }
    use super::*;

    /// An absolute root on this platform: a route only keeps absolute roots.
    fn root(name: &str) -> PathBuf {
        if cfg!(windows) {
            PathBuf::from(format!(r"C:\work\{name}"))
        } else {
            PathBuf::from(format!("/work/{name}"))
        }
    }

    // An application that hosts two agents; the first runs its hook through a shell and its
    // MCP server through a launcher.
    const TABLE: &str = "    1     0 Fri Nov  6 10:22:18 2026     /sbin/launchd
  200     1 Mon Nov  9 09:00:00 2026     /Applications/Agent Host.app/Contents/MacOS/Agent Host
  300   200 Mon Nov  9 09:00:01 2026     /opt/agent/bin/agent
  310   300 Mon Nov  9 09:00:02 2026     -zsh
  311   310 Mon Nov  9 09:00:03 2026     agentscm
  320   300 Mon Nov  9 09:00:04 2026     node
  321   320 Mon Nov  9 09:00:05 2026     agentscm
  400   200 Mon Nov  9 09:10:00 2026     /opt/agent/bin/agent
  421   400 Mon Nov  9 09:10:05 2026     agentscm
";

    #[test]
    fn a_hook_and_the_mcp_server_of_one_host_meet_at_that_host() {
        let table = parse(TABLE);
        let hook = chain(&table, 311);
        let pids: Vec<u32> = hook.iter().map(|process| process.pid).collect();
        assert_eq!(pids, [310, 300, 200, 1]);
        // The shell that runs the hook command is skipped; a name with spaces is read whole.
        let host = host(&hook).unwrap();
        assert_eq!(host.pid, 300);
        assert!(table[&200].1.name.ends_with("MacOS/Agent Host"));

        let config = tempfile::tempdir().unwrap();
        let directory = directory(config.path());
        record_under(&directory, host, "native-1").unwrap();
        let server = chain(&table, 321);
        assert_eq!(
            recorded(config.path(), &server),
            Recorded::One("native-1".into())
        );
        // The second agent has no record, and never takes its sibling's: the application above
        // both is not a host.
        assert_eq!(recorded(config.path(), &chain(&table, 421)), Recorded::None);

        // A second conversation in one host cannot be told from the first; ending it can.
        record_under(&directory, host, "native-2").unwrap();
        assert_eq!(recorded(config.path(), &server), Recorded::Several);
        forget(config.path(), "native-2");
        assert_eq!(
            recorded(config.path(), &server),
            Recorded::One("native-1".into())
        );

        // A later process with the host's id is not the host.
        let mut reused = server.clone();
        reused[1].started = "Tue Nov 10 08:00:00 2026".into();
        assert_eq!(recorded(config.path(), &reused), Recorded::None);
    }

    #[test]
    fn a_session_route_is_written_once_read_back_and_forgotten() {
        let config = tempfile::tempdir().unwrap();
        let (one, two) = (&*root("one"), &*root("two"));
        let get = |session: &str| route_of(config.path(), session, "https://hub", "o/r");
        assert_eq!(get("native-1"), None);
        record_route(config.path(), "native-1", "https://hub", "o/r", one).unwrap();
        assert_eq!(get("native-1").as_deref(), Some(one));
        assert_eq!(get("native-2"), None);

        // The same root is a no-op: the file is not replaced.
        let file = routes(config.path()).join(route_tag("native-1", "https://hub", "o/r"));
        let before = std::fs::metadata(&file).unwrap().modified().unwrap();
        std::thread::sleep(std::time::Duration::from_millis(20));
        record_route(config.path(), "native-1", "https://hub", "o/r", one).unwrap();
        assert_eq!(
            std::fs::metadata(&file).unwrap().modified().unwrap(),
            before
        );
        record_route(config.path(), "native-1", "https://hub", "o/r", two).unwrap();
        assert_eq!(get("native-1").as_deref(), Some(two));

        // A record that names another session, hub or repository, or a relative root, is not
        // a route.
        for text in [
            r#"{"session":"other","hub":"https://hub","repo":"o/r","root":"/work/one"}"#,
            r#"{"session":"native-1","hub":"https://other","repo":"o/r","root":"/work/one"}"#,
            r#"{"session":"native-1","hub":"https://hub","repo":"o/x","root":"/work/one"}"#,
            r#"{"session":"native-1","hub":"https://hub","repo":"o/r","root":"one"}"#,
        ] {
            std::fs::write(&file, text).unwrap();
            assert_eq!(get("native-1"), None);
        }

        record_route(config.path(), "native-1", "https://hub", "o/r", one).unwrap();
        forget(config.path(), "native-1");
        assert_eq!(get("native-1"), None);
    }

    #[test]
    fn a_session_keeps_one_route_per_repository_and_forgets_them_all() {
        let config = tempfile::tempdir().unwrap();
        let (one, two) = (&*root("one"), &*root("two"));
        record_route(config.path(), "s", "https://hub", "o/a", one).unwrap();
        record_route(config.path(), "s", "https://hub", "o/b", two).unwrap();
        record_route(config.path(), "t", "https://hub", "o/a", two).unwrap();
        let get = |session: &str, repo: &str| route_of(config.path(), session, "https://hub", repo);
        assert_eq!(get("s", "o/a").as_deref(), Some(one));
        assert_eq!(get("s", "o/b").as_deref(), Some(two));
        forget(config.path(), "s");
        assert_eq!(get("s", "o/a"), None);
        assert_eq!(get("s", "o/b"), None);
        assert_eq!(get("t", "o/a").as_deref(), Some(two));
    }

    #[test]
    fn session_routes_are_bounded_by_dropping_the_oldest() {
        let config = tempfile::tempdir().unwrap();
        let root = &*root("one");
        for index in 0..=MAX_RECORDS {
            record_route(
                config.path(),
                &format!("s{index}"),
                "https://hub",
                "o/r",
                root,
            )
            .unwrap();
        }
        let count = std::fs::read_dir(routes(config.path())).unwrap().count();
        assert_eq!(count, MAX_RECORDS);
        assert!(
            route_of(
                config.path(),
                &format!("s{MAX_RECORDS}"),
                "https://hub",
                "o/r"
            )
            .is_some()
        );
    }

    #[test]
    fn explicit_capture_associations_fan_out_by_work_and_survive_resume() {
        let config = tempfile::tempdir().unwrap();
        let one = CaptureAssociation {
            session: "native".into(),
            harness: agentscm::change_record::AgentHarnessV1::Codex,
            hub: "https://hub".into(),
            repo: "org/one".into(),
            checkout: root("one"),
            work_id: "wrk_one".into(),
            generation: 1,
            native_start_verified: false,
        };
        let two = CaptureAssociation {
            repo: "org/two".into(),
            checkout: root("two"),
            work_id: "wrk_two".into(),
            ..one.clone()
        };
        let newer = CaptureAssociation {
            generation: 2,
            ..one.clone()
        };
        for record in [&one, &two, &one, &newer] {
            record_capture(config.path(), record).unwrap();
        }
        let records = capture_associations(config.path(), "native");
        assert_eq!(records.len(), 2);
        assert!(!records.contains(&one) && records.contains(&two) && records.contains(&newer));
        assert!(capture_associations(config.path(), "other").is_empty());
        record_route(
            config.path(),
            "native",
            "https://hub",
            "org/three",
            &root("three"),
        )
        .unwrap();
        assert_eq!(capture_associations(config.path(), "native").len(), 2);
        let bad = CaptureAssociation {
            checkout: PathBuf::from("relative"),
            ..one.clone()
        };
        assert!(record_capture(config.path(), &bad).is_err());
        let mut verified = newer.clone();
        verified.native_start_verified = true;
        record_capture(config.path(), &verified).unwrap();
        record_capture(config.path(), &newer).unwrap();
        assert!(capture_associations(config.path(), "native").contains(&verified));
        let resumed = CaptureAssociation {
            generation: 3,
            ..newer.clone()
        };
        record_capture(config.path(), &resumed).unwrap();
        assert!(capture_associations(config.path(), "native").contains(&resumed));
        forget(config.path(), "native");
        assert_eq!(capture_associations(config.path(), "native").len(), 2);
    }

    #[cfg(unix)]
    #[test]
    fn this_process_has_ancestors_where_ps_runs() {
        // Skipped, not failed, on a host without `ps`: nothing is recorded or found there.
        if table().is_some() {
            assert!(!ancestors().is_empty());
        }
    }
}
