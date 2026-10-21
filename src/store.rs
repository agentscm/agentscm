use std::borrow::Cow;
use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File, OpenOptions};
use std::io::{self, ErrorKind, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use sha2::{Digest, Sha256};

use crate::error::{Error, Result};
use crate::model::{DiffEntry, DiffResult, EntryChange, EntryKind, FileChange, Revision};

const ID_BYTES: usize = 32;
const ID_HEX_LEN: usize = ID_BYTES * 2;
const MAX_PATH_BYTES: usize = 1_024;
pub const MAX_CHANGES: usize = 4_096;
pub const MAX_CHECKPOINT_PREFIXES: usize = 16_384;
pub const MAX_FILE_BYTES: usize = 64 * 1024 * 1024;
pub const MAX_CHECKPOINT_BYTES: usize = 128 * 1024 * 1024;
const MAX_DIFF_SCAN: usize = 100_000;
pub const MAX_OBJECT_BYTES: u64 = MAX_FILE_BYTES as u64 + 4_096;
pub const MAX_FILE_PAGE: usize = 4_096;
const MAX_FILE_PAGE_SCAN: usize = 100_000;
const MAX_FILE_CURSOR_BYTES: usize = 64 * 1024;
pub const MAX_DIFF_PAGE: usize = 4_096;
const MAX_DIFF_PAGE_SCAN: usize = 64;
const MAX_DIFF_CURSOR_BYTES: usize = 64 * 1024;
// A reuse attempt must not read an unbounded number of historical blob bytes.
#[cfg(not(test))]
const MAX_CAPTURE_QUALIFICATION_BYTES: u64 = MAX_CHECKPOINT_BYTES as u64;
// Exercise the same exhaustion/fallback path with small valid unit fixtures.
#[cfg(test)]
const MAX_CAPTURE_QUALIFICATION_BYTES: u64 = 128 * 1024;

const BLOB_MAGIC: &[u8; 9] = b"AGSCM-B1\0";
const TREE_MAGIC: &[u8; 9] = b"AGSCM-T2\0";
const BRANCH_MAGIC: &[u8; 9] = b"AGSCM-N1\0";
const LEAF_MAGIC: &[u8; 9] = b"AGSCM-L1\0";
// L1 layout plus one kind byte; used only for non-regular exact-file entries.
const LEAF_V2_MAGIC: &[u8; 9] = b"AGSCM-L2\0";
pub const MAX_SYMLINK_TARGET_BYTES: usize = 1_024;
const REVISION_V1_MAGIC: &[u8; 9] = b"AGSCM-R1\0";
const REVISION_V2_MAGIC: &[u8; 9] = b"AGSCM-R2\0";
const FILE_CURSOR_MAGIC: &[u8; 9] = b"AGSCM-F1\0";
const DIFF_CURSOR_MAGIC: &[u8; 9] = b"AGSCM-D1\0";
const DIFF_START_MAGIC: &[u8; 9] = b"AGSCM-D0\0";
const READ_BUDGET_EXCEEDED: &str = "operation source read budget exceeded";
const READ_BUDGET_CHUNK: usize = 64 * 1024;

static TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);

#[cfg(test)]
#[derive(Clone, Debug, PartialEq, Eq)]
enum SyncEvent {
    FileSync(String),
    Link(String),
    DirSync(PathBuf),
}

#[cfg(test)]
std::thread_local! {
    static SYNC_LOG: std::cell::RefCell<Option<Vec<SyncEvent>>> = const { std::cell::RefCell::new(None) };
    static FLUSH_FAIL_AFTER: std::cell::Cell<Option<usize>> = const { std::cell::Cell::new(None) };
    static FLUSH_DIR_SYNC_FAIL: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

#[cfg(test)]
fn record_sync(event: SyncEvent) {
    SYNC_LOG.with(|log| {
        if let Some(log) = log.borrow_mut().as_mut() {
            log.push(event);
        }
    });
}

type StagedObjects = BTreeMap<String, Vec<u8>>;

#[derive(Clone, Debug)]
pub struct Engine {
    objects: PathBuf,
    _access: crate::local_recovery::AccessGuard,
    read_budget: Option<Arc<ReadBudgetState>>,
    /// Private to a derived engine; never set on the shared engine.
    staged: Option<Arc<Mutex<StagedObjects>>>,
}

#[derive(Debug)]
struct ReadBudgetState {
    remaining: AtomicU64,
    exceeded: AtomicBool,
}

#[derive(Clone, Debug)]
pub(crate) struct ReadBudget {
    state: Arc<ReadBudgetState>,
}

struct BudgetedRead<R> {
    inner: R,
    budget: Option<Arc<ReadBudgetState>>,
}

#[derive(Clone, Debug)]
pub(crate) struct BlobChange {
    pub path: String,
    pub blob: Option<String>,
}

#[derive(Clone, Copy, Debug)]
enum BlobInput<'a> {
    Bytes(&'a [u8]),
    Existing(&'a str),
}

#[derive(Clone, Copy, Debug)]
struct TreeChange<'a> {
    path: &'a str,
    blob: Option<BlobInput<'a>>,
    /// None keeps the existing entry's kind (Regular for a new path).
    kind: Option<EntryKind>,
}

/// Opens one encoded object after the caller has authorized the requested ID.
///
/// Implementations must return [`Error::MissingObject`] only for a known
/// missing object and must put the requested canonical ID in that error.
/// Authorization failures and uncertain backend results must remain distinct.
pub trait EncodedObjectReader {
    fn open_encoded(&self, id: &str) -> Result<Box<dyn Read + '_>>;
}

/// A read-only source view backed by caller-authorized encoded objects.
pub struct SourceReader<'a> {
    reader: &'a dyn EncodedObjectReader,
}

/// High-level immutable source reads shared by local and callback-backed stores.
pub trait SourceRead {
    fn read_encoded(&self, id: &str) -> Result<Vec<u8>>;
    fn load_revision(&self, id: &str) -> Result<Revision>;
    fn read_file(&self, revision: &str, path: &str) -> Result<Vec<u8>>;
    fn list_files(&self, revision_id: &str, cursor: Option<&str>, limit: usize)
    -> Result<FilePage>;
    fn diff(&self, left: &str, right: &str, limit: usize) -> Result<DiffResult>;
    fn diff_page(
        &self,
        left_id: &str,
        right_id: &str,
        cursor: Option<&str>,
        limit: usize,
    ) -> Result<DiffPage>;
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub(crate) enum Namespace {
    Exact = 1,
    Portable = 2,
    Prefix = 3,
}

#[derive(Clone, Copy, Debug)]
enum PrefixChange {
    Increment,
    Decrement,
}

#[derive(Clone, Debug)]
enum Node {
    Branch {
        namespace: Namespace,
        depth: u8,
        children: Vec<(u8, String)>,
    },
    Leaf {
        namespace: Namespace,
        key: [u8; ID_BYTES],
        path: String,
        value: String,
        kind: EntryKind,
    },
}

#[derive(Clone, Debug, serde::Serialize, PartialEq, Eq)]
pub struct FileEntry {
    pub path: String,
    pub blob: String,
    pub kind: EntryKind,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FilePage {
    pub entries: Vec<FileEntry>,
    pub next: Option<String>,
}

/// A bounded diff page in manifest traversal order.
///
/// Changed and deleted entries from the left manifest are emitted first,
/// followed by paths that exist only in the right manifest.
#[derive(Clone, Debug, serde::Serialize, PartialEq, Eq)]
pub struct DiffPage {
    pub entries: Vec<DiffEntry>,
    pub needed_objects: Vec<String>,
    pub next: Option<String>,
}

#[derive(Clone, Debug)]
pub(crate) enum EncodedObject {
    Blob,
    Tree {
        exact: String,
        portable: String,
        prefix: String,
    },
    Branch {
        namespace: Namespace,
        depth: u8,
        children: Vec<(u8, String)>,
    },
    Leaf {
        namespace: Namespace,
        object_id: String,
        key: [u8; ID_BYTES],
        path: String,
        value: String,
        kind: EntryKind,
    },
    Revision(Revision),
}

#[derive(Clone, Debug)]
struct FileCursorFrame {
    object_id: String,
    next_child: u8,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
enum DiffPhase {
    Left = 1,
    RightAdditions = 2,
}

#[derive(Clone, Debug)]
struct DiffCursor {
    phase: DiffPhase,
    frames: Vec<FileCursorFrame>,
}

impl ReadBudgetState {
    fn reserve(&self, requested: usize) -> usize {
        let requested = requested as u64;
        let mut remaining = self.remaining.load(Ordering::Acquire);
        loop {
            let reserved = remaining.min(requested);
            if reserved == 0 {
                return 0;
            }
            match self.remaining.compare_exchange_weak(
                remaining,
                remaining - reserved,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => return reserved as usize,
                Err(actual) => remaining = actual,
            }
        }
    }

    fn refund(&self, unused: usize) {
        if unused != 0 {
            self.remaining.fetch_add(unused as u64, Ordering::AcqRel);
        }
    }

    fn mark_exceeded(&self) {
        self.exceeded.store(true, Ordering::Release);
    }
}

impl ReadBudget {
    #[cfg(test)]
    pub(crate) fn remaining(&self) -> u64 {
        self.state.remaining.load(Ordering::Acquire)
    }

    pub(crate) fn finish<T>(&self, result: Result<T>) -> Result<T> {
        if self.state.exceeded.load(Ordering::Acquire) {
            Err(Error::Limit(READ_BUDGET_EXCEEDED.into()))
        } else {
            result
        }
    }
}

impl<R: Read> Read for BudgetedRead<R> {
    fn read(&mut self, output: &mut [u8]) -> io::Result<usize> {
        let Some(budget) = &self.budget else {
            return self.inner.read(output);
        };
        if budget.exceeded.load(Ordering::Acquire) {
            return Err(io::Error::other(READ_BUDGET_EXCEEDED));
        }
        if output.is_empty() {
            return Ok(0);
        }
        let reserved = budget.reserve(output.len().min(READ_BUDGET_CHUNK));
        if reserved == 0 {
            let mut probe = [0u8; 1];
            return match self.inner.read(&mut probe) {
                Ok(0) => Ok(0),
                Ok(_) => {
                    budget.mark_exceeded();
                    Err(io::Error::other(READ_BUDGET_EXCEEDED))
                }
                Err(error) => Err(error),
            };
        }
        match self.inner.read(&mut output[..reserved]) {
            Ok(read) => {
                budget.refund(reserved - read);
                Ok(read)
            }
            Err(error) => {
                budget.refund(reserved);
                Err(error)
            }
        }
    }
}

impl Engine {
    #[cfg(unix)]
    pub(crate) fn data_root(&self) -> &Path {
        self.objects
            .parent()
            .expect("engine objects have a data root")
    }

    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        crate::local_recovery::check_before_create(path.as_ref(), false)?;
        let root = create_directory_durable(path)?;
        let access = crate::local_recovery::AccessGuard::shared(&root)?;
        let objects = create_directory_durable(root.join("objects"))?;
        Ok(Self {
            objects,
            _access: access,
            read_budget: None,
            staged: None,
        })
    }

    /// Opens an already provisioned store without creating directories or the lock file.
    /// Missing state fails closed.
    pub fn open_existing(path: impl AsRef<Path>) -> Result<Self> {
        let root = fs::canonicalize(path.as_ref())?;
        let access = crate::local_recovery::AccessGuard::shared_existing(&root)?;
        let objects = root.join("objects");
        if !fs::symlink_metadata(&objects)?.is_dir() {
            return Err(Error::Invalid(
                "store objects path is not a directory".into(),
            ));
        }
        Ok(Self {
            objects,
            _access: access,
            read_budget: None,
            staged: None,
        })
    }

    /// Derives an engine that buffers published objects in memory until `flush`.
    /// A staged call holds every object it publishes in memory until flush,
    /// including unreachable intermediates and a copy of blob bytes, bounded by
    /// `MAX_CHANGES`, `MAX_CHECKPOINT_PREFIXES` and `MAX_CHECKPOINT_BYTES`.
    fn staged(&self) -> Self {
        Self {
            objects: self.objects.clone(),
            _access: self._access.clone(),
            read_budget: self.read_budget.clone(),
            staged: Some(Arc::default()),
        }
    }

    /// Writes the pending objects reachable from `roots`, then syncs each
    /// touched directory once; the rest are discarded. Invariant: a revision is
    /// published only after every object it references is file-synced, linked
    /// and its directory synced.
    fn flush(&self, roots: &[&str]) -> Result<()> {
        let staged = self
            .staged
            .as_ref()
            .expect("flush requires a staged engine");
        let mut pending = std::mem::take(&mut *staged.lock().expect("staged objects lock"));
        let mut reachable = BTreeSet::new();
        let mut stack: Vec<String> = roots.iter().map(|id| (*id).to_owned()).collect();
        while let Some(id) = stack.pop() {
            let Some(bytes) = pending.get(&id) else {
                continue; // loaded from disk, so already durable
            };
            if !reachable.insert(id.clone()) {
                continue;
            }
            match inspect_encoded_object(&id, bytes)? {
                EncodedObject::Blob | EncodedObject::Revision(_) => {}
                EncodedObject::Tree {
                    exact,
                    portable,
                    prefix,
                } => stack.extend([exact, portable, prefix]),
                EncodedObject::Branch { children, .. } => {
                    stack.extend(children.into_iter().map(|(_, child)| child))
                }
                EncodedObject::Leaf {
                    namespace: Namespace::Prefix,
                    ..
                } => {}
                EncodedObject::Leaf { value, .. } => stack.push(value),
            }
        }
        if reachable.is_empty() {
            return Ok(());
        }
        let mut shards = BTreeSet::new();
        for id in &reachable {
            #[cfg(test)]
            FLUSH_FAIL_AFTER.with(|remaining| match remaining.get() {
                Some(0) => {
                    remaining.set(None);
                    Err(Error::Io(io::Error::other("injected flush interruption")))
                }
                Some(count) => {
                    remaining.set(Some(count - 1));
                    Ok(())
                }
                None => Ok(()),
            })?;
            let directory = self.shard_directory(id)?;
            let bytes = pending.remove(id).expect("reachable object is pending");
            self.link_object(&directory, id, &bytes)?;
            shards.insert(directory);
        }
        #[cfg(test)]
        if FLUSH_DIR_SYNC_FAIL.with(|fail| fail.replace(false)) {
            return Err(Error::Io(io::Error::other(
                "injected directory sync failure",
            )));
        }
        sync_directory(&self.objects)?;
        for directory in shards {
            sync_directory(&directory)?;
        }
        Ok(())
    }

    pub(crate) fn with_read_budget(&self, maximum: u64) -> (Self, ReadBudget) {
        let state = Arc::new(ReadBudgetState {
            remaining: AtomicU64::new(maximum),
            exceeded: AtomicBool::new(false),
        });
        (
            Self {
                objects: self.objects.clone(),
                _access: self._access.clone(),
                read_budget: Some(state.clone()),
                staged: None,
            },
            ReadBudget { state },
        )
    }

    pub fn checkpoint(&self, parent: Option<&str>, changes: &[FileChange]) -> Result<String> {
        let changes = validate_checkpoint_changes(changes)?;
        self.checkpoint_validated(parent, &changes)
    }

    /// [`Engine::checkpoint`] for callers that set entry kinds explicitly.
    /// Each change sets exactly the given kind, so a mode-only change is
    /// expressed by repeating the existing bytes with a different kind.
    pub fn checkpoint_entries(
        &self,
        parent: Option<&str>,
        changes: &[EntryChange],
    ) -> Result<String> {
        let changes = validate_entry_changes(changes)?;
        self.checkpoint_validated(parent, &changes)
    }

    /// [`Engine::checkpoint_with_parents`] for explicit entry kinds.
    pub fn checkpoint_entries_with_parents(
        &self,
        parents: &[String],
        changes: &[EntryChange],
    ) -> Result<String> {
        validate_v2_parents(parents)?;
        let changes = validate_entry_changes(changes)?;
        self.checkpoint_with_parents_validated(parents, &changes)
    }

    fn checkpoint_validated(
        &self,
        parent: Option<&str>,
        changes: &[TreeChange<'_>],
    ) -> Result<String> {
        let stage = self.staged();
        let (exact_root, portable_root, prefix_root, parents) = match parent {
            Some(id) => {
                let revision = self.load_revision(id)?;
                let (exact, portable, prefix) = self.load_tree(&revision.tree)?;
                (
                    Some(exact),
                    Some(portable),
                    Some(prefix),
                    vec![id.to_owned()],
                )
            }
            None => (
                Some(stage.empty_root(Namespace::Exact)?),
                Some(stage.empty_root(Namespace::Portable)?),
                Some(stage.empty_root(Namespace::Prefix)?),
                Vec::new(),
            ),
        };
        let tree = stage.apply_tree_changes(exact_root, portable_root, prefix_root, changes)?;
        stage.flush(&[&tree])?;
        self.publish(encode_revision(&Revision {
            version: 1,
            parents,
            tree,
        })?)
    }

    /// Publishes exactly one R1 revision built from several change batches.
    ///
    /// Each batch obeys the single-checkpoint limits, and a path may appear in
    /// only one batch. Intermediate trees are never referenced by a revision,
    /// so a partial result is not visible as complete. If a later batch fails,
    /// blobs and trees written by earlier batches remain as unreferenced
    /// orphans until the operator removes the store. One batch yields the same
    /// revision ID as `checkpoint`; zero batches yield an empty change set.
    pub fn checkpoint_batches<'a>(
        &self,
        parent: Option<&str>,
        batches: impl IntoIterator<Item = &'a [FileChange]>,
    ) -> Result<String> {
        self.checkpoint_batches_with_progress(parent, batches, &mut |_, _| {})
    }

    /// [`Engine::checkpoint_batches`], calling `on_batch(done, total)` once
    /// after each batch is durably flushed: exactly `total` calls on success.
    pub fn checkpoint_batches_with_progress<'a>(
        &self,
        parent: Option<&str>,
        batches: impl IntoIterator<Item = &'a [FileChange]>,
        on_batch: &mut dyn FnMut(usize, usize),
    ) -> Result<String> {
        let batches: Vec<&[FileChange]> = batches.into_iter().collect();
        let mut validated = Vec::with_capacity(batches.len());
        for batch in &batches {
            validated.push(validate_checkpoint_changes(batch)?);
        }
        self.checkpoint_validated_batches(parent, validated, on_batch)
    }

