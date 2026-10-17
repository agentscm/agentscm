use std::cell::{Cell, RefCell};
use std::collections::{BTreeMap, BTreeSet};
use std::io::{self, Cursor, Read};

use agentscm::history::{history_page, relation_page};
use agentscm::model::FileChange;
use agentscm::store::{EncodedObjectReader, Engine, MAX_OBJECT_BYTES, SourceReader, digest};
use agentscm::sync::{qualify_reader, qualify_source};
use agentscm::{Error, Result};
use tempfile::TempDir;

struct ScopedReader<'a> {
    engine: &'a Engine,
    missing: RefCell<BTreeSet<String>>,
    calls: RefCell<Vec<String>>,
}

impl<'a> ScopedReader<'a> {
    fn new(engine: &'a Engine) -> Self {
        Self {
            engine,
            missing: RefCell::new(BTreeSet::new()),
            calls: RefCell::new(Vec::new()),
        }
    }
}

impl EncodedObjectReader for ScopedReader<'_> {
    fn open_encoded(&self, id: &str) -> Result<Box<dyn Read + '_>> {
        self.calls.borrow_mut().push(id.to_owned());
        if self.missing.borrow().contains(id) {
            return Err(Error::MissingObject(id.to_owned()));
        }
        self.engine.open_encoded(id)
    }
}

fn put(path: &str, bytes: &[u8]) -> FileChange {
    FileChange {
        path: path.to_owned(),
        content: Some(bytes.to_vec()),
    }
}

#[test]
fn local_and_callback_reads_share_results_and_selective_retrieval() {
    let directory = TempDir::new().unwrap();
    let engine = Engine::open(directory.path()).unwrap();
    let base = engine
        .checkpoint(None, &[put("a", b"base"), put("dir/b", b"other")])
        .unwrap();
    let left = engine
        .checkpoint(Some(&base), &[put("a", b"left")])
        .unwrap();
    let right = engine
        .checkpoint(Some(&base), &[put("right", b"right")])
        .unwrap();
    let tip = engine
        .checkpoint_with_parents(&[left.clone(), right.clone()], &[])
        .unwrap();
    let scoped = ScopedReader::new(&engine);
    let reader = SourceReader::new(&scoped);
    assert_eq!(
        reader.load_revision(&tip).unwrap(),
        engine.load_revision(&tip).unwrap()
    );
    // Revision loading validates its tree and three index roots, but never walks parents.
    assert_eq!(scoped.calls.borrow()[0], tip);
    assert_eq!(scoped.calls.borrow().len(), 5);
    assert!(!scoped.calls.borrow().contains(&base));
    assert!(!scoped.calls.borrow().contains(&left));
    assert!(!scoped.calls.borrow().contains(&right));
    assert_eq!(
        reader.read_encoded(&tip).unwrap(),
        engine.read_encoded(&tip).unwrap()
    );
    assert_eq!(reader.read_file(&tip, "a").unwrap(), b"left");
    let proof = qualify_source(&engine, std::slice::from_ref(&tip)).unwrap();
    let reads: BTreeSet<_> = scoped.calls.borrow().iter().cloned().collect();
    assert!(reads.len() < proof.objects().len());
    assert_eq!(
        qualify_reader(&reader, std::slice::from_ref(&tip)).unwrap(),
        proof
    );
    let mut cursor = None;
    loop {
        let page = reader.list_files(&tip, cursor.as_deref(), 1).unwrap();
        assert_eq!(page, engine.list_files(&tip, cursor.as_deref(), 1).unwrap());
        cursor = page.next;
        if cursor.is_none() {
            break;
        }
    }
    let actual = reader.diff(&base, &tip, 100).unwrap();
    let expected = engine.diff(&base, &tip, 100).unwrap();
    assert_eq!(actual.entries, expected.entries);
    assert_eq!(actual.truncated, expected.truncated);
    let mut cursor = None;
    loop {
        let page = reader.diff_page(&base, &tip, cursor.as_deref(), 1).unwrap();
        assert_eq!(
            page,
            engine.diff_page(&base, &tip, cursor.as_deref(), 1).unwrap()
        );
        cursor = page.next;
        if cursor.is_none() {
            break;
        }
    }
    let mut cursor = None;
    loop {
        let page = history_page(&reader, &tip, cursor.as_deref(), 1).unwrap();
        assert_eq!(
            page,
            history_page(&engine, &tip, cursor.as_deref(), 1).unwrap()
        );
        cursor = page.next;
        if cursor.is_none() {
            break;
        }
    }
    let mut cursor = None;
    loop {
        let page = relation_page(&reader, &base, &tip, cursor.as_deref(), 1).unwrap();
        assert_eq!(
            page,
            relation_page(&engine, &base, &tip, cursor.as_deref(), 1).unwrap()
        );
        cursor = page.next;
        if cursor.is_none() {
            break;
        }
    }
}

