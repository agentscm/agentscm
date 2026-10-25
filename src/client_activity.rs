//! Best-effort local observations for the optional native tray.
//!
//! The hook and MCP clients send one bounded Unix datagram after a request has a known outcome.
//! A missing, stopped, or backpressured tray is deliberately indistinguishable from success here:
//! activity reporting must never extend or change the client request path.

use std::path::Path;
#[cfg(any(unix, test))]
use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::client_config::Connection;

const VERSION: u8 = 1;
/// Room for the longest valid unconnected report: a JSON-escaped root of [`MAX_ROOT_BYTES`].
const DATAGRAM_BYTES: usize = 16 * 1024;
const MAX_ROOT_BYTES: usize = 4096;
/// Unix datagram sockets cap one message at their buffer size: 2 KiB to send and 4 KiB to
/// receive by default on macOS. Both ends raise it so every valid report fits.
#[cfg(unix)]
const SOCKET_BUFFER_BYTES: libc::c_int = 64 * 1024;
#[cfg(any(target_os = "macos", test))]
const MAX_ROWS: usize = 256;
#[cfg(any(target_os = "macos", test))]
const MAX_DRAIN: usize = 64;
#[cfg(any(target_os = "macos", test))]
pub(crate) const STALE_AFTER: std::time::Duration = std::time::Duration::from_secs(15 * 60);
#[cfg(any(target_os = "macos", test))]
const MAX_UNCONNECTED: usize = 8;
/// The hook repeats an unconnected sighting every few minutes while an agent keeps working.
#[cfg(any(target_os = "macos", test))]
pub(crate) const UNCONNECTED_STALE_AFTER: std::time::Duration =
    std::time::Duration::from_secs(60 * 60);

