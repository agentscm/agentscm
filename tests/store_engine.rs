use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::sync::{Arc, Barrier};
use std::thread;

use agentscm::Error;
use agentscm::model::FileChange;
use agentscm::store::{Engine, digest, validate_id};
use tempfile::TempDir;

fn put(path: &str, content: &[u8]) -> FileChange {
    FileChange {
        path: path.to_owned(),
        content: Some(content.to_vec()),
    }
}

fn delete(path: &str) -> FileChange {
    FileChange {
        path: path.to_owned(),
        content: None,
    }
}

fn object_path(directory: &TempDir, id: &str) -> std::path::PathBuf {
    directory
        .path()
        .join("objects")
        .join(&id[..2])
        .join(&id[2..])
}

fn object_count(directory: &TempDir) -> usize {
    fs::read_dir(directory.path().join("objects"))
        .unwrap()
        .map(|entry| {
            fs::read_dir(entry.unwrap().path())
                .unwrap()
                .filter(|entry| {
                    !entry
                        .as_ref()
                        .unwrap()
                        .file_name()
                        .to_string_lossy()
                        .starts_with(".tmp-")
                })
                .count()
        })
        .sum()
}

#[test]
fn checkpoints_are_deterministic_and_survive_restart() {
    let directory = TempDir::new().unwrap();
    let nested_store = directory.path().join("new/parent/repository");
    let engine = Engine::open(&nested_store).unwrap();
    let forward = vec![put("src/lib.rs", b"library"), put("README.md", b"read me")];
    let reverse = vec![put("README.md", b"read me"), put("src/lib.rs", b"library")];

    let first = engine.checkpoint(None, &forward).unwrap();
    let second = engine.checkpoint(None, &reverse).unwrap();
    assert_eq!(first, second);
    validate_id(&first).unwrap();
    assert_eq!(digest(b"stable"), digest(b"stable"));

    drop(engine);
    let reopened = Engine::open(&nested_store).unwrap();
    assert_eq!(
        reopened.read_file(&first, "src/lib.rs").unwrap(),
        b"library"
    );
    let revision = reopened.load_revision(&first).unwrap();
    assert_eq!(revision.version, 1);
    assert!(revision.parents.is_empty());
}

#[test]
fn small_edit_publishes_only_bounded_path_copy() {
    let directory = TempDir::new().unwrap();
    let engine = Engine::open(directory.path()).unwrap();
    let files: Vec<_> = (0..256)
        .map(|index| put(&format!("src/file-{index:03}.txt"), b"same"))
        .collect();
    let base = engine.checkpoint(None, &files).unwrap();
    let before = object_count(&directory);

    let edited = engine
        .checkpoint(Some(&base), &[put("src/file-127.txt", b"changed")])
        .unwrap();
    let added = object_count(&directory) - before;

    assert!(
        added <= 24,
        "small edit unexpectedly published {added} objects"
    );
    assert_eq!(
        engine.read_file(&edited, "src/file-127.txt").unwrap(),
        b"changed"
    );
    assert_eq!(
        engine.read_file(&edited, "src/file-126.txt").unwrap(),
        b"same"
    );
}

#[test]
fn deletion_and_selective_diff_report_blob_id_changes() {
    let directory = TempDir::new().unwrap();
    let engine = Engine::open(directory.path()).unwrap();
    let base = engine
        .checkpoint(None, &[put("a.txt", b"a"), put("b.txt", b"b")])
        .unwrap();
    let next = engine
        .checkpoint(
            Some(&base),
            &[put("a.txt", b"new a"), delete("b.txt"), put("c.txt", b"c")],
        )
        .unwrap();

    let diff = engine.diff(&base, &next, 10).unwrap();
    assert!(!diff.truncated);
    assert_eq!(
        diff.entries
            .iter()
            .map(|entry| entry.path.as_str())
            .collect::<Vec<_>>(),
        vec!["a.txt", "b.txt", "c.txt"]
    );
    assert!(diff.entries[0].before.is_some() && diff.entries[0].after.is_some());
    assert!(diff.entries[1].before.is_some() && diff.entries[1].after.is_none());
    assert!(diff.entries[2].before.is_none() && diff.entries[2].after.is_some());
    assert!(matches!(
        engine.read_file(&next, "b.txt"),
        Err(Error::NotFound(_))
    ));
}