    /// [`Engine::checkpoint_batches_with_progress`] for explicit entry kinds.
    pub fn checkpoint_entry_batches_with_progress<'a>(
        &self,
        parent: Option<&str>,
        batches: impl IntoIterator<Item = &'a [EntryChange]>,
        on_batch: &mut dyn FnMut(usize, usize),
    ) -> Result<String> {
        let batches: Vec<&[EntryChange]> = batches.into_iter().collect();
        let mut validated = Vec::with_capacity(batches.len());
        for batch in &batches {
            validated.push(validate_entry_changes(batch)?);
        }
        self.checkpoint_validated_batches(parent, validated, on_batch)
    }

    fn checkpoint_validated_batches(
        &self,
        parent: Option<&str>,
        validated: Vec<Vec<TreeChange<'_>>>,
        on_batch: &mut dyn FnMut(usize, usize),
    ) -> Result<String> {
        let mut seen = BTreeSet::new();
        for change in validated.iter().flatten() {
            if !seen.insert(change.path) {
                return Err(Error::Invalid(format!(
                    "path appears in more than one batch: {}",
                    change.path
                )));
            }
        }
        // One stage per batch bounds memory; each batch is flushed before the
        // next one re-reads its tree from disk.
        let mut stage = self.staged();
        let (mut roots, parents) = match parent {
            Some(id) => {
                let revision = self.load_revision(id)?;
                (self.load_tree(&revision.tree)?, vec![id.to_owned()])
            }
            None => (
                (
                    stage.empty_root(Namespace::Exact)?,
                    stage.empty_root(Namespace::Portable)?,
                    stage.empty_root(Namespace::Prefix)?,
                ),
                Vec::new(),
            ),
        };
        let mut tree = None;
        let total = validated.len();
        for (index, batch) in validated.iter().enumerate() {
            let (exact, portable, prefix) = roots;
            let id = stage.apply_tree_changes(Some(exact), Some(portable), Some(prefix), batch)?;
            stage.flush(&[&id])?;
            on_batch(index + 1, total);
            roots = self.load_tree(&id)?;
            tree = Some(id);
            stage = self.staged();
        }
        let tree = match tree {
            Some(id) => id,
            None => {
                let id =
                    stage.apply_tree_changes(Some(roots.0), Some(roots.1), Some(roots.2), &[])?;
                stage.flush(&[&id])?;
                id
            }
        };
        self.publish(encode_revision(&Revision {
            version: 1,
            parents,
            tree,
        })?)
    }

