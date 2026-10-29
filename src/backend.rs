//! Bounded immutable storage for calls already authorized by a trusted
//! controller. The local adapter trusts its operator-owned filesystem and does
//! not authenticate remote clients. Deletion exists only for shared-conversation
//! bodies, and only after the Hub database has tombstoned them.

use std::fs::{self, File, OpenOptions};
use std::io::{ErrorKind, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use crate::error::{Error, Result};
use crate::store::{MAX_OBJECT_BYTES, create_directory_durable, digest, validate_id};
pub use crate::sync::{SOURCE_PROFILE_V1, SOURCE_PROFILE_V2};

pub const MAX_RANGE_BYTES: u64 = 1024 * 1024;
const MAX_IDENTIFIER_BYTES: usize = 128;

static TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct ObjectKey {
    pub trust_domain: String,
    pub repository: String,
    pub profile: String,
    pub id: String,
}

impl ObjectKey {
    pub fn new(
        trust_domain: impl Into<String>,
        repository: impl Into<String>,
        profile: impl Into<String>,
        id: impl Into<String>,
    ) -> Result<Self> {
        let key = Self {
            trust_domain: trust_domain.into(),
            repository: repository.into(),
            profile: profile.into(),
            id: id.into(),
        };
        key.validate()?;
        Ok(key)
    }

    pub fn trust_domain(&self) -> &str {
        &self.trust_domain
    }

    pub fn repository(&self) -> &str {
        &self.repository
    }

    pub fn profile(&self) -> &str {
        &self.profile
    }

    pub fn id(&self) -> &str {
        &self.id
    }

    pub fn validate(&self) -> Result<()> {
        validate_identifier("trust domain", &self.trust_domain)?;
        validate_identifier("repository", &self.repository)?;
        validate_identifier("source profile", &self.profile)?;
        if !matches!(self.profile.as_str(), SOURCE_PROFILE_V1 | SOURCE_PROFILE_V2) {
            return Err(Error::Invalid(format!(
                "unsupported source profile: {}",
                self.profile
            )));
        }
        validate_id(&self.id)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PutOutcome {
    Created,
    AlreadyPresent,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ObjectMetadata {
    pub size: u64,
    pub digest: String,
}

pub trait ImmutableBackend: Send + Sync {
    /// Durably prepare this namespace/incarnation once before any writes. All
    /// controller write paths must call this, including resumed publications.
    fn prepare_publication(&self, key: &ObjectKey, incarnation: &str) -> Result<()>;

    fn put_if_absent(&self, key: &ObjectKey, incarnation: &str, bytes: &[u8])
    -> Result<PutOutcome>;

    fn get_verified(&self, key: &ObjectKey, incarnation: &str) -> Result<Vec<u8>>;

    fn inspect_verified(&self, key: &ObjectKey, incarnation: &str) -> Result<ObjectMetadata>;

    /// Opens one raw encoded-object stream for a caller that has already
    /// authorized the exact namespace, object and physical incarnation.
    /// Implementations without this capability must reject it explicitly.
    fn open_encoded(
        &self,
        _key: &ObjectKey,
        _incarnation: &str,
    ) -> Result<Box<dyn Read + Send + '_>> {
        Err(Error::Invalid(
            "backend does not support raw encoded-object streams".into(),
        ))
    }

    /// Removes one physical incarnation. Absent is success. Callers must have
    /// recorded a database tombstone first; implementations without this
    /// capability must reject it explicitly.
    fn delete_incarnation(&self, _key: &ObjectKey, _incarnation: &str) -> Result<()> {
        Err(Error::Invalid(
            "backend does not support deleting an object incarnation".into(),
        ))
    }

    /// Verifies the complete object before returning a slice. The local adapter
    /// therefore reads at most [`MAX_OBJECT_BYTES`] even for a small range.
    fn get_range_verified(
        &self,
        key: &ObjectKey,
        incarnation: &str,
        offset: u64,
        length: u64,
    ) -> Result<Vec<u8>>;
}

#[derive(Clone, Debug)]
pub struct LocalBackend {
    root: PathBuf,
}

impl LocalBackend {
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        Ok(Self {
            root: create_directory_durable(path)?,
        })
    }

    fn object_path(&self, key: &ObjectKey, incarnation: &str) -> Result<PathBuf> {
        key.validate()?;
        validate_identifier("physical incarnation", incarnation)?;
        // Database identifiers are case-sensitive even on case-folding local
        // filesystems. Hex encoding is injective and case-stable; split into
        // two bounded components so 128-byte identifiers fit filesystem limits.
        let mut path = self.root.clone();
        append_component(&mut path, key.trust_domain());
        append_component(&mut path, key.repository());
        path.push(key.profile()); // Exact allowlisted lowercase profile.
        append_component(&mut path, incarnation);
        path.push(key.id()); // Canonical lowercase hexadecimal digest.
        Ok(path)
    }

    fn read_verified(&self, key: &ObjectKey, incarnation: &str) -> Result<Vec<u8>> {
        let path = self.object_path(key, incarnation)?;
        let mut file = File::open(&path).map_err(|error| {
            if error.kind() == ErrorKind::NotFound {
                Error::NotFound(format!("object incarnation: {}/{}", key.id(), incarnation))
            } else {
                Error::Io(error)
            }
        })?;
        let metadata = file.metadata()?;
        if !metadata.is_file() {
            return Err(Error::Invalid(format!(
                "object incarnation is not a file: {}/{}",
                key.id(),
                incarnation
            )));
        }
        if metadata.len() > MAX_OBJECT_BYTES {
            return Err(Error::Limit(format!(
                "object {} exceeds size limit",
                key.id()
            )));
        }

        let mut bytes = Vec::with_capacity(metadata.len() as usize);
        Read::by_ref(&mut file)
            .take(MAX_OBJECT_BYTES + 1)
            .read_to_end(&mut bytes)?;
        if bytes.len() as u64 > MAX_OBJECT_BYTES {
            return Err(Error::Limit(format!(
                "object {} exceeds size limit",
                key.id()
            )));
        }
        if digest(&bytes) != key.id() {
            return Err(Error::Invalid(format!(
                "object digest mismatch: {}",
                key.id()
            )));
        }
        Ok(bytes)
    }

    fn verify_existing(path: &Path, key: &ObjectKey, expected: &[u8]) -> Result<()> {
        let mut file = File::open(path)?;
        let metadata = file.metadata()?;
        if !metadata.is_file()
            || metadata.len() != expected.len() as u64
            || metadata.len() > MAX_OBJECT_BYTES
        {
            return Err(Error::Conflict(format!(
                "immutable object differs at backend key: {}",
                key.id()
            )));
        }

        let mut offset = 0usize;
        let mut buffer = [0u8; 64 * 1024];
        loop {
            let read = file.read(&mut buffer)?;
            if read == 0 {
                break;
            }
            let end = offset
                .checked_add(read)
                .ok_or_else(|| Error::Limit("existing object comparison offset overflow".into()))?;
            if expected.get(offset..end) != Some(&buffer[..read]) {
                return Err(Error::Conflict(format!(
                    "immutable object differs at backend key: {}",
                    key.id()
                )));
            }
            offset = end;
        }
        if offset != expected.len() || digest(expected) != key.id() {
            return Err(Error::Conflict(format!(
                "immutable object differs at backend key: {}",
                key.id()
            )));
        }
        Ok(())
    }
}

impl ImmutableBackend for LocalBackend {
    fn prepare_publication(&self, key: &ObjectKey, incarnation: &str) -> Result<()> {
        let path = self.object_path(key, incarnation)?;
        let directory = path
            .parent()
            .ok_or_else(|| Error::Invalid("backend namespace has no directory".into()))?;
        // Prepare once per publication, including every visible ancestor, so a
        // concurrent/crashed creator cannot lend us an unsynced parent entry.
        create_directory_durable(directory)?;
        Ok(())
    }

    fn put_if_absent(
        &self,
        key: &ObjectKey,
        incarnation: &str,
        bytes: &[u8],
    ) -> Result<PutOutcome> {
        if bytes.len() as u64 > MAX_OBJECT_BYTES {
            return Err(Error::Limit("object exceeds size limit".into()));
        }
        key.validate()?;
        validate_identifier("physical incarnation", incarnation)?;
        if digest(bytes) != key.id() {
            return Err(Error::Invalid(format!(
                "object bytes do not match object id: {}",
                key.id()
            )));
        }

        let path = self.object_path(key, incarnation)?;
        let directory = path
            .parent()
            .ok_or_else(|| Error::Invalid("backend object path has no parent".into()))?;
        if !fs::metadata(directory)?.is_dir() {
            return Err(Error::Invalid(
                "publication namespace is not prepared".into(),
            ));
        }
        if path.exists() {
            Self::verify_existing(&path, key, bytes)?;
            sync_directory(directory)?;
            return Ok(PutOutcome::AlreadyPresent);
        }

        let (temporary, mut file) = loop {
            let sequence = TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
            let temporary = directory.join(format!(
                ".tmp-{}-{sequence}-{}",
                std::process::id(),
                &key.id()[..12]
            ));
            match OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&temporary)
            {
                Ok(file) => break (temporary, file),
                Err(error) if error.kind() == ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(Error::Io(error)),
            }
        };

        let result = (|| -> Result<PutOutcome> {
            file.write_all(bytes)?;
            file.sync_all()?;
            match fs::hard_link(&temporary, &path) {
                Ok(()) => {
                    sync_directory(directory)?;
                    Ok(PutOutcome::Created)
                }
                Err(error) if error.kind() == ErrorKind::AlreadyExists => {
                    Self::verify_existing(&path, key, bytes)?;
                    sync_directory(directory)?;
                    Ok(PutOutcome::AlreadyPresent)
                }
                Err(error) => Err(Error::Io(error)),
            }
        })();
        drop(file);
        let _ = fs::remove_file(&temporary);
        result
    }

    fn get_verified(&self, key: &ObjectKey, incarnation: &str) -> Result<Vec<u8>> {
        self.read_verified(key, incarnation)
    }

    fn inspect_verified(&self, key: &ObjectKey, incarnation: &str) -> Result<ObjectMetadata> {
        let bytes = self.read_verified(key, incarnation)?;
        Ok(ObjectMetadata {
            size: bytes.len() as u64,
            digest: key.id().to_owned(),
        })
    }

    fn open_encoded(
        &self,
        key: &ObjectKey,
        incarnation: &str,
    ) -> Result<Box<dyn Read + Send + '_>> {
        let path = self.object_path(key, incarnation)?;
        let file = File::open(path).map_err(|error| {
            if error.kind() == ErrorKind::NotFound {
                Error::MissingObject(key.id().to_owned())
            } else {
                Error::Io(error)
            }
        })?;
        let metadata = file.metadata()?;
        if !metadata.is_file() {
            return Err(Error::Invalid(format!(
                "object incarnation is not a file: {}/{}",
                key.id(),
                incarnation
            )));
        }
        if metadata.len() > MAX_OBJECT_BYTES {
            return Err(Error::Limit(format!(
                "object {} exceeds size limit",
                key.id()
            )));
        }
        Ok(Box::new(file))
    }

    fn delete_incarnation(&self, key: &ObjectKey, incarnation: &str) -> Result<()> {
        let path = self.object_path(key, incarnation)?;
        match fs::remove_file(&path) {
            Ok(()) => {}
            Err(error) if error.kind() == ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(Error::Io(error)),
        }
        match path.parent() {
            Some(directory) => sync_directory(directory),
            None => Ok(()),
        }
    }

    fn get_range_verified(
        &self,
        key: &ObjectKey,
        incarnation: &str,
        offset: u64,
        length: u64,
    ) -> Result<Vec<u8>> {
        let end = range_end(offset, length)?;
        slice_range(self.read_verified(key, incarnation)?, offset, end)
    }
}

