use std::fs;
use std::path::PathBuf;

use agentscm::Error;
use agentscm::model::{EntryChange, EntryKind, FileChange};
use agentscm::store::{Engine, digest, validate_symlink_target};
use sha2::{Digest, Sha256};
use tempfile::TempDir;

fn put(path: &str, content: &[u8]) -> FileChange {
    FileChange {
        path: path.to_owned(),
        content: Some(content.to_vec()),
    }
}

fn entry(path: &str, content: &[u8], kind: EntryKind) -> EntryChange {
    EntryChange {
        path: path.to_owned(),
        content: Some(content.to_vec()),
        kind,
    }
}

fn delete_entry(path: &str) -> EntryChange {
    EntryChange {
        path: path.to_owned(),
        content: None,
        kind: EntryKind::Regular,
    }
}

fn id_bytes(id: &str) -> Vec<u8> {
    id.as_bytes()
        .chunks_exact(2)
        .map(|pair| u8::from_str_radix(std::str::from_utf8(pair).unwrap(), 16).unwrap())
        .collect()
}

fn object_path(directory: &TempDir, id: &str) -> PathBuf {
    directory
        .path()
        .join("objects")
        .join(&id[..2])
        .join(&id[2..])
}

fn tree_of(engine: &Engine, revision: &str) -> String {
    engine.load_revision(revision).unwrap().tree
}

/// Every stored object whose bytes start with `magic`, as (id, bytes).
fn objects_with_magic(directory: &TempDir, magic: &[u8]) -> Vec<(String, Vec<u8>)> {
    let mut found = Vec::new();
    for shard in fs::read_dir(directory.path().join("objects")).unwrap() {
        let shard = shard.unwrap().path();
        if !shard.is_dir() {
            continue;
        }
        for object in fs::read_dir(&shard).unwrap() {
            let object = object.unwrap().path();
            let bytes = fs::read(&object).unwrap();
            if bytes.starts_with(magic) {
                let id = format!(
                    "{}{}",
                    shard.file_name().unwrap().to_str().unwrap(),
                    object.file_name().unwrap().to_str().unwrap()
                );
                found.push((id, bytes));
            }
        }
    }
    found.sort();
    found
}

fn blob_id(content: &[u8]) -> String {
    let mut bytes = b"AGSCM-B1\0".to_vec();
    bytes.extend_from_slice(&(content.len() as u64).to_be_bytes());
    bytes.extend_from_slice(content);
    digest(&bytes)
}

/// Independent construction of the documented L2 layout: L1 plus one kind byte.
fn expected_l2(namespace: u8, path: &str, value: &str, kind: u8) -> Vec<u8> {
    let mut bytes = b"AGSCM-L2\0".to_vec();
    bytes.push(namespace);
    bytes.extend_from_slice(&Sha256::digest(path.as_bytes()));
    bytes.extend_from_slice(&(path.len() as u16).to_be_bytes());
    bytes.extend_from_slice(path.as_bytes());
    bytes.extend_from_slice(&id_bytes(value));
    bytes.push(kind);
    bytes
}

#[test]
fn l2_leaf_bytes_and_ids_are_frozen() {
    let directory = TempDir::new().unwrap();
    let engine = Engine::open(directory.path()).unwrap();
    let revision = engine
        .checkpoint_entries(
            None,
            &[
                entry("run.sh", b"#!/bin/sh\n", EntryKind::Executable),
                entry("link", b"run.sh", EntryKind::Symlink),
            ],
        )
        .unwrap();
    let leaves = objects_with_magic(&directory, b"AGSCM-L2\0");
    assert_eq!(leaves.len(), 2);
    let executable = expected_l2(1, "run.sh", &blob_id(b"#!/bin/sh\n"), 1);
    let symlink = expected_l2(1, "link", &blob_id(b"run.sh"), 2);
    let by_path = |bytes: &Vec<u8>| {
        leaves
            .iter()
            .find(|(_, found)| found == bytes)
            .unwrap()
            .0
            .clone()
    };
    assert_eq!(
        by_path(&executable),
        "76e5193bdd6dbbba0891afce4fc610d1bec8d3ef25d873c4e945b5c1699c7ef1"
    );
    assert_eq!(
        by_path(&symlink),
        "51f1e99cb2cf98a04fda3b12ce1d2ba70333229dd69326f91936321e501eefda"
    );
    assert_eq!(
        revision,
        "2b4e4ada6327c7031381d16f7712303a26d0a32199f34bf46eec7d017c0ac995"
    );
    // The portable index stays L1 and points at the exact leaf.
    assert_eq!(objects_with_magic(&directory, b"AGSCM-L1\0").len(), 2);
    assert_eq!(
        engine.read_entry(&revision, "run.sh").unwrap(),
        (EntryKind::Executable, b"#!/bin/sh\n".to_vec())
    );
    assert_eq!(
        engine.read_entry(&revision, "link").unwrap(),
        (EntryKind::Symlink, b"run.sh".to_vec())
    );
    let page = engine.list_files(&revision, None, 10).unwrap();
    let kinds: Vec<_> = page
        .entries
        .iter()
        .map(|e| (e.path.as_str(), e.kind))
        .collect();
    assert!(kinds.contains(&("run.sh", EntryKind::Executable)));
    assert!(kinds.contains(&("link", EntryKind::Symlink)));
}

