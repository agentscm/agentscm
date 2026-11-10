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

/// Drops `session` from every host, and every repository's route, when its conversation ends.
pub(crate) fn forget(config: &Path, session: &str) {
    for entry in std::fs::read_dir(routes(config))
        .into_iter()
        .flatten()
        .flatten()
        .take(MAX_RECORDS)
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
    use super::*;

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
        let (one, two) = (Path::new("/work/one"), Path::new("/work/two"));
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
        let (one, two) = (Path::new("/work/one"), Path::new("/work/two"));
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
        let root = Path::new("/work/one");
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

    #[cfg(unix)]
    #[test]
    fn this_process_has_ancestors_where_ps_runs() {
        // Skipped, not failed, on a host without `ps`: nothing is recorded or found there.
        if table().is_some() {
            assert!(!ancestors().is_empty());
        }
    }
}