#[test]
fn diff_limit_is_honest_and_zero_is_bounded() {
    let directory = TempDir::new().unwrap();
    let engine = Engine::open(directory.path()).unwrap();
    let left = engine
        .checkpoint(None, &[put("a", b"1"), put("b", b"1"), put("c", b"1")])
        .unwrap();
    let right = engine
        .checkpoint(
            Some(&left),
            &[put("a", b"2"), put("b", b"2"), put("c", b"2")],
        )
        .unwrap();

    let one = engine.diff(&left, &right, 1).unwrap();
    assert_eq!(one.entries.len(), 1);
    assert!(one.truncated);
    let zero = engine.diff(&left, &right, 0).unwrap();
    assert!(zero.entries.is_empty());
    assert!(zero.truncated);
    let identical = engine.diff(&left, &left, 0).unwrap();
    assert!(!identical.truncated);
}

#[test]
fn rejects_malformed_ids_paths_duplicates_and_case_collisions() {
    let directory = TempDir::new().unwrap();
    let engine = Engine::open(directory.path()).unwrap();
    for id in ["", "abc", &"A".repeat(64), &"g".repeat(64)] {
        assert!(validate_id(id).is_err());
    }
    for path in [
        "",
        "/absolute",
        "../escape",
        "a/./b",
        "a//b",
        "a\\b",
        "nul",
        "bad?.txt",
        "café",
    ] {
        assert!(
            engine.checkpoint(None, &[put(path, b"x")]).is_err(),
            "accepted {path:?}"
        );
    }
    assert!(
        engine
            .checkpoint(None, &[put("same", b"one"), put("same", b"two")])
            .is_err()
    );
    assert!(
        engine
            .checkpoint(None, &[put("Readme", b"one"), put("README", b"two")])
            .is_err()
    );
    let retained = engine.checkpoint(None, &[put("Readme", b"one")]).unwrap();
    assert!(
        engine
            .checkpoint(Some(&retained), &[put("README", b"two")])
            .is_err()
    );
    assert!(matches!(
        engine.load_revision(&"0".repeat(64)),
        Err(Error::NotFound(_))
    ));
}

#[test]
fn case_only_renames_apply_deletions_first_in_both_input_orders() {
    let directory = TempDir::new().unwrap();
    let engine = Engine::open(directory.path()).unwrap();
    for (from, to) in [("Readme", "README"), ("README", "Readme")] {
        let base = engine.checkpoint(None, &[put(from, b"old")]).unwrap();
        let delete_first = vec![delete(from), put(to, b"renamed")];
        let insert_first = vec![put(to, b"renamed"), delete(from)];
        let first = engine.checkpoint(Some(&base), &delete_first).unwrap();
        let second = engine.checkpoint(Some(&base), &insert_first).unwrap();

        assert_eq!(first, second);
        assert_eq!(engine.read_file(&first, to).unwrap(), b"renamed");
        assert!(matches!(
            engine.read_file(&first, from),
            Err(Error::NotFound(_))
        ));
        let paths: BTreeSet<_> = engine
            .diff(&base, &first, 10)
            .unwrap()
            .entries
            .into_iter()
            .map(|entry| entry.path)
            .collect();
        assert_eq!(paths, BTreeSet::from([from.to_owned(), to.to_owned()]));
    }
}

#[test]
fn rejects_file_directory_conflicts_in_genesis_and_incremental_revisions() {
    let directory = TempDir::new().unwrap();
    let engine = Engine::open(directory.path()).unwrap();

    assert!(
        engine
            .checkpoint(None, &[put("a", b"file"), put("a/b", b"child")])
            .is_err()
    );
    assert!(
        engine
            .checkpoint(None, &[put("a/b", b"child"), put("a", b"file")])
            .is_err()
    );

    let file = engine.checkpoint(None, &[put("a", b"file")]).unwrap();
    assert!(
        engine
            .checkpoint(Some(&file), &[put("a/b", b"child")])
            .is_err()
    );
    let descendant = engine.checkpoint(None, &[put("a/b", b"child")]).unwrap();
    assert!(
        engine
            .checkpoint(Some(&descendant), &[put("a", b"file")])
            .is_err()
    );
}

