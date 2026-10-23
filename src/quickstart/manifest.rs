//! `~/.config/agentscm/quickstart.json`: everything quickstart wrote, so `uninstall`
//! can take back exactly that and nothing else.
use std::fs::{OpenOptions, TryLockError};
use std::path::{Path, PathBuf};

use agentscm::hub::store::unix_now;
use agentscm::lease::Lease;
use agentscm::{Error, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::hub_cli::{config_dir, write_private};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct LocalHubRecord {
    pub(crate) dir: PathBuf,
    pub(crate) project: String,
    #[serde(default = "default_hub_port")]
    pub(crate) port: u16,
}

fn default_hub_port() -> u16 {
    7332
}

/// Quickstart signed in and created `hub-session.json` (it was not there before).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct SessionRecord {
    pub(crate) hub: String,
}

#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub(crate) struct Manifest {
    pub(crate) created_at: i64,
    pub(crate) updated_at: i64,
    /// Checkout roots quickstart connected with `client setup`; `uninstall` runs
    /// `client remove` for each.
    pub(crate) clients: Vec<PathBuf>,
    pub(crate) session: Option<SessionRecord>,
    pub(crate) local_hub: Option<LocalHubRecord>,
    pub(crate) native_hub: Option<super::hub_native::Record>,
    /// `~/.agentscm/bin` holds the program the agents' hooks call.
    pub(crate) bin_dir: Option<PathBuf>,
    /// Quickstart created `~/.agentscm` itself, not only `bin` inside it.
    pub(crate) bin_parent_created: bool,
}

pub(crate) fn path() -> Result<PathBuf> {
    Ok(config_dir()?.join("agentscm").join("quickstart.json"))
}

/// A process-wide quickstart/uninstall lease. The stable lock file is deliberately outside every
/// recorded install target and survives uninstall, so concurrent processes always lock one inode.
pub(crate) fn acquire_lease() -> Result<Lease> {
    let lock = path()?.with_file_name("quickstart.lock");
    let directory = lock
        .parent()
        .ok_or_else(|| Error::Invalid("quickstart lock has no parent directory".into()))?;
    super::private_dir(directory)?;
    acquire_lease_at(&lock)
}

fn acquire_lease_at(lock: &Path) -> Result<Lease> {
    if let Ok(metadata) = lock.symlink_metadata()
        && (!metadata.is_file() || metadata.file_type().is_symlink())
    {
        return Err(Error::Invalid(format!(
            "{} is not a regular quickstart lock file; refusing to follow it",
            lock.display()
        )));
    }
    let mut options = OpenOptions::new();
    options.read(true).write(true).create(true).truncate(false);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
    }
    let file = options.open(lock).map_err(|error| {
        Error::Invalid(format!(
            "cannot open quickstart lock {}: {error}",
            lock.display()
        ))
    })?;
    if !file.metadata().is_ok_and(|metadata| metadata.is_file()) {
        return Err(Error::Invalid(format!(
            "{} is not a regular quickstart lock file",
            lock.display()
        )));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        file.set_permissions(std::fs::Permissions::from_mode(0o600))
            .map_err(|error| {
                Error::Invalid(format!(
                    "cannot make quickstart lock {} private: {error}",
                    lock.display()
                ))
            })?;
    }
    let lease = match Lease::try_exclusive(file) {
        Ok(lease) => lease,
        Err(TryLockError::WouldBlock) => {
            return Err(Error::Invalid(
                "another agentscm quickstart or uninstall is running; wait for it to finish, then retry"
                    .into(),
            ));
        }
        Err(TryLockError::Error(error)) => {
            return Err(Error::Invalid(format!(
                "cannot lock {}: {error}",
                lock.display()
            )));
        }
    };
    Ok(lease)
}

pub(crate) fn sha_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

pub(crate) fn file_hash(path: &Path) -> Option<String> {
    std::fs::read(path).ok().map(|bytes| sha_hex(&bytes))
}

impl Manifest {
    /// `None` when quickstart has not been run (no file).
    pub(crate) fn load() -> Result<Option<Self>> {
        match std::fs::read_to_string(path()?) {
            Ok(text) => serde_json::from_str(&text).map(Some).map_err(|_| {
                Error::Invalid("quickstart.json is not valid; remove it by hand".into())
            }),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error.into()),
        }
    }

    pub(crate) fn fresh() -> Self {
        Self {
            created_at: unix_now(),
            ..Self::default()
        }
    }

    pub(crate) fn save(&mut self) -> Result<()> {
        let path = path()?;
        let unchanged = serde_json::to_string_pretty(self)
            .map_err(|_| Error::Invalid("cannot record quickstart.json".into()))?
            + "\n";
        #[cfg(unix)]
        let private = {
            use std::os::unix::fs::PermissionsExt;
            path.metadata()
                .is_ok_and(|meta| meta.permissions().mode() & 0o777 == 0o600)
        };
        #[cfg(not(unix))]
        let private = true;
        if private && std::fs::read_to_string(&path).ok().as_deref() == Some(unchanged.as_str()) {
            return Ok(());
        }
        self.updated_at = unix_now();
        let text = serde_json::to_string_pretty(self)
            .map_err(|_| Error::Invalid("cannot record quickstart.json".into()))?;
        write_private(&path, &format!("{text}\n"))
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.clients.is_empty()
            && self.session.is_none()
            && self.local_hub.is_none()
            && self.native_hub.is_none()
            && self.bin_dir.is_none()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quickstart_lease_is_exclusive_and_reusable() {
        let directory = tempfile::tempdir().unwrap();
        let lock = directory.path().join("quickstart.lock");
        let lease = acquire_lease_at(&lock).unwrap();
        let busy = acquire_lease_at(&lock).unwrap_err().to_string();
        assert!(busy.contains("another agentscm quickstart or uninstall is running"));
        drop(lease);
        acquire_lease_at(&lock).expect("the same stable lock file is reusable after release");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(lock.metadata().unwrap().permissions().mode() & 0o777, 0o600);
        }
    }

    #[test]
    fn quickstart_lease_rejects_nonregular_paths() {
        let directory = tempfile::tempdir().unwrap();
        let lock = directory.path().join("quickstart.lock");
        std::fs::create_dir(&lock).unwrap();
        assert!(acquire_lease_at(&lock).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn quickstart_lease_rejects_symlinks() {
        use std::os::unix::fs::symlink;

        let directory = tempfile::tempdir().unwrap();
        let target = directory.path().join("target");
        std::fs::write(&target, "").unwrap();
        let lock = directory.path().join("quickstart.lock");
        symlink(&target, &lock).unwrap();
        assert!(acquire_lease_at(&lock).is_err());
    }
}
