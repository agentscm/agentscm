use std::collections::BTreeMap;

use ring::digest::{Context, SHA1_FOR_LEGACY_USE_ONLY};

use super::git::Git;
use crate::git_path_profile::{PortableSpellings, uses_reserved_namespace};
use crate::model::EntryKind;
use crate::store::{Engine, MAX_CHECKPOINT_BYTES, MAX_FILE_BYTES, validate_path};
use crate::{Error, Result};

const MAX_ENTRIES: usize = 100_000;
const MAX_DEPTH: usize = 128;
const TREE_OUTPUT: u64 = 128 * 1024 * 1024;

#[derive(Default)]
struct Directory {
    files: BTreeMap<String, (&'static str, String)>,
    directories: BTreeMap<String, Directory>,
}

impl Directory {
    fn insert(&mut self, path: &str, mode: &'static str, oid: String) -> Result<()> {
        if let Some((first, rest)) = path.split_once('/') {
            if self.files.contains_key(first) {
                return Err(Error::Invalid("file/directory conflict".into()));
            }
            self.directories
                .entry(first.into())
                .or_default()
                .insert(rest, mode, oid)
        } else {
            if self.directories.contains_key(path)
                || self.files.insert(path.into(), (mode, oid)).is_some()
            {
                return Err(Error::Invalid("duplicate or conflicting path".into()));
            }
            Ok(())
        }
    }

    fn oid(&self) -> Result<String> {
        let mut entries = Vec::new();
        for (name, (mode, oid)) in &self.files {
            entries.push((name.clone(), mode.trim_start_matches('0'), oid.clone()));
        }
        for (name, directory) in &self.directories {
            // Git sorts a directory as if its name ended in '/'.
            entries.push((format!("{name}/"), "40000", directory.oid()?));
        }
        entries.sort();
        let mut body = Vec::new();
        for (key, mode, oid) in entries {
            body.extend_from_slice(mode.as_bytes());
            body.push(b' ');
            body.extend_from_slice(key.trim_end_matches('/').as_bytes());
            body.push(0);
            for pair in oid.as_bytes().chunks(2) {
                let pair = std::str::from_utf8(pair).unwrap_or_default();
                body.push(
                    u8::from_str_radix(pair, 16)
                        .map_err(|_| Error::Invalid("invalid object ID".into()))?,
                );
            }
        }
        Ok(git_object_oid("tree", &body))
    }