    /// Reuses a complete capture only after full source qualification.
    ///
    /// The reconciliation caller must supply its complete bounded diff and
    /// readbacks from `parent` to the fresh parentless `snapshot`. Batches retain
    /// the ordinary checkpoint validation even when their tree writes are skipped.
    /// Qualification uses a private aggregate read budget. A qualification limit
    /// selects the existing builder; incomplete qualification never authorizes reuse.
    pub(crate) fn checkpoint_captured_batches<'a>(
        &self,
        parent: &str,
        snapshot: &str,
        batches: impl IntoIterator<Item = &'a [FileChange]>,
    ) -> Result<String> {
        let batches: Vec<&[FileChange]> = batches.into_iter().collect();
        let mut seen = BTreeSet::new();
        for batch in &batches {
            validate_checkpoint_changes(batch)?;
            for change in *batch {
                if !seen.insert(change.path.as_str()) {
                    return Err(Error::Invalid(format!(
                        "path appears in more than one batch: {}",
                        change.path
                    )));
                }
            }
        }
        // Never replace a caller-owned budget with a fresh allowance.
        if self.read_budget.is_some() {
            return self.checkpoint_batches(Some(parent), batches);
        }
        let captured = self.load_revision(snapshot)?;
        if captured.version != 1 || !captured.parents.is_empty() {
            return Err(Error::Invalid(
                "reconciliation capture must be a parentless R1 revision".into(),
            ));
        }
        let (qualification_source, qualification_budget) =
            self.with_read_budget(MAX_CAPTURE_QUALIFICATION_BYTES);
        let qualification = crate::sync::validate_retained_reader(
            &SourceReader::new(&qualification_source),
            &[parent.to_owned(), snapshot.to_owned()],
        );
        // SourceReader preserves uncertainty for stream errors. The budget's
        // sticky exhaustion flag identifies our own limit without masking other
        // source failures or permitting a partially qualified tree to be reused.
        match qualification_budget.finish(qualification) {
            Ok(()) => {}
            Err(Error::Limit(_)) => {
                return self.checkpoint_batches(Some(parent), batches);
            }
            Err(error) => return Err(error),
        }
        self.publish(encode_revision(&Revision {
            version: 1,
            parents: vec![parent.to_owned()],
            tree: captured.tree,
        })?)
    }

    pub fn checkpoint_with_parents(
        &self,
        parents: &[String],
        changes: &[FileChange],
    ) -> Result<String> {
        validate_v2_parents(parents)?;
        let changes = validate_checkpoint_changes(changes)?;
        self.checkpoint_with_parents_validated(parents, &changes)
    }

    fn checkpoint_with_parents_validated(
        &self,
        parents: &[String],
        changes: &[TreeChange<'_>],
    ) -> Result<String> {
        let mut base_roots = None;
        for parent in parents {
            let revision = self.load_revision(parent)?;
            if base_roots.is_none() {
                base_roots = Some(self.load_tree(&revision.tree)?);
            }
        }
        let (exact, portable, prefix) =
            base_roots.ok_or_else(|| Error::Invalid("R2 revision has no first parent".into()))?;
        let stage = self.staged();
        let tree = stage.apply_tree_changes(Some(exact), Some(portable), Some(prefix), changes)?;
        stage.flush(&[&tree])?;
        self.publish_revision_tree(parents, &tree)
    }

    pub(crate) fn checkpoint_with_blob_changes(
        &self,
        parents: &[String],
        changes: &[BlobChange],
    ) -> Result<String> {
        validate_v2_parents(parents)?;
        let changes = validate_blob_changes(changes)?;

        let mut base_roots = None;
        for parent in parents {
            let revision = self.load_revision(parent)?;
            if base_roots.is_none() {
                base_roots = Some(self.load_tree(&revision.tree)?);
            }
        }
        self.validate_referenced_blobs(&changes)?;

        let (exact, portable, prefix) =
            base_roots.ok_or_else(|| Error::Invalid("R2 revision has no first parent".into()))?;
        let stage = self.staged();
        let tree = stage.apply_tree_changes(Some(exact), Some(portable), Some(prefix), &changes)?;
        stage.flush(&[&tree])?;
        self.publish_revision_tree(parents, &tree)
    }

    pub(crate) fn publish_blob(&self, content: &[u8]) -> Result<String> {
        if content.len() > MAX_FILE_BYTES {
            return Err(Error::Limit(format!(
                "blob content exceeds {MAX_FILE_BYTES} bytes"
            )));
        }
        self.publish(encode_blob(content))
    }

    pub(crate) fn publish_revision_tree(&self, parents: &[String], tree: &str) -> Result<String> {
        validate_v2_parents(parents)?;
        for parent in parents {
            self.load_revision(parent)?;
        }
        let (exact, portable, prefix) = self.load_tree(tree)?;
        self.load_node(&exact, Namespace::Exact)?;
        self.load_node(&portable, Namespace::Portable)?;
        self.load_node(&prefix, Namespace::Prefix)?;
        self.publish(encode_revision(&Revision {
            version: 2,
            parents: parents.to_vec(),
            tree: tree.to_owned(),
        })?)
    }

    fn validate_referenced_blobs(&self, changes: &[TreeChange<'_>]) -> Result<()> {
        let referenced: BTreeSet<&str> = changes
            .iter()
            .filter_map(|change| match change.blob {
                Some(BlobInput::Existing(id)) => Some(id),
                Some(BlobInput::Bytes(_)) | None => None,
            })
            .collect();
        for id in referenced {
            let bytes = self.load_object(id)?;
            validate_blob(&bytes)?;
        }
        Ok(())
    }

    fn apply_tree_changes(
        &self,
        mut exact_root: Option<String>,
        mut portable_root: Option<String>,
        mut prefix_root: Option<String>,
        changes: &[TreeChange<'_>],
    ) -> Result<String> {
        // Apply removals first so case-only renames do not collide with the
        // portable-name index. Ordering within each pass remains canonical.
        let mut ordered: Vec<&TreeChange<'_>> = changes.iter().collect();
        ordered.sort_by(|a, b| {
            a.blob
                .is_some()
                .cmp(&b.blob.is_some())
                .then_with(|| a.path.cmp(b.path))
        });
        for change in ordered {
            let exact_key = key_for(change.path.as_bytes());
            let portable_key = key_for(portable_path(change.path).as_bytes());

            match change.blob {
                Some(input) => {
                    self.validate_file_insertion(
                        exact_root.as_deref(),
                        portable_root.as_deref(),
                        prefix_root.as_deref(),
                        change.path,
                    )?;
                    let existing =
                        self.lookup(exact_root.as_deref(), Namespace::Exact, &exact_key)?;
                    let was_present = existing.is_some();
                    let kind = match (change.kind, existing) {
                        (Some(kind), _) => kind,
                        (None, Some(existing)) if existing.kind == EntryKind::Symlink => {
                            return Err(Error::Conflict(format!(
                                "cannot write file content over a symlink: {}",
                                change.path
                            )));
                        }
                        (None, Some(existing)) => existing.kind,
                        (None, None) => EntryKind::Regular,
                    };
                    // Symlink targets were checked in validate_tree_changes, which
                    // every caller runs; an existing symlink never takes content.
                    let blob = match input {
                        BlobInput::Bytes(content) => Cow::Owned(self.publish_blob(content)?),
                        BlobInput::Existing(id) => Cow::Borrowed(id),
                    };
                    exact_root = Some(
                        self.update(
                            exact_root.as_deref(),
                            Namespace::Exact,
                            exact_key,
                            change.path,
                            Some(blob.as_ref()),
                            kind,
                        )?
                        .expect("insertion returns a root"),
                    );
                    let exact_leaf = self
                        .lookup(exact_root.as_deref(), Namespace::Exact, &exact_key)?
                        .ok_or_else(|| Error::Invalid("new exact leaf is unavailable".into()))?;
                    portable_root = Some(
                        self.update(
                            portable_root.as_deref(),
                            Namespace::Portable,
                            portable_key,
                            change.path,
                            Some(&exact_leaf.object_id),
                            EntryKind::Regular,
                        )?
                        .expect("insertion returns a root"),
                    );
                    if !was_present {
                        prefix_root = self.adjust_prefix_counts(
                            prefix_root.as_deref(),
                            change.path,
                            PrefixChange::Increment,
                        )?;
                    }
                }
                None => {
                    if self
                        .lookup(exact_root.as_deref(), Namespace::Exact, &exact_key)?
                        .is_none()
                    {
                        return Err(Error::NotFound(format!("path: {}", change.path)));
                    }
                    exact_root = self.update(
                        exact_root.as_deref(),
                        Namespace::Exact,
                        exact_key,
                        change.path,
                        None,
                        EntryKind::Regular,
                    )?;
                    portable_root = self.update(
                        portable_root.as_deref(),
                        Namespace::Portable,
                        portable_key,
                        change.path,
                        None,
                        EntryKind::Regular,
                    )?;
                    prefix_root = self.adjust_prefix_counts(
                        prefix_root.as_deref(),
                        change.path,
                        PrefixChange::Decrement,
                    )?;
                }
            }
        }

        let exact = match exact_root {
            Some(id) => id,
            None => self.empty_root(Namespace::Exact)?,
        };
        let portable = match portable_root {
            Some(id) => id,
            None => self.empty_root(Namespace::Portable)?,
        };
        let prefix = match prefix_root {
            Some(id) => id,
            None => self.empty_root(Namespace::Prefix)?,
        };
        self.publish(encode_tree(&exact, &portable, &prefix)?)
    }

    /// Blob bytes at `path`; for a symlink these are the link target bytes.
    /// Use `read_entry` to learn the kind.
    pub fn read_file(&self, revision: &str, path: &str) -> Result<Vec<u8>> {
        ReadObjects::read_file_impl(self, revision, path)
    }

    /// Resolve exact manifest membership without fetching the file's blob.
    /// `None` means the path is absent; missing source objects remain errors.
    pub fn stat_file(&self, revision: &str, path: &str) -> Result<Option<FileEntry>> {
        ReadObjects::stat_file_impl(self, revision, path)
    }

    pub fn read_entry(&self, revision: &str, path: &str) -> Result<(EntryKind, Vec<u8>)> {
        ReadObjects::read_entry_impl(self, revision, path)
    }

    /// Resolve membership before reading a regular blob within a payload allowance.
    /// `None` is a size refusal, never a successful or qualified source observation.
    pub(crate) fn read_regular_file_bounded(
        &self,
        revision: &str,
        path: &str,
        allowance: usize,
    ) -> Result<Option<Vec<u8>>> {
        validate_path(path)?;
        let revision = self.load_revision_impl(revision)?;
        let (exact, _, _) = self.load_tree(&revision.tree)?;
        let key = key_for(path.as_bytes());
        let leaf = self
            .lookup(Some(&exact), Namespace::Exact, &key)?
            .filter(|leaf| leaf.path == path)
            .ok_or_else(|| Error::NotFound(format!("path: {path}")))?;
        require_regular_entry("bounded source read", path, leaf.kind)?;
        self.blob_by_digest_bounded(&leaf.value, allowance)
    }

    /// Refuse a blob outside the caller's payload allowance before allocating
    /// its body. None is an unverified size refusal, never object qualification.
    /// In-bound bytes use one retained FD and full canonical blob/hash checks.
    pub(crate) fn blob_by_digest_bounded(
        &self,
        id: &str,
        allowance: usize,
    ) -> Result<Option<Vec<u8>>> {
        validate_id(id)?;
        let file = File::open(self.object_path(id)).map_err(|error| {
            if error.kind() == ErrorKind::NotFound {
                Error::NotFound(format!("object: {id}"))
            } else {
                Error::Io(error)
            }
        })?;
        let metadata = file.metadata()?;
        if !metadata.is_file() {
            return Err(Error::Invalid("blob object must be a regular file".into()));
        }
        let encoded_length = metadata.len();
        if encoded_length > MAX_OBJECT_BYTES {
            return Err(Error::Limit(format!("object {id} exceeds size limit")));
        }
        const HEADER_BYTES: usize = BLOB_MAGIC.len() + 8;
        if encoded_length < HEADER_BYTES as u64 {
            return Err(Error::Invalid("invalid blob length".into()));
        }
        let mut reader = BudgetedRead {
            inner: file,
            budget: self.read_budget.clone(),
        };
        let mut header = [0u8; HEADER_BYTES];
        reader.read_exact(&mut header)?;
        if !header.starts_with(BLOB_MAGIC) {
            return Err(Error::Invalid("invalid blob encoding".into()));
        }
        let length = u64::from_be_bytes(
            header[BLOB_MAGIC.len()..]
                .try_into()
                .map_err(|_| Error::Invalid("invalid blob header".into()))?,
        );
        if length > MAX_FILE_BYTES as u64 || encoded_length != HEADER_BYTES as u64 + length {
            return Err(Error::Invalid("invalid blob length".into()));
        }
        if length > allowance as u64 {
            return Ok(None);
        }
        self.check_declared_read(length)?;
        let mut payload = vec![0u8; length as usize];
        reader.read_exact(&mut payload)?;
        // A separate stack probe catches growth without growing the payload Vec.
        let mut trailing = [0u8; 1];
        if reader.read(&mut trailing)? != 0 {
            return Err(Error::Invalid("invalid blob length".into()));
        }
        let mut hash = Sha256::new();
        hash.update(header);
        hash.update(&payload);
        if format!("{:x}", hash.finalize()) != id {
            return Err(Error::Invalid(format!("object digest mismatch: {id}")));
        }
        Ok(Some(payload))
    }

    pub fn load_revision(&self, id: &str) -> Result<Revision> {
        ReadObjects::load_revision_impl(self, id)
    }

    pub fn diff(&self, left: &str, right: &str, limit: usize) -> Result<DiffResult> {
        ReadObjects::diff_impl(self, left, right, limit)
    }

    pub fn diff_page(
        &self,
        left_id: &str,
        right_id: &str,
        cursor: Option<&str>,
        limit: usize,
    ) -> Result<DiffPage> {
        ReadObjects::diff_page_impl(self, left_id, right_id, cursor, limit)
    }

    pub fn list_files(
        &self,
        revision_id: &str,
        cursor: Option<&str>,
        limit: usize,
    ) -> Result<FilePage> {
        ReadObjects::list_files_impl(self, revision_id, cursor, limit)
    }

    pub(crate) fn has_verified_object(&self, id: &str) -> Result<bool> {
        validate_id(id)?;
        match fs::metadata(self.object_path(id)) {
            Ok(metadata) if metadata.is_file() => self.load_object(id).map(|_| true),
            Ok(_) => Err(Error::Invalid(format!(
                "object path is occupied by a non-file: {id}"
            ))),
            Err(error) if error.kind() == ErrorKind::NotFound => Ok(false),
            Err(error) => Err(Error::Io(error)),
        }
    }

    pub(crate) fn read_encoded_object(&self, id: &str) -> Result<Vec<u8>> {
        self.load_object(id)
    }

    pub fn read_encoded(&self, id: &str) -> Result<Vec<u8>> {
        ReadObjects::read_encoded_impl(self, id)
    }

    pub(crate) fn inspect_encoded_object(&self, id: &str, bytes: &[u8]) -> Result<EncodedObject> {
        inspect_encoded_object(id, bytes)
    }

    pub(crate) fn install_encoded_object(&self, id: &str, bytes: Vec<u8>) -> Result<()> {
        self.inspect_encoded_object(id, &bytes)?;
        let actual = self.publish(bytes)?;
        if actual != id {
            return Err(Error::Invalid(format!(
                "installed object identity changed: expected {id}, got {actual}"
            )));
        }
        Ok(())
    }

    fn empty_root(&self, namespace: Namespace) -> Result<String> {
        self.publish(encode_node(&Node::Branch {
            namespace,
            depth: 0,
            children: Vec::new(),
        })?)
    }

    fn validate_file_insertion(
        &self,
        exact_root: Option<&str>,
        portable_root: Option<&str>,
        prefix_root: Option<&str>,
        path: &str,
    ) -> Result<()> {
        let portable_key = key_for(portable_path(path).as_bytes());
        if let Some(existing) = self.lookup(portable_root, Namespace::Portable, &portable_key)?
            && existing.path != path
        {
            return Err(Error::Conflict(format!(
                "path collides on portable filesystems: {} and {path}",
                existing.path
            )));
        }
        if self
            .lookup(prefix_root, Namespace::Prefix, &portable_key)?
            .is_some()
        {
            return Err(Error::Conflict(format!(
                "file path has existing descendants: {path}"
            )));
        }

        for prefix in directory_prefixes(path) {
            let key = key_for(portable_path(prefix).as_bytes());
            if let Some(file) = self.lookup(portable_root, Namespace::Portable, &key)? {
                return Err(Error::Conflict(format!(
                    "path ancestor is already a file: {}",
                    file.path
                )));
            }
            if let Some(directory) = self.lookup(prefix_root, Namespace::Prefix, &key)?
                && directory.path != prefix
            {
                return Err(Error::Conflict(format!(
                    "directory spelling collides on portable filesystems: {} and {prefix}",
                    directory.path
                )));
            }
        }

        // An exact lookup catches a theoretical key collision before mutation.
        if let Some(existing) =
            self.lookup(exact_root, Namespace::Exact, &key_for(path.as_bytes()))?
            && existing.path != path
        {
            return Err(Error::Conflict("SHA-256 manifest-key collision".into()));
        }
        Ok(())
    }

    fn adjust_prefix_counts(
        &self,
        root: Option<&str>,
        path: &str,
        change: PrefixChange,
    ) -> Result<Option<String>> {
        let mut owned_root = root.map(str::to_owned);
        for prefix in directory_prefixes(path) {
            let current_root = owned_root.as_deref();
            let key = key_for(portable_path(prefix).as_bytes());
            let existing = self.lookup(current_root, Namespace::Prefix, &key)?;
            let next = match (existing, change) {
                (Some(existing), PrefixChange::Increment) => {
                    if existing.path != prefix {
                        return Err(Error::Conflict(format!(
                            "directory spelling collides on portable filesystems: {} and {prefix}",
                            existing.path
                        )));
                    }
                    Some(
                        parse_prefix_count(&existing.value)?
                            .checked_add(1)
                            .ok_or_else(|| {
                                Error::Limit("directory descendant count overflow".into())
                            })?,
                    )
                }
                (None, PrefixChange::Increment) => Some(1),
                (Some(existing), PrefixChange::Decrement) => {
                    if existing.path != prefix {
                        return Err(Error::Invalid(
                            "directory prefix spelling is inconsistent".into(),
                        ));
                    }
                    match parse_prefix_count(&existing.value)?.checked_sub(1) {
                        Some(0) => None,
                        Some(value) => Some(value),
                        None => {
                            return Err(Error::Invalid(
                                "directory descendant count underflow".into(),
                            ));
                        }
                    }
                }
                (None, PrefixChange::Decrement) => {
                    return Err(Error::Invalid("missing directory descendant count".into()));
                }
            };
            let value = next.map(encode_prefix_count);
            owned_root = self.update(
                current_root,
                Namespace::Prefix,
                key,
                prefix,
                value.as_deref(),
                EntryKind::Regular,
            )?;
        }
        Ok(owned_root)
    }

    fn update(
        &self,
        root: Option<&str>,
        namespace: Namespace,
        key: [u8; ID_BYTES],
        path: &str,
        value: Option<&str>,
        kind: EntryKind,
    ) -> Result<Option<String>> {
        match root {
            None => match value {
                Some(value) => Ok(Some(self.publish(encode_node(&Node::Leaf {
                    namespace,
                    key,
                    path: path.to_owned(),
                    value: value.to_owned(),
                    kind,
                })?)?)),
                None => Ok(None),
            },
            Some(id) => self.update_at(id, namespace, 0, key, path, value, kind),
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn update_at(
        &self,
        id: &str,
        namespace: Namespace,
        minimum_depth: u8,
        key: [u8; ID_BYTES],
        path: &str,
        value: Option<&str>,
        kind: EntryKind,
    ) -> Result<Option<String>> {
        match self.load_node(id, namespace)? {
            Node::Leaf {
                key: old_key,
                path: old_path,
                value: old_value,
                kind: old_kind,
                ..
            } => {
                if old_key == key {
                    if old_path != path {
                        return Err(Error::Conflict("SHA-256 manifest-key collision".into()));
                    }
                    return match value {
                        Some(new_value) if new_value == old_value && kind == old_kind => {
                            Ok(Some(id.to_owned()))
                        }
                        Some(new_value) => Ok(Some(self.publish(encode_node(&Node::Leaf {
                            namespace,
                            key,
                            path: path.to_owned(),
                            value: new_value.to_owned(),
                            kind,
                        })?)?)),
                        None => Ok(None),
                    };
                }
                let new_value = match value {
                    Some(value) => value,
                    None => return Ok(Some(id.to_owned())),
                };
                let divergence = first_differing_nibble(&old_key, &key, minimum_depth as usize)
                    .ok_or_else(|| Error::Conflict("SHA-256 manifest-key collision".into()))?;
                let old_slot = nibble_at(&old_key, divergence);
                let new_slot = nibble_at(&key, divergence);
                let new_leaf = self.publish(encode_node(&Node::Leaf {
                    namespace,
                    key,
                    path: path.to_owned(),
                    value: new_value.to_owned(),
                    kind,
                })?)?;
                let mut children = vec![(old_slot, id.to_owned()), (new_slot, new_leaf)];
                children.sort_by_key(|(slot, _)| *slot);
                Ok(Some(self.publish(encode_node(&Node::Branch {
                    namespace,
                    depth: divergence as u8,
                    children,
                })?)?))
            }
            Node::Branch {
                depth,
                mut children,
                ..
            } => {
                if depth < minimum_depth || depth as usize >= ID_HEX_LEN {
                    return Err(Error::Invalid("invalid manifest branch depth".into()));
                }
                if depth != 0 {
                    let representative = self.first_key(id, namespace)?;
                    let divergence =
                        first_differing_nibble(&representative, &key, minimum_depth as usize);
                    if divergence.is_some_and(|value| value < depth as usize) {
                        let new_value = match value {
                            Some(value) => value,
                            None => return Ok(Some(id.to_owned())),
                        };
                        let divergence = divergence.expect("checked as present");
                        let old_slot = nibble_at(&representative, divergence);
                        let new_slot = nibble_at(&key, divergence);
                        let leaf = self.publish(encode_node(&Node::Leaf {
                            namespace,
                            key,
                            path: path.to_owned(),
                            value: new_value.to_owned(),
                            kind,
                        })?)?;
                        let mut children = vec![(old_slot, id.to_owned()), (new_slot, leaf)];
                        children.sort_by_key(|(slot, _)| *slot);
                        return Ok(Some(self.publish(encode_node(&Node::Branch {
                            namespace,
                            depth: divergence as u8,
                            children,
                        })?)?));
                    }
                }
                let slot = nibble_at(&key, depth as usize);
                match children.binary_search_by_key(&slot, |(child_slot, _)| *child_slot) {
                    Ok(index) => {
                        let child = self.update_at(
                            &children[index].1,
                            namespace,
                            depth.saturating_add(1),
                            key,
                            path,
                            value,
                            kind,
                        )?;
                        match child {
                            Some(child) => children[index].1 = child,
                            None => {
                                children.remove(index);
                            }
                        }
                    }
                    Err(index) => {
                        if let Some(value) = value {
                            let leaf = self.publish(encode_node(&Node::Leaf {
                                namespace,
                                key,
                                path: path.to_owned(),
                                value: value.to_owned(),
                                kind,
                            })?)?;
                            children.insert(index, (slot, leaf));
                        } else {
                            return Ok(Some(id.to_owned()));
                        }
                    }
                }
                match children.len() {
                    0 => Ok(None),
                    1 if depth != 0 => Ok(Some(children.remove(0).1)),
                    _ => Ok(Some(self.publish(encode_node(&Node::Branch {
                        namespace,
                        depth,
                        children,
                    })?)?)),
                }
            }
        }
    }

    fn load_object(&self, id: &str) -> Result<Vec<u8>> {
        validate_id(id)?;
        if let Some(bytes) = self
            .staged
            .as_ref()
            .and_then(|staged| staged.lock().expect("staged objects lock").get(id).cloned())
        {
            return Ok(bytes);
        }
        let path = self.object_path(id);
        let mut file = File::open(&path).map_err(|error| {
            if error.kind() == ErrorKind::NotFound {
                Error::NotFound(format!("object: {id}"))
            } else {
                Error::Io(error)
            }
        })?;
        let length = file.metadata()?.len();
        if length > MAX_OBJECT_BYTES {
            return Err(Error::Limit(format!("object {id} exceeds size limit")));
        }
        let mut bytes;
        if self.read_budget.is_none() {
            bytes = Vec::with_capacity(length as usize);
            Read::by_ref(&mut file)
                .take(MAX_OBJECT_BYTES + 1)
                .read_to_end(&mut bytes)?;
        } else {
            self.check_declared_read(length)?;
            bytes = Vec::with_capacity(length as usize);
            let mut file = BudgetedRead {
                inner: file,
                budget: self.read_budget.clone(),
            };
            let mut buffer = [0u8; READ_BUDGET_CHUNK];
            loop {
                let remaining = (MAX_OBJECT_BYTES + 1).saturating_sub(bytes.len() as u64);
                let requested = usize::try_from(remaining.min(buffer.len() as u64))
                    .expect("bounded read length fits usize");
                let read = file.read(&mut buffer[..requested])?;
                if read == 0 {
                    break;
                }
                bytes.extend_from_slice(&buffer[..read]);
                if bytes.len() as u64 > MAX_OBJECT_BYTES {
                    break;
                }
            }
        }
        if bytes.len() as u64 > MAX_OBJECT_BYTES {
            return Err(Error::Limit(format!("object {id} exceeds size limit")));
        }
        if digest(&bytes) != id {
            return Err(Error::Invalid(format!("object digest mismatch: {id}")));
        }
        Ok(bytes)
    }

    fn publish(&self, bytes: Vec<u8>) -> Result<String> {
        if bytes.len() as u64 > MAX_OBJECT_BYTES {
            return Err(Error::Limit("encoded object exceeds size limit".into()));
        }
        let id = digest(&bytes);
        if let Some(staged) = &self.staged {
            staged
                .lock()
                .expect("staged objects lock")
                .entry(id.clone())
                .or_insert(bytes);
            return Ok(id);
        }
        let directory = self.shard_directory(&id)?;
        // Always publish the shard directory entry ourselves. Another creator
        // may have made it visible but not yet synced the objects directory.
        sync_directory(&self.objects)?;
        self.link_object(&directory, &id, &bytes)?;
        sync_directory(&directory)?;
        Ok(id)
    }

    fn shard_directory(&self, id: &str) -> Result<PathBuf> {
        let directory = self
            .object_path(id)
            .parent()
            .ok_or_else(|| Error::Invalid("object path has no parent".into()))?
            .to_path_buf();
        fs::create_dir_all(&directory)?;
        if !fs::metadata(&directory)?.is_dir() {
            return Err(Error::Invalid(format!(
                "object shard path is occupied by a non-directory: {}",
                directory.display()
            )));
        }
        Ok(directory)
    }

    /// Makes the object visible at its final path with synced contents. The
    /// caller must sync `directory`. Temporary files are removed on every path.
    fn link_object(&self, directory: &Path, id: &str, bytes: &[u8]) -> Result<()> {
        let path = self.object_path(id);
        if path.exists() {
            return self.verify_existing(id, bytes);
        }
        let (temporary, mut file) = loop {
            let sequence = TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
            let temporary = directory.join(format!(
                ".tmp-{}-{sequence}-{}",
                std::process::id(),
                &id[..12]
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
        let result = (|| -> Result<()> {
            file.write_all(bytes)?;
            file.sync_all()?;
            #[cfg(test)]
            record_sync(SyncEvent::FileSync(id.to_owned()));
            match fs::hard_link(&temporary, &path) {
                Ok(()) => {
                    #[cfg(test)]
                    record_sync(SyncEvent::Link(id.to_owned()));
                    Ok(())
                }
                Err(error) if error.kind() == ErrorKind::AlreadyExists => {
                    self.verify_existing(id, bytes)
                }
                Err(error) => Err(Error::Io(error)),
            }
        })();
        drop(file);
        let _ = fs::remove_file(&temporary);
        result
    }

    fn verify_existing(&self, id: &str, expected: &[u8]) -> Result<()> {
        let path = self.object_path(id);
        let file = File::open(&path)?;
        let length = file.metadata()?.len();
        if length != expected.len() as u64 || length > MAX_OBJECT_BYTES {
            return Err(Error::Conflict(format!(
                "immutable object differs at digest path: {id}"
            )));
        }
        self.check_declared_read(length)?;
        let mut file = BudgetedRead {
            inner: file,
            budget: self.read_budget.clone(),
        };
        let mut hasher = Sha256::new();
        let mut offset = 0usize;
        let mut buffer = [0u8; 64 * 1024];
        loop {
            let read = file.read(&mut buffer)?;
            if read == 0 {
                break;
            }
            if expected.get(offset..offset + read) != Some(&buffer[..read]) {
                return Err(Error::Conflict(format!(
                    "immutable object differs at digest path: {id}"
                )));
            }
            hasher.update(&buffer[..read]);
            offset += read;
        }
        if offset != expected.len() || format!("{:x}", hasher.finalize()) != id {
            return Err(Error::Conflict(format!(
                "immutable object differs at digest path: {id}"
            )));
        }
        Ok(())
    }

    fn check_declared_read(&self, length: u64) -> Result<()> {
        let Some(budget) = &self.read_budget else {
            return Ok(());
        };
        if length > budget.remaining.load(Ordering::Acquire) {
            budget.mark_exceeded();
            return Err(Error::Limit(READ_BUDGET_EXCEEDED.into()));
        }
        Ok(())
    }

    fn object_path(&self, id: &str) -> PathBuf {
        self.objects.join(&id[..2]).join(&id[2..])
    }
}

trait ReadObjects {
    fn load_object(&self, id: &str) -> Result<Vec<u8>>;

    fn read_encoded_impl(&self, id: &str) -> Result<Vec<u8>> {
        let bytes = self.load_object(id)?;
        inspect_encoded_object(id, &bytes)?;
        Ok(bytes)
    }

    fn read_file_impl(&self, revision: &str, path: &str) -> Result<Vec<u8>> {
        Ok(self.read_entry_impl(revision, path)?.1)
    }

    fn stat_file_impl(&self, revision: &str, path: &str) -> Result<Option<FileEntry>> {
        validate_path(path)?;
        let revision = self.load_revision_impl(revision)?;
        let (exact, _, _) = self.load_tree(&revision.tree)?;
        let key = key_for(path.as_bytes());
        Ok(self
            .lookup(Some(&exact), Namespace::Exact, &key)?
            .filter(|leaf| leaf.path == path)
            .map(|leaf| FileEntry {
                path: leaf.path,
                blob: leaf.value,
                kind: leaf.kind,
            }))
    }

    fn read_entry_impl(&self, revision: &str, path: &str) -> Result<(EntryKind, Vec<u8>)> {
        let entry = self
            .stat_file_impl(revision, path)?
            .ok_or_else(|| Error::NotFound(format!("path: {path}")))?;
        Ok((entry.kind, self.load_blob(&entry.blob)?))
    }

    fn load_revision_impl(&self, id: &str) -> Result<Revision> {
        let bytes = self.load_object(id)?;
        let revision = decode_revision(&bytes)?;
        let (exact, portable, prefix) = self.load_tree(&revision.tree)?;
        self.load_node(&exact, Namespace::Exact)?;
        self.load_node(&portable, Namespace::Portable)?;
        self.load_node(&prefix, Namespace::Prefix)?;
        Ok(revision)
    }

    fn diff_impl(&self, left: &str, right: &str, limit: usize) -> Result<DiffResult> {
        let left = self.load_revision_impl(left)?;
        let right = self.load_revision_impl(right)?;
        let (left_root, _, _) = self.load_tree(&left.tree)?;
        let (right_root, _, _) = self.load_tree(&right.tree)?;
        if left_root == right_root {
            return Ok(DiffResult {
                entries: Vec::new(),
                truncated: false,
            });
        }
        if limit == 0 {
            return Ok(DiffResult {
                entries: Vec::new(),
                truncated: true,
            });
        }

        let mut left_files = BTreeMap::new();
        let mut right_files = BTreeMap::new();
        let mut scan = 0usize;
        if !self.collect_changed(
            Some(&left_root),
            Some(&right_root),
            None,
            &mut left_files,
            &mut right_files,
            &mut scan,
        )? {
            return Ok(DiffResult {
                entries: Vec::new(),
                truncated: true,
            });
        }

        let mut paths = BTreeSet::new();
        paths.extend(left_files.keys().cloned());
        paths.extend(right_files.keys().cloned());
        let mut entries = Vec::new();
        let mut truncated = false;
        for path in paths {
            let before = left_files.get(&path).cloned();
            let after = right_files.get(&path).cloned();
            if before != after {
                let before_kind = before.as_ref().and_then(|(_, kind)| diff_kind(*kind));
                let after_kind = after.as_ref().and_then(|(_, kind)| diff_kind(*kind));
                if entries.len() == limit {
                    truncated = true;
                    break;
                }
                entries.push(DiffEntry {
                    path,
                    before: before.map(|(blob, _)| blob),
                    after: after.map(|(blob, _)| blob),
                    before_kind,
                    after_kind,
                });
            }
        }
        Ok(DiffResult { entries, truncated })
    }

    fn diff_page_impl(
        &self,
        left_id: &str,
        right_id: &str,
        cursor: Option<&str>,
        limit: usize,
    ) -> Result<DiffPage> {
        validate_id(left_id)?;
        validate_id(right_id)?;
        if !(1..=MAX_DIFF_PAGE).contains(&limit) {
            return Err(Error::Limit(format!(
                "diff page limit must be between 1 and {MAX_DIFF_PAGE}"
            )));
        }
        let position = match cursor {
            Some(token) if token.starts_with(&hex_encode(DIFF_START_MAGIC)) => {
                decode_diff_start(token, left_id, right_id)?;
                None
            }
            Some(token) => {
                decode_diff_cursor(token, left_id, right_id, None, None)?;
                Some(token)
            }
            None => None,
        };
        match self.diff_page_available(left_id, right_id, position, limit) {
            Ok(page) => Ok(page),
            Err(error) if error.missing_object_id().is_some() => {
                let missing = error
                    .missing_object_id()
                    .expect("guarded as a missing object");
                validate_id(missing)?;
                // The entire page is retried. No partially compared entries or
                // advanced frontier escape when a dependency is unavailable.
                Ok(DiffPage {
                    entries: Vec::new(),
                    needed_objects: vec![missing.to_owned()],
                    next: Some(match cursor {
                        Some(token) => token.to_owned(),
                        None => encode_diff_start(left_id, right_id)?,
                    }),
                })
            }
            Err(error) => Err(error),
        }
    }

    fn diff_page_available(
        &self,
        left_id: &str,
        right_id: &str,
        cursor: Option<&str>,
        limit: usize,
    ) -> Result<DiffPage> {
        let left = self.load_revision_impl(left_id)?;
        let right = self.load_revision_impl(right_id)?;
        let (left_root, _, _) = self.load_tree(&left.tree)?;
        let (right_root, _, _) = self.load_tree(&right.tree)?;
        if left_root == right_root {
            if cursor.is_some() {
                return Err(Error::Conflict(
                    "diff cursor cannot continue an equal-tree diff".into(),
                ));
            }
            return Ok(DiffPage {
                entries: Vec::new(),
                needed_objects: Vec::new(),
                next: None,
            });
        }

        let mut state = match cursor {
            Some(cursor) => decode_diff_cursor(
                cursor,
                left_id,
                right_id,
                Some(&left_root),
                Some(&right_root),
            )?,
            None => DiffCursor {
                phase: DiffPhase::Left,
                frames: vec![FileCursorFrame {
                    object_id: left_root.clone(),
                    next_child: 0,
                }],
            },
        };
        let state_root = match state.phase {
            DiffPhase::Left => &left_root,
            DiffPhase::RightAdditions => &right_root,
        };
        self.validate_file_cursor(&state.frames, state_root)?;

        let mut entries = Vec::with_capacity(limit.min(256));
        let mut scanned = 0usize;
        while entries.len() < limit && scanned < MAX_DIFF_PAGE_SCAN {
            if state.frames.is_empty() {
                match state.phase {
                    DiffPhase::Left => {
                        state.phase = DiffPhase::RightAdditions;
                        state.frames.push(FileCursorFrame {
                            object_id: right_root.clone(),
                            next_child: 0,
                        });
                    }
                    DiffPhase::RightAdditions => break,
                }
            }
            let Some(file) = self.advance_file_traversal(&mut state.frames, &mut scanned)? else {
                continue;
            };
            let key = key_for(file.path.as_bytes());
            match state.phase {
                DiffPhase::Left => {
                    let after = self
                        .lookup(Some(&right_root), Namespace::Exact, &key)?
                        .map(|leaf| {
                            if leaf.path != file.path {
                                return Err(Error::Invalid(
                                    "SHA-256 manifest-key collision".into(),
                                ));
                            }
                            Ok((leaf.value, leaf.kind))
                        })
                        .transpose()?;
                    if after.as_ref() != Some(&(file.blob.clone(), file.kind)) {
                        entries.push(DiffEntry {
                            path: file.path,
                            before: Some(file.blob),
                            after: after.as_ref().map(|(blob, _)| blob.clone()),
                            before_kind: diff_kind(file.kind),
                            after_kind: after.as_ref().and_then(|(_, kind)| diff_kind(*kind)),
                        });
                    }
                }
                DiffPhase::RightAdditions => {
                    let before = self.lookup(Some(&left_root), Namespace::Exact, &key)?;
                    if let Some(before) = before {
                        if before.path != file.path {
                            return Err(Error::Invalid("SHA-256 manifest-key collision".into()));
                        }
                    } else {
                        entries.push(DiffEntry {
                            path: file.path,
                            before: None,
                            after: Some(file.blob),
                            before_kind: None,
                            after_kind: diff_kind(file.kind),
                        });
                    }
                }
            }
        }

        if state.frames.is_empty() && state.phase == DiffPhase::Left {
            state.phase = DiffPhase::RightAdditions;
            state.frames.push(FileCursorFrame {
                object_id: right_root.clone(),
                next_child: 0,
            });
        }
        let next = if state.frames.is_empty() {
            None
        } else {
            Some(encode_diff_cursor(
                left_id,
                right_id,
                &left_root,
                &right_root,
                &state,
            )?)
        };
        Ok(DiffPage {
            entries,
            needed_objects: Vec::new(),
            next,
        })
    }

    fn list_files_impl(
        &self,
        revision_id: &str,
        cursor: Option<&str>,
        limit: usize,
    ) -> Result<FilePage> {
        if !(1..=MAX_FILE_PAGE).contains(&limit) {
            return Err(Error::Limit(format!(
                "file page limit must be between 1 and {MAX_FILE_PAGE}"
            )));
        }
        let revision = self.load_revision_impl(revision_id)?;
        let (exact_root, _, _) = self.load_tree(&revision.tree)?;
        let mut frames = match cursor {
            Some(cursor) => decode_file_cursor(cursor, revision_id, &exact_root)?,
            None => vec![FileCursorFrame {
                object_id: exact_root.clone(),
                next_child: 0,
            }],
        };
        self.validate_file_cursor(&frames, &exact_root)?;

        let mut entries = Vec::with_capacity(limit.min(256));
        let mut scanned = 0usize;
        while !frames.is_empty() && entries.len() < limit && scanned < MAX_FILE_PAGE_SCAN {
            if let Some(entry) = self.advance_file_traversal(&mut frames, &mut scanned)? {
                entries.push(entry);
            }
        }
        let next = if frames.is_empty() {
            None
        } else {
            Some(encode_file_cursor(revision_id, &exact_root, &frames)?)
        };
        Ok(FilePage { entries, next })
    }

    fn advance_file_traversal(
        &self,
        frames: &mut Vec<FileCursorFrame>,
        scanned: &mut usize,
    ) -> Result<Option<FileEntry>> {
        let Some(frame) = frames.last().cloned() else {
            return Ok(None);
        };
        *scanned += 1;
        match self.load_node(&frame.object_id, Namespace::Exact)? {
            Node::Leaf {
                path, value, kind, ..
            } => {
                if frame.next_child != 0 {
                    return Err(Error::Invalid("file cursor leaf state is invalid".into()));
                }
                frames.pop();
                Ok(Some(FileEntry {
                    path,
                    blob: value,
                    kind,
                }))
            }
            Node::Branch {
                depth, children, ..
            } => {
                let next = usize::from(frame.next_child);
                if next > children.len() {
                    return Err(Error::Invalid("file cursor branch state is invalid".into()));
                }
                if next == children.len() {
                    frames.pop();
                } else {
                    let (slot, child) = &children[next];
                    if nibble_at(&self.first_key(child, Namespace::Exact)?, depth as usize) != *slot
                    {
                        return Err(Error::Invalid(
                            "manifest child is stored in the wrong branch slot".into(),
                        ));
                    }
                    frames.last_mut().expect("frame remains present").next_child += 1;
                    frames.push(FileCursorFrame {
                        object_id: child.clone(),
                        next_child: 0,
                    });
                }
                Ok(None)
            }
        }
    }

    fn validate_file_cursor(&self, frames: &[FileCursorFrame], root: &str) -> Result<()> {
        if frames.is_empty() || frames.len() > ID_HEX_LEN + 1 || frames[0].object_id != root {
            return Err(Error::Invalid(
                "file cursor is not rooted in this manifest".into(),
            ));
        }
        let mut parent_depth = None;
        for (index, frame) in frames.iter().enumerate() {
            match self.load_node(&frame.object_id, Namespace::Exact)? {
                Node::Leaf { .. } => {
                    if index + 1 != frames.len() || frame.next_child != 0 {
                        return Err(Error::Invalid("file cursor leaf state is invalid".into()));
                    }
                }
                Node::Branch {
                    depth, children, ..
                } => {
                    if parent_depth.is_some_and(|parent| depth <= parent)
                        || usize::from(frame.next_child) > children.len()
                    {
                        return Err(Error::Invalid("file cursor branch state is invalid".into()));
                    }
                    if let Some(child) = frames.get(index + 1) {
                        let child_index = frame.next_child.checked_sub(1).ok_or_else(|| {
                            Error::Invalid("file cursor child is not reachable".into())
                        })?;
                        let Some((slot, child_id)) = children.get(usize::from(child_index)) else {
                            return Err(Error::Invalid(
                                "file cursor child is not reachable".into(),
                            ));
                        };
                        if child_id != &child.object_id
                            || nibble_at(
                                &self.first_key(child_id, Namespace::Exact)?,
                                depth as usize,
                            ) != *slot
                        {
                            return Err(Error::Invalid(
                                "file cursor child is not reachable".into(),
                            ));
                        }
                    }
                    parent_depth = Some(depth);
                }
            }
        }
        Ok(())
    }

    fn load_tree(&self, id: &str) -> Result<(String, String, String)> {
        let bytes = self.load_object(id)?;
        decode_tree(&bytes)
    }

    fn load_blob(&self, id: &str) -> Result<Vec<u8>> {
        let bytes = self.load_object(id)?;
        decode_blob(&bytes)
    }

    fn load_node(&self, id: &str, namespace: Namespace) -> Result<Node> {
        let bytes = self.load_object(id)?;
        let node = decode_node(&bytes)?;
        let actual = match &node {
            Node::Branch { namespace, .. } | Node::Leaf { namespace, .. } => *namespace,
        };
        if actual != namespace {
            return Err(Error::Invalid("manifest namespace mismatch".into()));
        }
        Ok(node)
    }

    fn lookup(
        &self,
        root: Option<&str>,
        namespace: Namespace,
        key: &[u8; ID_BYTES],
    ) -> Result<Option<FoundLeaf>> {
        let mut current = match root {
            Some(id) => id.to_owned(),
            None => return Ok(None),
        };
        let mut previous_depth = None;
        for _ in 0..=ID_HEX_LEN {
            match self.load_node(&current, namespace)? {
                Node::Leaf {
                    key: leaf_key,
                    path,
                    value,
                    kind,
                    ..
                } => {
                    return Ok((leaf_key == *key).then_some(FoundLeaf {
                        object_id: current,
                        path,
                        value,
                        kind,
                    }));
                }
                Node::Branch {
                    depth, children, ..
                } => {
                    if depth as usize >= ID_HEX_LEN
                        || previous_depth.is_some_and(|prior| depth <= prior)
                    {
                        return Err(Error::Invalid("invalid manifest depth".into()));
                    }
                    previous_depth = Some(depth);
                    let nibble = nibble_at(key, depth as usize);
                    match children.iter().find(|(slot, _)| *slot == nibble) {
                        Some((_, id)) => current = id.clone(),
                        None => return Ok(None),
                    }
                }
            }
        }
        Err(Error::Limit(
            "manifest traversal exceeded depth bound".into(),
        ))
    }

    fn collect_leaves(
        &self,
        id: &str,
        namespace: Namespace,
        parent_depth: Option<u8>,
        output: &mut BTreeMap<String, (String, EntryKind)>,
        scanned: &mut usize,
    ) -> Result<bool> {
        let mut pending = vec![(id.to_owned(), parent_depth)];
        while let Some((current, parent_depth)) = pending.pop() {
            *scanned += 1;
            if *scanned > MAX_DIFF_SCAN {
                return Ok(false);
            }
            match self.load_node(&current, namespace)? {
                Node::Leaf {
                    path, value, kind, ..
                } => {
                    if output.insert(path, (value, kind)).is_some() {
                        return Err(Error::Invalid("duplicate path in manifest".into()));
                    }
                }
                Node::Branch {
                    depth, children, ..
                } => {
                    if depth as usize >= ID_HEX_LEN
                        || parent_depth.is_some_and(|parent| depth <= parent)
                    {
                        return Err(Error::Invalid("invalid manifest depth".into()));
                    }
                    pending.extend(children.into_iter().rev().map(|(_, id)| (id, Some(depth))));
                }
            }
        }
        Ok(true)
    }

    fn first_key(&self, id: &str, namespace: Namespace) -> Result<[u8; ID_BYTES]> {
        let mut current = id.to_owned();
        for _ in 0..=ID_HEX_LEN {
            match self.load_node(&current, namespace)? {
                Node::Leaf { key, .. } => return Ok(key),
                Node::Branch { children, .. } => {
                    current = children
                        .first()
                        .ok_or_else(|| Error::Invalid("empty non-root branch".into()))?
                        .1
                        .clone();
                }
            }
        }
        Err(Error::Limit(
            "manifest traversal exceeded depth bound".into(),
        ))
    }

    fn collect_changed(
        &self,
        left: Option<&str>,
        right: Option<&str>,
        parent_depth: Option<u8>,
        left_files: &mut BTreeMap<String, (String, EntryKind)>,
        right_files: &mut BTreeMap<String, (String, EntryKind)>,
        scanned: &mut usize,
    ) -> Result<bool> {
        if left == right {
            return Ok(true);
        }
        let (Some(left_id), Some(right_id)) = (left, right) else {
            return match (left, right) {
                (Some(id), None) => {
                    self.collect_leaves(id, Namespace::Exact, parent_depth, left_files, scanned)
                }
                (None, Some(id)) => {
                    self.collect_leaves(id, Namespace::Exact, parent_depth, right_files, scanned)
                }
                (None, None) => Ok(true),
                _ => unreachable!(),
            };
        };
        *scanned += 2;
        if *scanned > MAX_DIFF_SCAN {
            return Ok(false);
        }
        let left_node = self.load_node(left_id, Namespace::Exact)?;
        let right_node = self.load_node(right_id, Namespace::Exact)?;
        match (left_node, right_node) {
            (
                Node::Branch {
                    depth: left_depth,
                    children: left_children,
                    ..
                },
                Node::Branch {
                    depth: right_depth,
                    children: right_children,
                    ..
                },
            ) if left_depth == right_depth => {
                if left_depth as usize >= ID_HEX_LEN
                    || parent_depth.is_some_and(|parent| left_depth <= parent)
                {
                    return Err(Error::Invalid("invalid manifest depth".into()));
                }
                let left_by_slot: BTreeMap<u8, String> = left_children.into_iter().collect();
                let right_by_slot: BTreeMap<u8, String> = right_children.into_iter().collect();
                let slots: BTreeSet<u8> = left_by_slot
                    .keys()
                    .chain(right_by_slot.keys())
                    .copied()
                    .collect();
                for slot in slots {
                    if !self.collect_changed(
                        left_by_slot.get(&slot).map(String::as_str),
                        right_by_slot.get(&slot).map(String::as_str),
                        Some(left_depth),
                        left_files,
                        right_files,
                        scanned,
                    )? {
                        return Ok(false);
                    }
                }
                Ok(true)
            }
            (left_node, right_node) => {
                if !self.collect_loaded(left_node, parent_depth, left_files, scanned)? {
                    return Ok(false);
                }
                self.collect_loaded(right_node, parent_depth, right_files, scanned)
            }
        }
    }

    fn collect_loaded(
        &self,
        node: Node,
        parent_depth: Option<u8>,
        output: &mut BTreeMap<String, (String, EntryKind)>,
        scanned: &mut usize,
    ) -> Result<bool> {
        match node {
            Node::Leaf {
                path, value, kind, ..
            } => {
                if output.insert(path, (value, kind)).is_some() {
                    return Err(Error::Invalid("duplicate path in manifest".into()));
                }
                Ok(true)
            }
            Node::Branch {
                depth, children, ..
            } => {
                if depth as usize >= ID_HEX_LEN
                    || parent_depth.is_some_and(|parent| depth <= parent)
                {
                    return Err(Error::Invalid("invalid manifest depth".into()));
                }
                // The branch itself was already counted and loaded by collect_changed.
                for (_, child) in children {
                    if !self.collect_leaves(
                        &child,
                        Namespace::Exact,
                        Some(depth),
                        output,
                        scanned,
                    )? {
                        return Ok(false);
                    }
                }
                Ok(true)
            }
        }
    }
}

impl ReadObjects for Engine {
    fn load_object(&self, id: &str) -> Result<Vec<u8>> {
        Engine::load_object(self, id)
    }
}

impl EncodedObjectReader for Engine {
    fn open_encoded(&self, id: &str) -> Result<Box<dyn Read + '_>> {
        validate_id(id)?;
        File::open(self.object_path(id))
            .map(|file| {
                Box::new(BudgetedRead {
                    inner: file,
                    budget: self.read_budget.clone(),
                }) as Box<dyn Read + '_>
            })
            .map_err(|error| match error.kind() {
                ErrorKind::NotFound => Error::MissingObject(id.to_owned()),
                ErrorKind::PermissionDenied => Error::SourceDenied,
                _ => Error::SourceIndeterminate,
            })
    }
}

impl<'a> SourceReader<'a> {
    pub fn new(reader: &'a dyn EncodedObjectReader) -> Self {
        Self { reader }
    }

    pub fn read_encoded(&self, id: &str) -> Result<Vec<u8>> {
        // Callback loading already checks both digest and encoding.
        ReadObjects::load_object(self, id)
    }

    pub fn load_revision(&self, id: &str) -> Result<Revision> {
        ReadObjects::load_revision_impl(self, id)
    }

    /// Blob bytes at `path`; for a symlink these are the link target bytes.
    /// Use `read_entry` to learn the kind.
    pub fn read_file(&self, revision: &str, path: &str) -> Result<Vec<u8>> {
        ReadObjects::read_file_impl(self, revision, path)
    }

    /// Resolve exact manifest membership without fetching the file's blob.
    /// `None` means the path is absent; missing source objects remain errors.
    pub fn stat_file(&self, revision: &str, path: &str) -> Result<Option<FileEntry>> {
        ReadObjects::stat_file_impl(self, revision, path)
    }

    pub fn read_entry(&self, revision: &str, path: &str) -> Result<(EntryKind, Vec<u8>)> {
        ReadObjects::read_entry_impl(self, revision, path)
    }

    pub fn list_files(
        &self,
        revision_id: &str,
        cursor: Option<&str>,
        limit: usize,
    ) -> Result<FilePage> {
        ReadObjects::list_files_impl(self, revision_id, cursor, limit)
    }

    pub fn diff(&self, left: &str, right: &str, limit: usize) -> Result<DiffResult> {
        ReadObjects::diff_impl(self, left, right, limit)
    }

    pub fn diff_page(
        &self,
        left_id: &str,
        right_id: &str,
        cursor: Option<&str>,
        limit: usize,
    ) -> Result<DiffPage> {
        ReadObjects::diff_page_impl(self, left_id, right_id, cursor, limit)
    }
}

impl ReadObjects for SourceReader<'_> {
    fn load_object(&self, id: &str) -> Result<Vec<u8>> {
        validate_id(id)?;
        let mut encoded = match self.reader.open_encoded(id) {
            Ok(encoded) => encoded,
            Err(Error::MissingObject(missing)) => {
                if validate_id(&missing).is_err() || missing != id {
                    return Err(Error::Invalid(
                        "encoded object reader returned a mismatched missing object ID".into(),
                    ));
                }
                return Err(Error::MissingObject(missing));
            }
            Err(Error::SourceDenied) => return Err(Error::SourceDenied),
            Err(Error::SourceIndeterminate) => return Err(Error::SourceIndeterminate),
            Err(_) => return Err(Error::SourceIndeterminate),
        };
        let mut bytes = Vec::new();
        match Read::by_ref(&mut encoded)
            .take(MAX_OBJECT_BYTES + 1)
            .read_to_end(&mut bytes)
        {
            Ok(_) => {}
            Err(error) if error.kind() == ErrorKind::PermissionDenied => {
                return Err(Error::SourceDenied);
            }
            Err(_) => return Err(Error::SourceIndeterminate),
        }
        if bytes.len() as u64 > MAX_OBJECT_BYTES {
            return Err(Error::Limit(format!("object {id} exceeds size limit")));
        }
        inspect_encoded_object(id, &bytes)?;
        Ok(bytes)
    }
}

impl SourceRead for Engine {
    fn read_encoded(&self, id: &str) -> Result<Vec<u8>> {
        Engine::read_encoded(self, id)
    }

    fn load_revision(&self, id: &str) -> Result<Revision> {
        Engine::load_revision(self, id)
    }

    fn read_file(&self, revision: &str, path: &str) -> Result<Vec<u8>> {
        Engine::read_file(self, revision, path)
    }

    fn list_files(
        &self,
        revision_id: &str,
        cursor: Option<&str>,
        limit: usize,
    ) -> Result<FilePage> {
        Engine::list_files(self, revision_id, cursor, limit)
    }

    fn diff(&self, left: &str, right: &str, limit: usize) -> Result<DiffResult> {
        Engine::diff(self, left, right, limit)
    }

    fn diff_page(
        &self,
        left_id: &str,
        right_id: &str,
        cursor: Option<&str>,
        limit: usize,
    ) -> Result<DiffPage> {
        Engine::diff_page(self, left_id, right_id, cursor, limit)
    }
}

impl SourceRead for SourceReader<'_> {
    fn read_encoded(&self, id: &str) -> Result<Vec<u8>> {
        SourceReader::read_encoded(self, id)
    }

    fn load_revision(&self, id: &str) -> Result<Revision> {
        SourceReader::load_revision(self, id)
    }

    fn read_file(&self, revision: &str, path: &str) -> Result<Vec<u8>> {
        SourceReader::read_file(self, revision, path)
    }

    fn list_files(
        &self,
        revision_id: &str,
        cursor: Option<&str>,
        limit: usize,
    ) -> Result<FilePage> {
        SourceReader::list_files(self, revision_id, cursor, limit)
    }

    fn diff(&self, left: &str, right: &str, limit: usize) -> Result<DiffResult> {
        SourceReader::diff(self, left, right, limit)
    }

    fn diff_page(
        &self,
        left_id: &str,
        right_id: &str,
        cursor: Option<&str>,
        limit: usize,
    ) -> Result<DiffPage> {
        SourceReader::diff_page(self, left_id, right_id, cursor, limit)
    }
}

pub(crate) fn inspect_encoded_object(id: &str, bytes: &[u8]) -> Result<EncodedObject> {
    validate_id(id)?;
    if bytes.len() as u64 > MAX_OBJECT_BYTES {
        return Err(Error::Limit(format!("object {id} exceeds size limit")));
    }
    if digest(bytes) != id {
        return Err(Error::Invalid(format!("object digest mismatch: {id}")));
    }
    if bytes.starts_with(BLOB_MAGIC) {
        validate_blob(bytes)?;
        Ok(EncodedObject::Blob)
    } else if bytes.starts_with(TREE_MAGIC) {
        let (exact, portable, prefix) = decode_tree(bytes)?;
        Ok(EncodedObject::Tree {
            exact,
            portable,
            prefix,
        })
    } else if bytes.starts_with(BRANCH_MAGIC)
        || bytes.starts_with(LEAF_MAGIC)
        || bytes.starts_with(LEAF_V2_MAGIC)
    {
        match decode_node(bytes)? {
            Node::Branch {
                namespace,
                depth,
                children,
            } => Ok(EncodedObject::Branch {
                namespace,
                depth,
                children,
            }),
            Node::Leaf {
                namespace,
                key,
                path,
                value,
                kind,
            } => Ok(EncodedObject::Leaf {
                namespace,
                object_id: id.to_owned(),
                key,
                path,
                value,
                kind,
            }),
        }
    } else if bytes.starts_with(REVISION_V1_MAGIC) || bytes.starts_with(REVISION_V2_MAGIC) {
        Ok(EncodedObject::Revision(decode_revision(bytes)?))
    } else {
        Err(Error::Invalid(format!("unknown object encoding: {id}")))
    }
}

#[derive(Debug)]
struct FoundLeaf {
    object_id: String,
    path: String,
    value: String,
    kind: EntryKind,
}

/// Typed refusal for consumers that cannot represent executable or symlink
/// entries. They must fail rather than flatten the kind to a regular file.
pub fn require_regular_entry(consumer: &str, path: &str, kind: EntryKind) -> Result<()> {
    if kind == EntryKind::Regular {
        return Ok(());
    }
    Err(Error::Invalid(format!(
        "unsupported entry kind: {path} is {}; {consumer} preserves regular files only",
        kind.as_str()
    )))
}

/// Kind recorded on a `DiffEntry` side: only non-regular kinds are named.
fn diff_kind(kind: EntryKind) -> Option<EntryKind> {
    (kind != EntryKind::Regular).then_some(kind)
}

pub fn digest(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

pub fn validate_id(id: &str) -> Result<()> {
    if id.len() != ID_HEX_LEN
        || !id
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
    {
        return Err(Error::Invalid(
            "object id must be 64 lowercase hex characters".into(),
        ));
    }
    Ok(())
}

fn validate_v2_parents(parents: &[String]) -> Result<()> {
    if !(2..=8).contains(&parents.len()) {
        return Err(Error::Invalid(
            "R2 revisions require between 2 and 8 parents".into(),
        ));
    }
    let mut unique = BTreeSet::new();
    for parent in parents {
        validate_id(parent)?;
        if !unique.insert(parent.as_str()) {
            return Err(Error::Invalid("revision parents must be unique".into()));
        }
    }
    Ok(())
}

fn validate_checkpoint_changes(changes: &[FileChange]) -> Result<Vec<TreeChange<'_>>> {
    validate_change_count(changes.len())?;
    let changes: Vec<TreeChange<'_>> = changes
        .iter()
        .map(|change| TreeChange {
            path: &change.path,
            blob: change.content.as_deref().map(BlobInput::Bytes),
            kind: None,
        })
        .collect();
    validate_tree_changes(&changes)?;
    Ok(changes)
}

fn validate_entry_changes(changes: &[EntryChange]) -> Result<Vec<TreeChange<'_>>> {
    validate_change_count(changes.len())?;
    let changes: Vec<TreeChange<'_>> = changes
        .iter()
        .map(|change| TreeChange {
            path: &change.path,
            blob: change.content.as_deref().map(BlobInput::Bytes),
            kind: Some(change.kind),
        })
        .collect();
    validate_tree_changes(&changes)?;
    Ok(changes)
}

fn validate_blob_changes(changes: &[BlobChange]) -> Result<Vec<TreeChange<'_>>> {
    validate_change_count(changes.len())?;
    let changes: Vec<TreeChange<'_>> = changes
        .iter()
        .map(|change| TreeChange {
            path: &change.path,
            blob: change.blob.as_deref().map(BlobInput::Existing),
            kind: None,
        })
        .collect();
    validate_tree_changes(&changes)?;
    Ok(changes)
}

fn validate_tree_changes(changes: &[TreeChange<'_>]) -> Result<()> {
    validate_change_count(changes.len())?;

    let mut paths = BTreeSet::new();
    let mut total_bytes = 0usize;
    let mut total_prefixes = 0usize;
    for change in changes {
        validate_path(change.path)?;
        if !paths.insert(change.path) {
            return Err(Error::Invalid(format!(
                "duplicate changed path: {}",
                change.path
            )));
        }
        total_prefixes = total_prefixes
            .checked_add(change.path.bytes().filter(|byte| *byte == b'/').count())
            .ok_or_else(|| Error::Limit("checkpoint prefix count overflow".into()))?;
        if total_prefixes > MAX_CHECKPOINT_PREFIXES {
            return Err(Error::Limit(format!(
                "checkpoint has more than {MAX_CHECKPOINT_PREFIXES} directory prefixes"
            )));
        }
        match change.blob {
            Some(BlobInput::Bytes(content)) => {
                if content.len() > MAX_FILE_BYTES {
                    return Err(Error::Limit(format!(
                        "file exceeds {MAX_FILE_BYTES} bytes: {}",
                        change.path
                    )));
                }
                if change.kind == Some(EntryKind::Symlink) {
                    validate_symlink_target(change.path, content)?;
                }
                total_bytes = total_bytes
                    .checked_add(content.len())
                    .ok_or_else(|| Error::Limit("checkpoint byte count overflow".into()))?;
                if total_bytes > MAX_CHECKPOINT_BYTES {
                    return Err(Error::Limit(format!(
                        "checkpoint content exceeds {MAX_CHECKPOINT_BYTES} bytes"
                    )));
                }
            }
            Some(BlobInput::Existing(id)) => validate_id(id)?,
            None => {}
        }
    }
    Ok(())
}

fn validate_change_count(count: usize) -> Result<()> {
    if count > MAX_CHANGES {
        return Err(Error::Limit(format!(
            "checkpoint has more than {MAX_CHANGES} changes"
        )));
    }
    Ok(())
}

pub fn validate_path(path: &str) -> Result<()> {
    if path.is_empty() || path.len() > MAX_PATH_BYTES {
        return Err(Error::Invalid(format!(
            "path length must be between 1 and {MAX_PATH_BYTES} bytes"
        )));
    }
    if !path.is_ascii() || path.starts_with('/') || path.contains('\\') || path.contains('\0') {
        return Err(Error::Invalid(
            "path must be relative portable ASCII with '/' separators".into(),
        ));
    }
    for segment in path.split('/') {
        if segment.is_empty() || segment == "." || segment == ".." {
            return Err(Error::Invalid(
                "path contains an empty or dot segment".into(),
            ));
        }
        if segment.len() > 255 {
            return Err(Error::Invalid("path component exceeds 255 bytes".into()));
        }
        if segment.ends_with(['.', ' '])
            || segment.bytes().any(|byte| {
                byte < 0x20 || matches!(byte, b'<' | b'>' | b':' | b'"' | b'|' | b'?' | b'*')
            })
        {
            return Err(Error::Invalid(
                "path is not portable across supported filesystems".into(),
            ));
        }
        let stem = segment
            .split('.')
            .next()
            .unwrap_or(segment)
            .to_ascii_lowercase();
        if matches!(stem.as_str(), "con" | "prn" | "aux" | "nul")
            || (stem.len() == 4
                && (stem.starts_with("com") || stem.starts_with("lpt"))
                && stem.as_bytes()[3].is_ascii_digit()
                && stem.as_bytes()[3] != b'0')
        {
            return Err(Error::Invalid(
                "path contains a reserved device name".into(),
            ));
        }
    }
    Ok(())
}

/// Checks a symlink target for the entry at `link_path`.
///
/// Canonical form: UTF-8, relative, `/`-separated, no empty or `.` components,
/// k leading `..` components (k at most the link's directory depth) and then
/// at least one normal component, with no `..` after a normal component. Every
/// normal component must satisfy the manifest path rules and avoid reserved
/// metadata names. By induction such a target cannot leave the tree root, and
/// the check needs no filesystem access, so dangling targets are accepted.
pub fn validate_symlink_target(link_path: &str, target: &[u8]) -> Result<()> {
    let invalid = |reason: &str| Error::Invalid(format!("symlink target {reason}: {link_path}"));
    if target.is_empty() || target.len() > MAX_SYMLINK_TARGET_BYTES {
        return Err(invalid("length must be between 1 and 1024 bytes"));
    }
    let target = std::str::from_utf8(target).map_err(|_| invalid("is not UTF-8"))?;
    if target.starts_with('/') || target.contains('\\') || target.chars().any(char::is_control) {
        return Err(invalid(
            "must be relative without backslash or control characters",
        ));
    }
    let depth = link_path.matches('/').count();
    let mut parents = 0usize;
    let mut rest = Vec::new();
    for component in target.split('/') {
        match component {
            "" | "." => return Err(invalid("has an empty or dot component")),
            ".." if rest.is_empty() => parents += 1,
            ".." => return Err(invalid("has '..' after a normal component")),
            _ => rest.push(component),
        }
    }
    if parents > depth {
        return Err(invalid("escapes the tree root"));
    }
    if rest.is_empty() {
        return Err(invalid("must name at least one normal component"));
    }
    let remainder = rest.join("/");
    validate_path(&remainder).map_err(|_| invalid("has a non-portable component"))?;
    if crate::git_path_profile::uses_reserved_namespace(&remainder) {
        return Err(invalid("uses a reserved metadata name"));
    }
    Ok(())
}

pub fn portable_path(path: &str) -> String {
    path.to_ascii_lowercase()
}

fn directory_prefixes(path: &str) -> impl Iterator<Item = &str> {
    path.match_indices('/').map(|(index, _)| &path[..index])
}

fn encode_prefix_count(count: u64) -> String {
    format!("{count:064x}")
}

fn parse_prefix_count(value: &str) -> Result<u64> {
    validate_id(value)?;
    if !value[..48].bytes().all(|byte| byte == b'0') {
        return Err(Error::Invalid(
            "directory descendant count is not canonical".into(),
        ));
    }
    let count = u64::from_str_radix(&value[48..], 16)
        .map_err(|_| Error::Invalid("directory descendant count is invalid".into()))?;
    if count == 0 {
        return Err(Error::Invalid(
            "directory descendant count must be positive".into(),
        ));
    }
    Ok(count)
}

fn key_for(bytes: &[u8]) -> [u8; ID_BYTES] {
    Sha256::digest(bytes).into()
}

fn nibble_at(key: &[u8; ID_BYTES], depth: usize) -> u8 {
    let byte = key[depth / 2];
    if depth.is_multiple_of(2) {
        byte >> 4
    } else {
        byte & 0x0f
    }
}

fn first_differing_nibble(
    left: &[u8; ID_BYTES],
    right: &[u8; ID_BYTES],
    start: usize,
) -> Option<usize> {
    (start..ID_HEX_LEN).find(|depth| nibble_at(left, *depth) != nibble_at(right, *depth))
}

fn encode_blob(content: &[u8]) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(BLOB_MAGIC.len() + 8 + content.len());
    bytes.extend_from_slice(BLOB_MAGIC);
    bytes.extend_from_slice(&(content.len() as u64).to_be_bytes());
    bytes.extend_from_slice(content);
    bytes
}

pub(crate) fn decode_blob(bytes: &[u8]) -> Result<Vec<u8>> {
    validate_blob(bytes)?;
    let mut input = Input::new(bytes);
    input.expect(BLOB_MAGIC)?;
    let length = input.u64()?;
    Ok(input.take(length as usize)?.to_vec())
}

fn validate_blob(bytes: &[u8]) -> Result<()> {
    let mut input = Input::new(bytes);
    input.expect(BLOB_MAGIC)?;
    let length = input.u64()?;
    if length > MAX_FILE_BYTES as u64 || length != input.remaining() as u64 {
        return Err(Error::Invalid("invalid blob length".into()));
    }
    Ok(())
}

fn encode_tree(exact: &str, portable: &str, prefix: &str) -> Result<Vec<u8>> {
    let mut bytes = Vec::with_capacity(TREE_MAGIC.len() + ID_BYTES * 3);
    bytes.extend_from_slice(TREE_MAGIC);
    push_id(&mut bytes, exact)?;
    push_id(&mut bytes, portable)?;
    push_id(&mut bytes, prefix)?;
    Ok(bytes)
}

fn decode_tree(bytes: &[u8]) -> Result<(String, String, String)> {
    let mut input = Input::new(bytes);
    input.expect(TREE_MAGIC)?;
    let exact = input.id()?;
    let portable = input.id()?;
    let prefix = input.id()?;
    input.finish()?;
    Ok((exact, portable, prefix))
}

fn encode_revision(revision: &Revision) -> Result<Vec<u8>> {
    let magic = match revision.version {
        1 if revision.parents.len() <= 1 => REVISION_V1_MAGIC,
        2 => {
            validate_v2_parents(&revision.parents)?;
            REVISION_V2_MAGIC
        }
        1 => {
            return Err(Error::Invalid(
                "R1 revisions require at most one parent".into(),
            ));
        }
        _ => return Err(Error::Invalid("unsupported revision encoding".into())),
    };
    let mut bytes = Vec::with_capacity(magic.len() + 6 + ID_BYTES * 9);
    bytes.extend_from_slice(magic);
    bytes.extend_from_slice(&revision.version.to_be_bytes());
    bytes.push(revision.parents.len() as u8);
    for parent in &revision.parents {
        push_id(&mut bytes, parent)?;
    }
    push_id(&mut bytes, &revision.tree)?;
    Ok(bytes)
}

fn decode_revision(bytes: &[u8]) -> Result<Revision> {
    let (magic, expected_version) = if bytes.starts_with(REVISION_V1_MAGIC) {
        (REVISION_V1_MAGIC.as_slice(), 1)
    } else if bytes.starts_with(REVISION_V2_MAGIC) {
        (REVISION_V2_MAGIC.as_slice(), 2)
    } else {
        return Err(Error::Invalid("unknown revision encoding".into()));
    };
    let mut input = Input::new(bytes);
    input.expect(magic)?;
    let version = input.u32()?;
    let parent_count = input.u8()? as usize;
    if version != expected_version
        || (version == 1 && parent_count > 1)
        || (version == 2 && !(2..=8).contains(&parent_count))
    {
        return Err(Error::Invalid("unsupported revision encoding".into()));
    }
    let mut parents = Vec::with_capacity(parent_count);
    let mut unique = BTreeSet::new();
    for _ in 0..parent_count {
        let parent = input.id()?;
        if version == 2 && !unique.insert(parent.clone()) {
            return Err(Error::Invalid("revision parents must be unique".into()));
        }
        parents.push(parent);
    }
    let tree = input.id()?;
    input.finish()?;
    Ok(Revision {
        version,
        parents,
        tree,
    })
}

fn encode_node(node: &Node) -> Result<Vec<u8>> {
    let mut bytes = Vec::new();
    match node {
        Node::Branch {
            namespace,
            depth,
            children,
        } => {
            if *depth as usize >= ID_HEX_LEN || children.len() > 16 {
                return Err(Error::Invalid("invalid branch shape".into()));
            }
            bytes.extend_from_slice(BRANCH_MAGIC);
            bytes.push(*namespace as u8);
            bytes.push(*depth);
            bytes.push(children.len() as u8);
            let mut previous = None;
            for (slot, id) in children {
                if *slot > 15 || previous.is_some_and(|value| *slot <= value) {
                    return Err(Error::Invalid("branch slots are not canonical".into()));
                }
                previous = Some(*slot);
                bytes.push(*slot);
                push_id(&mut bytes, id)?;
            }
        }
        Node::Leaf {
            namespace,
            key,
            path,
            value,
            kind,
        } => {
            validate_path(path)?;
            if *kind != EntryKind::Regular && *namespace != Namespace::Exact {
                return Err(Error::Invalid(
                    "only exact file manifest leaves may carry an entry kind".into(),
                ));
            }
            bytes.extend_from_slice(if *kind == EntryKind::Regular {
                LEAF_MAGIC
            } else {
                LEAF_V2_MAGIC
            });
            bytes.push(*namespace as u8);
            bytes.extend_from_slice(key);
            bytes.extend_from_slice(&(path.len() as u16).to_be_bytes());
            bytes.extend_from_slice(path.as_bytes());
            push_id(&mut bytes, value)?;
            match kind {
                EntryKind::Regular => {}
                EntryKind::Executable => bytes.push(1),
                EntryKind::Symlink => bytes.push(2),
            }
        }
    }
    Ok(bytes)
}

fn decode_node(bytes: &[u8]) -> Result<Node> {
    let mut input = Input::new(bytes);
    if input.consume(BRANCH_MAGIC) {
        let namespace = decode_namespace(input.u8()?)?;
        let depth = input.u8()?;
        let count = input.u8()? as usize;
        if depth as usize >= ID_HEX_LEN || count > 16 {
            return Err(Error::Invalid("invalid branch shape".into()));
        }
        let mut children = Vec::with_capacity(count);
        let mut previous = None;
        for _ in 0..count {
            let slot = input.u8()?;
            if slot > 15 || previous.is_some_and(|value| slot <= value) {
                return Err(Error::Invalid("noncanonical branch slots".into()));
            }
            previous = Some(slot);
            children.push((slot, input.id()?));
        }
        input.finish()?;
        Ok(Node::Branch {
            namespace,
            depth,
            children,
        })
    } else if input.consume(LEAF_MAGIC) || input.consume(LEAF_V2_MAGIC) {
        let versioned = bytes.starts_with(LEAF_V2_MAGIC);
        let namespace = decode_namespace(input.u8()?)?;
        let mut key = [0u8; ID_BYTES];
        key.copy_from_slice(input.take(ID_BYTES)?);
        let path_length = input.u16()? as usize;
        if path_length > MAX_PATH_BYTES {
            return Err(Error::Invalid("manifest path exceeds limit".into()));
        }
        let path = std::str::from_utf8(input.take(path_length)?)
            .map_err(|_| Error::Invalid("manifest path is not UTF-8".into()))?
            .to_owned();
        validate_path(&path)?;
        let expected_key = match namespace {
            Namespace::Exact => key_for(path.as_bytes()),
            Namespace::Portable | Namespace::Prefix => key_for(portable_path(&path).as_bytes()),
        };
        if key != expected_key {
            return Err(Error::Invalid(
                "manifest leaf key does not match its path".into(),
            ));
        }
        let value = input.id()?;
        if namespace == Namespace::Prefix {
            parse_prefix_count(&value)?;
        }
        let kind = if versioned {
            if namespace != Namespace::Exact {
                return Err(Error::Invalid(
                    "only exact file manifest leaves may carry an entry kind".into(),
                ));
            }
            match input.u8()? {
                1 => EntryKind::Executable,
                2 => EntryKind::Symlink,
                _ => return Err(Error::Invalid("unknown or redundant entry kind".into())),
            }
        } else {
            EntryKind::Regular
        };
        input.finish()?;
        Ok(Node::Leaf {
            namespace,
            key,
            path,
            value,
            kind,
        })
    } else {
        Err(Error::Invalid("unknown manifest object encoding".into()))
    }
}

fn decode_namespace(byte: u8) -> Result<Namespace> {
    match byte {
        1 => Ok(Namespace::Exact),
        2 => Ok(Namespace::Portable),
        3 => Ok(Namespace::Prefix),
        _ => Err(Error::Invalid("unknown manifest namespace".into())),
    }
}

fn push_id(output: &mut Vec<u8>, id: &str) -> Result<()> {
    validate_id(id)?;
    for pair in id.as_bytes().chunks_exact(2) {
        let text = std::str::from_utf8(pair)
            .map_err(|_| Error::Invalid("object id is not ASCII".into()))?;
        output.push(
            u8::from_str_radix(text, 16)
                .map_err(|_| Error::Invalid("object id is not hexadecimal".into()))?,
        );
    }
    Ok(())
}

fn encode_file_cursor(
    revision: &str,
    exact_root: &str,
    frames: &[FileCursorFrame],
) -> Result<String> {
    if frames.is_empty() || frames.len() > ID_HEX_LEN + 1 {
        return Err(Error::Invalid("file cursor frame count is invalid".into()));
    }
    let mut bytes = Vec::with_capacity(
        FILE_CURSOR_MAGIC.len() + ID_BYTES * 2 + 2 + frames.len() * (ID_BYTES + 1) + ID_BYTES,
    );
    bytes.extend_from_slice(FILE_CURSOR_MAGIC);
    push_id(&mut bytes, revision)?;
    push_id(&mut bytes, exact_root)?;
    bytes.extend_from_slice(&(frames.len() as u16).to_be_bytes());
    for frame in frames {
        push_id(&mut bytes, &frame.object_id)?;
        bytes.push(frame.next_child);
    }
    let checksum = Sha256::digest(&bytes);
    bytes.extend_from_slice(&checksum);
    if bytes.len() > MAX_FILE_CURSOR_BYTES / 2 {
        return Err(Error::Limit("file cursor exceeds size limit".into()));
    }
    Ok(hex_encode(&bytes))
}

fn decode_file_cursor(
    token: &str,
    expected_revision: &str,
    expected_root: &str,
) -> Result<Vec<FileCursorFrame>> {
    if token.len() > MAX_FILE_CURSOR_BYTES || !token.len().is_multiple_of(2) {
        return Err(Error::Limit("file cursor exceeds size limit".into()));
    }
    let bytes = hex_decode(token)?;
    if bytes.len() < FILE_CURSOR_MAGIC.len() + ID_BYTES * 3 + 2 {
        return Err(Error::Invalid("file cursor is truncated".into()));
    }
    let payload_length = bytes.len() - ID_BYTES;
    let (payload, checksum) = bytes.split_at(payload_length);
    if Sha256::digest(payload).as_slice() != checksum {
        return Err(Error::Invalid("file cursor checksum mismatch".into()));
    }
    let mut input = Input::new(payload);
    input.expect(FILE_CURSOR_MAGIC)?;
    let revision = input.id()?;
    let exact_root = input.id()?;
    if revision != expected_revision || exact_root != expected_root {
        return Err(Error::Conflict(
            "file cursor does not match the requested revision".into(),
        ));
    }
    let count = input.u16()? as usize;
    if count == 0 || count > ID_HEX_LEN + 1 {
        return Err(Error::Invalid("file cursor frame count is invalid".into()));
    }
    let mut frames = Vec::with_capacity(count);
    for _ in 0..count {
        frames.push(FileCursorFrame {
            object_id: input.id()?,
            next_child: input.u8()?,
        });
    }
    input.finish()?;
    Ok(frames)
}

fn encode_diff_start(left: &str, right: &str) -> Result<String> {
    let mut bytes = DIFF_START_MAGIC.to_vec();
    push_id(&mut bytes, left)?;
    push_id(&mut bytes, right)?;
    let checksum = Sha256::digest(&bytes);
    bytes.extend_from_slice(&checksum);
    Ok(hex_encode(&bytes))
}

fn decode_diff_start(token: &str, left: &str, right: &str) -> Result<()> {
    if token.len() != (DIFF_START_MAGIC.len() + ID_BYTES * 3) * 2 {
        return Err(Error::Invalid("diff start cursor length is invalid".into()));
    }
    let bytes = hex_decode(token)?;
    let (payload, checksum) = bytes.split_at(bytes.len() - ID_BYTES);
    if Sha256::digest(payload).as_slice() != checksum {
        return Err(Error::Invalid("diff start cursor checksum mismatch".into()));
    }
    let mut input = Input::new(payload);
    input.expect(DIFF_START_MAGIC)?;
    if input.id()? != left || input.id()? != right {
        return Err(Error::Conflict(
            "diff start cursor does not match requested pair".into(),
        ));
    }
    input.finish()
}

fn encode_diff_cursor(
    left_revision: &str,
    right_revision: &str,
    left_root: &str,
    right_root: &str,
    state: &DiffCursor,
) -> Result<String> {
    if state.frames.is_empty() || state.frames.len() > ID_HEX_LEN + 1 {
        return Err(Error::Invalid("diff cursor frame count is invalid".into()));
    }
    let mut bytes = Vec::with_capacity(
        DIFF_CURSOR_MAGIC.len() + ID_BYTES * 4 + 3 + state.frames.len() * (ID_BYTES + 1) + ID_BYTES,
    );
    bytes.extend_from_slice(DIFF_CURSOR_MAGIC);
    push_id(&mut bytes, left_revision)?;
    push_id(&mut bytes, right_revision)?;
    push_id(&mut bytes, left_root)?;
    push_id(&mut bytes, right_root)?;
    bytes.push(state.phase as u8);
    bytes.extend_from_slice(&(state.frames.len() as u16).to_be_bytes());
    for frame in &state.frames {
        push_id(&mut bytes, &frame.object_id)?;
        bytes.push(frame.next_child);
    }
    let checksum = Sha256::digest(&bytes);
    bytes.extend_from_slice(&checksum);
    if bytes.len() > MAX_DIFF_CURSOR_BYTES / 2 {
        return Err(Error::Limit("diff cursor exceeds size limit".into()));
    }
    Ok(hex_encode(&bytes))
}

fn decode_diff_cursor(
    token: &str,
    expected_left_revision: &str,
    expected_right_revision: &str,
    expected_left_root: Option<&str>,
    expected_right_root: Option<&str>,
) -> Result<DiffCursor> {
    if token.len() > MAX_DIFF_CURSOR_BYTES || !token.len().is_multiple_of(2) {
        return Err(Error::Limit("diff cursor exceeds size limit".into()));
    }
    let bytes = hex_decode(token)?;
    if bytes.len() < DIFF_CURSOR_MAGIC.len() + ID_BYTES * 5 + 3 {
        return Err(Error::Invalid("diff cursor is truncated".into()));
    }
    let payload_length = bytes.len() - ID_BYTES;
    let (payload, checksum) = bytes.split_at(payload_length);
    if Sha256::digest(payload).as_slice() != checksum {
        return Err(Error::Invalid("diff cursor checksum mismatch".into()));
    }
    let mut input = Input::new(payload);
    input.expect(DIFF_CURSOR_MAGIC)?;
    let left_revision = input.id()?;
    let right_revision = input.id()?;
    let left_root = input.id()?;
    let right_root = input.id()?;
    if left_revision != expected_left_revision
        || right_revision != expected_right_revision
        || expected_left_root.is_some_and(|root| left_root != root)
        || expected_right_root.is_some_and(|root| right_root != root)
    {
        return Err(Error::Conflict(
            "diff cursor does not match the requested revision pair".into(),
        ));
    }
    let phase = match input.u8()? {
        1 => DiffPhase::Left,
        2 => DiffPhase::RightAdditions,
        _ => return Err(Error::Invalid("diff cursor phase is invalid".into())),
    };
    let count = input.u16()? as usize;
    if count == 0 || count > ID_HEX_LEN + 1 {
        return Err(Error::Invalid("diff cursor frame count is invalid".into()));
    }
    let mut frames = Vec::with_capacity(count);
    for _ in 0..count {
        frames.push(FileCursorFrame {
            object_id: input.id()?,
            next_child: input.u8()?,
        });
    }
    input.finish()?;
    Ok(DiffCursor { phase, frames })
}

fn hex_encode(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        output.push(HEX[(byte >> 4) as usize] as char);
        output.push(HEX[(byte & 0x0f) as usize] as char);
    }
    output
}

fn hex_decode(value: &str) -> Result<Vec<u8>> {
    let mut bytes = Vec::with_capacity(value.len() / 2);
    let mut input = value.bytes();
    while let Some(high) = input.next() {
        let low = input
            .next()
            .ok_or_else(|| Error::Invalid("file cursor has odd-length hex".into()))?;
        bytes.push((hex_nibble(high)? << 4) | hex_nibble(low)?);
    }
    Ok(bytes)
}

fn hex_nibble(byte: u8) -> Result<u8> {
    match byte {
        b'0'..=b'9' => Ok(byte - b'0'),
        b'a'..=b'f' => Ok(byte - b'a' + 10),
        _ => Err(Error::Invalid(
            "file cursor must use lowercase hexadecimal".into(),
        )),
    }
}

// Windows cannot open a directory as a file; NTFS journals renames, so there is nothing to sync.
fn sync_directory(path: &Path) -> Result<()> {
    #[cfg(unix)]
    File::open(path)?.sync_all()?;
    #[cfg(not(unix))]
    let _ = path;
    #[cfg(test)]
    record_sync(SyncEvent::DirSync(path.to_path_buf()));
    Ok(())
}

pub(crate) fn create_directory_durable(path: impl AsRef<Path>) -> Result<PathBuf> {
    let path = if path.as_ref().is_absolute() {
        path.as_ref().to_path_buf()
    } else {
        std::env::current_dir()?.join(path)
    };
    fs::create_dir_all(&path)?;
    if !fs::metadata(&path)?.is_dir() {
        return Err(Error::Invalid(format!(
            "directory path is occupied by a non-directory: {}",
            path.display()
        )));
    }

    // Sync every visible ancestor. This makes a racing opener independent of
    // whether the process that first called mkdir has synced each parent yet.
    let mut current = Some(path.as_path());
    while let Some(directory) = current {
        sync_directory(directory)?;
        current = directory.parent();
    }
    Ok(path)
}

struct Input<'a> {
    bytes: &'a [u8],
    offset: usize,
}

