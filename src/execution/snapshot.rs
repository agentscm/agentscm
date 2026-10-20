use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

#[cfg(unix)]
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};

use crate::store::{Engine, MAX_FILE_PAGE};
use crate::{Error, Result};

pub(crate) const MAX_SNAPSHOT_FILES: u32 = 10_000;
pub(crate) const MAX_SNAPSHOT_BYTES: u64 = 256 * 1024 * 1024;

#[derive(Debug)]
pub struct Snapshot {
    path: PathBuf,
    revision: String,
}

impl Snapshot {
    pub fn materialize(
        engine: &Engine,
        revision: &str,
        destination: &Path,
        max_files: u32,
        max_bytes: u64,
    ) -> Result<Self> {
        validate_limits(max_files, max_bytes)?;
        // Validate the revision before reserving a destination on disk.
        engine.load_revision(revision)?;
        let parent_owner = validate_private_parent(destination)?;
        create_private_directory(destination)?;

        let result = (|| {
            let canonical = fs::canonicalize(destination)?;
            validate_destination_owner(&canonical, parent_owner)?;
            validate_mount_path(&canonical)?;
            materialize_into(engine, revision, &canonical, max_files, max_bytes)?;
            freeze_tree(&canonical)?;
            Ok(canonical)
        })();
        match result {
            Ok(path) => Ok(Self {
                path,
                revision: revision.to_owned(),
            }),
            Err(error) => {
                remove_private_tree(destination);
                Err(error)
            }
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn revision(&self) -> &str {
        &self.revision
    }
}

#[cfg(unix)]
fn validate_private_parent(destination: &Path) -> Result<u32> {
    let parent = destination
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let canonical = fs::canonicalize(parent)?;
    let metadata = fs::metadata(canonical)?;
    if !metadata.is_dir() || metadata.mode() & 0o077 != 0 {
        return Err(Error::Invalid(
            "snapshot destination parent must be an owner-private directory".into(),
        ));
    }
    Ok(metadata.uid())
}

#[cfg(not(unix))]
fn validate_private_parent(_destination: &Path) -> Result<u32> {
    Ok(0)
}

#[cfg(unix)]
fn validate_destination_owner(destination: &Path, parent_owner: u32) -> Result<()> {
    if fs::metadata(destination)?.uid() != parent_owner {
        return Err(Error::Invalid(
            "snapshot destination and parent must have the same owner".into(),
        ));
    }
    Ok(())
}

#[cfg(not(unix))]
fn validate_destination_owner(_destination: &Path, _parent_owner: u32) -> Result<()> {
    Ok(())
}

impl Drop for Snapshot {
    fn drop(&mut self) {
        remove_private_tree(&self.path);
    }
}

fn materialize_into(
    engine: &Engine,
    revision: &str,
    destination: &Path,
    max_files: u32,
    max_bytes: u64,
) -> Result<()> {
    let mut cursor = None;
    let mut files = 0u32;
    let mut bytes = 0u64;
    loop {
        let remaining = max_files.saturating_sub(files);
        if remaining == 0 {
            let page = engine.list_files(revision, cursor.as_deref(), 1)?;
            if page.entries.is_empty() {
                return Ok(());
            }
            return Err(Error::Limit(format!(
                "snapshot exceeds its {max_files}-file limit"
            )));
        }
        let page_limit = usize::try_from(remaining)
            .unwrap_or(usize::MAX)
            .min(MAX_FILE_PAGE);
        let page = engine.list_files(revision, cursor.as_deref(), page_limit)?;
        if page.entries.is_empty() {
            if page.next.is_some() {
                return Err(Error::Invalid(
                    "source file listing returned an empty continuation page".into(),
                ));
            }
            return Ok(());
        }
        for entry in page.entries {
            // A check must never see Git or AgentSCM metadata planted in native source.
            if crate::git_path_profile::uses_reserved_namespace(&entry.path) {
                return Err(Error::Conflict(format!(
                    "revision {revision} uses reserved path {}",
                    entry.path
                )));
            }
            crate::store::require_regular_entry("execution snapshot", &entry.path, entry.kind)?;
            let content = engine.read_file(revision, &entry.path)?;
            bytes = bytes
                .checked_add(content.len() as u64)
                .ok_or_else(|| Error::Limit("snapshot byte count overflow".into()))?;
            if bytes > max_bytes {
                return Err(Error::Limit(format!(
                    "snapshot exceeds its {max_bytes}-byte limit"
                )));
            }
            files += 1;
            write_source_file(destination, &entry.path, &content)?;
        }
        match page.next {
            Some(next) => cursor = Some(next),
            None => return Ok(()),
        }
    }
}

fn validate_limits(max_files: u32, max_bytes: u64) -> Result<()> {
    if max_files == 0 || max_files > MAX_SNAPSHOT_FILES {
        return Err(Error::Limit(format!(
            "snapshot max_files must be between 1 and {MAX_SNAPSHOT_FILES}"
        )));
    }
    if max_bytes == 0 || max_bytes > MAX_SNAPSHOT_BYTES {
        return Err(Error::Limit(format!(
            "snapshot max_bytes must be between 1 and {MAX_SNAPSHOT_BYTES}"
        )));
    }
    Ok(())
}

fn create_private_directory(path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        fs::DirBuilder::new().mode(0o700).create(path)?;
    }
    #[cfg(not(unix))]
    fs::create_dir(path)?;
    Ok(())
}

fn validate_mount_path(path: &Path) -> Result<()> {
    let path = path
        .to_str()
        .ok_or_else(|| Error::Invalid("snapshot path must be UTF-8".into()))?;
    if path.contains([',', '\n', '\r', '"']) {
        return Err(Error::Invalid(
            "snapshot path contains characters unsupported by the OCI mount encoding".into(),
        ));
    }
    Ok(())
}

fn write_source_file(root: &Path, relative: &str, content: &[u8]) -> Result<()> {
    let mut parent = root.to_path_buf();
    let mut components = relative.split('/').peekable();
    while let Some(component) = components.next() {
        if component.is_empty() || component == "." || component == ".." {
            return Err(Error::Invalid(
                "source snapshot path is not portable".into(),
            ));
        }
        if components.peek().is_none() {
            let path = parent.join(component);
            let mut options = OpenOptions::new();
            options.write(true).create_new(true);
            #[cfg(unix)]
            options.mode(0o600);
            let mut file = options.open(path)?;
            file.write_all(content)?;
            file.sync_all()?;
        } else {
            parent.push(component);
            match fs::symlink_metadata(&parent) {
                Ok(metadata) if metadata.file_type().is_dir() => {}
                Ok(_) => {
                    return Err(Error::Invalid(
                        "snapshot path crosses a non-directory entry".into(),
                    ));
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    create_private_directory(&parent)?;
                }
                Err(error) => return Err(error.into()),
            }
        }
    }
    Ok(())
}

pub(crate) fn remove_private_tree(path: &Path) {
    #[cfg(unix)]
    let _ = make_tree_owner_writable(path);
    let _ = fs::remove_dir_all(path);
}

#[cfg(unix)]
fn freeze_tree(path: &Path) -> Result<()> {
    let metadata = fs::symlink_metadata(path)?;
    if metadata.file_type().is_symlink() {
        return Err(Error::Invalid("snapshot tree contains a symlink".into()));
    }
    if metadata.is_dir() {
        for entry in fs::read_dir(path)? {
            freeze_tree(&entry?.path())?;
        }
        fs::set_permissions(path, fs::Permissions::from_mode(0o555))?;
    } else {
        fs::set_permissions(path, fs::Permissions::from_mode(0o444))?;
    }
    Ok(())
}

#[cfg(not(unix))]
fn freeze_tree(_path: &Path) -> Result<()> {
    Ok(())
}

#[cfg(unix)]
fn make_tree_owner_writable(path: &Path) -> std::io::Result<()> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
    };
    if metadata.file_type().is_symlink() {
        return Ok(());
    }
    if metadata.is_dir() {
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
        for entry in fs::read_dir(path)? {
            make_tree_owner_writable(&entry?.path())?;
        }
    } else {
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::FileChange;

    fn temporary_path(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "agentscm-snapshot-{name}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ))
    }

    #[test]
    fn materializes_and_cleans_up_source() {
        let store = temporary_path("store");
        let parent = temporary_path("private-parent");
        fs::create_dir(&parent).unwrap();
        fs::set_permissions(&parent, fs::Permissions::from_mode(0o700)).unwrap();
        let destination = parent.join("destination");
        let engine = Engine::open(&store).unwrap();
        let revision = engine
            .checkpoint(
                None,
                &[FileChange {
                    path: "src/main.rs".into(),
                    content: Some(b"fn main() {}\n".to_vec()),
                }],
            )
            .unwrap();
        {
            let snapshot =
                Snapshot::materialize(&engine, &revision, &destination, 10, 1024).unwrap();
            assert_eq!(snapshot.revision(), revision);
            assert_eq!(
                fs::read(snapshot.path().join("src/main.rs")).unwrap(),
                b"fn main() {}\n"
            );
        }
        assert!(!destination.exists());
        fs::remove_dir(parent).unwrap();
        fs::remove_dir_all(store).unwrap();
    }

    #[test]
    fn preserves_preexisting_destination() {
        let store = temporary_path("existing-store");
        let parent = temporary_path("existing-private-parent");
        fs::create_dir(&parent).unwrap();
        fs::set_permissions(&parent, fs::Permissions::from_mode(0o700)).unwrap();
        let destination = parent.join("existing-destination");
        let engine = Engine::open(&store).unwrap();
        let revision = engine.checkpoint(None, &[]).unwrap();
        fs::create_dir(&destination).unwrap();
        assert!(Snapshot::materialize(&engine, &revision, &destination, 1, 1).is_err());
        assert!(destination.exists());
        fs::remove_dir_all(destination).unwrap();
        fs::remove_dir(parent).unwrap();
        fs::remove_dir_all(store).unwrap();
    }

    #[test]
    fn requires_private_same_owner_parent_before_materializing() {
        let store = temporary_path("parent-validation-store");
        let engine = Engine::open(&store).unwrap();
        let revision = engine
            .checkpoint(
                None,
                &[FileChange {
                    path: "source.txt".into(),
                    content: Some(b"source".to_vec()),
                }],
            )
            .unwrap();
        let shared = temporary_path("shared-parent");
        fs::create_dir(&shared).unwrap();
        fs::set_permissions(&shared, fs::Permissions::from_mode(0o755)).unwrap();
        let rejected = shared.join("snapshot");
        assert!(Snapshot::materialize(&engine, &revision, &rejected, 1, 16).is_err());
        assert!(!rejected.exists());

        let private = temporary_path("accepted-parent");
        fs::create_dir(&private).unwrap();
        fs::set_permissions(&private, fs::Permissions::from_mode(0o700)).unwrap();
        let accepted = private.join("snapshot");
        let snapshot = Snapshot::materialize(&engine, &revision, &accepted, 1, 16).unwrap();
        assert_eq!(
            fs::read(snapshot.path().join("source.txt")).unwrap(),
            b"source"
        );
        drop(snapshot);

        fs::remove_dir(shared).unwrap();
        fs::remove_dir(private).unwrap();
        fs::remove_dir_all(store).unwrap();
    }
}
