use std::collections::{BTreeMap, BTreeSet};
use std::fs;

use agentscm::Error;
use agentscm::model::FileChange;
use agentscm::store::{Engine, digest};
use tempfile::TempDir;

fn put(path: &str, content: impl Into<Vec<u8>>) -> FileChange {
    FileChange {
        path: path.to_owned(),
        content: Some(content.into()),
    }
}

fn delete(path: &str) -> FileChange {
    FileChange {
        path: path.to_owned(),
        content: None,
    }
}

fn id_bytes(id: &str) -> Vec<u8> {
    id.as_bytes()
        .chunks_exact(2)
        .map(|pair| u8::from_str_radix(std::str::from_utf8(pair).unwrap(), 16).unwrap())
        .collect()
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn object_path(directory: &TempDir, id: &str) -> std::path::PathBuf {
    directory
        .path()
        .join("objects")
        .join(&id[..2])
        .join(&id[2..])
}

fn install_raw(directory: &TempDir, bytes: &[u8]) -> String {
    let id = digest(bytes);
    let path = object_path(directory, &id);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, bytes).unwrap();
    id
}

fn object_count(directory: &TempDir) -> usize {
    fs::read_dir(directory.path().join("objects"))
        .unwrap()
        .map(|entry| fs::read_dir(entry.unwrap().path()).unwrap().count())
        .sum()
}

fn raw_revision(
    magic: &[u8; 9],
    version: u32,
    parents: &[String],
    tree: &str,
    trailing: &[u8],
) -> Vec<u8> {
    let mut bytes = magic.to_vec();
    bytes.extend_from_slice(&version.to_be_bytes());
    bytes.push(parents.len() as u8);
    for parent in parents {
        bytes.extend_from_slice(&id_bytes(parent));
    }
    bytes.extend_from_slice(&id_bytes(tree));
    bytes.extend_from_slice(trailing);
    bytes
}

#[test]
fn r1_checkpoint_bytes_and_identity_remain_frozen() {
    let directory = TempDir::new().unwrap();
    let engine = Engine::open(directory.path()).unwrap();
    let revision = engine
        .checkpoint(None, &[put("README.md", b"hello".to_vec())])
        .unwrap();
    let bytes = fs::read(object_path(&directory, &revision)).unwrap();

    assert_eq!(
        revision,
        "11140b42c390d970cec5c589f0620f8500d97c0e0290fd9f68ee96aa46d2af2f"
    );
    assert_eq!(
        hex(&bytes),
        "414753434d2d52310000000001008540ee65faade55332b6e9580e2edaf72e9ce619528f1b29e329ef56027dc342"
    );
    assert_eq!(engine.load_revision(&revision).unwrap().version, 1);
}

#[test]
fn r2_preserves_parent_order_and_uses_the_first_parent_tree() {
    let directory = TempDir::new().unwrap();
    let engine = Engine::open(directory.path()).unwrap();
    let first = engine
        .checkpoint(None, &[put("first", b"one".to_vec())])
        .unwrap();
    let second = engine
        .checkpoint(None, &[put("second", b"two".to_vec())])
        .unwrap();

    let merged = engine
        .checkpoint_with_parents(
            &[first.clone(), second.clone()],
            &[put("added", b"three".to_vec())],
        )
        .unwrap();
    let revision = engine.load_revision(&merged).unwrap();
    assert_eq!(revision.version, 2);
    assert_eq!(revision.parents, vec![first, second]);
    assert_eq!(engine.read_file(&merged, "first").unwrap(), b"one");
    assert_eq!(engine.read_file(&merged, "added").unwrap(), b"three");
    assert!(matches!(
        engine.read_file(&merged, "second"),
        Err(Error::NotFound(_))
    ));

    let bytes = fs::read(object_path(&directory, &merged)).unwrap();
    assert_eq!(&bytes[..9], b"AGSCM-R2\0");
    assert_eq!(&bytes[9..13], &2u32.to_be_bytes());
    assert_eq!(bytes[13], 2);
    assert_eq!(
        bytes,
        raw_revision(b"AGSCM-R2\0", 2, &revision.parents, &revision.tree, &[])
    );
}