impl<'a> Input<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, offset: 0 }
    }

    fn remaining(&self) -> usize {
        self.bytes.len().saturating_sub(self.offset)
    }

    fn take(&mut self, length: usize) -> Result<&'a [u8]> {
        let end = self
            .offset
            .checked_add(length)
            .ok_or_else(|| Error::Invalid("object length overflow".into()))?;
        if end > self.bytes.len() {
            return Err(Error::Invalid("truncated object".into()));
        }
        let value = &self.bytes[self.offset..end];
        self.offset = end;
        Ok(value)
    }

    fn consume(&mut self, expected: &[u8]) -> bool {
        if self.bytes.get(self.offset..self.offset + expected.len()) == Some(expected) {
            self.offset += expected.len();
            true
        } else {
            false
        }
    }

    fn expect(&mut self, expected: &[u8]) -> Result<()> {
        if self.consume(expected) {
            Ok(())
        } else {
            Err(Error::Invalid("unexpected object encoding".into()))
        }
    }

    fn u8(&mut self) -> Result<u8> {
        Ok(self.take(1)?[0])
    }

    fn u16(&mut self) -> Result<u16> {
        Ok(u16::from_be_bytes(
            self.take(2)?.try_into().expect("fixed length"),
        ))
    }

    fn u32(&mut self) -> Result<u32> {
        Ok(u32::from_be_bytes(
            self.take(4)?.try_into().expect("fixed length"),
        ))
    }

    fn u64(&mut self) -> Result<u64> {
        Ok(u64::from_be_bytes(
            self.take(8)?.try_into().expect("fixed length"),
        ))
    }

    fn id(&mut self) -> Result<String> {
        Ok(self
            .take(ID_BYTES)?
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect())
    }

    fn finish(&self) -> Result<()> {
        if self.remaining() == 0 {
            Ok(())
        } else {
            Err(Error::Invalid("object has trailing bytes".into()))
        }
    }
}