#[test]
fn atomically_replaces_ancestors_and_descendants() {
    let directory = TempDir::new().unwrap();
    let engine = Engine::open(directory.path()).unwrap();

    let ancestor = engine.checkpoint(None, &[put("a", b"file")]).unwrap();
    let descendant = engine
        .checkpoint(Some(&ancestor), &[put("a/b", b"child"), delete("a")])
        .unwrap();
    assert!(matches!(
        engine.read_file(&descendant, "a"),
        Err(Error::NotFound(_))
    ));
    assert_eq!(engine.read_file(&descendant, "a/b").unwrap(), b"child");

    let ancestor_again = engine
        .checkpoint(
            Some(&descendant),
            &[put("a", b"replacement"), delete("a/b")],
        )
        .unwrap();
    assert_eq!(
        engine.read_file(&ancestor_again, "a").unwrap(),
        b"replacement"
    );
    assert!(matches!(
        engine.read_file(&ancestor_again, "a/b"),
        Err(Error::NotFound(_))
    ));
}

#[test]
fn prefix_counts_survive_sibling_deletion_and_release_at_zero() {
    let directory = TempDir::new().unwrap();
    let engine = Engine::open(directory.path()).unwrap();
    let base = engine
        .checkpoint(None, &[put("dir/one", b"1"), put("dir/two", b"2")])
        .unwrap();
    let one_left = engine
        .checkpoint(Some(&base), &[delete("dir/one")])
        .unwrap();
    assert!(
        engine
            .checkpoint(Some(&one_left), &[put("dir", b"file")])
            .is_err()
    );

    let replaced = engine
        .checkpoint(Some(&one_left), &[put("dir", b"file"), delete("dir/two")])
        .unwrap();
    assert_eq!(engine.read_file(&replaced, "dir").unwrap(), b"file");
}

#[test]
fn directory_spelling_aliases_fail_but_whole_directory_case_rename_succeeds() {
    let directory = TempDir::new().unwrap();
    let engine = Engine::open(directory.path()).unwrap();
    assert!(
        engine
            .checkpoint(None, &[put("A/x", b"x"), put("a/y", b"y")])
            .is_err()
    );

    let base = engine
        .checkpoint(None, &[put("A/x", b"x"), put("A/y", b"y")])
        .unwrap();
    assert!(engine.checkpoint(Some(&base), &[put("a/z", b"z")]).is_err());

    let renamed = engine
        .checkpoint(
            Some(&base),
            &[
                put("a/y", b"y"),
                delete("A/x"),
                put("a/x", b"x"),
                delete("A/y"),
            ],
        )
        .unwrap();
    assert_eq!(engine.read_file(&renamed, "a/x").unwrap(), b"x");
    assert_eq!(engine.read_file(&renamed, "a/y").unwrap(), b"y");
    assert!(matches!(
        engine.read_file(&renamed, "A/x"),
        Err(Error::NotFound(_))
    ));
}

#[test]
fn enforces_portable_component_length() {
    let directory = TempDir::new().unwrap();
    let engine = Engine::open(directory.path()).unwrap();
    let accepted = "a".repeat(255);
    let rejected = "b".repeat(256);

    let revision = engine.checkpoint(None, &[put(&accepted, b"ok")]).unwrap();
    assert_eq!(engine.read_file(&revision, &accepted).unwrap(), b"ok");
    assert!(engine.checkpoint(None, &[put(&rejected, b"no")]).is_err());
}

#[test]
fn missing_and_corrupt_objects_are_never_read_as_empty() {
    let directory = TempDir::new().unwrap();
    let engine = Engine::open(directory.path()).unwrap();
    let revision_id = engine.checkpoint(None, &[put("file", b"content")]).unwrap();
    let tree_id = engine.load_revision(&revision_id).unwrap().tree;
    fs::write(object_path(&directory, &tree_id), b"corrupt").unwrap();

    assert!(matches!(
        engine.read_file(&revision_id, "file"),
        Err(Error::Invalid(_))
    ));
    fs::remove_file(object_path(&directory, &tree_id)).unwrap();
    assert!(matches!(
        engine.read_file(&revision_id, "file"),
        Err(Error::NotFound(_))
    ));
}

