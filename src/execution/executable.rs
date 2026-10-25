//! Trusted operator executable identities. This is local integrity checking, not
//! hostile-host attestation; the operator account remains trusted.
use crate::{Error, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::fs::{File, Metadata};
use std::io::Read;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ExecutableIdentity {
    pub path: PathBuf,
    pub sha256: String,
    pub device: u64,
    pub inode: u64,
    pub size: u64,
    pub mode: u32,
    pub owner: u32,
    pub modified_seconds: i64,
    pub modified_nanos: i64,
}
fn facts(path: PathBuf, metadata: &Metadata, sha256: String) -> ExecutableIdentity {
    ExecutableIdentity {
        path,
        sha256,
        device: metadata.dev(),
        inode: metadata.ino(),
        size: metadata.len(),
        mode: metadata.mode(),
        owner: metadata.uid(),
        modified_seconds: metadata.mtime(),
        modified_nanos: metadata.mtime_nsec(),
    }
}

pub fn capture(path: &Path, excluded_roots: &[&Path]) -> Result<ExecutableIdentity> {
    if !path.is_absolute() {
        return Err(Error::Invalid(
            "operator executable must be absolute".into(),
        ));
    }
    let canonical = std::fs::canonicalize(path)?;
    for root in excluded_roots {
        if canonical.starts_with(std::fs::canonicalize(root)?) {
            return Err(Error::Invalid(
                "operator executable overlaps source or authority data".into(),
            ));
        }
    }
    // geteuid reads process metadata and takes no pointers.
    let owner = unsafe { libc::geteuid() };
    for ancestor in canonical.ancestors().skip(1) {
        let metadata = std::fs::symlink_metadata(ancestor)?;
        if std::fs::symlink_metadata(ancestor.join(".git")).is_ok()
            || !metadata.is_dir()
            || metadata.uid() != 0 && metadata.uid() != owner
            || (metadata.permissions().mode() & 0o022 != 0
                && metadata.permissions().mode() & 0o1000 == 0)
        {
            return Err(Error::Invalid(
                "operator executable must be outside Git in owner-controlled directories".into(),
            ));
        }
    }
    let metadata = std::fs::symlink_metadata(&canonical)?;
    if !metadata.is_file()
        || metadata.permissions().mode() & 0o111 == 0
        || metadata.permissions().mode() & 0o022 != 0
        || metadata.uid() != 0 && metadata.uid() != owner
        || metadata.len() > 512 * 1024 * 1024
    {
        return Err(Error::Invalid(
            "operator executable must be an owner-controlled regular executable".into(),
        ));
    }
    let mut file = File::open(&canonical)?;
    if facts(canonical.clone(), &file.metadata()?, String::new())
        != facts(canonical.clone(), &metadata, String::new())
    {
        return Err(Error::Conflict(
            "operator executable changed before reading".into(),
        ));
    }
    let mut hash = Sha256::new();
    let mut buffer = [0; 65536];
    let mut total = 0u64;
    loop {
        let count = file.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        total += count as u64;
        if total > 512 * 1024 * 1024 {
            return Err(Error::Limit("operator executable exceeds 512 MiB".into()));
        }
        hash.update(&buffer[..count]);
    }
    let identity = facts(
        canonical.clone(),
        &metadata,
        format!("{:x}", hash.finalize()),
    );
    if facts(canonical.clone(), &file.metadata()?, String::new())
        != facts(canonical.clone(), &metadata, String::new())
        || facts(
            canonical.clone(),
            &std::fs::symlink_metadata(&canonical)?,
            String::new(),
        ) != facts(canonical, &metadata, String::new())
    {
        return Err(Error::Conflict(
            "operator executable changed while reading".into(),
        ));
    }
    Ok(identity)
}
pub fn verify(expected: &ExecutableIdentity, excluded_roots: &[&Path]) -> Result<()> {
    if capture(&expected.path, excluded_roots)? != *expected {
        return Err(Error::Conflict(
            "operator executable identity changed".into(),
        ));
    }
    Ok(())
}
