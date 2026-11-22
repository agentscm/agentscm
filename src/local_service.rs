//! The discovery record left by an older version's local dashboard, read only so that
//! `agentscm open --stop` can stop it.
//!
//! That service wrote one owner-only record under the user's config directory and removed
//! it on shutdown. The record holds the address and a per-process admin secret that can only mint
//! single-use browser connection codes; it never grants reads by itself.

use std::io::Read as _;
use std::path::PathBuf;

use agentscm::Result;
use serde::{Deserialize, Serialize};

const RECORD_BYTES: u64 = 16 * 1024;

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct ServiceRecord {
    pub version: u8,
    pub instance: String,
    pub pid: u32,
    pub base: String,
    pub admin: String,
    pub started_at: i64,
}

/// The service's private directory: its record, native store and log.
pub(crate) fn dir() -> Result<PathBuf> {
    Ok(crate::hub_cli::config_dir()?
        .join("agentscm")
        .join("front-door"))
}

fn record_path() -> Result<PathBuf> {
    Ok(dir()?.join("service.json"))
}

pub(crate) fn valid_hex(value: &str, len: usize) -> bool {
    value.len() == len
        && value
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
}

/// The published record, if one exists, is owner-only and names a live process. A record left
/// by a process that no longer exists is removed.
pub(crate) fn load() -> Result<Option<ServiceRecord>> {
    let path = record_path()?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt as _;
        let metadata = match std::fs::symlink_metadata(&path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error.into()),
        };
        // SAFETY: getuid has no preconditions.
        if !metadata.is_file()
            || metadata.mode() & 0o077 != 0
            || metadata.uid() != unsafe { libc::getuid() }
        {
            return Ok(None);
        }
    }
    let mut text = String::new();
    match std::fs::File::open(&path) {
        Ok(file) => file.take(RECORD_BYTES + 1).read_to_string(&mut text)?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    let Ok(record) = serde_json::from_str::<ServiceRecord>(&text) else {
        return Ok(None);
    };
    if record.version != 1
        || !valid_hex(&record.instance, 32)
        || !valid_hex(&record.admin, 64)
        || !loopback_base(&record.base)
    {
        return Ok(None);
    }
    if !process_alive(record.pid) {
        let _ = std::fs::remove_file(&path);
        return Ok(None);
    }
    Ok(Some(record))
}

/// `http://127.0.0.1:PORT` or `http://[::1]:PORT`, nothing else.
pub(crate) fn loopback_base(base: &str) -> bool {
    let Some(rest) = base.strip_prefix("http://") else {
        return false;
    };
    let Some((host, port)) = rest.rsplit_once(':') else {
        return false;
    };
    matches!(host, "127.0.0.1" | "[::1]")
        && !port.is_empty()
        && port.len() <= 5
        && port.bytes().all(|byte| byte.is_ascii_digit())
}

pub(crate) fn process_alive(pid: u32) -> bool {
    #[cfg(unix)]
    {
        let Ok(pid) = libc::pid_t::try_from(pid) else {
            return false;
        };
        // SAFETY: signal 0 performs only the existence and permission check.
        unsafe {
            libc::kill(pid, 0) == 0
                || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
        }
    }
    #[cfg(not(unix))]
    {
        let _ = pid;
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_loopback_http_bases_are_accepted() {
        assert!(loopback_base("http://127.0.0.1:7340"));
        assert!(loopback_base("http://[::1]:7340"));
        assert!(!loopback_base("https://127.0.0.1:7340"));
        assert!(!loopback_base("http://localhost:7340"));
        assert!(!loopback_base("http://example.com:80"));
        assert!(!loopback_base("http://127.0.0.1:80/path"));
        assert!(!loopback_base("http://127.0.0.1"));
    }
}
