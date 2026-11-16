//! Which agent conversation a process belongs to, found without the model's help.
//!
//! An agent host (Claude Code, Codex and the like) starts AgentSCM twice: its hooks, which are
//! given the native session ID, and the checkout's MCP server, which is not. Both descend from
//! the same host process, so the start hook records the session under that process and the MCP
//! server looks through its own ancestors for it. Reading the process table needs `ps`, so this
//! is Unix only: elsewhere, and where `ps` is missing, nothing is recorded or found.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};

use agentscm::hub::session_lifecycle::Qualification;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub(crate) struct HostIdentity {
    pub(crate) pid: u32,
    pub(crate) started: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum HostObservation {
    Same,
    Exited,
    Unknown,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct SessionContext {
    pub(crate) session: String,
    pub(crate) start_nonce: String,
    pub(crate) incarnation: Option<String>,
    pub(crate) agent: String,
    pub(crate) hub: String,
    pub(crate) repo: String,
    pub(crate) root: PathBuf,
    pub(crate) protocol: u8,
    #[serde(default)]
    pub(crate) protocol_checked_at: i64,
    #[serde(default)]
    pub(crate) terminal: bool,
    #[serde(default)]
    pub(crate) registered: bool,
    #[serde(default)]
    pub(crate) qualification: Option<Qualification>,
    /// Local exit maintenance stopped; this does not assert a terminal Hub session.
    #[serde(default)]
    pub(crate) exit_ineligible: bool,
    #[serde(default)]
    pub(crate) revoked: bool,
    #[serde(default)]
    pub(crate) exit_observed_at: Option<i64>,
    #[serde(default)]
    pub(crate) explicit_end_requested: bool,
    pub(crate) observed_at: i64,
    pub(crate) host: Option<HostIdentity>,
    /// Current process evidence for registration replay only; never qualifies terminal effects.
    #[serde(default)]
    pub(crate) registration_host: Option<HostIdentity>,
    /// The existing saved connection's token identity, never its credential contents.
    pub(crate) credential_id: String,
    pub(crate) binding_id: String,
}

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

#[cfg(any(unix, test))]
fn complete_table(text: &str) -> Option<Table> {
    let mut table = Table::new();
    for line in text.lines().filter(|line| !line.trim().is_empty()) {
        let parsed = parse(line);
        if parsed.len() != 1 {
            return None;
        }
        for (pid, value) in parsed {
            if table.insert(pid, value).is_some() {
                return None;
            }
        }
    }
    (!table.is_empty()).then_some(table)
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
    if !output.status.success() || output.stdout.len() > 4 * 1024 * 1024 {
        return None;
    }
    complete_table(std::str::from_utf8(&output.stdout).ok()?)
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

// Native lifecycle records exist on Unix. Keep legacy routing available on other platforms.
fn records_locked<T>(
    config: &Path,
    run: impl FnOnce() -> std::io::Result<T>,
) -> std::io::Result<T> {
    #[cfg(unix)]
    {
        crate::client_journal::locked(config, run)
    }
    #[cfg(not(unix))]
    {
        let _ = config;
        run()
    }
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
    proof: Option<Value>,
}

/// Every readable record. One file per host and session, so adding or removing a session
/// never rewrites another's file.
fn records(directory: &Path) -> Vec<Record> {
    use std::io::Read;
    if private_directory(directory).is_err() {
        return Vec::new();
    }
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
            private_file(&file)
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
                proof: record.get("proof").cloned(),
            })
        })
        .collect()
}

pub(crate) fn nonce() -> std::io::Result<String> {
    let mut random = [0_u8; 16];
    getrandom::fill(&mut random)
        .map_err(|_| std::io::Error::other("native session random source unavailable"))?;
    Ok(random.iter().map(|byte| format!("{byte:02x}")).collect())
}

/// Records that `host` has `session` open.
pub(crate) fn record_under(directory: &Path, host: &Process, session: &str) -> std::io::Result<()> {
    use std::io::Write;
    let mut builder = std::fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    std::os::unix::fs::DirBuilderExt::mode(&mut builder, 0o700);
    builder.create(directory)?;
    private_directory(directory)?;
    // Written whole and renamed, so the MCP server never reads half a record.
    let mut file = tempfile::NamedTempFile::new_in(directory)?;
    // A fresh start may reuse both the native session ID and the same host process.
    let association = nonce()?;
    let record = json!({"started": host.started, "session": session, "association": association});
    file.write_all(record.to_string().as_bytes())?;
    file.persist(directory.join(format!("{}.{}", host.pid, tag(session))))?;
    Ok(())
}

fn prune_host_records(
    config: &Path,
    journal: &crate::client_journal::Scan,
    mut observe: impl FnMut(&HostIdentity) -> HostObservation,
) -> std::io::Result<()> {
    let mut retained = HashSet::new();
    for entry in std::fs::read_dir(routes(config))? {
        let entry = entry?;
        let Some(record) = route_record(&entry.path()) else {
            return Ok(());
        };
        if route_context(&entry.path(), &record).is_err() {
            return Ok(());
        }
        retained.insert(record["session"].as_str().unwrap().to_owned());
    }
    let host_directory = directory(config);
    let entries = match std::fs::read_dir(&host_directory) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
    };
    private_directory(&host_directory)?;
    let mut observations = HashMap::new();
    let mut removed = 0;
    for entry in entries {
        if removed == MAX_RECORDS {
            break;
        }
        let file = entry?.path();
        let Some(record) = route_record(&file) else {
            continue;
        };
        let Some(session) = record["session"].as_str() else {
            continue;
        };
        if retained.contains(session)
            || journal
                .records
                .iter()
                .any(|entry| entry.payload["session"] == session)
        {
            continue;
        }
        let Ok(host) = serde_json::from_value::<HostIdentity>(record["proof"]["identity"].clone())
        else {
            continue;
        };
        if file.file_name()
            != Some(std::ffi::OsStr::new(&format!(
                "{}.{}",
                host.pid,
                tag(session)
            )))
            || host.started.split('\0').nth(2) != record["started"].as_str()
        {
            continue;
        }
        let observed = observations
            .entry((host.pid, host.started.clone()))
            .or_insert_with(|| observe(&host));
        if *observed == HostObservation::Exited {
            std::fs::remove_file(file)?;
            removed += 1;
        }
    }
    Ok(())
}

