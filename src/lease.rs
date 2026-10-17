//! An advisory file lock held for a value's lifetime.
use std::fs::{File, TryLockError};

/// Unlocks explicitly on drop: closing the file does not release the flock while a child
/// forked by another thread still shares the open file description before its exec.
#[derive(Debug)]
pub struct Lease(File);

impl Lease {
    pub fn try_exclusive(file: File) -> Result<Self, TryLockError> {
        file.try_lock()?;
        Ok(Self(file))
    }

    pub fn try_shared(file: File) -> Result<Self, TryLockError> {
        file.try_lock_shared()?;
        Ok(Self(file))
    }
}

impl Drop for Lease {
    fn drop(&mut self) {
        let _ = self.0.unlock();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn drop_releases_the_lock_while_a_duplicate_descriptor_survives() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("lock");
        let open = || {
            std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .create(true)
                .truncate(false)
                .open(&path)
                .unwrap()
        };
        let lease = Lease::try_exclusive(open()).unwrap();
        // A forked child's copy shares the open file description, as a duplicate does.
        let inherited = lease.0.try_clone().unwrap();
        assert!(matches!(
            Lease::try_shared(open()),
            Err(TryLockError::WouldBlock)
        ));
        drop(lease);
        Lease::try_exclusive(open()).expect("unlocked although a duplicate is still open");
        drop(inherited);
    }
}
