use std::collections::BTreeMap;

use super::git::Git;
use crate::git_path_profile::{PortableSpellings, uses_reserved_namespace};
use crate::store::{Engine, MAX_CHECKPOINT_BYTES, MAX_FILE_BYTES, validate_path};
use crate::{Error, Result};

const MAX_ENTRIES: usize = 100_000;
const MAX_DEPTH: usize = 128;
const TREE_OUTPUT: u64 = 128 * 1024 * 1024;

#[derive(Default)]
struct Directory {
    files: BTreeMap<String, String>,
    directories: BTreeMap<String, Directory>,
}

impl Directory {
    fn insert(&mut self, path: &str, oid: String) -> Result<()> {
        if let Some((first, rest)) = path.split_once('/') {
            if self.files.contains_key(first) {
                return Err(Error::Invalid("file/directory conflict".into()));
            }
            self.directories
                .entry(first.into())
                .or_default()
                .insert(rest, oid)
        } else {
            if self.directories.contains_key(path) || self.files.insert(path.into(), oid).is_some()
            {
                return Err(Error::Invalid("duplicate or conflicting path".into()));
            }
            Ok(())
        }
    }

    fn write(&self, git: &Git) -> Result<String> {
        let mut input = Vec::new();
        for (name, oid) in &self.files {
            input.extend_from_slice(format!("100644 blob {oid}\t{name}\0").as_bytes());
        }
        for (name, directory) in &self.directories {
            let oid = directory.write(git)?;
            input.extend_from_slice(format!("040000 tree {oid}\t{name}\0").as_bytes());
        }
        let bytes = git.run(&["mktree", "-z"], Some(input), 8192)?;
        let oid =
            String::from_utf8(bytes).map_err(|_| Error::Invalid("invalid tree OID".into()))?;
        let oid = oid.trim().to_owned();
        super::git::validate_oid(&oid)?;
        Ok(oid)
    }
}

/// No index, checkout, attributes, filters or source commands are involved.
/// The complete listing is compared, then every blob is read back byte for byte.
pub(super) fn build_and_verify(git: &Git, engine: &Engine, revision: &str) -> Result<String> {
    engine.load_revision(revision)?;
    let mut directory = Directory::default();
    let mut manifest = BTreeMap::new();
    let mut spellings = PortableSpellings::default();
    let mut cursor = None;
    let mut total = 0usize;
    loop {
        let page = engine.list_files(revision, cursor.as_deref(), 4096)?;
        for entry in page.entries {
            crate::store::require_regular_entry("Git export", &entry.path, entry.kind)?;
            validate_path(&entry.path)?;
            if entry.path.bytes().filter(|b| *b == b'/').count() > MAX_DEPTH {
                return Err(Error::Limit("snapshot depth exceeds 128".into()));
            }
            if uses_reserved_namespace(&entry.path) {
                return Err(Error::Invalid(
                    "snapshot path uses a reserved namespace".into(),
                ));
            }
            if spellings.insert(&entry.path).is_some() {
                return Err(Error::Invalid("snapshot has case aliases".into()));
            }
            if spellings.len() >= MAX_ENTRIES {
                return Err(Error::Limit(
                    "snapshot files plus directories must be below 100000".into(),
                ));
            }
            let bytes = engine.read_file(revision, &entry.path)?;
            total = total
                .checked_add(bytes.len())
                .ok_or_else(|| Error::Limit("snapshot size overflow".into()))?;
            if bytes.len() > MAX_FILE_BYTES
                || total > MAX_CHECKPOINT_BYTES
                || manifest.len() >= MAX_ENTRIES
            {
                return Err(Error::Limit(
                    "snapshot export exceeds capture profile bounds".into(),
                ));
            }
            let oid = git.write_object("blob", &bytes)?;
            directory.insert(&entry.path, oid.clone())?;
            if manifest.insert(entry.path, oid).is_some() {
                return Err(Error::Invalid("duplicate native path".into()));
            }
        }
        cursor = page.next;
        if cursor.is_none() {
            break;
        }
    }
    let tree = directory.write(git)?;
    let listing = git.run(
        &["ls-tree", "-r", "-z", "--full-tree", &tree],
        None,
        TREE_OUTPUT,
    )?;
    let mut expected = Vec::new();
    for (path, oid) in &manifest {
        expected.extend_from_slice(format!("100644 blob {oid}\t{path}\0").as_bytes());
    }
    if listing != expected {
        return Err(Error::Conflict(
            "complete Git tree differs from native manifest".into(),
        ));
    }
    for (path, oid) in &manifest {
        let bytes = git.run(
            &["cat-file", "blob", oid],
            None,
            MAX_FILE_BYTES as u64 + 4096,
        )?;
        if bytes != engine.read_file(revision, path)? {
            return Err(Error::Conflict(format!(
                "exported Git bytes differ at {path}"
            )));
        }
    }
    Ok(tree)
}