/// Records the host for MCP association. Dead-host evidence is retained for lifecycle reconciliation.
pub(crate) fn record(config: &Path, session: &str) {
    let Some(table) = table() else {
        return;
    };
    let directory = directory(config);
    if let Some(host) = host(&chain(&table, std::process::id())) {
        let _ = records_locked(config, || record_under(&directory, host, session));
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
    private_file(file)
        .ok()?
        .take(RECORD_BYTES)
        .read_to_string(&mut text)
        .ok()?;
    serde_json::from_str(&text).ok()
}

fn route_context(file: &Path, record: &Value) -> std::io::Result<Option<SessionContext>> {
    let invalid = || std::io::Error::other("session context route changed");
    let session = record["session"].as_str().ok_or_else(invalid)?;
    let hub = record["hub"].as_str().ok_or_else(invalid)?;
    let repo = record["repo"].as_str().ok_or_else(invalid)?;
    let root = Path::new(record["root"].as_str().ok_or_else(invalid)?);
    if !root.is_absolute()
        || file.file_name() != Some(std::ffi::OsStr::new(&route_tag(session, hub, repo)))
    {
        return Err(invalid());
    }
    let Some(value) = record.get("lifecycle") else {
        return Ok(None);
    };
    let context: SessionContext = serde_json::from_value(value.clone())?;
    if context.session != session
        || context.hub != hub
        || !context.repo.eq_ignore_ascii_case(repo)
        || context.root != root
    {
        return Err(invalid());
    }
    Ok(Some(context))
}

fn private_file(path: &Path) -> std::io::Result<std::fs::File> {
    private_directory(
        path.parent()
            .ok_or_else(|| std::io::Error::other("session directory missing"))?,
    )?;
    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC);
    }
    let file = options.open(path)?;
    let metadata = file.metadata()?;
    if !metadata.is_file() || metadata.len() > RECORD_BYTES {
        return Err(std::io::Error::other("invalid session record"));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        // SAFETY: geteuid has no arguments or preconditions.
        if metadata.uid() != unsafe { libc::geteuid() }
            || metadata.mode() & 0o777 != 0o600
            || metadata.nlink() != 1
        {
            return Err(std::io::Error::other("session record is not private"));
        }
    }
    Ok(file)
}

fn private_directory(path: &Path) -> std::io::Result<()> {
    let metadata = std::fs::symlink_metadata(path)?;
    if !metadata.is_dir() {
        return Err(std::io::Error::other("session directory is not private"));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        // SAFETY: geteuid has no arguments or preconditions.
        if metadata.uid() != unsafe { libc::geteuid() } || metadata.mode() & 0o777 != 0o700 {
            return Err(std::io::Error::other("session directory is not private"));
        }
    }
    Ok(())
}

fn host_fingerprint(process: &Process) -> HostIdentity {
    HostIdentity {
        pid: process.pid,
        started: format!(
            "{}\0{}\0{}\0{}",
            machine_identity().unwrap_or_default(),
            boot_identity().unwrap_or_default(),
            process.started,
            process.name
        ),
    }
}

fn machine_identity() -> Option<String> {
    #[cfg(target_os = "macos")]
    {
        let mut uuid = [0u8; 16];
        let timeout = libc::timespec {
            tv_sec: 1,
            tv_nsec: 0,
        };
        // SAFETY: uuid is exactly the 16-byte output gethostuuid requires; timeout is valid.
        if unsafe { libc::gethostuuid(uuid.as_mut_ptr(), &timeout) } == 0 {
            return Some(uuid.iter().map(|byte| format!("{byte:02x}")).collect());
        }
        None
    }
    #[cfg(target_os = "linux")]
    {
        std::fs::read_to_string("/etc/machine-id")
            .ok()
            .map(|value| value.trim().to_owned())
            .filter(|value| !value.is_empty() && value.len() <= 128)
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        None
    }
}

fn boot_identity() -> Option<String> {
    #[cfg(target_os = "macos")]
    {
        // SAFETY: timeval is valid zeroed output storage; sysctl receives its exact size.
        let mut boot: libc::timeval = unsafe { std::mem::zeroed() };
        let mut length = std::mem::size_of::<libc::timeval>();
        // SAFETY: the name is NUL terminated, output has the declared size, and input is null.
        if unsafe {
            libc::sysctlbyname(
                c"kern.boottime".as_ptr(),
                (&mut boot as *mut libc::timeval).cast(),
                &mut length,
                std::ptr::null_mut(),
                0,
            )
        } == 0
            && length == std::mem::size_of::<libc::timeval>()
        {
            return Some(format!("{}:{}", boot.tv_sec, boot.tv_usec));
        }
        None
    }
    #[cfg(target_os = "linux")]
    {
        std::fs::read_to_string("/proc/sys/kernel/random/boot_id")
            .ok()
            .map(|value| value.trim().to_owned())
            .filter(|value| !value.is_empty() && value.len() <= 128)
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        None
    }
}

fn native_host(process: &Process, agent: &str) -> bool {
    use std::io::Read;
    let path = Path::new(&process.name);
    if !path.is_absolute() || process.name.contains(".app/") {
        return false;
    }
    let Ok(path) = path.canonicalize() else {
        return false;
    };
    if path.to_string_lossy().contains(".app/")
        || matches!(
            path.file_name().and_then(|s| s.to_str()),
            Some("node" | "env" | "timeout" | "sh" | "bash" | "zsh")
        )
    {
        return false;
    }
    if !matches!(agent, "claude" | "codex") {
        return false;
    }
    let named = path.file_name().and_then(|s| s.to_str()) == Some(agent);
    let installed = [agent].iter().any(|name| {
        std::env::var_os("PATH").is_some_and(|value| {
            std::env::split_paths(&value)
                .any(|directory| directory.join(name).canonicalize().is_ok_and(|p| p == path))
        })
    });
    if !named && !installed {
        return false;
    }
    let Ok(mut file) = std::fs::File::open(path) else {
        return false;
    };
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if !file
            .metadata()
            .is_ok_and(|metadata| metadata.mode() & 0o111 != 0)
        {
            return false;
        }
    }
    let mut magic = [0; 4];
    file.read_exact(&mut magic).is_ok()
        && matches!(
            magic,
            [0x7f, b'E', b'L', b'F']
                | [0xcf, 0xfa, 0xed, 0xfe]
                | [0xfe, 0xed, 0xfa, 0xcf]
                | [0xce, 0xfa, 0xed, 0xfe]
                | [0xfe, 0xed, 0xfa, 0xce]
                | [0xca, 0xfe, 0xba, 0xbe]
        )
}

/// A genuine adapter event contributes proof; a new start resets the two-event qualification.
pub(crate) fn observe_event(
    config: &Path,
    agent: &str,
    session: &str,
    event_id: &str,
    fresh_start: bool,
) -> std::io::Result<Option<HostIdentity>> {
    let Some(table) = table() else {
        return Ok(None);
    };
    let ancestors = chain(&table, std::process::id());
    let Some(process) = ancestors.iter().find(|process| native_host(process, agent)) else {
        return Ok(None);
    };
    record_event_for(config, process, session, event_id, fresh_start)
}