#[test]
fn typed_missing_is_resumable_but_never_complete_qualification() {
    let directory = TempDir::new().unwrap();
    let engine = Engine::open(directory.path()).unwrap();
    let base = engine.checkpoint(None, &[put("a", b"base")]).unwrap();
    let tip = engine.checkpoint(Some(&base), &[put("a", b"tip")]).unwrap();
    let scoped = ScopedReader::new(&engine);
    scoped.missing.borrow_mut().insert(base.clone());
    let reader = SourceReader::new(&scoped);
    let history = history_page(&reader, &tip, None, 100).unwrap();
    assert_eq!(history.entries.len(), 1);
    assert_eq!(history.needed_objects, vec![base.clone()]);
    assert!(
        matches!(qualify_reader(&reader, std::slice::from_ref(&tip)), Err(Error::MissingObject(id)) if id == base)
    );
    let diff = reader.diff_page(&base, &tip, None, 1).unwrap();
    assert!(diff.entries.is_empty());
    assert_eq!(diff.needed_objects, vec![base.clone()]);
    scoped.missing.borrow_mut().clear();
    let resumed = history_page(&reader, &tip, history.next.as_deref(), 100).unwrap();
    assert_eq!(resumed.entries[0].id, base);
    assert!(resumed.next.is_none());
    let resumed = reader
        .diff_page(&base, &tip, diff.next.as_deref(), 1)
        .unwrap();
    assert_eq!(resumed, engine.diff_page(&base, &tip, None, 1).unwrap());
    assert_eq!(
        qualify_reader(&reader, std::slice::from_ref(&tip)).unwrap(),
        qualify_source(&engine, &[tip]).unwrap()
    );
}

#[derive(Clone, Copy)]
enum Failure {
    Denied,
    Indeterminate,
    LegacyMissing,
    MismatchedMissing,
    StreamDenied,
    StreamIo,
}

struct FailedReader(Failure);

struct FailedStream(io::ErrorKind);

impl Read for FailedStream {
    fn read(&mut self, _: &mut [u8]) -> io::Result<usize> {
        Err(io::Error::from(self.0))
    }
}

impl EncodedObjectReader for FailedReader {
    fn open_encoded(&self, id: &str) -> Result<Box<dyn Read + '_>> {
        match self.0 {
            Failure::Denied => Err(Error::SourceDenied),
            Failure::Indeterminate => Err(Error::SourceIndeterminate),
            Failure::LegacyMissing => Err(Error::NotFound(format!("object: {id}"))),
            Failure::MismatchedMissing => Err(Error::MissingObject("f".repeat(64))),
            Failure::StreamDenied => Ok(Box::new(FailedStream(io::ErrorKind::PermissionDenied))),
            Failure::StreamIo => Ok(Box::new(FailedStream(io::ErrorKind::UnexpectedEof))),
        }
    }
}

#[test]
fn denial_uncertainty_and_provider_strings_never_become_needed_objects() {
    let id = "0".repeat(64);
    for failure in [
        Failure::Denied,
        Failure::Indeterminate,
        Failure::LegacyMissing,
        Failure::StreamDenied,
        Failure::StreamIo,
    ] {
        let provider = FailedReader(failure);
        let reader = SourceReader::new(&provider);
        let error = history_page(&reader, &id, None, 1).unwrap_err();
        assert!(error.missing_object_id().is_none());
        if matches!(failure, Failure::Denied | Failure::StreamDenied) {
            assert!(matches!(error, Error::SourceDenied));
        } else {
            assert!(matches!(error, Error::SourceIndeterminate));
        }
        assert!(reader.diff_page(&id, &id, None, 1).is_err());
        assert!(qualify_reader(&reader, std::slice::from_ref(&id)).is_err());
    }
    let provider = FailedReader(Failure::MismatchedMissing);
    assert!(matches!(
        SourceReader::new(&provider).read_encoded(&id),
        Err(Error::Invalid(_))
    ));
}

struct BytesReader(Vec<u8>, Cell<usize>);

impl EncodedObjectReader for BytesReader {
    fn open_encoded(&self, _: &str) -> Result<Box<dyn Read + '_>> {
        self.1.set(self.1.get() + 1);
        Ok(Box::new(Cursor::new(self.0.as_slice())))
    }
}