#[test]
fn r2_rejects_bad_magic_version_counts_duplicates_and_trailing_bytes() {
    let directory = TempDir::new().unwrap();
    let engine = Engine::open(directory.path()).unwrap();
    let first = engine.checkpoint(None, &[put("a", b"a".to_vec())]).unwrap();
    let second = engine.checkpoint(None, &[put("b", b"b".to_vec())]).unwrap();
    let tree = engine.load_revision(&first).unwrap().tree;
    let parents = vec![first.clone(), second.clone()];

    let malformed = [
        raw_revision(b"AGSCM-R9\0", 2, &parents, &tree, &[]),
        raw_revision(b"AGSCM-R2\0", 1, &parents, &tree, &[]),
        raw_revision(b"AGSCM-R1\0", 2, &[], &tree, &[]),
        raw_revision(b"AGSCM-R1\0", 1, &parents, &tree, &[]),
        raw_revision(b"AGSCM-R1\0", 1, &[], &tree, &[0]),
        raw_revision(b"AGSCM-R2\0", 2, &[], &tree, &[]),
        raw_revision(b"AGSCM-R2\0", 2, std::slice::from_ref(&first), &tree, &[]),
        raw_revision(
            b"AGSCM-R2\0",
            2,
            &[first.clone(), first.clone()],
            &tree,
            &[],
        ),
        raw_revision(b"AGSCM-R2\0", 2, &parents, &tree, &[0]),
        raw_revision(b"AGSCM-R2\0", 2, &vec![first.clone(); 9], &tree, &[]),
    ];
    let mut truncated = raw_revision(b"AGSCM-R2\0", 2, &parents, &tree, &[]);
    truncated.pop();
    let truncated_id = install_raw(&directory, &truncated);
    assert!(matches!(
        engine.load_revision(&truncated_id),
        Err(Error::Invalid(_))
    ));
    for bytes in malformed {
        let id = install_raw(&directory, &bytes);
        assert!(
            matches!(engine.load_revision(&id), Err(Error::Invalid(_))),
            "accepted malformed revision {id}"
        );
    }
}

#[test]
fn multi_parent_checkpoint_validates_every_parent_before_publishing() {
    let directory = TempDir::new().unwrap();
    let engine = Engine::open(directory.path()).unwrap();
    let first = engine.checkpoint(None, &[put("a", b"a".to_vec())]).unwrap();
    let before = object_count(&directory);
    let missing = "0".repeat(64);

    assert!(matches!(
        engine.checkpoint_with_parents(
            &[first, missing],
            &[put("unpublished", b"content".to_vec())]
        ),
        Err(Error::NotFound(_))
    ));
    assert_eq!(object_count(&directory), before);
}

#[test]
fn diff_pages_cover_large_changes_once_and_bind_the_cursor_pair() {
    let directory = TempDir::new().unwrap();
    let engine = Engine::open(directory.path()).unwrap();
    let files: Vec<_> = (0..120)
        .map(|index| put(&format!("src/file-{index:04}"), vec![b'a']))
        .collect();
    let left = engine.checkpoint(None, &files).unwrap();
    let mut changes = Vec::new();
    let mut expected = BTreeMap::new();
    for index in 0..120 {
        let path = format!("src/file-{index:04}");
        if index % 7 == 0 {
            changes.push(delete(&path));
            expected.insert(path, (true, false));
        } else if index % 5 == 0 {
            changes.push(put(&path, vec![b'b']));
            expected.insert(path, (true, true));
        }
    }
    for index in 0..25 {
        let path = format!("new/file-{index:04}");
        changes.push(put(&path, vec![b'n']));
        expected.insert(path, (false, true));
    }
    let right = engine.checkpoint(Some(&left), &changes).unwrap();

    let first_page = engine.diff_page(&left, &right, None, 7).unwrap();
    let token = first_page.next.clone().unwrap();
    let unrelated = engine
        .checkpoint(None, &[put("other", b"x".to_vec())])
        .unwrap();
    assert!(matches!(
        engine.diff_page(&left, &unrelated, Some(&token), 7),
        Err(Error::Conflict(_))
    ));
    let mut damaged = token.clone();
    let replacement = if damaged.ends_with('0') { '1' } else { '0' };
    damaged.pop();
    damaged.push(replacement);
    assert!(matches!(
        engine.diff_page(&left, &right, Some(&damaged), 7),
        Err(Error::Invalid(_))
    ));

    let mut pages = 1;
    let mut entries = first_page.entries;
    let mut cursor = first_page.next;
    while let Some(token) = cursor {
        let page = engine.diff_page(&left, &right, Some(&token), 7).unwrap();
        entries.extend(page.entries);
        cursor = page.next;
        pages += 1;
        assert!(pages < 100);
    }
    assert!(pages > 1);
    assert_eq!(entries.len(), expected.len());
    let mut seen = BTreeSet::new();
    for entry in entries {
        assert!(seen.insert(entry.path.clone()));
        let expected_sides = expected.get(&entry.path).unwrap();
        assert_eq!(entry.before.is_some(), expected_sides.0);
        assert_eq!(entry.after.is_some(), expected_sides.1);
    }
}