#[cfg(test)]
mod blob_reference_tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn repeated_blob_references_do_not_require_repeated_payload_admission() {
        let data = TempDir::new().unwrap();
        let engine = Engine::open(data.path()).unwrap();
        let base = engine.checkpoint(None, &[]).unwrap();
        let child = engine.checkpoint(Some(&base), &[]).unwrap();
        let bytes = vec![42; 1024 * 1024];
        let blob = engine.publish_blob(&bytes).unwrap();
        // Logical content exceeds the byte-payload checkpoint cap, but one
        // existing blob is admitted once and all changes retain only IDs.
        let changes: Vec<_> = (0..256)
            .map(|index| BlobChange {
                path: format!("file-{index:03}"),
                blob: Some(blob.clone()),
            })
            .collect();
        let revision = engine
            .checkpoint_with_blob_changes(&[base.clone(), child.clone()], &changes)
            .unwrap();
        assert_eq!(
            engine.load_revision(&revision).unwrap().parents,
            [base, child]
        );
        assert_eq!(
            engine
                .list_files(&revision, None, 1000)
                .unwrap()
                .entries
                .len(),
            256
        );
        assert_eq!(engine.read_file(&revision, "file-255").unwrap(), bytes);
    }

    #[test]
    fn reference_checkpoint_rejects_missing_wrong_type_and_corrupt_blob() {
        let data = TempDir::new().unwrap();
        let engine = Engine::open(data.path()).unwrap();
        let base = engine.checkpoint(None, &[]).unwrap();
        let child = engine.checkpoint(Some(&base), &[]).unwrap();
        let parents = [base.clone(), child];
        let apply = |blob: String| {
            engine.checkpoint_with_blob_changes(
                &parents,
                &[BlobChange {
                    path: "file".into(),
                    blob: Some(blob),
                }],
            )
        };
        assert!(apply("f".repeat(64)).is_err());
        assert!(matches!(apply(base), Err(Error::Invalid(_))));
        let blob = engine.publish_blob(b"payload").unwrap();
        fs::write(engine.object_path(&blob), b"damaged").unwrap();
        assert!(apply(blob).is_err());
        assert!(
            engine
                .list_files(&parents[0], None, 1000)
                .unwrap()
                .entries
                .is_empty()
        );
    }

    #[test]
    fn operation_budget_counts_distinct_blob_reads_and_keeps_reference_deduplication() {
        let data = TempDir::new().unwrap();
        let engine = Engine::open(data.path()).unwrap();
        let first = engine.publish_blob(&[1; 128]).unwrap();
        let second = engine.publish_blob(&[2; 128]).unwrap();
        let first_bytes = engine.read_encoded_object(&first).unwrap().len() as u64;
        let second_bytes = engine.read_encoded_object(&second).unwrap().len() as u64;
        let distinct = [
            TreeChange {
                path: "first",
                blob: Some(BlobInput::Existing(&first)),
                kind: None,
            },
            TreeChange {
                path: "second",
                blob: Some(BlobInput::Existing(&second)),
                kind: None,
            },
        ];
        let (scoped, budget) = engine.with_read_budget(first_bytes + second_bytes - 1);
        assert!(matches!(
            budget.finish(scoped.validate_referenced_blobs(&distinct)),
            Err(Error::Limit(_))
        ));

        let repeated: Vec<_> = (0..256)
            .map(|_| TreeChange {
                path: "same",
                blob: Some(BlobInput::Existing(&first)),
                kind: None,
            })
            .collect();
        let (scoped, budget) = engine.with_read_budget(first_bytes);
        budget
            .finish(scoped.validate_referenced_blobs(&repeated))
            .unwrap();
    }

    #[test]
    fn operation_budget_covers_mutation_and_existing_object_verification_reads() {
        let data = TempDir::new().unwrap();
        let engine = Engine::open(data.path()).unwrap();
        let base = engine
            .checkpoint(
                None,
                &[FileChange {
                    path: "file".into(),
                    content: Some(b"base".to_vec()),
                }],
            )
            .unwrap();
        let revision = engine.load_revision(&base).unwrap();
        let (exact, _, _) = engine.load_tree(&revision.tree).unwrap();
        let other_blob = engine.publish_blob(b"other").unwrap();
        let (scoped, budget) = engine.with_read_budget(0);
        let mutation = scoped.update(
            Some(&exact),
            Namespace::Exact,
            key_for(b"other"),
            "other",
            Some(&other_blob),
            EntryKind::Regular,
        );
        assert!(matches!(budget.finish(mutation), Err(Error::Limit(_))));

        let content = b"existing object";
        let blob = engine.publish_blob(content).unwrap();
        let encoded_bytes = engine.read_encoded_object(&blob).unwrap().len() as u64;
        let (scoped, budget) = engine.with_read_budget(encoded_bytes);
        budget.finish(scoped.publish_blob(content)).unwrap();
        let (scoped, budget) = engine.with_read_budget(encoded_bytes - 1);
        assert!(matches!(
            budget.finish(scoped.publish_blob(content)),
            Err(Error::Limit(_))
        ));
    }

    #[test]
    fn zero_budget_stream_probes_eof_without_delivering_an_over_budget_byte() {
        let data = TempDir::new().unwrap();
        let engine = Engine::open(data.path()).unwrap();
        let id = "a".repeat(ID_HEX_LEN);
        let path = engine.object_path(&id);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, []).unwrap();

        let (scoped, budget) = engine.with_read_budget(0);
        let mut stream = scoped.open_encoded(&id).unwrap();
        let mut output = [0u8; 8];
        assert_eq!(stream.read(&mut output).unwrap(), 0);
        drop(stream);
        budget.finish(Ok(())).unwrap();

        fs::write(&path, b"x").unwrap();
        let (scoped, budget) = engine.with_read_budget(0);
        let mut stream = scoped.open_encoded(&id).unwrap();
        assert!(stream.read(&mut output).is_err());
        assert_eq!(output, [0; 8]);
        output.fill(7);
        assert!(stream.read(&mut output).is_err());
        assert_eq!(output, [7; 8]);
        drop(stream);
        assert!(matches!(budget.finish(Ok(())), Err(Error::Limit(_))));
    }

    #[test]
    fn open_existing_requires_provisioned_state_and_creates_nothing() {
        fn names(root: &Path) -> BTreeSet<PathBuf> {
            fn walk(dir: &Path, out: &mut BTreeSet<PathBuf>) {
                for entry in fs::read_dir(dir).unwrap() {
                    let path = entry.unwrap().path();
                    out.insert(path.clone());
                    if path.is_dir() {
                        walk(&path, out);
                    }
                }
            }
            let mut out = BTreeSet::new();
            walk(root, &mut out);
            out
        }
        let data = TempDir::new().unwrap();
        let missing = data.path().join("absent");
        assert!(Engine::open_existing(&missing).is_err());
        assert!(!missing.exists());

        let store = data.path().join("store");
        let engine = Engine::open(&store).unwrap();
        let revision = engine.checkpoint(None, &[]).unwrap();
        drop(engine);
        let provisioned = names(&store);
        let engine = Engine::open_existing(&store).unwrap();
        assert!(engine.checkpoint(Some(&revision), &[]).is_ok());
        drop(engine);

        let lock = store.join(".agentscm-local-access.lock");
        let objects = store.join("objects");
        fs::rename(&objects, store.join("moved-objects")).unwrap();
        let before = names(&store);
        assert!(Engine::open_existing(&store).is_err());
        assert_eq!(names(&store), before);
        fs::rename(store.join("moved-objects"), &objects).unwrap();

        fs::remove_file(&lock).unwrap();
        let before = names(&store);
        assert!(Engine::open_existing(&store).is_err());
        assert_eq!(names(&store), before);
        assert!(!lock.exists());
        assert!(provisioned.contains(&objects));
    }
}