/// Local activity fence for one exact saved destination and credential.
/// This is separate from the Hub-facing onboarding client identity.
pub(crate) fn activity_binding(connection: &Connection, token: &str) -> String {
    fn field(digest: &mut Sha256, bytes: &[u8]) {
        digest.update((bytes.len() as u64).to_be_bytes());
        digest.update(bytes);
    }

    let mut digest = Sha256::new();
    digest.update(b"agentscm-local-activity-binding-v1\0");
    field(&mut digest, connection.root.as_os_str().as_encoded_bytes());
    field(&mut digest, connection.origin_host.as_bytes());
    field(&mut digest, connection.hub.as_bytes());
    field(&mut digest, connection.repo.as_bytes());
    field(&mut digest, token.as_bytes());
    format!("{:x}", digest.finalize())
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ActivityPath {
    Hook,
    Mcp,
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ActivityOutcome {
    Acknowledged,
    TokenRejected,
    Unavailable,
    InvalidResponse,
    BindingChanged,
    SessionEnded,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
struct WireObservation {
    version: u8,
    binding: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    agent: Option<String>,
    path: ActivityPath,
    outcome: ActivityOutcome,
}

/// A coding agent ran in a Git checkout with no client connection. `root` is the main worktree.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
struct WireUnconnected {
    version: u8,
    root: String,
    host: String,
    repo: String,
    agent: String,
}

/// One observation timestamped by the receiving tray's monotonic clock.
#[derive(Clone, Debug)]
#[cfg(any(target_os = "macos", test))]
pub(crate) struct ActivityObservation {
    pub(crate) binding: String,
    pub(crate) agent: Option<String>,
    pub(crate) path: ActivityPath,
    pub(crate) outcome: ActivityOutcome,
    pub(crate) observed_at: std::time::Instant,
}

#[cfg(any(target_os = "macos", test))]
impl ActivityObservation {
    pub(crate) fn is_fresh_at(&self, now: std::time::Instant) -> bool {
        now.saturating_duration_since(self.observed_at) <= STALE_AFTER
    }
}

/// An unconnected checkout, deduplicated by main worktree, with its agents oldest first.
#[derive(Clone, Debug)]
#[cfg(any(target_os = "macos", test))]
pub(crate) struct UnconnectedCheckout {
    pub(crate) root: PathBuf,
    pub(crate) host: String,
    pub(crate) repo: String,
    pub(crate) agents: Vec<String>,
    pub(crate) seen_at: std::time::Instant,
}

/// Bounded tray-owned activity state. A snapshot is fenced to the caller's current binding.
#[derive(Debug, Default)]
#[cfg(any(target_os = "macos", test))]
pub(crate) struct ActivityStore {
    rows: std::collections::VecDeque<ActivityObservation>,
    unconnected: Vec<UnconnectedCheckout>,
}

#[cfg(any(target_os = "macos", test))]
impl ActivityStore {
    fn push(&mut self, observation: ActivityObservation) {
        if self.rows.len() == MAX_ROWS {
            self.rows.pop_front();
        }
        self.rows.push_back(observation);
    }

    pub(crate) fn snapshot(
        &self,
        current_binding: &str,
        now: std::time::Instant,
    ) -> Vec<ActivityObservation> {
        self.rows
            .iter()
            .filter(|row| row.binding == current_binding && row.is_fresh_at(now))
            .cloned()
            .collect()
    }

    pub(crate) fn len(&self) -> usize {
        self.rows.len()
    }

    pub(crate) fn push_unconnected(&mut self, seen: UnconnectedCheckout) {
        if let Some(row) = self
            .unconnected
            .iter_mut()
            .find(|row| row.root == seen.root)
        {
            row.agents.retain(|agent| !seen.agents.contains(agent));
            row.agents.extend(seen.agents);
            row.host = seen.host;
            row.repo = seen.repo;
            row.seen_at = seen.seen_at;
            return;
        }
        if self.unconnected.len() == MAX_UNCONNECTED
            && let Some(oldest) =
                (0..self.unconnected.len()).min_by_key(|index| self.unconnected[*index].seen_at)
        {
            self.unconnected.swap_remove(oldest);
        }
        self.unconnected.push(seen);
    }

    /// Drop the sighting of a checkout that is now connected. Removing that connection later
    /// lists it again only after an agent is seen there again.
    pub(crate) fn forget_unconnected(&mut self, root: &Path) {
        self.unconnected.retain(|row| row.root != root);
    }

    /// Fresh unconnected checkouts, most recently seen first.
    pub(crate) fn unconnected(&self, now: std::time::Instant) -> Vec<UnconnectedCheckout> {
        let mut rows = self
            .unconnected
            .iter()
            .filter(|row| now.saturating_duration_since(row.seen_at) <= UNCONNECTED_STALE_AFTER)
            .cloned()
            .collect::<Vec<_>>();
        rows.sort_by_key(|row| std::cmp::Reverse(row.seen_at));
        rows
    }

    /// Discard observations after a suspend/wake or wall-clock discontinuity detected by the tray.
    #[cfg(target_os = "macos")]
    pub(crate) fn clear(&mut self) {
        self.rows.clear();
        self.unconnected.clear();
    }
}

fn valid_binding(binding: &str) -> bool {
    binding.len() == 64 && binding.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn valid_agent(agent: Option<&str>) -> bool {
    agent.is_none_or(|agent| ["claude", "codex", "cursor", "copilot", "gemini"].contains(&agent))
}

#[cfg(any(target_os = "macos", test))]
fn decode(bytes: &[u8], observed_at: std::time::Instant) -> Option<ActivityObservation> {
    if bytes.is_empty() || bytes.len() > DATAGRAM_BYTES {
        return None;
    }
    let wire: WireObservation = serde_json::from_slice(bytes).ok()?;
    if wire.version != VERSION
        || !valid_binding(&wire.binding)
        || !valid_agent(wire.agent.as_deref())
    {
        return None;
    }
    Some(ActivityObservation {
        binding: wire.binding,
        agent: wire.agent,
        path: wire.path,
        outcome: wire.outcome,
        observed_at,
    })
}

fn valid_unconnected(root: &str, host: &str, repo: &str, agent: &str) -> bool {
    let printable = |text: &str| !text.is_empty() && !text.chars().any(char::is_control);
    printable(root)
        && root.len() <= MAX_ROOT_BYTES
        && Path::new(root).is_absolute()
        && printable(host)
        && host.len() <= 253
        && !host.contains(char::is_whitespace)
        && crate::intent_cli::valid_repo(repo)
        && valid_agent(Some(agent))
}

#[cfg(any(target_os = "macos", test))]
fn decode_unconnected(bytes: &[u8], seen_at: std::time::Instant) -> Option<UnconnectedCheckout> {
    if bytes.is_empty() || bytes.len() > DATAGRAM_BYTES {
        return None;
    }
    let wire: WireUnconnected = serde_json::from_slice(bytes).ok()?;
    if wire.version != VERSION
        || !valid_unconnected(&wire.root, &wire.host, &wire.repo, &wire.agent)
    {
        return None;
    }
    Some(UnconnectedCheckout {
        root: PathBuf::from(wire.root),
        host: wire.host,
        repo: wire.repo,
        agents: vec![wire.agent],
        seen_at,
    })
}

#[cfg(unix)]
pub(crate) fn socket_path() -> Option<PathBuf> {
    crate::hub_cli::config_dir()
        .ok()
        .map(|dir| dir.join("agentscm").join("tray").join("activity.sock"))
}

/// Report a known request outcome without waiting for the tray or surfacing an error.
pub(crate) fn observe(
    binding: &str,
    agent: Option<&str>,
    path: ActivityPath,
    outcome: ActivityOutcome,
) {
    #[cfg(unix)]
    if let Some(pathname) = socket_path() {
        observe_at(&pathname, binding, agent, path, outcome);
    }
    #[cfg(not(unix))]
    let _ = (binding, agent, path, outcome);
}

#[cfg(unix)]
fn observe_at(
    pathname: &Path,
    binding: &str,
    agent: Option<&str>,
    path: ActivityPath,
    outcome: ActivityOutcome,
) {
    if !valid_binding(binding) || !valid_agent(agent) {
        return;
    }
    send_at(
        pathname,
        &WireObservation {
            version: VERSION,
            binding: binding.to_owned(),
            agent: agent.map(str::to_owned),
            path,
            outcome,
        },
    );
}

/// Whether a tray is bound to the socket at `pathname`. A stale socket left by a tray that
/// crashed refuses the connection.
#[cfg(unix)]
pub(crate) fn listening_at(pathname: &Path) -> bool {
    std::os::unix::net::UnixDatagram::unbound()
        .and_then(|socket| socket.connect(pathname))
        .is_ok()
}

/// Report a coding agent working in a checkout that has no client connection. Like [`observe`],
/// this never waits for the tray, and nothing leaves the machine. True when a tray accepted it.
#[cfg(unix)]
pub(crate) fn observe_unconnected_at(
    pathname: &Path,
    root: &Path,
    host: &str,
    repo: &str,
    agent: &str,
) -> bool {
    let Some(root) = root.to_str() else {
        return false;
    };
    if !valid_unconnected(root, host, repo, agent) {
        return false;
    }
    send_at(
        pathname,
        &WireUnconnected {
            version: VERSION,
            root: root.to_owned(),
            host: host.to_owned(),
            repo: repo.to_owned(),
            agent: agent.to_owned(),
        },
    )
}

/// True when the datagram was queued at a listening, owner-controlled socket.
#[cfg(unix)]
fn send_at(pathname: &Path, wire: &impl Serialize) -> bool {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    use std::os::unix::net::UnixDatagram;

    let Some(parent) = pathname.parent() else {
        return false;
    };
    let Ok(metadata) = std::fs::symlink_metadata(parent) else {
        return false;
    };
    // SAFETY: geteuid has no arguments or memory access preconditions.
    if !metadata.file_type().is_dir()
        || metadata.uid() != unsafe { libc::geteuid() }
        || metadata.permissions().mode() & 0o077 != 0
    {
        return false;
    }
    let Ok(bytes) = serde_json::to_vec(wire) else {
        return false;
    };
    if bytes.len() > DATAGRAM_BYTES {
        return false;
    }
    let Ok(socket) = UnixDatagram::unbound() else {
        return false;
    };
    socket.set_nonblocking(true).is_ok()
        && set_buffer(&socket, libc::SO_SNDBUF)
        && socket.send_to(&bytes, pathname).is_ok()
}

#[cfg(unix)]
fn set_buffer(socket: &std::os::unix::net::UnixDatagram, option: libc::c_int) -> bool {
    use std::os::fd::AsRawFd;
    let size = SOCKET_BUFFER_BYTES;
    // SAFETY: the descriptor stays open for the call, and the value pointer and length describe
    // `size`, which outlives it.
    unsafe {
        libc::setsockopt(
            socket.as_raw_fd(),
            libc::SOL_SOCKET,
            option,
            (&raw const size).cast(),
            size_of::<libc::c_int>() as libc::socklen_t,
        ) == 0
    }
}

/// Tray-owned socket lifecycle. The tray is responsible for singleton coordination before bind.
#[cfg(all(unix, any(target_os = "macos", test)))]
#[derive(Debug)]
pub(crate) struct ActivityReceiver {
    socket: std::os::unix::net::UnixDatagram,
    path: PathBuf,
    identity: (u64, u64),
}

#[cfg(all(unix, any(target_os = "macos", test)))]
impl ActivityReceiver {
    /// Bind after the tray has acquired its singleton lock. That lock makes removal of a
    /// same-owner stale socket from a crashed prior tray safe.
    #[cfg(target_os = "macos")]
    pub(crate) fn bind() -> std::io::Result<Self> {
        let path = socket_path().ok_or_else(|| std::io::Error::other("config unavailable"))?;
        Self::bind_at(path)
    }

    fn bind_at(path: PathBuf) -> std::io::Result<Self> {
        use std::os::unix::fs::{FileTypeExt, MetadataExt, PermissionsExt};
        use std::os::unix::net::UnixDatagram;

        let parent = path
            .parent()
            .ok_or_else(|| std::io::Error::other("invalid activity socket path"))?;
        std::fs::create_dir_all(parent)?;
        let metadata = std::fs::symlink_metadata(parent)?;
        // SAFETY: geteuid has no arguments or memory access preconditions.
        if !metadata.file_type().is_dir() || metadata.uid() != unsafe { libc::geteuid() } {
            return Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "activity socket directory is not owner-controlled",
            ));
        }
        std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700))?;
        match std::fs::symlink_metadata(&path) {
            Ok(metadata)
                if metadata.file_type().is_socket()
                    && metadata.uid() == unsafe { libc::geteuid() } =>
            {
                std::fs::remove_file(&path)?;
            }
            Ok(_) => {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::AlreadyExists,
                    "activity socket path is not an owned socket",
                ));
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
        let socket = UnixDatagram::bind(&path)?;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
        let metadata = std::fs::symlink_metadata(&path)?;
        if !metadata.file_type().is_socket() || metadata.uid() != unsafe { libc::geteuid() } {
            let _ = std::fs::remove_file(&path);
            return Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "activity socket is not owner-controlled",
            ));
        }
        let identity = (metadata.dev(), metadata.ino());
        socket.set_nonblocking(true)?;
        if !set_buffer(&socket, libc::SO_RCVBUF) {
            return Err(std::io::Error::last_os_error());
        }
        Ok(Self {
            socket,
            path,
            identity,
        })
    }

    /// Drain currently queued datagrams. Invalid, truncated, and oversized input is ignored.
    pub(crate) fn drain(&self, store: &mut ActivityStore) {
        self.drain_with_limit(store, MAX_DRAIN);
    }

    fn drain_with_limit(&self, store: &mut ActivityStore, limit: usize) {
        let mut bytes = vec![0_u8; DATAGRAM_BYTES + 1];
        for _ in 0..limit {
            match self.socket.recv(&mut bytes) {
                Ok(size) if size <= DATAGRAM_BYTES => {
                    let now = std::time::Instant::now();
                    if let Some(observation) = decode(&bytes[..size], now) {
                        store.push(observation);
                    } else if let Some(seen) = decode_unconnected(&bytes[..size], now) {
                        store.push_unconnected(seen);
                    }
                }
                Ok(_) => {}
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => break,
                Err(_) => break,
            }
        }
    }
}