#[test]
fn invalid_ids_do_not_call_provider_and_corrupt_or_unknown_bytes_fail() {
    let provider = BytesReader(b"unknown encoding".to_vec(), Cell::new(0));
    let reader = SourceReader::new(&provider);
    assert!(matches!(
        reader.read_encoded("../object"),
        Err(Error::Invalid(_))
    ));
    assert_eq!(provider.1.get(), 0);
    assert!(matches!(
        reader.read_encoded(&"0".repeat(64)),
        Err(Error::Invalid(_))
    ));
    assert!(matches!(
        reader.read_encoded(&digest(&provider.0)),
        Err(Error::Invalid(_))
    ));
}

struct EndlessReader(Cell<u64>);

struct EndlessStream<'a>(&'a Cell<u64>);

impl Read for EndlessStream<'_> {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        buffer.fill(0);
        self.0.set(self.0.get() + buffer.len() as u64);
        Ok(buffer.len())
    }
}

impl EncodedObjectReader for EndlessReader {
    fn open_encoded(&self, _: &str) -> Result<Box<dyn Read + '_>> {
        Ok(Box::new(EndlessStream(&self.0)))
    }
}

#[test]
fn endless_stream_stops_at_object_cap_plus_one() {
    let provider = EndlessReader(Cell::new(0));
    assert!(matches!(
        SourceReader::new(&provider).read_encoded(&"0".repeat(64)),
        Err(Error::Limit(_))
    ));
    assert_eq!(provider.0.get(), MAX_OBJECT_BYTES + 1);
}

#[test]
fn wrong_object_type_and_incomplete_blob_closure_fail() {
    let directory = TempDir::new().unwrap();
    let engine = Engine::open(directory.path()).unwrap();
    let root = engine.checkpoint(None, &[put("a", b"bytes")]).unwrap();
    let blob = engine.list_files(&root, None, 1).unwrap().entries[0]
        .blob
        .clone();
    let scoped = ScopedReader::new(&engine);
    let reader = SourceReader::new(&scoped);
    assert!(matches!(
        reader.load_revision(&blob),
        Err(Error::Invalid(_))
    ));
    assert!(matches!(
        qualify_reader(&reader, std::slice::from_ref(&blob)),
        Err(Error::Invalid(_))
    ));
    scoped.missing.borrow_mut().insert(blob.clone());
    assert!(matches!(reader.read_file(&root, "a"), Err(Error::MissingObject(id)) if id == blob));
    assert!(
        matches!(qualify_reader(&reader, &[root]), Err(Error::MissingObject(id)) if id == blob)
    );
}

struct OverlayReader<'a> {
    engine: &'a Engine,
    objects: BTreeMap<String, Vec<u8>>,
}

impl EncodedObjectReader for OverlayReader<'_> {
    fn open_encoded(&self, id: &str) -> Result<Box<dyn Read + '_>> {
        match self.objects.get(id) {
            Some(bytes) => Ok(Box::new(Cursor::new(bytes.as_slice()))),
            None => self.engine.open_encoded(id),
        }
    }
}

#[test]
fn correctly_hashed_but_inconsistent_indexes_cannot_qualify() {
    let directory = TempDir::new().unwrap();
    let engine = Engine::open(directory.path()).unwrap();
    let path = format!("root{}/file", "/a".repeat(500));
    let root = engine.checkpoint(None, &[put(&path, b"bytes")]).unwrap();
    let empty = engine.checkpoint(None, &[]).unwrap();
    let original_tree = engine
        .read_encoded(&engine.load_revision(&root).unwrap().tree)
        .unwrap();
    let empty_tree = engine
        .read_encoded(&engine.load_revision(&empty).unwrap().tree)
        .unwrap();
    // Keep the populated exact index, replacing either other index with its empty root.
    for range in [41..73, 73..105] {
        let mut tree = original_tree.clone();
        tree[range.clone()].copy_from_slice(&empty_tree[range]);
        let tree_id = digest(&tree);
        let mut revision = engine.read_encoded(&root).unwrap();
        let offset = revision.len() - 32;
        revision[offset..].copy_from_slice(&id_bytes(&tree_id));
        let revision_id = digest(&revision);
        let provider = OverlayReader {
            engine: &engine,
            objects: BTreeMap::from([(tree_id, tree), (revision_id.clone(), revision)]),
        };
        let reader = SourceReader::new(&provider);
        assert!(reader.load_revision(&revision_id).is_ok());
        assert!(matches!(
            qualify_reader(&reader, &[revision_id]),
            Err(Error::Invalid(_))
        ));
    }
}

