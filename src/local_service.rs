//! The discovery record of this user's local front door (`agentscm serve --front-door`).
//!
//! The running service writes one owner-only record under the user's config directory and removes
//! it on shutdown. The record holds the address and a per-process admin secret that can only mint
//! single-use browser connection codes; it never grants reads by itself.

use std::io::Read as _;
use std::path::PathBuf;

use agentscm::{Error, Result};
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

pub(crate) fn random_hex(bytes: usize) -> Result<String> {
    let mut random = vec![0_u8; bytes];
    getrandom::fill(&mut random).map_err(|_| Error::Invalid("random source unavailable".into()))?;
    Ok(random.iter().map(|byte| format!("{byte:02x}")).collect())
}

/// Create the private directory with owner-only permissions.
pub(crate) fn ensure_dir() -> Result<PathBuf> {
    let dir = dir()?;
    std::fs::create_dir_all(&dir)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700))?;
    }
    Ok(dir)
}

/// The short host name, which the page shows to identify this computer.
pub(crate) fn computer_name() -> String {
    #[cfg(unix)]
    let name = {
        let mut buffer = [0_u8; 256];
        // SAFETY: the buffer is valid for its length; a name that fills it is cut at the end.
        if unsafe { libc::gethostname(buffer.as_mut_ptr().cast(), buffer.len()) } == 0 {
            let end = buffer
                .iter()
                .position(|byte| *byte == 0)
                .unwrap_or(buffer.len());
            String::from_utf8_lossy(&buffer[..end]).into_owned()
        } else {
            String::new()
        }
    };
    #[cfg(not(unix))]
    let name = std::env::var("COMPUTERNAME").unwrap_or_default();
    let short = name.split('.').next().unwrap_or_default();
    let clean: String = short
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_'))
        .take(63)
        .collect();
    if clean.is_empty() {
        "This computer".into()
    } else {
        clean
    }
}

/// Publish this process's record; dropping the guard removes it if it is still ours.
pub(crate) fn publish(record: &ServiceRecord) -> Result<RecordGuard> {
    ensure_dir()?;
    let path = record_path()?;
    crate::hub_cli::write_private(&path, &serde_json::to_string(record)?)?;
    Ok(RecordGuard {
        path,
        instance: record.instance.clone(),
    })
}

pub(crate) struct RecordGuard {
    path: PathBuf,
    instance: String,
}

impl Drop for RecordGuard {
    fn drop(&mut self) {
        // A newer service may have replaced the record; leave its record alone.
        if load()
            .ok()
            .flatten()
            .is_some_and(|record| record.instance == self.instance)
        {
            let _ = std::fs::remove_file(&self.path);
        }
    }
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

    #[test]
    fn computer_name_is_a_short_safe_label() {
        let name = computer_name();
        assert!(!name.is_empty() && name.len() <= 63);
        assert!(!name.contains('.') && !name.contains('/'));
    }
}