#[cfg(test)]
mod bounded_blob_tests {
    use super::*;

    #[test]
    fn bounded_regular_file_checks_membership_and_refuses_before_reading_large_payload() {
        let temp = tempfile::TempDir::new().unwrap();
        let engine = Engine::open(temp.path()).unwrap();
        let revision = engine
            .checkpoint_entries(
                None,
                &[
                    EntryChange {
                        path: "src/large".into(),
                        content: Some(vec![b'x'; 1024 * 1024]),
                        kind: EntryKind::Regular,
                    },
                    EntryChange {
                        path: "src/small".into(),
                        content: Some(b"abc".to_vec()),
                        kind: EntryKind::Regular,
                    },
                    EntryChange {
                        path: "src/link".into(),
                        content: Some(b"large".to_vec()),
                        kind: EntryKind::Symlink,
                    },
                ],
            )
            .unwrap();
        let (scoped, budget) = engine.with_read_budget(64 * 1024);
        assert_eq!(
            scoped
                .read_regular_file_bounded(&revision, "src/large", 1)
                .unwrap(),
            None
        );
        budget.finish(Ok(())).unwrap();
        assert_eq!(
            engine
                .read_regular_file_bounded(&revision, "src/small", 3)
                .unwrap(),
            Some(b"abc".to_vec())
        );
        assert!(matches!(
            engine.read_regular_file_bounded(&revision, "src/missing", 3),
            Err(Error::NotFound(_))
        ));
        assert!(matches!(
            engine.read_regular_file_bounded(&revision, "src/link", 3),
            Err(Error::Invalid(_))
        ));
    }