fn record_event_for(
    config: &Path,
    process: &Process,
    session: &str,
    event_id: &str,
    fresh_start: bool,
) -> std::io::Result<Option<HostIdentity>> {
    if session.is_empty()
        || session.len() > 128
        || session.chars().any(char::is_control)
        || event_id.is_empty()
        || event_id.len() > 128
        || event_id.chars().any(char::is_control)
    {
        return Err(std::io::Error::other("invalid native session event"));
    }
    crate::client_journal::locked(config, || {
        let directory = directory(config);
        let prior = records(&directory).into_iter().find(|record| {
            record.pid == process.pid
                && record.started == process.started
                && record.session == session
        });
        if !fresh_start && prior.is_none() {
            return Ok(None);
        }
        let previous = prior
            .as_ref()
            .and_then(|record| record.proof.as_ref())
            .filter(|_| !fresh_start);
        let first = previous
            .and_then(|proof| proof["first"].as_str())
            .unwrap_or(event_id);
        let qualified =
            previous.is_some_and(|proof| proof["qualified"] == true) || first != event_id;
        let identity = host_fingerprint(process);
        let proof = json!({"first":first,"qualified":qualified,"identity":identity});
        // Qualification is mutable evidence within one start. Only a genuine fresh start
        // replaces its association; an ordinary event must not supersede an end in flight.
        if fresh_start {
            record_under(&directory, process, session)?;
        }
        let destination = directory.join(format!("{}.{}", process.pid, tag(session)));
        let mut record = route_record(&destination)
            .ok_or_else(|| std::io::Error::other("session record unavailable"))?;
        record["proof"] = proof;
        write_record(&destination, &record)?;
        Ok(if qualified {
            qualified_host_in(config, session)
        } else {
            None
        })
    })
}

fn write_record(destination: &Path, value: &Value) -> std::io::Result<()> {
    use std::io::Write;
    let bytes = serde_json::to_vec(value)?;
    private_directory(
        destination
            .parent()
            .ok_or_else(|| std::io::Error::other("session directory missing"))?,
    )?;
    if bytes.len() as u64 > RECORD_BYTES {
        return Err(std::io::Error::other("session record too large"));
    }
    let mut file = tempfile::NamedTempFile::new_in(
        destination
            .parent()
            .ok_or_else(|| std::io::Error::other("session directory missing"))?,
    )?;
    file.write_all(&bytes)?;
    file.as_file().sync_all()?;
    file.persist(destination).map_err(|error| error.error)?;
    Ok(())
}

pub(crate) fn qualified_host(session: &str) -> Option<HostIdentity> {
    let config = crate::hub_cli::config_dir().ok()?;
    qualified_host_in(&config, session)
}

pub(crate) fn registration_host() -> Option<HostIdentity> {
    let ancestors = ancestors();
    host(&ancestors).map(host_fingerprint)
}

fn qualified_host_in(config: &Path, session: &str) -> Option<HostIdentity> {
    let count = std::fs::read_dir(directory(config))
        .ok()?
        .take(MAX_RECORDS + 1)
        .collect::<std::io::Result<Vec<_>>>()
        .ok()?
        .len();
    if count > MAX_RECORDS {
        return None;
    }
    let records = records(&directory(config));
    if count != records.len() {
        return None;
    }
    let table = table()?;
    let mut candidates = records.iter().filter(|record| {
        record.session == session
            && table
                .get(&record.pid)
                .is_some_and(|(_, process)| process.started == record.started)
            && record
                .proof
                .as_ref()
                .is_some_and(|proof| proof["qualified"] == true)
    });
    let found = candidates.next()?;
    if candidates.next().is_some() {
        return None;
    }
    if records
        .iter()
        .filter(|record| record.pid == found.pid && record.started == found.started)
        .count()
        != 1
    {
        return None;
    }
    let identity: HostIdentity =
        serde_json::from_value(found.proof.as_ref()?["identity"].clone()).ok()?;
    (identity.pid == found.pid
        && identity
            .started
            .split('\0')
            .nth(2)
            .is_some_and(|started| started == found.started)
        && observe_host(&identity) == HostObservation::Same)
        .then_some(identity)
}

/// No signal is sent: signal zero distinguishes authoritative absence from a denied observation.
pub(crate) fn observe_host(host: &HostIdentity) -> HostObservation {
    let fields = host.started.splitn(4, '\0').collect::<Vec<_>>();
    let [machine, boot, started, name] = fields.as_slice() else {
        return HostObservation::Unknown;
    };
    if machine.is_empty() || machine_identity().as_deref() != Some(*machine) {
        return HostObservation::Unknown;
    }
    if host.pid == 0 || host.pid > i32::MAX as u32 {
        return HostObservation::Unknown;
    }
    if !boot.is_empty()
        && let Some(current) = boot_identity()
        && current != *boot
    {
        return HostObservation::Exited;
    }
    let Some(table) = table() else {
        return HostObservation::Unknown;
    };
    if let Some((_, process)) = table.get(&host.pid) {
        return if process.started == *started {
            if process.name == *name {
                HostObservation::Same
            } else {
                HostObservation::Unknown
            }
        } else {
            HostObservation::Exited
        };
    }
    #[cfg(unix)]
    {
        // SAFETY: signal zero only observes a validated positive process ID.
        if unsafe { libc::kill(host.pid as i32, 0) } == -1
            && std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH)
        {
            return HostObservation::Exited;
        }
    }
    HostObservation::Unknown
}

pub(crate) fn context(
    config: &Path,
    session: &str,
    hub: &str,
    repo: &str,
) -> std::io::Result<Option<SessionContext>> {
    let path = routes(config).join(route_tag(session, hub, repo));
    match std::fs::symlink_metadata(&path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
        Ok(_) => {}
    }
    let record =
        route_record(&path).ok_or_else(|| std::io::Error::other("session route unavailable"))?;
    let Some(context) = route_context(&path, &record)? else {
        return Ok(None);
    };
    if context.session != session
        || context.hub != hub
        || !context.repo.eq_ignore_ascii_case(repo)
        || !context.root.is_absolute()
    {
        return Err(std::io::Error::other("session context route changed"));
    }
    Ok(Some(context))
}

pub(crate) fn save_context(config: &Path, context: &SessionContext) -> std::io::Result<()> {
    crate::client_journal::locked(config, || save_context_under_lock(config, context))
}