fn append_component(path: &mut PathBuf, value: &str) {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let encoded: String = value
        .bytes()
        .flat_map(|byte| {
            [
                HEX[(byte >> 4) as usize] as char,
                HEX[(byte & 15) as usize] as char,
            ]
        })
        .collect();
    let boundary = encoded.len().min(128);
    path.push(&encoded[..boundary]);
    // '_' cannot be produced by hexadecimal encoding, so short and long
    // components cannot alias even on a case-folding filesystem.
    path.push(if boundary == encoded.len() {
        "_"
    } else {
        &encoded[boundary..]
    });
}

pub(crate) fn validate_identifier(label: &str, value: &str) -> Result<()> {
    if value.is_empty() || value.len() > MAX_IDENTIFIER_BYTES {
        return Err(Error::Invalid(format!(
            "{label} length must be between 1 and {MAX_IDENTIFIER_BYTES} bytes"
        )));
    }
    if !value
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
    {
        return Err(Error::Invalid(format!(
            "{label} must contain only ASCII letters, digits, '-' or '_'"
        )));
    }
    Ok(())
}

/// The bounds that need no object, so they are checked before any read.
pub(crate) fn range_end(offset: u64, length: u64) -> Result<u64> {
    if length > MAX_RANGE_BYTES {
        return Err(Error::Limit(format!(
            "range exceeds {MAX_RANGE_BYTES} bytes"
        )));
    }
    offset
        .checked_add(length)
        .ok_or_else(|| Error::Limit("range end overflow".into()))
}

pub(crate) fn slice_range(mut bytes: Vec<u8>, offset: u64, end: u64) -> Result<Vec<u8>> {
    if end > bytes.len() as u64 {
        return Err(Error::Invalid("range exceeds object bounds".into()));
    }
    let start = usize::try_from(offset)
        .map_err(|_| Error::Limit("range offset cannot fit in memory".into()))?;
    let end =
        usize::try_from(end).map_err(|_| Error::Limit("range end cannot fit in memory".into()))?;
    bytes.truncate(end);
    bytes.drain(..start);
    Ok(bytes)
}

// Windows cannot open a directory as a file; NTFS journals renames, so there is nothing to sync.
fn sync_directory(path: &Path) -> Result<()> {
    #[cfg(unix)]
    File::open(path)?.sync_all()?;
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
}