    #[test]
    fn bounded_blob_exact_zero_and_oversize_use_real_read_budget() {
        let temp = tempfile::TempDir::new().unwrap();
        let engine = Engine::open(temp.path()).unwrap();
        let id = engine.publish(encode_blob(b"abc")).unwrap();
        assert_eq!(
            engine.blob_by_digest_bounded(&id, 3).unwrap(),
            Some(b"abc".to_vec())
        );
        let (header_only, budget) = engine.with_read_budget(17);
        assert_eq!(header_only.blob_by_digest_bounded(&id, 2).unwrap(), None);
        budget.finish(Ok(())).unwrap(); // No body or EOF probe was read for a refusal.
        let (exact, budget) = engine.with_read_budget(20);
        assert_eq!(
            exact.blob_by_digest_bounded(&id, 3).unwrap(),
            Some(b"abc".to_vec())
        );
        budget.finish(Ok(())).unwrap();
        let (short, budget) = engine.with_read_budget(19);
        assert!(short.blob_by_digest_bounded(&id, 3).is_err());
        assert!(matches!(budget.finish(Ok(())), Err(Error::Limit(_))));
        let empty = engine.publish(encode_blob(b"")).unwrap();
        assert_eq!(
            engine.blob_by_digest_bounded(&empty, 0).unwrap(),
            Some(Vec::new())
        );
        assert_eq!(engine.blob_by_digest_bounded(&id, 0).unwrap(), None);
    }

    #[test]
    fn bounded_blob_rejects_missing_nonblob_corrupt_truncated_and_trailing_objects() {
        let temp = tempfile::TempDir::new().unwrap();
        let engine = Engine::open(temp.path()).unwrap();
        assert!(engine.blob_by_digest_bounded("bad-id", 16).is_err());
        assert!(matches!(
            engine.blob_by_digest_bounded(&"a".repeat(64), 16),
            Err(Error::NotFound(_))
        ));
        let nonblob = engine.publish(vec![b'x'; 17]).unwrap();
        assert!(engine.blob_by_digest_bounded(&nonblob, 16).is_err());
        let encoded = encode_blob(b"abc");
        let id = engine.publish(encoded.clone()).unwrap();
        let path = engine.object_path(&id);
        let mut corrupt = encoded.clone();
        *corrupt.last_mut().unwrap() = b'z';
        fs::write(&path, &corrupt).unwrap();
        assert!(matches!(
            engine.blob_by_digest_bounded(&id, 3),
            Err(Error::Invalid(_))
        ));
        // Oversized refusal deliberately does not attest payload integrity.
        assert_eq!(engine.blob_by_digest_bounded(&id, 2).unwrap(), None);
        fs::write(&path, &encoded[..encoded.len() - 1]).unwrap();
        assert!(engine.blob_by_digest_bounded(&id, 3).is_err());
        let mut trailing = encoded;
        trailing.push(0);
        fs::write(&path, &trailing).unwrap();
        assert!(engine.blob_by_digest_bounded(&id, 3).is_err());
        let mut oversized_header = Vec::from(BLOB_MAGIC.as_slice());
        oversized_header.extend_from_slice(&(MAX_FILE_BYTES as u64 + 1).to_be_bytes());
        let invalid = engine.publish(oversized_header).unwrap();
        assert!(engine.blob_by_digest_bounded(&invalid, 0).is_err());
    }
}

#[cfg(test)]
mod captured_checkpoint_tests {
    use super::*;
    use tempfile::TempDir;

    fn change(path: &str, content: Option<&[u8]>) -> FileChange {
        FileChange {
            path: path.into(),
            content: content.map(<[u8]>::to_vec),
        }
    }

    fn object_ids(engine: &Engine) -> BTreeSet<String> {
        fs::read_dir(&engine.objects)
            .unwrap()
            .flat_map(|shard| {
                let shard = shard.unwrap();
                let prefix = shard.file_name().into_string().unwrap();
                fs::read_dir(shard.path()).unwrap().map(move |file| {
                    format!("{prefix}{}", file.unwrap().file_name().to_str().unwrap())
                })
            })
            .collect()
    }

    fn candidate_id(engine: &Engine, parent: &str, snapshot: &str) -> String {
        digest(
            &encode_revision(&Revision {
                version: 1,
                parents: vec![parent.into()],
                tree: engine.load_revision(snapshot).unwrap().tree,
            })
            .unwrap(),
        )
    }

    #[test]
    fn reuse_preserves_capture_and_legacy_files_across_batches_and_publishes_only_the_r1() {
        let data = TempDir::new().unwrap();
        let engine = Engine::open(data.path()).unwrap();
        let baseline = engine
            .checkpoint(None, &[change("old/file", Some(b"old"))])
            .unwrap();
        let first = [change("old/file", None), change("new/file", Some(b"new"))];
        let second = [change("other", Some(&[0, 255]))];
        let snapshot = engine
            .checkpoint_batches(None, [&first[1..], second.as_slice()])
            .unwrap();
        let before = object_ids(&engine);
        let candidate = engine
            .checkpoint_captured_batches(
                &baseline,
                &snapshot,
                [first.as_slice(), second.as_slice()],
            )
            .unwrap();
        let added: BTreeSet<_> = object_ids(&engine).difference(&before).cloned().collect();
        assert_eq!(added, BTreeSet::from([candidate.clone()]));
        let revision = engine.load_revision(&candidate).unwrap();
        assert_eq!(revision.version, 1);
        assert_eq!(revision.parents, std::slice::from_ref(&baseline));
        assert_eq!(revision.tree, engine.load_revision(&snapshot).unwrap().tree);
        assert_eq!(engine.read_file(&candidate, "new/file").unwrap(), b"new");
        assert_eq!(engine.read_file(&candidate, "other").unwrap(), [0, 255]);
        assert!(engine.read_file(&candidate, "old/file").is_err());
        let legacy = engine
            .checkpoint_batches(Some(&baseline), [first.as_slice(), second.as_slice()])
            .unwrap();
        assert_eq!(
            engine
                .checkpoint_captured_batches(
                    &baseline,
                    &snapshot,
                    [first.as_slice(), second.as_slice()],
                )
                .unwrap(),
            candidate
        );
        let merged_baseline = engine
            .publish_revision_tree(
                &[baseline.clone(), snapshot.clone()],
                &engine.load_revision(&baseline).unwrap().tree,
            )
            .unwrap();
        let merged_candidate = engine
            .checkpoint_captured_batches(
                &merged_baseline,
                &snapshot,
                [first.as_slice(), second.as_slice()],
            )
            .unwrap();
        assert_eq!(engine.load_revision(&merged_candidate).unwrap().version, 1);
        assert_eq!(
            engine.load_revision(&merged_candidate).unwrap().parents,
            std::slice::from_ref(&merged_baseline)
        );
        let merged_legacy = engine
            .checkpoint_batches(
                Some(&merged_baseline),
                [first.as_slice(), second.as_slice()],
            )
            .unwrap();
        // Deleting the last entry and reinserting can yield a different valid
        // HAMT root shape than a fresh capture. Reuse preserves the capture's
        // exact encoding; the legacy builder must match its complete file set.
        for (parent, reused, reconstructed) in [
            (&baseline, &candidate, &legacy),
            (&merged_baseline, &merged_candidate, &merged_legacy),
        ] {
            assert_eq!(reused, &candidate_id(&engine, parent, &snapshot));
            let revision = engine.load_revision(reused).unwrap();
            assert_eq!(revision.version, 1);
            assert_eq!(revision.parents, [parent.to_owned()]);
            assert_eq!(revision.tree, engine.load_revision(&snapshot).unwrap().tree);
            let legacy_revision = engine.load_revision(reconstructed).unwrap();
            assert_eq!(legacy_revision.version, 1);
            assert_eq!(legacy_revision.parents, [parent.to_owned()]);
            crate::sync::validate_retained_reader(
                &SourceReader::new(&engine),
                &[reused.to_owned(), reconstructed.to_owned()],
            )
            .unwrap();
            let diff = engine.diff_page(reused, reconstructed, None, 1).unwrap();
            assert!(diff.entries.is_empty());
            assert!(diff.needed_objects.is_empty());
            assert!(diff.next.is_none());
            assert_eq!(engine.read_file(reused, "new/file").unwrap(), b"new");
            assert_eq!(engine.read_file(reused, "other").unwrap(), [0, 255]);
            assert!(engine.read_file(reused, "old/file").is_err());
        }
        assert!(matches!(
            engine.checkpoint_captured_batches(&baseline, &merged_baseline, std::iter::empty()),
            Err(Error::Invalid(_))
        ));
        assert_eq!(
            engine
                .checkpoint_captured_batches(&snapshot, &snapshot, std::iter::empty())
                .unwrap(),
            engine
                .checkpoint_batches(Some(&snapshot), std::iter::empty())
                .unwrap()
        );
    }