fn save_context_under_lock(config: &Path, context: &SessionContext) -> std::io::Result<()> {
    let mut updated = context.clone();
    if let Some(previous) = self::context(config, &context.session, &context.hub, &context.repo)? {
        if previous.start_nonce != context.start_nonce
            || previous.binding_id != context.binding_id
            || previous.credential_id != context.credential_id
            || previous.root != context.root
        {
            return Err(std::io::Error::other(
                "native session context was superseded",
            ));
        }
        // Only a fresh start can replace captured process identity.
        for (prior, next) in [
            (&previous.host, &mut updated.host),
            (&previous.registration_host, &mut updated.registration_host),
        ] {
            if let Some(prior) = prior {
                if next.as_ref().is_some_and(|next| next != prior) {
                    return Err(std::io::Error::other("native session host changed"));
                }
                *next = Some(prior.clone());
            }
        }
        updated.terminal |= previous.terminal;
        updated.revoked |= previous.revoked;
        updated.explicit_end_requested |= previous.explicit_end_requested;
        updated.exit_observed_at = match (previous.exit_observed_at, updated.exit_observed_at) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (Some(a), None) => Some(a),
            (_, next) => next,
        };
        if previous.registered && !updated.registered {
            updated.registered = true;
            updated.incarnation = previous.incarnation.clone();
        }
        match (&previous.incarnation, &updated.incarnation) {
            (Some(prior), Some(next)) if previous.registered && prior != next => {
                return Err(std::io::Error::other("native session incarnation changed"));
            }
            (Some(prior), None) => updated.incarnation = Some(prior.clone()),
            _ => {}
        }
        // Same-incarnation qualification only advances; delayed replies cannot undo it.
        if previous.registered {
            updated.qualification = [
                Qualification::Mixed,
                Qualification::Qualified,
                Qualification::Unknown,
            ]
            .into_iter()
            .find(|value| {
                previous.qualification == Some(*value) || updated.qualification == Some(*value)
            });
            updated.exit_ineligible |= previous.exit_ineligible;
        }
    }
    if updated.qualification == Some(Qualification::Qualified) {
        updated.exit_ineligible = false;
    }
    write_context(config, &updated)
}

/// An end observed before an await has authority only over that original start/association.
pub(crate) fn save_end_context(
    config: &Path,
    fence: &CleanupFence,
    context: &SessionContext,
) -> std::io::Result<Option<SessionContext>> {
    crate::client_journal::locked(config, || {
        if cleanup_hosts(config, &fence.session) != fence.hosts
            || !fence.contexts.iter().any(|original| {
                original.session == context.session
                    && original.hub == context.hub
                    && original.repo.eq_ignore_ascii_case(&context.repo)
                    && original.start_nonce == context.start_nonce
            })
        {
            return Ok(None);
        }
        let Some(mut current) =
            self::context(config, &context.session, &context.hub, &context.repo)?
        else {
            return Ok(None);
        };
        if current.start_nonce != context.start_nonce
            || current.binding_id != context.binding_id
            || current.credential_id != context.credential_id
            || current.root != context.root
        {
            return Ok(None);
        }
        // Registration/qualification may have advanced this same start while identification
        // awaited. Apply only the original end observation to the current same-start context.
        current.explicit_end_requested |= context.explicit_end_requested;
        current.exit_observed_at = match (current.exit_observed_at, context.exit_observed_at) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (Some(a), None) => Some(a),
            (_, next) => next,
        };
        save_context_under_lock(config, &current)?;
        Ok(Some(current))
    })
}

/// A genuine fresh start replaces exactly the local nonce it observed, never a later start.
pub(crate) fn begin_context(
    config: &Path,
    expected_nonce: Option<&str>,
    context: &SessionContext,
) -> std::io::Result<()> {
    crate::client_journal::locked(config, || {
        let previous = self::context(config, &context.session, &context.hub, &context.repo)?;
        if previous
            .as_ref()
            .map(|context| context.start_nonce.as_str())
            != expected_nonce
            || expected_nonce == Some(context.start_nonce.as_str())
        {
            return Err(std::io::Error::other("native session start was superseded"));
        }
        write_context(config, context)
    })
}

fn write_context(config: &Path, context: &SessionContext) -> std::io::Result<()> {
    record_route_under_lock(
        config,
        &context.session,
        &context.hub,
        &context.repo,
        &context.root,
    )?;
    let destination = routes(config).join(route_tag(&context.session, &context.hub, &context.repo));
    let mut record = route_record(&destination)
        .ok_or_else(|| std::io::Error::other("session route unavailable"))?;
    record["lifecycle"] = serde_json::to_value(context)?;
    write_record(&destination, &record)
}

#[derive(Debug, Default)]
pub(crate) struct ContextScan {
    pub(crate) contexts: Vec<SessionContext>,
    pub(crate) stalled: usize,
    pub(crate) truncated: usize,
}

impl ContextScan {
    pub(crate) fn complete(&self) -> bool {
        self.stalled == 0 && self.truncated == 0
    }
}

fn referenced(context: &SessionContext, journal: &crate::client_journal::Scan) -> bool {
    journal.stalled != 0
        || journal.records.iter().any(|record| {
            let value = &record.payload;
            !["session", "hub", "repo"]
                .iter()
                .all(|key| value[*key].is_string())
                || (value["session"] == context.session
                    && value["hub"] == context.hub
                    && value["repo"]
                        .as_str()
                        .is_some_and(|repo| repo.eq_ignore_ascii_case(&context.repo)))
        })
}

pub(crate) fn contexts(config: &Path) -> std::io::Result<ContextScan> {
    scan_contexts(config, None)
}

fn scan_contexts(config: &Path, session: Option<&str>) -> std::io::Result<ContextScan> {
    let mut scan = ContextScan::default();
    let entries = match std::fs::read_dir(routes(config)) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(scan),
        Err(error) => return Err(error),
    };
    private_directory(&routes(config))?;
    let journal = crate::client_journal::inspect(config).ok();
    let mut selected = BTreeMap::new();
    for entry in entries {
        let Ok(entry) = entry else {
            scan.stalled += 1;
            continue;
        };
        let Some(record) = route_record(&entry.path()) else {
            scan.stalled += 1;
            continue;
        };
        let Ok(context) = route_context(&entry.path(), &record) else {
            scan.stalled += 1;
            continue;
        };
        if session.is_some_and(|session| record["session"].as_str() != Some(session)) {
            continue;
        }
        if let Some(context) = context {
            let priority = if !context.terminal && !context.revoked && !context.exit_ineligible {
                0
            } else if journal
                .as_ref()
                .is_none_or(|journal| referenced(&context, journal))
            {
                1
            } else {
                2
            };
            selected.insert(
                (
                    priority,
                    std::cmp::Reverse(context.observed_at),
                    entry.path(),
                ),
                context,
            );
            if selected.len() > MAX_RECORDS {
                selected.pop_last();
                scan.truncated += 1;
            }
        }
    }
    scan.contexts = selected.into_values().collect();
    Ok(scan)
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
    records_locked(config, || {
        record_route_under_lock(config, session, hub, repo, root)
    })
}

fn record_route_under_lock(
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
    private_directory(&directory)?;
    let mut file = tempfile::NamedTempFile::new_in(&directory)?;
    let record = json!({"session": session, "hub": hub, "repo": repo, "root": root});
    file.write_all(record.to_string().as_bytes())?;
    let destination = directory.join(route_tag(session, hub, repo));
    file.persist(&destination)?;
    prune_routes_under_lock(config, Some(&destination))
}

/// Settled local tombstones can go only after every retained journal reference is gone.
pub(crate) fn prune_contexts(config: &Path) -> std::io::Result<()> {
    records_locked(config, || prune_routes_under_lock(config, None))
}

