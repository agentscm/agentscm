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
    capture_inner(path, excluded_roots, false)
}

/// Capture with a diagnostic naming a refused directory. Use only for local
/// operator errors, because the diagnostic contains an absolute path.
pub fn capture_for_local_cli(path: &Path, excluded_roots: &[&Path]) -> Result<ExecutableIdentity> {
    capture_inner(path, excluded_roots, true)
}

fn capture_inner(
    path: &Path,
    excluded_roots: &[&Path],
    report_refused_directory: bool,
) -> Result<ExecutableIdentity> {
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
    if let Some((directory, reason)) = refused_ancestor(&canonical, owner)? {
        let message = if report_refused_directory {
            format!(
                "cannot pin operator executable: directory {directory:?} {reason}; choose an executable in owner-controlled directories outside Git"
            )
        } else {
            "operator executable must be outside Git in owner-controlled directories".into()
        };
        return Err(Error::Invalid(message));
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

/// The first ancestor directory `capture` refuses, with the reason.
fn refused_ancestor(canonical: &Path, owner: u32) -> Result<Option<(&Path, &'static str)>> {
    for ancestor in canonical.ancestors().skip(1) {
        let metadata = std::fs::symlink_metadata(ancestor)?;
        let mode = metadata.permissions().mode();
        let reason = if std::fs::symlink_metadata(ancestor.join(".git")).is_ok() {
            "contains a Git repository entry (.git)"
        } else if !metadata.is_dir() {
            "is not a directory"
        } else if metadata.uid() != 0 && metadata.uid() != owner {
            "is owned by neither root nor the current user"
        } else if mode & 0o022 != 0 && mode & 0o1000 == 0 {
            match mode & 0o022 {
                0o020 => "is group-writable without the sticky bit",
                0o002 => "is world-writable without the sticky bit",
                _ => "is group- and world-writable without the sticky bit",
            }
        } else {
            continue;
        };
        return Ok(Some((ancestor, reason)));
    }
    Ok(None)
}

pub fn verify(expected: &ExecutableIdentity, excluded_roots: &[&Path]) -> Result<()> {
    if capture(&expected.path, excluded_roots)? != *expected {
        return Err(Error::Conflict(
            "operator executable identity changed".into(),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const GENERIC: &str = "operator executable must be outside Git in owner-controlled directories";

    /// A synthetic executable in `holder`, whose mode is set last.
    fn refused(holder_mode: u32, git_marker: bool) -> (tempfile::TempDir, PathBuf, PathBuf) {
        let root = tempfile::tempdir().unwrap();
        let holder = root.path().join("holder");
        std::fs::create_dir(&holder).unwrap();
        if git_marker {
            std::fs::create_dir(holder.join(".git")).unwrap();
        }
        let tool = holder.join("tool");
        std::fs::write(&tool, "#!/bin/sh\nexit 0\n").unwrap();
        std::fs::set_permissions(&tool, std::fs::Permissions::from_mode(0o700)).unwrap();
        std::fs::set_permissions(&holder, std::fs::Permissions::from_mode(holder_mode)).unwrap();
        let canonical = std::fs::canonicalize(&holder).unwrap();
        (root, tool, canonical)
    }

    #[test]
    fn refused_ancestor_is_named_locally_while_capture_stays_generic() {
        for (mode, marker, reason) in [
            (0o775, false, "is group-writable without the sticky bit"),
            (0o757, false, "is world-writable without the sticky bit"),
            (
                0o777,
                false,
                "is group- and world-writable without the sticky bit",
            ),
            (0o755, true, "contains a Git repository entry (.git)"),
        ] {
            let (_root, tool, holder) = refused(mode, marker);
            match capture(&tool, &[]) {
                Err(Error::Invalid(message)) => assert_eq!(message, GENERIC),
                other => panic!("expected generic refusal, got {other:?}"),
            }
            let Err(Error::Invalid(detail)) = capture_for_local_cli(&tool, &[]) else {
                panic!("expected a local refusal for {tool:?}");
            };
            assert!(
                detail.contains(&format!("directory {holder:?} {reason};")),
                "{detail}"
            );
        }
    }

    #[test]
    fn sticky_or_owner_only_directories_have_no_refusal_to_name() {
        for mode in [0o1777, 0o755] {
            let (_root, tool, _holder) = refused(mode, false);
            let identity = capture(&tool, &[]).unwrap();
            assert_eq!(capture_for_local_cli(&tool, &[]).unwrap(), identity);
        }
    }
}