#[test]
fn all_regular_trees_keep_their_identities() {
    let directory = TempDir::new().unwrap();
    let engine = Engine::open(directory.path()).unwrap();
    let plain = engine
        .checkpoint(None, &[put("README.md", b"hello")])
        .unwrap();
    assert_eq!(
        plain,
        "11140b42c390d970cec5c589f0620f8500d97c0e0290fd9f68ee96aa46d2af2f"
    );
    let explicit = engine
        .checkpoint_entries(None, &[entry("README.md", b"hello", EntryKind::Regular)])
        .unwrap();
    assert_eq!(explicit, plain);
    assert!(objects_with_magic(&directory, b"AGSCM-L2\0").is_empty());
}

#[test]
fn strict_decode_rejects_malformed_l2_leaves() {
    let directory = TempDir::new().unwrap();
    let engine = Engine::open(directory.path()).unwrap();
    let blob = {
        let revision = engine.checkpoint(None, &[put("x", b"v")]).unwrap();
        engine.list_files(&revision, None, 1).unwrap().entries[0]
            .blob
            .clone()
    };
    let install = |bytes: &[u8]| {
        let id = digest(bytes);
        let path = object_path(&directory, &id);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, bytes).unwrap();
        id
    };
    let valid = expected_l2(1, "x", &blob, 1);
    let valid_id = install(&valid);
    engine.read_encoded(&valid_id).unwrap();

    let mut kind_zero = valid.clone();
    *kind_zero.last_mut().unwrap() = 0;
    let mut kind_three = valid.clone();
    *kind_three.last_mut().unwrap() = 3;
    let mut trailing = valid.clone();
    trailing.push(0);
    let mut missing = valid.clone();
    missing.pop();
    let portable = expected_l2(2, "x", &blob, 1);
    let prefix = {
        let mut bytes = expected_l2(3, "x", &format!("{:064x}", 1), 1);
        bytes.shrink_to_fit();
        bytes
    };
    for (label, bytes) in [
        ("kind 0", kind_zero),
        ("kind 3", kind_three),
        ("trailing byte", trailing),
        ("missing kind", missing),
        ("portable namespace", portable),
        ("prefix namespace", prefix),
    ] {
        let id = install(&bytes);
        assert!(
            matches!(engine.read_encoded(&id), Err(Error::Invalid(_))),
            "{label} was accepted"
        );
    }
}

#[test]
fn mode_changes_are_new_trees_and_file_changes_keep_the_kind() {
    let directory = TempDir::new().unwrap();
    let engine = Engine::open(directory.path()).unwrap();
    let regular = engine
        .checkpoint(None, &[put("bin/tool", b"same"), put("other", b"o")])
        .unwrap();
    let executable = engine
        .checkpoint_entries(
            Some(&regular),
            &[entry("bin/tool", b"same", EntryKind::Executable)],
        )
        .unwrap();
    assert_ne!(tree_of(&engine, &regular), tree_of(&engine, &executable));
    assert_eq!(
        engine.read_entry(&executable, "bin/tool").unwrap().0,
        EntryKind::Executable
    );
    assert_eq!(
        engine.read_entry(&executable, "other").unwrap().0,
        EntryKind::Regular
    );

    // Setting the same kind again is a no-op for the tree.
    let again = engine
        .checkpoint_entries(
            Some(&executable),
            &[entry("bin/tool", b"same", EntryKind::Executable)],
        )
        .unwrap();
    assert_eq!(tree_of(&engine, &again), tree_of(&engine, &executable));

    // Executable back to Regular restores the original tree.
    let back = engine
        .checkpoint_entries(
            Some(&executable),
            &[entry("bin/tool", b"same", EntryKind::Regular)],
        )
        .unwrap();
    assert_eq!(tree_of(&engine, &back), tree_of(&engine, &regular));
    assert!(matches!(
        engine.read_entry(&back, "bin/tool"),
        Ok((EntryKind::Regular, _))
    ));

    // A plain FileChange keeps Executable.
    let edited = engine
        .checkpoint(Some(&executable), &[put("bin/tool", b"edited")])
        .unwrap();
    assert_eq!(
        engine.read_entry(&edited, "bin/tool").unwrap(),
        (EntryKind::Executable, b"edited".to_vec())
    );
    // The same holds for R2 checkpoints.
    let sibling = engine
        .checkpoint(Some(&regular), &[put("side", b"s")])
        .unwrap();
    let merged = engine
        .checkpoint_entries_with_parents(
            &[executable.clone(), sibling],
            &[entry("link", b"other", EntryKind::Symlink)],
        )
        .unwrap();
    assert_eq!(
        engine.read_entry(&merged, "bin/tool").unwrap().0,
        EntryKind::Executable
    );
    assert_eq!(
        engine.read_entry(&merged, "link").unwrap().0,
        EntryKind::Symlink
    );
}