fn prune_routes_under_lock(config: &Path, preserve: Option<&Path>) -> std::io::Result<()> {
    let directory = routes(config);
    if !directory.try_exists()? {
        return Ok(());
    }
    private_directory(&directory)?;
    let Ok(journal) = crate::client_journal::inspect(config) else {
        return Ok(());
    };
    // Unknown journal entries may name any route; preserve all potential evidence.
    if journal.stalled != 0
        || journal.records.iter().any(|record| {
            !["session", "hub", "repo"]
                .iter()
                .all(|key| record.payload[*key].is_string())
        })
    {
        return Ok(());
    }
    let mut kept: Vec<_> = std::fs::read_dir(&directory)?
        .flatten()
        .filter_map(|entry| {
            let modified = entry.metadata().ok()?.modified().ok()?;
            Some((modified, entry.path()))
        })
        .collect();
    kept.sort();
    let mut excess = kept.len().saturating_sub(MAX_RECORDS);
    for (modified, file) in kept {
        let old = modified.elapsed().is_ok_and(|age| age > ROUTE_KEEP);
        if preserve == Some(file.as_path()) || (excess == 0 && !old) {
            continue;
        }
        let Some(record) = route_record(&file) else {
            continue;
        };
        let safe = match route_context(&file, &record) {
            Ok(Some(context)) => {
                (context.terminal || context.revoked || (context.exit_ineligible && old))
                    && !referenced(&context, &journal)
            }
            Ok(None) => !journal.records.iter().any(|entry| {
                entry.payload["session"] == record["session"]
                    && entry.payload["hub"] == record["hub"]
                    && entry.payload["repo"].as_str().is_some_and(|repo| {
                        record["repo"]
                            .as_str()
                            .is_some_and(|saved| repo.eq_ignore_ascii_case(saved))
                    })
            }),
            Err(_) => false,
        };
        if safe {
            std::fs::remove_file(file)?;
            excess = excess.saturating_sub(1);
        }
    }
    prune_host_records(config, &journal, observe_host)
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

/// Captured before an end hook awaits; it cannot authorize cleanup of a later native start.
pub(crate) struct CleanupFence {
    session: String,
    starts: Vec<(String, String, String)>,
    files: Vec<(PathBuf, Value)>,
    hosts: Vec<(PathBuf, Value)>,
    contexts: Vec<SessionContext>,
    complete: bool,
}

impl CleanupFence {
    pub(crate) fn end_contexts(&self) -> &[SessionContext] {
        &self.contexts
    }

    pub(crate) fn complete(&self) -> bool {
        self.complete
    }
}

fn cleanup_hosts(config: &Path, session: &str) -> Vec<(PathBuf, Value)> {
    let mut hosts = records(&directory(config))
        .into_iter()
        .filter(|record| record.session == session)
        .filter_map(|record| {
            let start = route_record(&record.file)?;
            Some((
                record.file,
                json!({
                    "started": start["started"],
                    "session": start["session"],
                    "association": start["association"],
                }),
            ))
        })
        .collect::<Vec<_>>();
    hosts.sort_by(|a, b| a.0.cmp(&b.0));
    hosts
}

fn cleanup_starts(config: &Path, session: &str) -> std::io::Result<Vec<(String, String, String)>> {
    let scan = scan_contexts(config, Some(session))?;
    if !scan.complete() {
        return Err(std::io::Error::other(
            "session cleanup coverage is incomplete",
        ));
    }
    Ok(context_starts(&scan.contexts))
}

fn context_starts(contexts: &[SessionContext]) -> Vec<(String, String, String)> {
    let mut starts = contexts
        .iter()
        .map(|context| {
            (
                context.hub.clone(),
                context.repo.to_ascii_lowercase(),
                context.start_nonce.clone(),
            )
        })
        .collect::<Vec<_>>();
    starts.sort();
    starts
}

pub(crate) fn cleanup_fence(config: &Path, session: &str) -> std::io::Result<CleanupFence> {
    records_locked(config, || {
        let scan = scan_contexts(config, Some(session))?;
        let mut complete = scan.complete();
        let starts = context_starts(&scan.contexts);
        let contexts = scan.contexts;
        let hosts = cleanup_hosts(config, session);
        let mut files = Vec::new();
        for entry in std::fs::read_dir(routes(config)).into_iter().flatten() {
            let file = entry?.path();
            if let Some(record) = route_record(&file)
                && record["session"].as_str() == Some(session)
                && record.get("lifecycle").is_none()
            {
                if files.len() < MAX_RECORDS {
                    files.push((file, record));
                } else {
                    complete = false;
                }
            }
        }
        Ok(CleanupFence {
            session: session.into(),
            starts,
            files,
            hosts,
            contexts,
            complete,
        })
    })
}

/// Drops only the associations observed at entry; lifecycle tombstones and explicit Work survive.
pub(crate) fn forget(config: &Path, fence: &CleanupFence) -> std::io::Result<()> {
    records_locked(config, || {
        if !fence.complete
            || cleanup_starts(config, &fence.session)? != fence.starts
            || cleanup_hosts(config, &fence.session) != fence.hosts
        {
            return Ok(());
        }
        for (file, expected) in &fence.files {
            if route_record(file).as_ref() == Some(expected) {
                std::fs::remove_file(file)?;
            }
        }
        for (file, _) in &fence.hosts {
            std::fs::remove_file(file)?;
        }
        Ok(())
    })
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
    fn session_nonces_have_128_random_bits_in_lowercase_hex() {
        let first = super::nonce().unwrap();
        let second = super::nonce().unwrap();
        for value in [&first, &second] {
            assert_eq!(value.len(), 32);
            assert!(
                value
                    .bytes()
                    .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
            );
        }
        assert_ne!(first, second);
    }

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

    #[cfg(unix)]
    fn lifecycle_context(session: &str, terminal: bool) -> SessionContext {
        SessionContext {
            session: session.into(),
            start_nonce: format!("nonce-{session}"),
            incarnation: Some(format!("inc-{session}")),
            agent: "codex".into(),
            hub: "https://hub".into(),
            repo: "o/r".into(),
            root: root("one"),
            protocol: 1,
            protocol_checked_at: 10,
            terminal,
            registered: true,
            qualification: None,
            exit_ineligible: false,
            revoked: false,
            exit_observed_at: None,
            explicit_end_requested: false,
            observed_at: 10,
            host: None,
            registration_host: None,
            credential_id: "token-id".into(),
            binding_id: "binding".into(),
        }
    }

    #[cfg(unix)]
    fn retained_context(config: &Path, context: &SessionContext) {
        write_record(
            &routes(config).join(route_tag(&context.session, &context.hub, &context.repo)),
            &json!({"session":context.session,"hub":context.hub,"repo":context.repo,
                "root":context.root,"lifecycle":context}),
        )
        .unwrap();
    }

    #[cfg(unix)]
    fn pending_context(config: &Path, context: &SessionContext) {
        crate::client_journal::put(config, &crate::client_journal::Record {
            id: "retained-terminal-report".into(),
            kind: crate::client_journal::Kind::TerminalEnd,
            phase: crate::client_journal::Phase::Admitted,
            payload: json!({"session":context.session,"hub":context.hub,"repo":context.repo,
                "root":context.root,"start_nonce":context.start_nonce,"incarnation":context.incarnation}),
        }).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn context_scan_prioritizes_active_and_pending_routes_without_hiding_corruption() {
        let config = tempfile::tempdir().unwrap();
        let live = lifecycle_context("live", false);
        save_context(config.path(), &live).unwrap();
        for index in 0..MAX_RECORDS {
            let mut settled = lifecycle_context(&format!("settled-{index}"), true);
            settled.observed_at = 100;
            retained_context(config.path(), &settled);
        }
        let pending = lifecycle_context("pending", true);
        retained_context(config.path(), &pending);
        pending_context(config.path(), &pending);
        let corrupt = routes(config.path()).join("corrupt");
        write_record(&corrupt, &json!({"unknown":true})).unwrap();
        let before = std::fs::read_dir(routes(config.path())).unwrap().count();
        let scan = contexts(config.path()).unwrap();
        assert_eq!(scan.contexts.len(), MAX_RECORDS);
        assert_eq!(scan.contexts[0], live);
        assert_eq!(scan.contexts[1], pending);
        assert_eq!(scan.stalled, 1);
        assert_eq!(scan.truncated, 2);
        assert!(!scan.complete());
        assert_eq!(
            std::fs::read_dir(routes(config.path())).unwrap().count(),
            before,
            "status scanning cannot delete evidence"
        );

        let fence = cleanup_fence(config.path(), "live").unwrap();
        assert!(!fence.complete());
        assert_eq!(fence.end_contexts(), std::slice::from_ref(&live));
        let mut ending = live.clone();
        ending.explicit_end_requested = true;
        ending.exit_observed_at = Some(20);
        assert!(
            save_end_context(config.path(), &fence, &ending)
                .unwrap()
                .is_some()
        );
        assert!(corrupt.exists());
        std::fs::remove_file(corrupt).unwrap();
        assert!(
            cleanup_fence(config.path(), "live").unwrap().complete(),
            "unrelated valid routes do not truncate an exact session fence"
        );
    }

    #[cfg(unix)]
    #[test]
    fn capacity_pruning_preserves_live_pending_and_unknown_journal_evidence() {
        let config = tempfile::tempdir().unwrap();
        let live = lifecycle_context("live", false);
        save_context(config.path(), &live).unwrap();
        let pending = lifecycle_context("pending", true);
        retained_context(config.path(), &pending);
        pending_context(config.path(), &pending);
        for index in 0..MAX_RECORDS {
            retained_context(
                config.path(),
                &lifecycle_context(&format!("settled-{index}"), true),
            );
        }
        prune_contexts(config.path()).unwrap();
        assert_eq!(
            std::fs::read_dir(routes(config.path())).unwrap().count(),
            MAX_RECORDS
        );
        for expected in [&live, &pending] {
            assert_eq!(
                context(
                    config.path(),
                    &expected.session,
                    &expected.hub,
                    &expected.repo
                )
                .unwrap()
                .as_ref(),
                Some(expected)
            );
        }
        retained_context(config.path(), &lifecycle_context("extra", true));
        let unknown = config.path().join("agentscm/native-journal/unknown.json");
        write_record(&unknown, &json!({"version":999})).unwrap();
        assert_eq!(
            crate::client_journal::inspect(config.path())
                .unwrap()
                .stalled,
            1
        );
        prune_contexts(config.path()).unwrap();
        assert_eq!(
            std::fs::read_dir(routes(config.path())).unwrap().count(),
            MAX_RECORDS + 1
        );
        assert!(unknown.exists());
    }

    #[cfg(unix)]
    #[test]
    fn host_pruning_requires_no_route_or_journal_and_a_positive_exact_exit() {
        let config = tempfile::tempdir().unwrap();
        let context = lifecycle_context("native", true);
        save_context(config.path(), &context).unwrap();
        let process = Process {
            pid: 123,
            started: "synthetic-start".into(),
            name: "codex".into(),
        };
        record_under(&directory(config.path()), &process, "native").unwrap();
        let host = HostIdentity {
            pid: process.pid,
            started: "machine\0boot\0synthetic-start\0codex".into(),
        };
        let file = directory(config.path()).join(format!("{}.{}", process.pid, tag("native")));
        let mut record = route_record(&file).unwrap();
        record["proof"] = json!({"identity":host});
        write_record(&file, &record).unwrap();
        let empty = crate::client_journal::Scan::default();
        prune_host_records(config.path(), &empty, |_| HostObservation::Exited).unwrap();
        assert!(
            file.exists(),
            "a retained route still needs the host association"
        );
        std::fs::remove_file(routes(config.path()).join(route_tag(
            "native",
            &context.hub,
            &context.repo,
        )))
        .unwrap();
        for observed in [HostObservation::Unknown, HostObservation::Same] {
            prune_host_records(config.path(), &empty, |_| observed).unwrap();
            assert!(file.exists());
        }
        pending_context(config.path(), &context);
        let pending = crate::client_journal::inspect(config.path()).unwrap();
        prune_host_records(config.path(), &pending, |_| HostObservation::Exited).unwrap();
        assert!(
            file.exists(),
            "retained reports still need their association"
        );
        crate::client_journal::remove(config.path(), "retained-terminal-report").unwrap();
        record["started"] = json!("successor-start");
        write_record(&file, &record).unwrap();
        prune_host_records(config.path(), &empty, |_| HostObservation::Exited).unwrap();
        assert!(
            file.exists(),
            "a mismatched successor record cannot use old exit proof"
        );
        record["started"] = json!("synthetic-start");
        write_record(&file, &record).unwrap();
        prune_host_records(config.path(), &empty, |observed| {
            assert_eq!(observed, &host);
            HostObservation::Exited
        })
        .unwrap();
        assert!(!file.exists());
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
    fn partial_or_empty_process_tables_cannot_prove_absence() {
        assert!(complete_table("").is_none());
        assert!(complete_table(&format!("{TABLE}\nnot a process row")).is_none());
        assert!(complete_table(TABLE).is_some());
    }

    #[cfg(unix)]
    #[test]
    fn native_binary_evidence_excludes_node_and_desktop_hosts() {
        use std::os::unix::fs::PermissionsExt;
        let directory = tempfile::tempdir().unwrap();
        let executable = directory.path().join("codex");
        std::fs::write(&executable, b"\x7fELFsynthetic").unwrap();
        std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o700)).unwrap();
        let mut process = Process {
            pid: 42,
            started: "start".into(),
            name: executable.to_string_lossy().into_owned(),
        };
        assert!(native_host(&process, "codex"));
        assert!(!native_host(&process, "claude"));
        process.name = directory.path().join("node").to_string_lossy().into_owned();
        std::fs::rename(&executable, &process.name).unwrap();
        assert!(!native_host(&process, "codex"));
        process.name = "/Applications/Host.app/Contents/MacOS/codex".into();
        assert!(!native_host(&process, "codex"));
    }

    #[cfg(unix)]
    #[test]
    fn qualification_needs_two_events_and_never_accepts_a_shared_host() {
        let config = tempfile::tempdir().unwrap();
        let Some(table) = table() else {
            return;
        };
        let Some((_, process)) = table.get(&std::process::id()) else {
            return;
        };
        if machine_identity().is_none() {
            return;
        }
        assert!(
            record_event_for(config.path(), process, "s-a", "event-1", true)
                .unwrap()
                .is_none()
        );
        assert!(
            record_event_for(config.path(), process, "s-a", "event-1", false)
                .unwrap()
                .is_none()
        );
        assert!(
            record_event_for(config.path(), process, "s-a", "event-2", false)
                .unwrap()
                .is_some()
        );
        assert!(qualified_host_in(config.path(), "s-a").is_some());
        let old = Process {
            pid: 0,
            started: "old-process".into(),
            name: "/synthetic/codex".into(),
        };
        record_under(&directory(config.path()), &old, "s-a").unwrap();
        write_record(&directory(config.path()).join(format!("{}.{}",old.pid,tag("s-a"))),&json!({"session":"s-a","started":old.started,"proof":{"qualified":true,"identity":host_fingerprint(&old)}})).unwrap();
        assert!(qualified_host_in(config.path(), "s-a").is_some());
        record_under(&directory(config.path()), process, "s-b").unwrap();
        assert!(qualified_host_in(config.path(), "s-a").is_none());
        assert!(
            record_event_for(config.path(), process, "s-a", "event-3", true)
                .unwrap()
                .is_none()
        );
    }

    #[cfg(unix)]
    #[test]
    fn live_and_foreign_host_observations_remain_distinct() {
        let Some(table) = table() else {
            return;
        };
        let Some((_, process)) = table.get(&std::process::id()) else {
            return;
        };
        let identity = host_fingerprint(process);
        if machine_identity().is_some() {
            assert_eq!(observe_host(&identity), HostObservation::Same);
        }
        assert_eq!(
            observe_host(&HostIdentity {
                pid: process.pid,
                started: "foreign\0boot\0start".into()
            }),
            HostObservation::Unknown
        );
        assert_eq!(
            observe_host(&HostIdentity {
                pid: 0,
                started: identity.started
            }),
            HostObservation::Unknown
        );
    }

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
        let fence = cleanup_fence(config.path(), "native-2").unwrap();
        forget(config.path(), &fence).unwrap();
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
        let fence = cleanup_fence(config.path(), "native-1").unwrap();
        forget(config.path(), &fence).unwrap();
        assert_eq!(get("native-1"), None);
    }

    #[test]
    fn late_cleanup_does_not_remove_a_republished_legacy_host_association() {
        let config = tempfile::tempdir().unwrap();
        let process = Process {
            pid: 123,
            started: "synthetic-start".into(),
            name: "/synthetic/host".into(),
        };
        record_under(&directory(config.path()), &process, "native").unwrap();
        let fence = cleanup_fence(config.path(), "native").unwrap();
        // Native ID, PID and process-start fingerprint can all be reused by a fresh start.
        record_under(&directory(config.path()), &process, "native").unwrap();
        forget(config.path(), &fence).unwrap();
        assert_eq!(
            recorded(config.path(), &[process]),
            Recorded::One("native".into())
        );
        let current = cleanup_fence(config.path(), "native").unwrap();
        forget(config.path(), &current).unwrap();
        assert!(records(&directory(config.path())).is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn ordinary_event_during_end_identification_preserves_shared_host_end_intent() {
        let config = tempfile::tempdir().unwrap();
        let process = Process {
            pid: 123,
            started: "synthetic-start".into(),
            name: "/synthetic/codex".into(),
        };
        let context: SessionContext = serde_json::from_value(json!({
            "session": "native", "start_nonce": "nonce", "incarnation": "inc",
            "agent": "codex", "hub": "https://hub.example", "repo": "acme/widgets",
            "root": root("one"), "protocol": 1, "registered": true,
            "observed_at": 10, "host": null,
            "credential_id": "token-id", "binding_id": "binding"
        }))
        .unwrap();
        save_context(config.path(), &context).unwrap();
        assert!(
            record_event_for(
                config.path(),
                &process,
                "native",
                "PreToolUse-before-start",
                false
            )
            .unwrap()
            .is_none()
        );
        assert!(records(&directory(config.path())).is_empty());
        assert!(
            record_event_for(config.path(), &process, "native", "SessionStart-1", true)
                .unwrap()
                .is_none()
        );
        record_under(&directory(config.path()), &process, "sibling").unwrap();
        assert_eq!(
            recorded(config.path(), std::slice::from_ref(&process)),
            Recorded::Several
        );
        let file = directory(config.path()).join(format!("{}.{}", process.pid, tag("native")));
        let before = route_record(&file).unwrap();
        let fence = cleanup_fence(config.path(), "native").unwrap();
        // An earlier ordinary event reaches publication while SessionEnd identifies its host.
        assert!(
            record_event_for(config.path(), &process, "native", "PreToolUse-2", false)
                .unwrap()
                .is_none()
        );
        let after = route_record(&file).unwrap();
        assert_eq!(before["association"], after["association"]);
        assert_ne!(before["proof"], after["proof"]);
        crate::client_cooperation::remember_end(config.path(), "codex", &fence, 20).unwrap();
        let saved = super::context(config.path(), "native", &context.hub, &context.repo)
            .unwrap()
            .unwrap();
        assert!(saved.explicit_end_requested);
        assert_eq!(saved.exit_observed_at, Some(20));
        // The shared host has no qualified process-exit recovery; explicit end must remain.
        assert!(saved.host.is_none());
        let journal = crate::client_journal::inspect(config.path()).unwrap();
        assert_eq!(journal.records.len(), 1);
        let intent = &journal.records[0];
        assert_eq!(intent.phase, crate::client_journal::Phase::Prepared);
        assert_eq!(intent.payload["start_nonce"], "nonce");
        assert_eq!(intent.payload["end_intent"], true);
        assert_eq!(intent.payload["observed_at"], 20);
        assert!(intent.payload["disposition"].is_null());
        forget(config.path(), &fence).unwrap();
        assert_eq!(
            recorded(config.path(), &[process]),
            Recorded::One("sibling".into())
        );
        assert!(
            super::context(config.path(), "native", &context.hub, &context.repo)
                .unwrap()
                .unwrap()
                .explicit_end_requested
        );
        assert_eq!(
            crate::client_journal::inspect(config.path())
                .unwrap()
                .records,
            journal.records
        );
    }

    #[cfg(unix)]
    #[test]
    fn refreshing_the_same_route_preserves_terminal_context() {
        let config = tempfile::tempdir().unwrap();
        let context = SessionContext {
            session: "s".into(),
            start_nonce: "nonce".into(),
            incarnation: Some("inc".into()),
            agent: "codex".into(),
            hub: "https://hub".into(),
            repo: "o/r".into(),
            root: root("one"),
            protocol: 1,
            protocol_checked_at: 10,
            terminal: true,
            registered: true,
            qualification: None,
            exit_ineligible: false,
            revoked: false,
            exit_observed_at: None,
            explicit_end_requested: false,
            observed_at: 10,
            host: None,
            registration_host: None,
            credential_id: "token-id".into(),
            binding_id: "binding".into(),
        };
        save_context(config.path(), &context).unwrap();
        record_route(config.path(), "s", "https://hub", "o/r", &context.root).unwrap();
        assert_eq!(
            super::context(config.path(), "s", "https://hub", "o/r").unwrap(),
            Some(context.clone())
        );
        assert_eq!(
            contexts(config.path()).unwrap().contexts,
            std::slice::from_ref(&context)
        );
        let mut pending = context.clone();
        pending.start_nonce = "fresh".into();
        pending.terminal = false;
        pending.registered = false;
        begin_context(config.path(), Some(&context.start_nonce), &pending).unwrap();
        let mut registered = pending.clone();
        registered.incarnation = Some("new-inc".into());
        registered.registered = true;
        registered.exit_observed_at = Some(20);
        save_context(config.path(), &registered).unwrap();
        save_context(config.path(), &pending).unwrap();
        let current = super::context(config.path(), "s", "https://hub", "o/r")
            .unwrap()
            .unwrap();
        assert_eq!(current.incarnation.as_deref(), Some("new-inc"));
        assert!(current.registered);
        assert_eq!(current.exit_observed_at, Some(20));
        assert!(save_context(config.path(), &context).is_err());
        assert!(begin_context(config.path(), Some(&context.start_nonce), &pending).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn stale_context_preserves_captured_hosts_until_a_fresh_start() {
        let config = tempfile::tempdir().unwrap();
        let pending = lifecycle_context("native", false);
        save_context(config.path(), &pending).unwrap();
        let host = HostIdentity {
            pid: 123,
            started: "synthetic-host-a".into(),
        };
        let other = HostIdentity {
            pid: 456,
            started: "synthetic-host-b".into(),
        };
        let mut qualified = pending.clone();
        qualified.host = Some(host.clone());
        qualified.registration_host = Some(host);
        qualified.qualification = Some(Qualification::Qualified);
        save_context(config.path(), &qualified).unwrap();
        save_context(config.path(), &pending).unwrap();
        assert_eq!(
            context(config.path(), &pending.session, &pending.hub, &pending.repo).unwrap(),
            Some(qualified.clone())
        );
        for registration in [false, true] {
            let mut conflicting = qualified.clone();
            if registration {
                conflicting.registration_host = Some(other.clone());
            } else {
                conflicting.host = Some(other.clone());
            }
            assert!(save_context(config.path(), &conflicting).is_err());
            assert_eq!(
                context(config.path(), &pending.session, &pending.hub, &pending.repo).unwrap(),
                Some(qualified.clone())
            );
        }
        let mut fresh = pending.clone();
        fresh.start_nonce = "fresh-start".into();
        fresh.registration_host = Some(other);
        begin_context(config.path(), Some(&pending.start_nonce), &fresh).unwrap();
        assert_eq!(
            context(config.path(), &pending.session, &pending.hub, &pending.repo).unwrap(),
            Some(fresh)
        );
        assert!(save_context(config.path(), &qualified).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn exit_parking_respects_monotone_qualification_in_both_reply_orders() {
        for observed in [Qualification::Unknown, Qualification::Mixed] {
            for parked_first in [false, true] {
                let config = tempfile::tempdir().unwrap();
                let initial = lifecycle_context("native", false);
                save_context(config.path(), &initial).unwrap();
                let mut parked = initial.clone();
                parked.qualification = Some(observed);
                parked.exit_observed_at = Some(20);
                parked.exit_ineligible = true;
                let mut registered = initial.clone();
                registered.qualification = Some(Qualification::Qualified);
                let writes = if parked_first {
                    [&parked, &registered]
                } else {
                    [&registered, &parked]
                };
                for value in writes {
                    save_context(config.path(), value).unwrap();
                }
                let current = context(config.path(), &initial.session, &initial.hub, &initial.repo)
                    .unwrap()
                    .unwrap();
                assert!(!current.terminal);
                assert_eq!(current.exit_observed_at, Some(20));
                assert_eq!(current.exit_ineligible, observed == Qualification::Mixed);
                assert_eq!(
                    current.qualification,
                    Some(if observed == Qualification::Mixed {
                        Qualification::Mixed
                    } else {
                        Qualification::Qualified
                    })
                );

                let mut successor = initial.clone();
                successor.start_nonce = "fresh-start".into();
                successor.registered = false;
                successor.incarnation = None;
                begin_context(config.path(), Some(&initial.start_nonce), &successor).unwrap();
                assert!(save_context(config.path(), &parked).is_err());
                assert_eq!(
                    context(config.path(), &initial.session, &initial.hub, &initial.repo).unwrap(),
                    Some(successor)
                );
            }
        }
    }

    #[cfg(unix)]
    #[test]
    fn parked_exit_pruning_requires_retention_and_no_pending_evidence() {
        let config = tempfile::tempdir().unwrap();
        let mut parked = lifecycle_context("parked", false);
        parked.qualification = Some(Qualification::Mixed);
        parked.exit_ineligible = true;
        parked.exit_observed_at = Some(20);
        save_context(config.path(), &parked).unwrap();
        let file =
            routes(config.path()).join(route_tag(&parked.session, &parked.hub, &parked.repo));
        prune_contexts(config.path()).unwrap();
        assert!(file.exists(), "a new parked record is retained");
        std::fs::File::open(&file)
            .unwrap()
            .set_times(std::fs::FileTimes::new().set_modified(
                std::time::SystemTime::now() - ROUTE_KEEP - std::time::Duration::from_secs(1),
            ))
            .unwrap();
        pending_context(config.path(), &parked);
        prune_contexts(config.path()).unwrap();
        assert!(
            file.exists(),
            "an uncertain report still references this route"
        );
        let pending = crate::client_journal::inspect(config.path())
            .unwrap()
            .records
            .pop()
            .unwrap();
        crate::client_journal::remove_if(config.path(), &pending).unwrap();
        prune_contexts(config.path()).unwrap();
        assert!(!file.exists());
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
        let fence = cleanup_fence(config.path(), "s").unwrap();
        forget(config.path(), &fence).unwrap();
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
        let fence = cleanup_fence(config.path(), "native").unwrap();
        forget(config.path(), &fence).unwrap();
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