#[test]
fn concurrent_publishers_get_one_deterministic_revision() {
    let directory = TempDir::new().unwrap();
    let engine = Arc::new(Engine::open(directory.path()).unwrap());
    let barrier = Arc::new(Barrier::new(8));
    let threads: Vec<_> = (0..8)
        .map(|_| {
            let engine = Arc::clone(&engine);
            let barrier = Arc::clone(&barrier);
            thread::spawn(move || {
                barrier.wait();
                engine
                    .checkpoint(None, &[put("shared.txt", b"same bytes")])
                    .unwrap()
            })
        })
        .collect();
    let ids: Vec<_> = threads
        .into_iter()
        .map(|thread| thread.join().unwrap())
        .collect();

    assert!(ids.iter().all(|id| id == &ids[0]));
    assert_eq!(
        engine.read_file(&ids[0], "shared.txt").unwrap(),
        b"same bytes"
    );
    for shard in fs::read_dir(directory.path().join("objects")).unwrap() {
        for object in fs::read_dir(shard.unwrap().path()).unwrap() {
            assert!(
                !object
                    .unwrap()
                    .file_name()
                    .to_string_lossy()
                    .starts_with(".tmp-")
            );
        }
    }
}

#[test]
fn concurrent_openers_initialize_a_fresh_nested_store() {
    let directory = TempDir::new().unwrap();
    let store = Arc::new(directory.path().join("fresh/nested/repository"));
    let barrier = Arc::new(Barrier::new(8));
    let threads: Vec<_> = (0..8)
        .map(|_| {
            let store = Arc::clone(&store);
            let barrier = Arc::clone(&barrier);
            thread::spawn(move || {
                barrier.wait();
                let engine = Engine::open(store.as_path()).unwrap();
                engine
                    .checkpoint(None, &[put("shared.txt", b"same bytes")])
                    .unwrap()
            })
        })
        .collect();
    let ids: Vec<_> = threads
        .into_iter()
        .map(|thread| thread.join().unwrap())
        .collect();

    assert!(ids.iter().all(|id| id == &ids[0]));
    let reopened = Engine::open(store.as_path()).unwrap();
    assert_eq!(
        reopened.read_file(&ids[0], "shared.txt").unwrap(),
        b"same bytes"
    );
}

#[test]
fn validates_parent_and_rejects_deleting_a_missing_path() {
    let directory = TempDir::new().unwrap();
    let engine = Engine::open(directory.path()).unwrap();
    assert!(matches!(
        engine.checkpoint(Some(&"0".repeat(64)), &[]),
        Err(Error::NotFound(_))
    ));
    assert!(matches!(
        engine.checkpoint(None, &[delete("missing")]),
        Err(Error::NotFound(_))
    ));
}

#[test]
fn mixed_edits_match_a_state_oracle_through_path_compression() {
    let directory = TempDir::new().unwrap();
    let engine = Engine::open(directory.path()).unwrap();
    let mut same_prefix = Vec::new();
    for index in 0..10_000 {
        let path = format!("shared/path-{index:04}.txt");
        if digest(path.as_bytes()).starts_with('a') {
            same_prefix.push(path);
            if same_prefix.len() == 80 {
                break;
            }
        }
    }
    assert_eq!(same_prefix.len(), 80);

    let mut state = BTreeMap::<String, Vec<u8>>::new();
    let initial: Vec<_> = same_prefix[..48]
        .iter()
        .enumerate()
        .map(|(index, path)| {
            let content = format!("initial-{index}").into_bytes();
            state.insert(path.clone(), content.clone());
            put(path, &content)
        })
        .collect();
    let mut revision = engine.checkpoint(None, &initial).unwrap();

    for round in 0..8 {
        let before = state.clone();
        let mut changes = Vec::new();
        for offset in 0..4 {
            let path = &same_prefix[(round * 5 + offset) % 40];
            let content = format!("replacement-{round}-{offset}").into_bytes();
            state.insert(path.clone(), content.clone());
            changes.push(put(path, &content));
        }
        let removed = same_prefix[40 + round].clone();
        if state.remove(&removed).is_some() {
            changes.push(delete(&removed));
        }
        let added = same_prefix[48 + round].clone();
        let content = format!("added-{round}").into_bytes();
        state.insert(added.clone(), content.clone());
        changes.push(put(&added, &content));

        let previous = revision;
        revision = engine.checkpoint(Some(&previous), &changes).unwrap();
        for (path, expected) in &state {
            assert_eq!(engine.read_file(&revision, path).unwrap(), *expected);
        }
        let expected_paths: BTreeSet<_> = before
            .keys()
            .chain(state.keys())
            .filter(|path| before.get(*path) != state.get(*path))
            .cloned()
            .collect();
        let actual_paths: BTreeSet<_> = engine
            .diff(&previous, &revision, 32)
            .unwrap()
            .entries
            .into_iter()
            .map(|entry| entry.path)
            .collect();
        assert_eq!(actual_paths, expected_paths);
    }
}