#[test]
fn symlinks_reject_content_writes_but_can_be_deleted_and_replaced() {
    let directory = TempDir::new().unwrap();
    let engine = Engine::open(directory.path()).unwrap();
    let base = engine
        .checkpoint_entries(
            None,
            &[
                entry("link", b"target.txt", EntryKind::Symlink),
                entry("dir/file", b"f", EntryKind::Regular),
            ],
        )
        .unwrap();
    assert!(matches!(
        engine.checkpoint(Some(&base), &[put("link", b"bytes")]),
        Err(Error::Conflict(_))
    ));
    let deleted = engine
        .checkpoint(
            Some(&base),
            &[FileChange {
                path: "link".into(),
                content: None,
            }],
        )
        .unwrap();
    assert!(matches!(
        engine.read_file(&deleted, "link"),
        Err(Error::NotFound(_))
    ));
    let deleted_entry = engine
        .checkpoint_entries(Some(&base), &[delete_entry("link")])
        .unwrap();
    assert_eq!(deleted, deleted_entry);
    // Explicit conversions in both directions are allowed.
    let converted = engine
        .checkpoint_entries(
            Some(&base),
            &[entry("link", b"now a file", EntryKind::Regular)],
        )
        .unwrap();
    assert_eq!(
        engine.read_entry(&converted, "link").unwrap(),
        (EntryKind::Regular, b"now a file".to_vec())
    );
    let retargeted = engine
        .checkpoint_entries(
            Some(&base),
            &[entry("link", b"dir/file", EntryKind::Symlink)],
        )
        .unwrap();
    assert_eq!(engine.read_file(&retargeted, "link").unwrap(), b"dir/file");
}

#[test]
fn hostile_symlink_targets_are_rejected_at_checkpoint() {
    let directory = TempDir::new().unwrap();
    let engine = Engine::open(directory.path()).unwrap();
    let long = "a/".repeat(600);
    let hostile: Vec<(&str, &str, Vec<u8>)> = vec![
        ("absolute", "l", b"/etc/passwd".to_vec()),
        ("empty", "l", Vec::new()),
        ("top-level parent", "top", b"..".to_vec()),
        ("beyond depth", "a/l", b"../..".to_vec()),
        ("parent after normal", "x/l", b"x/../..".to_vec()),
        ("parent after normal inside", "a/b/m", b"l/../..".to_vec()),
        ("dot component", "l", b"a/./b".to_vec()),
        ("empty component", "l", b"a//b".to_vec()),
        ("trailing slash", "l", b"a/".to_vec()),
        ("nul", "l", b"a\0b".to_vec()),
        ("control", "l", b"a\nb".to_vec()),
        ("backslash", "l", b"a\\b".to_vec()),
        ("dot git", "l", b".git/config".to_vec()),
        ("upper git", "l", b".GIT".to_vec()),
        ("nested git", "l", b"a/.Git/hooks".to_vec()),
        ("workspace metadata", "l", b".agentscm-workspace/x".to_vec()),
        ("overlength", "l", long.into_bytes()),
        ("not utf8", "l", vec![0xff, 0xfe]),
        ("reserved device", "l", b"a/NUL".to_vec()),
        ("non portable", "l", b"a/b:c".to_vec()),
        ("only parents", "a/b/l", b"../..".to_vec()),
    ];
    for (label, path, target) in hostile {
        assert!(
            matches!(
                engine.checkpoint_entries(None, &[entry(path, &target, EntryKind::Symlink)]),
                Err(Error::Invalid(_))
            ),
            "{label} was accepted"
        );
        assert!(validate_symlink_target(path, &target).is_err(), "{label}");
    }

    for (path, target) in [
        ("a/b/l", "../../top"),
        ("a/b/l", "../c"),
        ("l", "target.txt"),
        ("a/l", "../b/c.txt"),
        ("l", "dangling/never/created"),
    ] {
        let revision = engine
            .checkpoint_entries(None, &[entry(path, target.as_bytes(), EntryKind::Symlink)])
            .unwrap();
        assert_eq!(
            engine.read_entry(&revision, path).unwrap(),
            (EntryKind::Symlink, target.as_bytes().to_vec())
        );
    }
}