#[cfg(all(unix, any(target_os = "macos", test)))]
impl Drop for ActivityReceiver {
    fn drop(&mut self) {
        use std::os::unix::fs::MetadataExt;
        if std::fs::symlink_metadata(&self.path)
            .is_ok_and(|metadata| (metadata.dev(), metadata.ino()) == self.identity)
        {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    const BINDING: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

    fn connection() -> Connection {
        Connection {
            root: PathBuf::from("/synthetic/root"),
            origin_host: "github.com".into(),
            hub: "https://hub.example.test".into(),
            repo: "owner/repo".into(),
            token_file: PathBuf::from("/synthetic/token"),
            report_completion: false,
            owned_token_id: None,
            owned_token_expires_at: None,
            agents: Vec::new(),
        }
    }

    #[test]
    fn activity_binding_fences_every_destination_field_and_token() {
        let base = connection();
        let expected = activity_binding(&base, "synthetic-secret");
        let variants = [
            Connection {
                root: PathBuf::from("/synthetic/other"),
                ..base.clone()
            },
            Connection {
                origin_host: "gitlab.com".into(),
                ..base.clone()
            },
            Connection {
                hub: "https://other.example.test".into(),
                ..base.clone()
            },
            Connection {
                repo: "owner/other".into(),
                ..base.clone()
            },
        ];
        for variant in variants {
            assert_ne!(activity_binding(&variant, "synthetic-secret"), expected);
        }
        assert_ne!(activity_binding(&base, "other-secret"), expected);
        assert_eq!(expected.len(), 64);
        assert!(!expected.contains("synthetic") && !expected.contains("secret"));
    }

    #[test]
    fn wire_format_has_no_secret_or_content_fields() {
        let bytes = serde_json::to_vec(&WireObservation {
            version: VERSION,
            binding: BINDING.into(),
            agent: Some("codex".into()),
            path: ActivityPath::Hook,
            outcome: ActivityOutcome::Acknowledged,
        })
        .unwrap();
        assert!(bytes.len() <= DATAGRAM_BYTES);
        let value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        let keys = value
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect::<std::collections::BTreeSet<_>>();
        assert_eq!(
            keys,
            ["agent", "binding", "outcome", "path", "version"]
                .into_iter()
                .collect()
        );
        let observation = decode(&bytes, std::time::Instant::now()).unwrap();
        assert_eq!(observation.path, ActivityPath::Hook);
        assert_eq!(observation.outcome, ActivityOutcome::Acknowledged);
    }

    #[test]
    fn store_is_bounded_stale_neutral_and_binding_fenced() {
        let now = std::time::Instant::now();
        let other = "abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789";
        let mut store = ActivityStore::default();
        for index in 0..(MAX_ROWS + 4) {
            store.push(ActivityObservation {
                binding: if index == MAX_ROWS + 3 {
                    other
                } else {
                    BINDING
                }
                .into(),
                agent: None,
                path: ActivityPath::Mcp,
                outcome: ActivityOutcome::Unavailable,
                observed_at: now,
            });
        }
        assert_eq!(store.len(), MAX_ROWS);
        assert_eq!(store.snapshot(other, now).len(), 1);
        assert!(
            store
                .snapshot(
                    BINDING,
                    now + STALE_AFTER + std::time::Duration::from_millis(1)
                )
                .is_empty()
        );
    }

    #[test]
    fn invalid_and_oversized_input_is_ignored() {
        let now = std::time::Instant::now();
        assert!(decode(b"not-json", now).is_none());
        assert!(decode(&vec![b'x'; DATAGRAM_BYTES + 1], now).is_none());
        assert!(
            decode(
                br#"{"version":1,"binding":"short","path":"mcp","outcome":"acknowledged"}"#,
                now
            )
            .is_none()
        );
    }

    fn seen(root: &str, agent: &str, seen_at: std::time::Instant) -> UnconnectedCheckout {
        UnconnectedCheckout {
            root: PathBuf::from(root),
            host: "github.com".into(),
            repo: "acme/widgets".into(),
            agents: vec![agent.into()],
            seen_at,
        }
    }

    /// An absolute root on every platform: `/synthetic/x` has no drive and is relative on Windows.
    fn absolute(name: &str) -> String {
        std::env::temp_dir().join(name).to_str().unwrap().to_owned()
    }

    #[test]
    fn unconnected_wire_format_round_trips_and_is_distinct_from_activity() {
        let root = absolute("acme widgets");
        let bytes = serde_json::to_vec(&WireUnconnected {
            version: VERSION,
            root: root.clone(),
            host: "github.com".into(),
            repo: "acme/widgets".into(),
            agent: "claude".into(),
        })
        .unwrap();
        let value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        let keys = value
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect::<std::collections::BTreeSet<_>>();
        assert_eq!(
            keys,
            ["agent", "host", "repo", "root", "version"]
                .into_iter()
                .collect()
        );
        let now = std::time::Instant::now();
        let decoded = decode_unconnected(&bytes, now).unwrap();
        assert_eq!(decoded.root, PathBuf::from(root));
        assert_eq!(decoded.agents, ["claude"]);
        assert!(decode(&bytes, now).is_none());

        let activity = serde_json::to_vec(&WireObservation {
            version: VERSION,
            binding: BINDING.into(),
            agent: None,
            path: ActivityPath::Mcp,
            outcome: ActivityOutcome::Acknowledged,
        })
        .unwrap();
        assert!(decode_unconnected(&activity, now).is_none());
    }

    #[test]
    fn invalid_unconnected_input_is_ignored() {
        let now = std::time::Instant::now();
        let valid = absolute("root");
        // Each case breaks one rule of an otherwise valid observation.
        assert!(valid_unconnected(
            &valid,
            "github.com",
            "acme/widgets",
            "claude"
        ));
        let broken = absolute("\nroot");
        for (root, host, repo, agent) in [
            ("relative/root", "github.com", "acme/widgets", "claude"),
            (broken.as_str(), "github.com", "acme/widgets", "claude"),
            (valid.as_str(), "", "acme/widgets", "claude"),
            (valid.as_str(), "git hub.com", "acme/widgets", "claude"),
            (valid.as_str(), "github.com", "not-a-repo", "claude"),
            (valid.as_str(), "github.com", "acme/widgets", "unknown"),
        ] {
            assert!(
                !valid_unconnected(root, host, repo, agent),
                "{root} {host} {repo} {agent}"
            );
            let bytes = serde_json::to_vec(&WireUnconnected {
                version: VERSION,
                root: root.into(),
                host: host.into(),
                repo: repo.into(),
                agent: agent.into(),
            })
            .unwrap();
            assert!(decode_unconnected(&bytes, now).is_none());
        }
        let wire = serde_json::json!({"version":1, "root":valid, "host":"github.com", "repo":"a/b", "agent":"codex"});
        assert!(decode_unconnected(&serde_json::to_vec(&wire).unwrap(), now).is_some());
        for (key, value) in [
            ("version", serde_json::json!(2)),
            ("token", serde_json::json!("x")),
        ] {
            let mut changed = wire.clone();
            changed[key] = value;
            assert!(
                decode_unconnected(&serde_json::to_vec(&changed).unwrap(), now).is_none(),
                "{key}"
            );
        }
    }

    #[test]
    fn a_connected_checkout_is_forgotten_until_it_is_seen_again() {
        let now = std::time::Instant::now();
        let mut store = ActivityStore::default();
        store.push_unconnected(seen("/synthetic/a", "claude", now));
        store.push_unconnected(seen("/synthetic/b", "codex", now));
        store.forget_unconnected(Path::new("/synthetic/a"));
        let rows = store.unconnected(now);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].root, Path::new("/synthetic/b"));
        store.push_unconnected(seen("/synthetic/a", "claude", now + Duration::from_secs(1)));
        assert_eq!(store.unconnected(now + Duration::from_secs(1)).len(), 2);
    }