struct InterruptedReader(Vec<u8>);

impl EncodedObjectReader for InterruptedReader {
    fn open_encoded(&self, _: &str) -> Result<Box<dyn Read + '_>> {
        Ok(Box::new(
            Cursor::new(self.0.as_slice()).chain(FailedStream(io::ErrorKind::UnexpectedEof)),
        ))
    }
}

#[test]
fn stream_failure_after_valid_bytes_discards_the_entire_result() {
    let directory = TempDir::new().unwrap();
    let engine = Engine::open(directory.path()).unwrap();
    let root = engine.checkpoint(None, &[]).unwrap();
    let provider = InterruptedReader(engine.read_encoded(&root).unwrap());
    let reader = SourceReader::new(&provider);
    assert!(matches!(
        reader.load_revision(&root),
        Err(Error::SourceIndeterminate)
    ));
    assert!(matches!(
        history_page(&reader, &root, None, 1),
        Err(Error::SourceIndeterminate)
    ));
    assert!(matches!(
        qualify_reader(&reader, &[root]),
        Err(Error::SourceIndeterminate)
    ));
}

fn id_bytes(id: &str) -> Vec<u8> {
    id.as_bytes()
        .chunks_exact(2)
        .map(|pair| u8::from_str_radix(std::str::from_utf8(pair).unwrap(), 16).unwrap())
        .collect()
}

fn install_raw(directory: &TempDir, bytes: Vec<u8>) -> String {
    let id = digest(&bytes);
    let path = directory
        .path()
        .join("objects")
        .join(&id[..2])
        .join(&id[2..]);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, bytes).unwrap();
    id
}

fn repeated_depth_revision(directory: &TempDir, byte: u8, depth: u8) -> String {
    let mut blob = b"AGSCM-B1\0".to_vec();
    blob.extend_from_slice(&1u64.to_be_bytes());
    blob.push(byte);
    let blob = install_raw(directory, blob);
    let key = digest(b"a");
    let mut leaf = b"AGSCM-L1\0".to_vec();
    leaf.push(1);
    leaf.extend_from_slice(&id_bytes(&key));
    leaf.extend_from_slice(&1u16.to_be_bytes());
    leaf.push(b'a');
    leaf.extend_from_slice(&id_bytes(&blob));
    let mut exact = install_raw(directory, leaf);
    let slot = u8::from_str_radix(&key[usize::from(depth)..usize::from(depth) + 1], 16).unwrap();
    for _ in 0..128 {
        let mut branch = b"AGSCM-N1\0".to_vec();
        branch.extend_from_slice(&[1, depth, 1, slot]);
        branch.extend_from_slice(&id_bytes(&exact));
        exact = install_raw(directory, branch);
    }
    let mut portable = b"AGSCM-N1\0".to_vec();
    portable.extend_from_slice(&[2, 0, 0]);
    let portable = install_raw(directory, portable);
    let mut prefix = b"AGSCM-N1\0".to_vec();
    prefix.extend_from_slice(&[3, 0, 0]);
    let prefix = install_raw(directory, prefix);
    let mut tree = b"AGSCM-T2\0".to_vec();
    for id in [&exact, &portable, &prefix] {
        tree.extend_from_slice(&id_bytes(id));
    }
    let tree = install_raw(directory, tree);
    let mut revision = b"AGSCM-R1\0".to_vec();
    revision.extend_from_slice(&1u32.to_be_bytes());
    revision.push(0);
    revision.extend_from_slice(&id_bytes(&tree));
    install_raw(directory, revision)
}

#[test]
fn unpaged_diff_rejects_repeated_depth_before_recursive_or_flattened_descent() {
    for right_depth in [0, 1] {
        let directory = TempDir::new().unwrap();
        let engine = Engine::open(directory.path()).unwrap();
        let left = repeated_depth_revision(&directory, 1, 0);
        let right = repeated_depth_revision(&directory, 2, right_depth);
        assert!(engine.load_revision(&left).is_ok());
        assert!(engine.load_revision(&right).is_ok());
        assert!(matches!(
            engine.diff(&left, &right, 1),
            Err(Error::Invalid(_))
        ));
        let provider = ScopedReader::new(&engine);
        assert!(matches!(
            SourceReader::new(&provider).diff(&left, &right, 1),
            Err(Error::Invalid(_))
        ));
        assert!(provider.calls.borrow().len() <= 20);
    }
}