#[test]
fn rejects_excessive_aggregate_prefix_work_before_publication() {
    let directory = TempDir::new().unwrap();
    let engine = Engine::open(directory.path()).unwrap();
    let deep_prefix = "a/".repeat(497);
    let changes: Vec<_> = (0..33)
        .map(|index| put(&format!("{deep_prefix}f{index:02}"), b"x"))
        .collect();

    assert!(matches!(
        engine.checkpoint(None, &changes),
        Err(Error::Limit(_))
    ));
    assert_eq!(object_count(&directory), 0);
}

fn many_files(count: usize) -> Vec<FileChange> {
    (0..count)
        .map(|index| put(&format!("d{}/f{index:05}", index % 50), b"x"))
        .collect()
}

#[test]
fn single_batch_matches_checkpoint() {
    let directory = TempDir::new().unwrap();
    let engine = Engine::open(directory.path()).unwrap();
    let changes = vec![put("a/b", b"1"), put("c", b"2")];
    let direct = engine.checkpoint(None, &changes).unwrap();
    let batched = engine
        .checkpoint_batches(None, [changes.as_slice()])
        .unwrap();
    assert_eq!(direct, batched);
    assert_eq!(
        engine.checkpoint_batches(None, std::iter::empty()).unwrap(),
        engine.checkpoint(None, &[]).unwrap()
    );
}

#[test]
fn batches_beyond_one_checkpoint_publish_one_revision() {
    let directory = TempDir::new().unwrap();
    let engine = Engine::open(directory.path()).unwrap();
    let all = many_files(60);
    let batches: Vec<&[FileChange]> = all.chunks(25).collect();
    assert_eq!(batches.len(), 3);

    let id = engine.checkpoint_batches(None, batches.clone()).unwrap();
    assert!(engine.load_revision(&id).unwrap().parents.is_empty());
    assert_eq!(engine.checkpoint_batches(None, batches).unwrap(), id);

    let mut listed = BTreeSet::new();
    let mut cursor: Option<String> = None;
    loop {
        let page = engine.list_files(&id, cursor.as_deref(), 1_000).unwrap();
        listed.extend(page.entries.into_iter().map(|entry| entry.path));
        cursor = page.next;
        if cursor.is_none() {
            break;
        }
    }
    let expected: BTreeSet<_> = all.iter().map(|change| change.path.clone()).collect();
    assert_eq!(listed, expected);
    assert_eq!(engine.read_file(&id, "d0/f00000").unwrap(), b"x");
    assert_eq!(engine.read_file(&id, "d9/f00059").unwrap(), b"x");
}

#[test]
fn batches_reject_duplicate_paths_and_oversized_batches() {
    let directory = TempDir::new().unwrap();
    let engine = Engine::open(directory.path()).unwrap();
    let first = vec![put("a", b"1")];
    let second = vec![put("a", b"2")];
    assert!(matches!(
        engine.checkpoint_batches(None, [first.as_slice(), second.as_slice()]),
        Err(Error::Invalid(_))
    ));

    let big = many_files(4_097);
    assert!(matches!(
        engine.checkpoint_batches(None, [first.as_slice(), big.as_slice()]),
        Err(Error::Limit(_))
    ));
    assert_eq!(object_count(&directory), 0);
}

#[test]
fn batches_reject_case_aliases_across_batches() {
    let directory = TempDir::new().unwrap();
    let engine = Engine::open(directory.path()).unwrap();
    let first = vec![put("A.txt", b"1")];
    let second = vec![put("a.txt", b"2")];
    let before = object_count(&directory);
    let result = engine.checkpoint_batches(None, [first.as_slice(), second.as_slice()]);
    assert!(matches!(result, Err(Error::Conflict(_))), "{result:?}");
    // No revision is returned; earlier-batch objects stay as unreferenced orphans.
    let orphaned = object_count(&directory);
    assert!(orphaned > before);

    let corrected = vec![put("b.txt", b"2")];
    let revision = engine
        .checkpoint_batches(None, [first.as_slice(), corrected.as_slice()])
        .unwrap();
    assert!(!revision.is_empty());
}