#[test]
fn diff_pages_can_be_empty_with_continuation_and_equal_trees_finish() {
    let directory = TempDir::new().unwrap();
    let engine = Engine::open(directory.path()).unwrap();
    let files: Vec<_> = (0..64)
        .map(|index| put(&format!("file-{index:04}"), vec![b'a']))
        .collect();
    let left = engine.checkpoint(None, &files).unwrap();
    let right = engine
        .checkpoint(Some(&left), &[put("only-on-right", b"new".to_vec())])
        .unwrap();

    let first = engine.diff_page(&left, &right, None, 1).unwrap();
    assert!(first.entries.is_empty());
    assert!(first.next.is_some());

    let equal = engine.diff_page(&left, &left, None, 1).unwrap();
    assert!(equal.entries.is_empty());
    assert!(equal.next.is_none());
    assert!(matches!(
        engine.diff_page(&left, &right, None, 0),
        Err(Error::Limit(_))
    ));
}

#[test]
fn eight_ordered_parents_are_supported_and_duplicate_api_inputs_publish_nothing() {
    let directory = TempDir::new().unwrap();
    let engine = Engine::open(directory.path()).unwrap();
    let parents: Vec<_> = (0u8..8)
        .map(|value| engine.checkpoint(None, &[put("a", vec![value])]).unwrap())
        .collect();
    let revision = engine.checkpoint_with_parents(&parents, &[]).unwrap();
    assert_eq!(engine.load_revision(&revision).unwrap().parents, parents);
    let bytes = fs::read(object_path(&directory, &revision)).unwrap();
    for index in 0..=8 {
        for offset in [0, 1, 31] {
            let cut = 14 + index * 32 + offset;
            let truncated = install_raw(&directory, &bytes[..cut]);
            assert!(
                matches!(engine.load_revision(&truncated), Err(Error::Invalid(_))),
                "accepted R2 truncation at {cut}"
            );
        }
    }
    let before = object_count(&directory);
    assert!(matches!(
        engine.checkpoint_with_parents(
            &[parents[0].clone(), parents[0].clone()],
            &[put("unpublished", vec![1])]
        ),
        Err(Error::Invalid(_))
    ));
    assert_eq!(object_count(&directory), before);
}

#[test]
fn parented_r1_and_multi_parent_r2_have_fixed_external_byte_vectors() {
    let directory = TempDir::new().unwrap();
    let engine = Engine::open(directory.path()).unwrap();
    let genesis = engine
        .checkpoint(None, &[put("README.md", b"hello".to_vec())])
        .unwrap();
    let child = engine
        .checkpoint(Some(&genesis), &[put("README.md", b"hello!".to_vec())])
        .unwrap();
    assert_eq!(
        child,
        "df681c0b870c699a6e304ce952ce790b371ee2308207ff668e976c25ebbd73ff"
    );
    assert_eq!(
        hex(&fs::read(object_path(&directory, &child)).unwrap()),
        "414753434d2d523100000000010111140b42c390d970cec5c589f0620f8500d97c0e0290fd9f68ee96aa46d2af2f1ffd6a7a5b307de7fab9c1487c5afa981e72ac1bc52933116a63aa6bc58204d0"
    );
    let merge = engine
        .checkpoint_with_parents(&[genesis, child], &[])
        .unwrap();
    assert_eq!(
        merge,
        "b630d5da20f348d00ce0f29d1860d2f71d4a8203643f7e8b98ac98ffccf180dc"
    );
    assert_eq!(
        hex(&fs::read(object_path(&directory, &merge)).unwrap()),
        "414753434d2d523200000000020211140b42c390d970cec5c589f0620f8500d97c0e0290fd9f68ee96aa46d2af2fdf681c0b870c699a6e304ce952ce790b371ee2308207ff668e976c25ebbd73ff8540ee65faade55332b6e9580e2edaf72e9ce619528f1b29e329ef56027dc342"
    );
}