    fn write(&self, git: &Git) -> Result<String> {
        let mut input = Vec::new();
        for (name, (mode, oid)) in &self.files {
            input.extend_from_slice(format!("{mode} blob {oid}\t{name}\0").as_bytes());
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

/// Visits every native entry after the profile, path, symlink and size bounds are enforced.
fn walk_entries(
    engine: &Engine,
    revision: &str,
    profile: &str,
    mut visit: impl FnMut(&str, &'static str, &[u8]) -> Result<()>,
) -> Result<()> {
    if !matches!(profile, super::LEGACY_PROFILE | super::PROFILE) {
        return Err(Error::Invalid("unsupported export profile".into()));
    }
    engine.load_revision(revision)?;
    let mut spellings = PortableSpellings::default();
    let mut cursor = None;
    let mut total = 0usize;
    let mut visited = 0usize;
    loop {
        let page = engine.list_files(revision, cursor.as_deref(), 4096)?;
        for entry in page.entries {
            if profile == super::LEGACY_PROFILE {
                crate::store::require_regular_entry("Git export", &entry.path, entry.kind)?;
            }
            let mode = match entry.kind {
                EntryKind::Regular => "100644",
                EntryKind::Executable => "100755",
                EntryKind::Symlink => "120000",
            };
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
            if entry.kind == EntryKind::Symlink {
                crate::store::validate_symlink_target(&entry.path, &bytes)?;
            }
            total = total
                .checked_add(bytes.len())
                .ok_or_else(|| Error::Limit("snapshot size overflow".into()))?;
            if bytes.len() > MAX_FILE_BYTES
                || total > MAX_CHECKPOINT_BYTES
                || visited >= MAX_ENTRIES
            {
                return Err(Error::Limit(
                    "snapshot export exceeds capture profile bounds".into(),
                ));
            }
            visited += 1;
            visit(&entry.path, mode, &bytes)?;
        }
        cursor = page.next;
        if cursor.is_none() {
            return Ok(());
        }
    }
}

fn git_object_oid(kind: &str, body: &[u8]) -> String {
    let mut hash = Context::new(&SHA1_FOR_LEGACY_USE_ONLY);
    hash.update(format!("{kind} {}\0", body.len()).as_bytes());
    hash.update(body);
    hex(hash.finish().as_ref())
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// The SHA-1 Git tree OID `build_and_verify` would produce, computed without Git or
/// writing any object. Same listing validation and bounds; no readback is needed
/// because nothing is stored.
#[cfg(unix)]
pub(crate) fn native_tree_oid(engine: &Engine, revision: &str) -> Result<String> {
    let mut directory = Directory::default();
    walk_entries(engine, revision, super::PROFILE, |path, mode, bytes| {
        directory.insert(path, mode, git_object_oid("blob", bytes))
    })?;
    directory.oid()
}

/// No index, checkout, attributes, filters or source commands are involved.
/// The complete listing is compared, then every blob is read back byte for byte.
pub(super) fn build_and_verify(
    git: &Git,
    engine: &Engine,
    revision: &str,
    profile: &str,
) -> Result<String> {
    let mut directory = Directory::default();
    let mut manifest = BTreeMap::new();
    walk_entries(engine, revision, profile, |path, mode, bytes| {
        let oid = git.write_object("blob", bytes)?;
        directory.insert(path, mode, oid.clone())?;
        if manifest.insert(path.to_owned(), (mode, oid)).is_some() {
            return Err(Error::Invalid("duplicate native path".into()));
        }
        Ok(())
    })?;
    let tree = directory.write(git)?;
    let listing = git.run(
        &["ls-tree", "-r", "-z", "--full-tree", &tree],
        None,
        TREE_OUTPUT,
    )?;
    let mut expected = Vec::new();
    for (path, (mode, oid)) in &manifest {
        expected.extend_from_slice(format!("{mode} blob {oid}\t{path}\0").as_bytes());
    }
    if listing != expected {
        return Err(Error::Conflict(
            "complete Git tree differs from native manifest".into(),
        ));
    }
    for (path, (_, oid)) in &manifest {
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

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use super::*;
    use crate::model::EntryChange;

    fn change(path: &str, bytes: &[u8], kind: EntryKind) -> EntryChange {
        EntryChange {
            path: path.into(),
            content: Some(bytes.to_vec()),
            kind,
        }
    }

    #[cfg(unix)]
    #[test]
    fn native_tree_oid_matches_git_mktree() {
        let git_path = PathBuf::from(
            std::env::var("AGENTSCM_TEST_GIT").unwrap_or_else(|_| "/usr/bin/git".into()),
        );
        assert!(git_path.is_absolute() && git_path.exists());
        let root = tempfile::tempdir().unwrap();
        let repo = root.path().join("mirror.git");
        std::fs::create_dir(&repo).unwrap();
        let git = Git::new(Path::new(&git_path), repo).unwrap();
        git.run(&["init", "--bare", "--object-format=sha1", "."], None, 4096)
            .unwrap();
        let native = root.path().join("native");
        let engine = Engine::open(&native).unwrap();
        // "a.b" and "a-b/z" sort before directory "a" ("a/"); "a0" sorts after it.
        let revision = engine
            .checkpoint_entries(
                None,
                &[
                    change("a.b", b"dot\n", EntryKind::Regular),
                    change("a/x", b"nested\n", EntryKind::Regular),
                    change("a/deep/y", b"deeper\n", EntryKind::Regular),
                    change("a-b/z", b"dash\n", EntryKind::Regular),
                    change("a0", b"zero\n", EntryKind::Regular),
                    change("bin/tool", b"#!/bin/sh\n", EntryKind::Executable),
                    change("link", b"a.b", EntryKind::Symlink),
                    change("empty", b"", EntryKind::Regular),
                ],
            )
            .unwrap();
        let expected = build_and_verify(&git, &engine, &revision, super::super::PROFILE).unwrap();
        assert_eq!(native_tree_oid(&engine, &revision).unwrap(), expected);
        let other = engine
            .checkpoint_entries(
                Some(&revision),
                &[change("bin/tool", b"#!/bin/sh\n", EntryKind::Regular)],
            )
            .unwrap();
        assert_ne!(native_tree_oid(&engine, &other).unwrap(), expected);
    }
}