#[test]
fn plain_file_changes_cannot_smuggle_symlink_targets_and_moves_revalidate() {
    let directory = TempDir::new().unwrap();
    let engine = Engine::open(directory.path()).unwrap();
    let base = engine
        .checkpoint_entries(None, &[entry("a/b/l", b"../../top", EntryKind::Symlink)])
        .unwrap();
    // Moving the link shallower keeps the bytes but changes its depth.
    assert!(matches!(
        engine.checkpoint_entries(
            Some(&base),
            &[
                delete_entry("a/b/l"),
                entry("a/l", b"../../top", EntryKind::Symlink),
            ],
        ),
        Err(Error::Invalid(_))
    ));
    let moved = engine
        .checkpoint_entries(
            Some(&base),
            &[
                delete_entry("a/b/l"),
                entry("a/b/c/l", b"../../top", EntryKind::Symlink),
            ],
        )
        .unwrap();
    assert_eq!(
        engine.read_entry(&moved, "a/b/c/l").unwrap().0,
        EntryKind::Symlink
    );
}

fn revision_with(engine: &Engine, changes: &[EntryChange]) -> String {
    engine.checkpoint_entries(None, changes).unwrap()
}

fn paged_entries(engine: &Engine, left: &str, right: &str) -> Vec<agentscm::model::DiffEntry> {
    let mut entries = Vec::new();
    let mut cursor: Option<String> = None;
    loop {
        let page = engine
            .diff_page(left, right, cursor.as_deref(), 100)
            .unwrap();
        assert!(page.needed_objects.is_empty());
        entries.extend(page.entries);
        cursor = page.next;
        if cursor.is_none() {
            break;
        }
    }
    entries.sort_by(|a, b| a.path.cmp(&b.path));
    entries
}

#[test]
fn kind_only_and_retarget_changes_appear_in_full_and_paged_diffs() {
    let directory = TempDir::new().unwrap();
    let engine = Engine::open(directory.path()).unwrap();
    let left = revision_with(
        &engine,
        &[
            entry("flip", b"same", EntryKind::Regular),
            entry("unflip", b"same", EntryKind::Executable),
            entry("link", b"old", EntryKind::Symlink),
            entry("turn", b"same", EntryKind::Regular),
            entry("steady", b"same", EntryKind::Regular),
        ],
    );
    let right = revision_with(
        &engine,
        &[
            entry("flip", b"same", EntryKind::Executable),
            entry("unflip", b"same", EntryKind::Regular),
            entry("link", b"new", EntryKind::Symlink),
            entry("turn", b"same", EntryKind::Symlink),
            entry("steady", b"same", EntryKind::Regular),
        ],
    );
    let full = engine.diff(&left, &right, 100).unwrap();
    assert!(!full.truncated);
    let mut full = full.entries;
    full.sort_by(|a, b| a.path.cmp(&b.path));
    let paged = paged_entries(&engine, &left, &right);
    assert_eq!(full, paged);
    let summary: Vec<_> = full
        .iter()
        .map(|e| {
            (
                e.path.as_str(),
                e.before == e.after,
                e.before_kind,
                e.after_kind,
            )
        })
        .collect();
    assert_eq!(
        summary,
        vec![
            ("flip", true, None, Some(EntryKind::Executable)),
            (
                "link",
                false,
                Some(EntryKind::Symlink),
                Some(EntryKind::Symlink)
            ),
            ("turn", true, None, Some(EntryKind::Symlink)),
            ("unflip", true, Some(EntryKind::Executable), None),
        ]
    );
    // Regular-only entries serialize exactly as before the kind fields existed.
    let plain_left = revision_with(&engine, &[entry("a", b"1", EntryKind::Regular)]);
    let plain_right = revision_with(&engine, &[entry("a", b"2", EntryKind::Regular)]);
    let plain = engine.diff(&plain_left, &plain_right, 10).unwrap();
    let json = serde_json::to_value(&plain.entries[0]).unwrap();
    assert_eq!(
        json.as_object().unwrap().keys().collect::<Vec<_>>(),
        vec!["after", "before", "path"]
    );
}