#[test]
fn paged_diff_missing_index_preserves_previous_cursor_after_retrieval() {
    let directory = TempDir::new().unwrap();
    let engine = Engine::open(directory.path()).unwrap();
    let files: Vec<_> = (0..40)
        .map(|i| put(&format!("file-{i}"), vec![1]))
        .collect();
    let left = engine.checkpoint(None, &files).unwrap();
    let right = engine
        .checkpoint(Some(&left), &[put("file-0", vec![2])])
        .unwrap();
    let first = engine.diff_page(&left, &right, None, 1).unwrap();
    let cursor = first.next.unwrap();
    let tree = engine.load_revision(&right).unwrap().tree;
    let path = object_path(&directory, &tree);
    let bytes = fs::read(&path).unwrap();
    fs::remove_file(&path).unwrap();
    let incomplete = engine.diff_page(&left, &right, Some(&cursor), 1).unwrap();
    assert_eq!(incomplete.needed_objects, vec![tree.clone()]);
    assert!(incomplete.entries.is_empty());
    assert_eq!(incomplete.next.as_deref(), Some(cursor.as_str()));
    let initial = engine.diff_page(&left, &right, None, 1).unwrap();
    assert_eq!(initial.needed_objects, vec![tree]);
    let initial_cursor = initial.next.unwrap();
    assert!(
        engine
            .diff_page(&right, &left, Some(&initial_cursor), 1)
            .is_err()
    );
    fs::write(path, bytes).unwrap();
    let initial_resumed = engine
        .diff_page(&left, &right, Some(&initial_cursor), 1)
        .unwrap();
    assert!(initial_resumed.needed_objects.is_empty());
    let mut entries = first.entries;
    let mut cursor = Some(cursor);
    while let Some(previous) = cursor {
        let page = engine.diff_page(&left, &right, Some(&previous), 1).unwrap();
        entries.extend(page.entries);
        cursor = page.next;
    }
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].path, "file-0");
}

#[test]
fn diff_missing_upcoming_leaf_discards_partial_page_before_retry() {
    let directory = TempDir::new().unwrap();
    let engine = Engine::open(directory.path()).unwrap();
    let files: Vec<_> = (0..40)
        .map(|i| put(&format!("file-{i}"), vec![1]))
        .collect();
    let left = engine.checkpoint(None, &files).unwrap();
    let changes: Vec<_> = (0..40)
        .map(|i| put(&format!("file-{i}"), vec![2]))
        .collect();
    let right = engine.checkpoint(Some(&left), &changes).unwrap();
    let ordered = engine.list_files(&right, None, 4096).unwrap().entries;
    let upcoming = &ordered[2];
    let mut leaf = b"AGSCM-L1\0".to_vec();
    leaf.push(1); // Exact index namespace in the declared source profile.
    leaf.extend_from_slice(&id_bytes(&digest(upcoming.path.as_bytes())));
    leaf.extend_from_slice(&(upcoming.path.len() as u16).to_be_bytes());
    leaf.extend_from_slice(upcoming.path.as_bytes());
    leaf.extend_from_slice(&id_bytes(&upcoming.blob));
    let leaf_id = digest(&leaf);
    let leaf_path = object_path(&directory, &leaf_id);
    assert_eq!(fs::read(&leaf_path).unwrap(), leaf);
    let first = engine.diff_page(&left, &right, None, 1).unwrap();
    assert_eq!(first.entries.len(), 1);
    let cursor = first.next.unwrap();
    fs::remove_file(&leaf_path).unwrap();
    let partial = engine
        .diff_page(&left, &right, Some(&cursor), 4096)
        .unwrap();
    assert!(partial.entries.is_empty());
    assert_eq!(partial.needed_objects, vec![leaf_id]);
    assert_eq!(partial.next.as_deref(), Some(cursor.as_str()));
    fs::write(&leaf_path, leaf).unwrap();
    let mut entries = first.entries;
    let mut cursor = partial.next;
    while let Some(previous) = cursor {
        let page = engine
            .diff_page(&left, &right, Some(&previous), 4096)
            .unwrap();
        assert!(page.needed_objects.is_empty());
        entries.extend(page.entries);
        cursor = page.next;
    }
    assert_eq!(entries.len(), 40);
    assert_eq!(
        entries
            .into_iter()
            .map(|entry| entry.path)
            .collect::<BTreeSet<_>>()
            .len(),
        40
    );
}