    #[test]
    fn unconnected_rows_dedupe_by_checkout_expire_and_are_capped() {
        let start = std::time::Instant::now();
        let mut store = ActivityStore::default();
        store.push_unconnected(seen("/synthetic/a", "claude", start));
        store.push_unconnected(seen(
            "/synthetic/a",
            "codex",
            start + Duration::from_secs(1),
        ));
        store.push_unconnected(seen(
            "/synthetic/a",
            "claude",
            start + Duration::from_secs(2),
        ));
        let rows = store.unconnected(start + Duration::from_secs(2));
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].agents, ["codex", "claude"]);
        assert_eq!(rows[0].seen_at, start + Duration::from_secs(2));

        for index in 0..MAX_UNCONNECTED + 2 {
            store.push_unconnected(seen(
                &format!("/synthetic/{index}"),
                "gemini",
                start + Duration::from_secs(10 + index as u64),
            ));
        }
        let now = start + Duration::from_secs(100);
        let rows = store.unconnected(now);
        assert_eq!(rows.len(), MAX_UNCONNECTED);
        assert_eq!(
            rows[0].root,
            PathBuf::from(format!("/synthetic/{}", MAX_UNCONNECTED + 1))
        );
        assert!(!rows.iter().any(|row| row.root == Path::new("/synthetic/a")));
        assert!(!rows.iter().any(|row| row.root == Path::new("/synthetic/0")));
        assert!(
            store
                .unconnected(
                    start
                        + UNCONNECTED_STALE_AFTER
                        + Duration::from_secs(10 + MAX_UNCONNECTED as u64 + 2)
                )
                .is_empty()
        );
    }

    #[cfg(unix)]
    #[test]
    fn unconnected_observations_reach_the_receiver() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("private").join("activity.sock");
        let receiver = ActivityReceiver::bind_at(path.clone()).unwrap();
        observe_unconnected_at(
            &path,
            Path::new("/synthetic/widgets"),
            "github.com",
            "acme/widgets",
            "codex",
        );
        observe_unconnected_at(
            &path,
            Path::new("relative"),
            "github.com",
            "acme/widgets",
            "codex",
        );
        let mut store = ActivityStore::default();
        receiver.drain(&mut store);
        assert_eq!(store.len(), 0);
        let rows = store.unconnected(std::time::Instant::now());
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].repo, "acme/widgets");
        assert_eq!(rows[0].agents, ["codex"]);
    }

    #[cfg(unix)]
    #[test]
    fn the_longest_valid_root_is_delivered_and_longer_roots_are_refused() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("private").join("activity.sock");
        let receiver = ActivityReceiver::bind_at(path.clone()).unwrap();
        // Quotes and backslashes double in JSON, so this is the largest possible report.
        let longest = format!("/{}", "\"\\".repeat((MAX_ROOT_BYTES - 1) / 2));
        assert!(longest.len() <= MAX_ROOT_BYTES && longest.len() > MAX_ROOT_BYTES - 2);
        let too_long = format!("/{}", "a".repeat(MAX_ROOT_BYTES));
        for root in [&too_long, &longest] {
            observe_unconnected_at(
                &path,
                Path::new(root),
                &"h".repeat(253),
                "acme/widgets",
                "copilot",
            );
        }
        let mut store = ActivityStore::default();
        receiver.drain(&mut store);
        let rows = store.unconnected(std::time::Instant::now());
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].root, Path::new(&longest));
    }

    #[cfg(unix)]
    #[test]
    fn missing_receiver_is_silent_and_receiver_drains_valid_only() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("missing.sock");
        observe_at(
            &missing,
            BINDING,
            Some("codex"),
            ActivityPath::Hook,
            ActivityOutcome::Unavailable,
        );

        let path = dir.path().join("private").join("activity.sock");
        let receiver = ActivityReceiver::bind_at(path.clone()).unwrap();
        let sender = std::os::unix::net::UnixDatagram::unbound().unwrap();
        assert!(set_buffer(&sender, libc::SO_SNDBUF));
        sender.send_to(b"bad", &path).unwrap();
        sender
            .send_to(&vec![b'x'; DATAGRAM_BYTES * 2], &path)
            .unwrap();
        observe_at(
            &path,
            BINDING,
            None,
            ActivityPath::Mcp,
            ActivityOutcome::Acknowledged,
        );
        let mut store = ActivityStore::default();
        receiver.drain(&mut store);
        assert_eq!(store.len(), 1);
        let rows = store.snapshot(BINDING, std::time::Instant::now());
        assert_eq!(rows[0].agent, None);
    }

    #[cfg(unix)]
    #[test]
    fn full_receiver_never_surfaces_backpressure() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("private").join("activity.sock");
        let _receiver = ActivityReceiver::bind_at(path.clone()).unwrap();
        for _ in 0..10_000 {
            observe_at(
                &path,
                BINDING,
                Some("claude"),
                ActivityPath::Hook,
                ActivityOutcome::Unavailable,
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn each_drain_is_bounded() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("private").join("activity.sock");
        let receiver = ActivityReceiver::bind_at(path.clone()).unwrap();
        let sender = std::os::unix::net::UnixDatagram::unbound().unwrap();
        let bytes = serde_json::to_vec(&WireObservation {
            version: VERSION,
            binding: BINDING.into(),
            agent: Some("codex".into()),
            path: ActivityPath::Hook,
            outcome: ActivityOutcome::Acknowledged,
        })
        .unwrap();
        for _ in 0..5 {
            sender.send_to(&bytes, &path).unwrap();
        }
        let mut store = ActivityStore::default();
        receiver.drain_with_limit(&mut store, 4);
        assert_eq!(store.len(), 4);
        receiver.drain_with_limit(&mut store, 4);
        assert_eq!(store.len(), 5);
    }

    #[cfg(unix)]
    #[test]
    fn owned_stale_socket_is_recovered_after_singleton() {
        let dir = tempfile::tempdir().unwrap();
        let parent = dir.path().join("private");
        std::fs::create_dir(&parent).unwrap();
        let path = parent.join("activity.sock");
        let stale = std::os::unix::net::UnixDatagram::bind(&path).unwrap();
        drop(stale);
        let receiver = ActivityReceiver::bind_at(path.clone()).unwrap();
        assert!(path.exists());
        drop(receiver);
        assert!(!path.exists());
    }

    #[cfg(unix)]
    #[test]
    fn regular_file_and_symlink_socket_paths_are_refused() {
        let dir = tempfile::tempdir().unwrap();
        let parent = dir.path().join("private");
        std::fs::create_dir(&parent).unwrap();
        let path = parent.join("activity.sock");
        std::fs::write(&path, b"keep").unwrap();
        assert!(ActivityReceiver::bind_at(path.clone()).is_err());
        assert_eq!(std::fs::read(&path).unwrap(), b"keep");
        std::fs::remove_file(&path).unwrap();
        std::os::unix::fs::symlink(parent.join("missing"), &path).unwrap();
        assert!(ActivityReceiver::bind_at(path.clone()).is_err());
        assert!(
            std::fs::symlink_metadata(path)
                .unwrap()
                .file_type()
                .is_symlink()
        );
    }
}