    #[test]
    fn missing_or_corrupt_capture_and_baseline_closures_never_publish_a_candidate() {
        let data = TempDir::new().unwrap();
        let engine = Engine::open(data.path()).unwrap();
        let baseline = engine
            .checkpoint(None, &[change("old/file", Some(b"old"))])
            .unwrap();
        let snapshot = engine
            .checkpoint(None, &[change("new/file", Some(b"new"))])
            .unwrap();
        let changes = [change("old/file", None), change("new/file", Some(b"new"))];
        let candidate = candidate_id(&engine, &baseline, &snapshot);
        assert!(!engine.object_path(&candidate).exists());
        for revision in [&baseline, &snapshot] {
            let tree = engine.load_revision(revision).unwrap().tree;
            let (exact, portable, prefix) = engine.load_tree(&tree).unwrap();
            let mut dependencies = vec![(*revision).clone(), tree];
            for (root, namespace) in [
                (exact, Namespace::Exact),
                (portable, Namespace::Portable),
                (prefix, Namespace::Prefix),
            ] {
                dependencies.push(root.clone());
                if let Node::Branch { children, .. } = engine.load_node(&root, namespace).unwrap() {
                    dependencies.extend(children.into_iter().map(|(_, id)| id));
                }
            }
            dependencies.extend(
                engine
                    .list_files(revision, None, 10)
                    .unwrap()
                    .entries
                    .into_iter()
                    .map(|e| e.blob),
            );
            for dependency in dependencies {
                let path = engine.object_path(&dependency);
                let bytes = fs::read(&path).unwrap();
                for corrupt in [false, true] {
                    if corrupt {
                        fs::write(&path, b"damaged").unwrap();
                    } else {
                        fs::remove_file(&path).unwrap();
                    }
                    let before = object_ids(&engine);
                    assert!(
                        engine
                            .checkpoint_captured_batches(&baseline, &snapshot, [changes.as_slice()])
                            .is_err()
                    );
                    assert_eq!(object_ids(&engine), before);
                    assert!(!engine.object_path(&candidate).exists());
                    fs::write(&path, &bytes).unwrap();
                }
            }
        }
    }

    #[test]
    fn valid_hash_but_inconsistent_indexes_and_missing_ancestry_refuse_reuse() {
        let data = TempDir::new().unwrap();
        let engine = Engine::open(data.path()).unwrap();
        let baseline = engine.checkpoint(None, &[]).unwrap();
        let snapshot = engine
            .checkpoint(None, &[change("dir/file", Some(b"new"))])
            .unwrap();
        let tree = engine.load_revision(&snapshot).unwrap().tree;
        let (exact, portable, prefix) = engine.load_tree(&tree).unwrap();
        let invalid_trees = [
            engine
                .publish(
                    encode_tree(
                        &exact,
                        &engine.empty_root(Namespace::Portable).unwrap(),
                        &prefix,
                    )
                    .unwrap(),
                )
                .unwrap(),
            engine
                .publish(
                    encode_tree(
                        &exact,
                        &portable,
                        &engine.empty_root(Namespace::Prefix).unwrap(),
                    )
                    .unwrap(),
                )
                .unwrap(),
        ];
        let changes = [change("dir/file", Some(b"new"))];
        for tree in invalid_trees {
            let invalid = engine
                .publish(
                    encode_revision(&Revision {
                        version: 1,
                        parents: vec![],
                        tree,
                    })
                    .unwrap(),
                )
                .unwrap();
            let candidate = candidate_id(&engine, &baseline, &invalid);
            let before = object_ids(&engine);
            assert!(matches!(
                engine.checkpoint_captured_batches(&baseline, &invalid, [changes.as_slice()]),
                Err(Error::Invalid(_))
            ));
            assert_eq!(object_ids(&engine), before);
            assert!(!engine.object_path(&candidate).exists());
        }
        let descendant = engine
            .publish(
                encode_revision(&Revision {
                    version: 1,
                    parents: vec!["f".repeat(64)],
                    tree: engine.load_revision(&baseline).unwrap().tree,
                })
                .unwrap(),
            )
            .unwrap();
        let candidate = candidate_id(&engine, &descendant, &snapshot);
        let before = object_ids(&engine);
        assert!(matches!(
            engine.checkpoint_captured_batches(&descendant, &snapshot, [changes.as_slice()]),
            Err(Error::MissingObject(_))
        ));
        assert_eq!(object_ids(&engine), before);
        assert!(!engine.object_path(&candidate).exists());
        assert!(matches!(
            engine.checkpoint_captured_batches(&baseline, &descendant, [changes.as_slice()]),
            Err(Error::Invalid(_))
        ));
    }

    #[test]
    fn reuse_still_validates_batch_count_paths_and_prefix_bounds() {
        let data = TempDir::new().unwrap();
        let engine = Engine::open(data.path()).unwrap();
        let baseline = engine.checkpoint(None, &[]).unwrap();
        let snapshot = engine
            .checkpoint(None, &[change("file", Some(b"new"))])
            .unwrap();
        let before = object_ids(&engine);
        let duplicate = [change("file", Some(b"new"))];
        assert!(matches!(
            engine.checkpoint_captured_batches(
                &baseline,
                &snapshot,
                [duplicate.as_slice(), duplicate.as_slice()]
            ),
            Err(Error::Invalid(_))
        ));
        let too_many: Vec<_> = (0..=MAX_CHANGES)
            .map(|n| change(&format!("f{n}"), None))
            .collect();
        assert!(matches!(
            engine.checkpoint_captured_batches(&baseline, &snapshot, [too_many.as_slice()]),
            Err(Error::Limit(_))
        ));
        let invalid = [change("../file", None)];
        assert!(matches!(
            engine.checkpoint_captured_batches(&baseline, &snapshot, [invalid.as_slice()]),
            Err(Error::Invalid(_))
        ));
        let deep = "d/".repeat(500);
        let prefixes: Vec<_> = (0..33)
            .map(|n| change(&format!("{deep}f{n}"), None))
            .collect();
        assert!(matches!(
            engine.checkpoint_captured_batches(&baseline, &snapshot, [prefixes.as_slice()]),
            Err(Error::Limit(_))
        ));
        assert_eq!(object_ids(&engine), before);
    }

    #[test]
    fn aggregate_qualification_budget_falls_back_for_a_valid_small_fixture() {
        let data = TempDir::new().unwrap();
        let engine = Engine::open(data.path()).unwrap();
        // Unit builds use a small private budget; production uses 128 MiB.
        // The complete inputs are valid and the changed payload is tiny, but
        // reading both distinct superseded blobs plus their metadata exceeds
        // the aggregate allowance. Each blob individually is below that cap.
        let historical = vec![7; MAX_CAPTURE_QUALIFICATION_BYTES as usize / 2];
        let other = vec![8; MAX_CAPTURE_QUALIFICATION_BYTES as usize / 2];
        let original = [change("a", Some(&historical)), change("b", Some(&other))];
        let baseline = engine.checkpoint(None, &original).unwrap();
        let changes = [change("a", Some(b"new-a")), change("b", Some(b"new-b"))];
        let snapshot = engine.checkpoint(None, &changes).unwrap();
        let before = object_ids(&engine);
        SYNC_LOG.with(|log| *log.borrow_mut() = Some(Vec::new()));
        let candidate = engine
            .checkpoint_captured_batches(&baseline, &snapshot, [changes.as_slice()])
            .unwrap();
        let log = SYNC_LOG.with(|log| log.borrow_mut().take().unwrap());
        let revision_sync = log
            .iter()
            .position(|e| *e == SyncEvent::FileSync(candidate.clone()))
            .unwrap();
        assert!(
            log[..revision_sync]
                .iter()
                .any(|e| matches!(e, SyncEvent::DirSync(dir) if *dir != engine.objects)),
            "fallback rebuild flushes the tree's shard directories before the revision"
        );
        let added: BTreeSet<_> = object_ids(&engine).difference(&before).cloned().collect();
        // Staging discards the unreachable mixed old/new intermediates, so
        // only the reachable candidate revision is new.
        assert_eq!(added, BTreeSet::from([candidate.clone()]));
        assert!(added.contains(&candidate));
        assert_eq!(
            candidate,
            engine
                .checkpoint_batches(Some(&baseline), [changes.as_slice()])
                .unwrap()
        );
        assert_eq!(
            engine.load_revision(&candidate).unwrap().parents,
            [baseline]
        );
        assert_eq!(
            engine.load_revision(&candidate).unwrap().tree,
            engine.load_revision(&snapshot).unwrap().tree
        );
    }

    #[test]
    fn caller_owned_read_budget_is_not_replaced_by_the_reuse_allowance() {
        let data = TempDir::new().unwrap();
        let engine = Engine::open(data.path()).unwrap();
        let baseline = engine.checkpoint(None, &[]).unwrap();
        let changes = [change("file", Some(b"new"))];
        let snapshot = engine.checkpoint(None, &changes).unwrap();
        let candidate = candidate_id(&engine, &baseline, &snapshot);
        let before = object_ids(&engine);
        let (scoped, budget) = engine.with_read_budget(0);
        assert!(matches!(
            budget.finish(scoped.checkpoint_captured_batches(
                &baseline,
                &snapshot,
                [changes.as_slice()]
            )),
            Err(Error::Limit(_))
        ));
        assert_eq!(object_ids(&engine), before);
        assert!(!engine.object_path(&candidate).exists());
    }

    fn staged_files(count: usize) -> Vec<FileChange> {
        (0..count)
            .map(|index| {
                change(
                    &format!("dir{}/file{index}", index % 5),
                    Some(format!("content {index}").as_bytes()),
                )
            })
            .collect()
    }

    fn reachable_ids(engine: &Engine, root: &str) -> BTreeSet<String> {
        let mut seen = BTreeSet::new();
        let mut stack = vec![root.to_owned()];
        while let Some(id) = stack.pop() {
            if !seen.insert(id.clone()) {
                continue;
            }
            let bytes = engine.load_object(&id).unwrap();
            match inspect_encoded_object(&id, &bytes).unwrap() {
                EncodedObject::Blob => {}
                EncodedObject::Revision(revision) => {
                    stack.push(revision.tree);
                    stack.extend(revision.parents);
                }
                EncodedObject::Tree {
                    exact,
                    portable,
                    prefix,
                } => stack.extend([exact, portable, prefix]),
                EncodedObject::Branch { children, .. } => {
                    stack.extend(children.into_iter().map(|(_, child)| child))
                }
                EncodedObject::Leaf {
                    namespace: Namespace::Prefix,
                    ..
                } => {}
                EncodedObject::Leaf { value, .. } => stack.push(value),
            }
        }
        seen
    }

    fn temporary_files(engine: &Engine) -> usize {
        fs::read_dir(&engine.objects)
            .unwrap()
            .flat_map(|shard| fs::read_dir(shard.unwrap().path()).unwrap())
            .filter(|file| {
                file.as_ref()
                    .unwrap()
                    .file_name()
                    .to_string_lossy()
                    .starts_with(".tmp-")
            })
            .count()
    }

    #[test]
    fn staged_batches_sync_each_directory_once_before_the_revision() {
        let data = TempDir::new().unwrap();
        let engine = Engine::open(data.path()).unwrap();
        let changes = staged_files(60);
        let batches = [&changes[..30], &changes[30..]];
        SYNC_LOG.with(|log| *log.borrow_mut() = Some(Vec::new()));
        let revision = engine.checkpoint_batches(None, batches).unwrap();
        let log = SYNC_LOG.with(|log| log.borrow_mut().take().unwrap());

        let revision_sync = log
            .iter()
            .position(|event| *event == SyncEvent::FileSync(revision.clone()))
            .unwrap();
        for (index, event) in log.iter().enumerate() {
            if let SyncEvent::Link(id) = event {
                let synced = log[..index]
                    .iter()
                    .position(|e| *e == SyncEvent::FileSync(id.clone()));
                assert!(synced.is_some(), "link before file sync: {id}");
            }
        }
        // Split the prefix into flushes: a run of FileSync/Link events
        // followed by that flush's DirSync events.
        // The revision's own publish syncs the objects root just before its
        // file sync; that event belongs to the publish, not to a flush.
        let before = &log[..revision_sync];
        assert_eq!(
            before.last(),
            Some(&SyncEvent::DirSync(engine.objects.clone()))
        );
        let before = &before[..before.len() - 1];
        let mut flushes: Vec<Vec<&SyncEvent>> = Vec::new();
        let mut in_dirs = true;
        for event in before {
            let is_dir = matches!(event, SyncEvent::DirSync(_));
            if in_dirs && !is_dir {
                flushes.push(Vec::new());
            }
            in_dirs = is_dir;
            flushes.last_mut().unwrap().push(event);
        }
        assert!(flushes.len() >= 2, "each batch flushes separately");
        for flush in &flushes {
            let links: Vec<&String> = flush
                .iter()
                .filter_map(|e| match e {
                    SyncEvent::Link(id) => Some(id),
                    _ => None,
                })
                .collect();
            assert!(!links.is_empty());
            let dirs: Vec<&PathBuf> = flush
                .iter()
                .filter_map(|e| match e {
                    SyncEvent::DirSync(dir) => Some(dir),
                    _ => None,
                })
                .collect();
            let last_link = flush
                .iter()
                .rposition(|e| matches!(e, SyncEvent::Link(_)))
                .unwrap();
            let first_dir = flush
                .iter()
                .position(|e| matches!(e, SyncEvent::DirSync(_)))
                .expect("flush syncs directories");
            assert!(last_link < first_dir, "directories sync after all links");
            assert_eq!(
                dirs.iter().filter(|dir| ***dir == engine.objects).count(),
                1,
                "objects root synced once per flush"
            );
            for id in links {
                let shard = engine.object_path(id).parent().unwrap().to_path_buf();
                assert_eq!(shard, engine.objects.join(&id[..2]));
                assert_eq!(
                    dirs.iter().filter(|dir| ***dir == shard).count(),
                    1,
                    "shard of {id} synced exactly once per flush"
                );
            }
            let unique: BTreeSet<_> = dirs.iter().collect();
            assert_eq!(unique.len(), dirs.len(), "no directory synced twice");
        }
    }

    #[test]
    fn directory_sync_failure_publishes_no_revision_and_reruns_identically() {
        let expected_data = TempDir::new().unwrap();
        let changes = staged_files(40);
        let expected = Engine::open(expected_data.path())
            .unwrap()
            .checkpoint_batches(None, [changes.as_slice()])
            .unwrap();

        let data = TempDir::new().unwrap();
        let engine = Engine::open(data.path()).unwrap();
        FLUSH_DIR_SYNC_FAIL.with(|fail| fail.set(true));
        let error = engine
            .checkpoint_batches(None, [changes.as_slice()])
            .unwrap_err();
        assert!(matches!(error, Error::Io(_)));
        assert!(!engine.object_path(&expected).exists());
        assert_eq!(temporary_files(&engine), 0);
        SYNC_LOG.with(|log| *log.borrow_mut() = Some(Vec::new()));
        assert_eq!(
            engine
                .checkpoint_batches(None, [changes.as_slice()])
                .unwrap(),
            expected
        );
        let log = SYNC_LOG.with(|log| log.borrow_mut().take().unwrap());
        // Every object is already linked, yet its unsynced shard is still synced.
        let revision_sync = log
            .iter()
            .position(|e| *e == SyncEvent::FileSync(expected.clone()))
            .unwrap();
        assert!(
            log[..revision_sync]
                .iter()
                .any(|e| matches!(e, SyncEvent::DirSync(dir) if *dir != engine.objects))
        );
        assert_eq!(object_ids(&engine), reachable_ids(&engine, &expected));
    }

    #[test]
    fn staged_batches_publish_a_closed_revision_without_garbage() {
        let data = TempDir::new().unwrap();
        let engine = Engine::open(data.path()).unwrap();
        let changes = staged_files(60);
        let batched = engine
            .checkpoint_batches(None, [&changes[..30], &changes[30..]])
            .unwrap();
        crate::sync::validate_retained_reader(
            &SourceReader::new(&engine),
            std::slice::from_ref(&batched),
        )
        .unwrap();
        // The first batch's superseded tree objects remain as orphans.
        assert!(reachable_ids(&engine, &batched).is_subset(&object_ids(&engine)));
        assert_eq!(temporary_files(&engine), 0);

        let other = TempDir::new().unwrap();
        let single = Engine::open(other.path()).unwrap();
        assert_eq!(single.checkpoint(None, &changes).unwrap(), batched);
        assert_eq!(
            single
                .checkpoint_batches(None, [changes.as_slice()])
                .unwrap(),
            batched
        );
        let all = object_ids(&single);
        assert_eq!(all, reachable_ids(&single, &batched));
    }

    #[test]
    fn interrupted_flush_leaves_no_revision_or_temporaries_and_reruns_identically() {
        let expected_data = TempDir::new().unwrap();
        let changes = staged_files(40);
        let expected = Engine::open(expected_data.path())
            .unwrap()
            .checkpoint_batches(None, [changes.as_slice()])
            .unwrap();

        let data = TempDir::new().unwrap();
        let engine = Engine::open(data.path()).unwrap();
        FLUSH_FAIL_AFTER.with(|remaining| remaining.set(Some(5)));
        let error = engine
            .checkpoint_batches(None, [changes.as_slice()])
            .unwrap_err();
        assert!(matches!(error, Error::Io(_)));
        assert!(!engine.object_path(&expected).exists());
        assert_eq!(temporary_files(&engine), 0);
        assert_eq!(
            engine
                .checkpoint_batches(None, [changes.as_slice()])
                .unwrap(),
            expected
        );
        assert_eq!(object_ids(&engine), reachable_ids(&engine, &expected));
    }
}
