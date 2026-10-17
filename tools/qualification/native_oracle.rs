//! Independent, offline observation of the documented draft native source wire.
//!
//! This module validates a raw, caller-owned object map and follows the complete
//! reference closure of requested revision roots. It is deliberately not a full
//! native qualification implementation: the unfrozen v7 context rules listed in
//! [`PendingOracleWork`] are never guessed.

use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::convert::TryInto;

pub type ObjectId = [u8; 32];

const MAGIC_BLOB: &[u8; 9] = b"AGSCM-B1\0";
const MAGIC_TREE: &[u8; 9] = b"AGSCM-T2\0";
const MAGIC_REVISION_1: &[u8; 9] = b"AGSCM-R1\0";
const MAGIC_REVISION_2: &[u8; 9] = b"AGSCM-R2\0";
const MAGIC_BRANCH: &[u8; 9] = b"AGSCM-N1\0";
const MAGIC_LEAF: &[u8; 9] = b"AGSCM-L1\0";
const MAGIC_LEAF_2: &[u8; 9] = b"AGSCM-L2\0";
const MAX_BLOB_BYTES: usize = 64 * 1024 * 1024;
const WIRE_MAX_OBJECT_BYTES: usize = MAX_BLOB_BYTES + 4096;
const MAX_ROOTS: usize = 256;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd)]
pub enum Namespace {
    Exact,
    Portable,
    Prefix,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SourceProfile {
    Profile1,
    Profile2,
    Profile3,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ObjectKind {
    Blob,
    Tree,
    Revision1,
    Revision2,
    Branch(Namespace),
    Leaf(Namespace),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReachableDescriptor {
    pub id: ObjectId,
    pub encoded_len: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RevisionNode {
    pub version: u32,
    /// Wire order is identity-bearing and is preserved exactly here.
    pub parents: Vec<ObjectId>,
    pub tree: ObjectId,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PendingOracleWork {
    NativeWireOwnerConfirmation,
    PortableIndexComparison,
    ContextualTriePlacement,
    CrossIndexEquality,
    DerivedPrefixCounts,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ObservationCompleteness {
    Incomplete {
        pending_checks: Vec<PendingOracleWork>,
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AvailableVerdicts {
    pub requested_roots_canonical: bool,
    pub reachable_digests_valid: bool,
    pub documented_draft_wire_valid: bool,
    pub reference_closure_complete: bool,
    pub reference_kinds_valid: bool,
    pub revision_dag_valid: bool,
    pub requested_roots_antichain: bool,
}

/// A successful raw observation is still explicitly incomplete.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NativeObservation {
    pub requested_roots: Vec<ObjectId>,
    pub source_profile: SourceProfile,
    pub reachable: Vec<ReachableDescriptor>,
    pub revisions: BTreeMap<ObjectId, RevisionNode>,
    pub available: AvailableVerdicts,
    pub completeness: ObservationCompleteness,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ObservationLimits {
    pub max_input_objects: usize,
    pub max_total_input_bytes: u64,
    pub max_encoded_object_bytes: usize,
    pub max_traversal_work: u64,
}

impl Default for ObservationLimits {
    fn default() -> Self {
        Self {
            max_input_objects: 1_000_000,
            max_total_input_bytes: 256 * 1024 * 1024,
            max_encoded_object_bytes: WIRE_MAX_OBJECT_BYTES,
            max_traversal_work: 1_000_000,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ExpectedReference {
    Revision,
    Tree,
    Trie(Namespace),
    Blob,
    ExactLeaf,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum DecodeError {
    Truncated,
    TrailingBytes,
    UnknownMagic,
    InvalidVersion,
    InvalidParentCount,
    DuplicateParent,
    InvalidNamespace,
    InvalidEntryKind,
    InvalidDepth,
    InvalidChildCount,
    ChildSlotsNotIncreasing,
    InvalidBlobLength,
    InvalidPathLength,
    InvalidPath,
    InvalidLeafKey,
    InvalidPrefixValue,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ObservationError {
    NoRoots,
    TooManyRoots {
        actual: usize,
        limit: usize,
    },
    RootsNotStrictlySorted,
    InputObjectLimit {
        actual: usize,
        limit: usize,
    },
    TotalInputBytesLimit {
        actual: u64,
        limit: u64,
    },
    ObjectBytesLimit {
        id: ObjectId,
        actual: usize,
        limit: usize,
    },
    TraversalWorkLimit {
        limit: u64,
    },
    ScratchAllocationFailed,
    MissingObject {
        id: ObjectId,
    },
    DigestMismatch {
        requested: ObjectId,
        derived: ObjectId,
    },
    Malformed {
        id: ObjectId,
        error: DecodeError,
    },
    WrongReferenceKind {
        id: ObjectId,
        expected: ExpectedReference,
        actual: ObjectKind,
    },
    RevisionCycle {
        id: ObjectId,
    },
    RootIsAncestor {
        ancestor: ObjectId,
        descendant: ObjectId,
    },
}

/// Observe the full stable-wire reference closure rooted at canonical revisions.
///
/// The object buffers remain owned by `objects`; blob bodies are parsed as
/// borrowed slices and are never cloned. This offline map API says nothing about
/// callback transport, provider timeouts, authorization, retention, or origin.
/// Scratch vectors reserve fallibly; standard map/result allocations can still
/// abort on allocator exhaustion. Logical caps do not enforce a hard RSS limit.
pub fn observe_native_closure(
    objects: &BTreeMap<ObjectId, Vec<u8>>,
    roots: &[ObjectId],
    limits: ObservationLimits,
) -> Result<NativeObservation, ObservationError> {
    validate_roots(roots)?;
    validate_input_bounds(objects, limits)?;

    let mut pending = Vec::new();
    let mut work = 0u64;
    for root in roots.iter().rev() {
        schedule(
            &mut pending,
            (*root, ExpectedReference::Revision),
            &mut work,
            limits.max_traversal_work,
        )?;
    }
    let mut seen = BTreeMap::<ObjectId, ObjectKind>::new();
    let mut descriptors = BTreeMap::<ObjectId, u64>::new();
    let mut revisions = BTreeMap::<ObjectId, RevisionNode>::new();
    let mut any_r2 = false;
    let mut any_entry_kind = false;

    while let Some((id, expected)) = pending.pop() {
        charge_work(&mut work, limits.max_traversal_work)?;
        if let Some(kind) = seen.get(&id).copied() {
            require_kind(id, expected, kind)?;
            continue;
        }
        let bytes = objects
            .get(&id)
            .ok_or(ObservationError::MissingObject { id })?;
        let derived = sha256(bytes);
        if derived != id {
            return Err(ObservationError::DigestMismatch {
                requested: id,
                derived,
            });
        }
        let decoded = decode(bytes).map_err(|error| ObservationError::Malformed { id, error })?;
        let kind = decoded.kind();
        require_kind(id, expected, kind)?;
        descriptors.insert(id, bytes.len() as u64);
        seen.insert(id, kind);

        match decoded {
            Decoded::Blob { content } => {
                let _borrowed_without_copy = content;
            }
            Decoded::Tree {
                exact,
                portable,
                prefix,
            } => {
                schedule(
                    &mut pending,
                    (prefix, ExpectedReference::Trie(Namespace::Prefix)),
                    &mut work,
                    limits.max_traversal_work,
                )?;
                schedule(
                    &mut pending,
                    (portable, ExpectedReference::Trie(Namespace::Portable)),
                    &mut work,
                    limits.max_traversal_work,
                )?;
                schedule(
                    &mut pending,
                    (exact, ExpectedReference::Trie(Namespace::Exact)),
                    &mut work,
                    limits.max_traversal_work,
                )?;
            }
            Decoded::Revision {
                version,
                parents,
                tree,
            } => {
                any_r2 |= version == 2;
                schedule(
                    &mut pending,
                    (tree, ExpectedReference::Tree),
                    &mut work,
                    limits.max_traversal_work,
                )?;
                for parent in parents.iter().rev() {
                    schedule(
                        &mut pending,
                        (*parent, ExpectedReference::Revision),
                        &mut work,
                        limits.max_traversal_work,
                    )?;
                }
                revisions.insert(
                    id,
                    RevisionNode {
                        version,
                        parents,
                        tree,
                    },
                );
            }
            Decoded::Branch {
                namespace,
                children,
                ..
            } => {
                for (_, child) in children.into_iter().rev() {
                    schedule(
                        &mut pending,
                        (child, ExpectedReference::Trie(namespace)),
                        &mut work,
                        limits.max_traversal_work,
                    )?;
                }
            }
            Decoded::Leaf {
                namespace,
                value,
                has_entry_kind,
            } => {
                any_entry_kind |= has_entry_kind;
                match namespace {
                    Namespace::Exact => schedule(
                        &mut pending,
                        (value, ExpectedReference::Blob),
                        &mut work,
                        limits.max_traversal_work,
                    )?,
                    Namespace::Portable => schedule(
                        &mut pending,
                        (value, ExpectedReference::ExactLeaf),
                        &mut work,
                        limits.max_traversal_work,
                    )?,
                    Namespace::Prefix => {}
                }
            }
        }
    }

    validate_revision_dag_and_antichain(&revisions, roots, &mut work, limits.max_traversal_work)?;

    Ok(NativeObservation {
        requested_roots: roots.to_vec(),
        source_profile: if any_entry_kind {
            SourceProfile::Profile3
        } else if any_r2 {
            SourceProfile::Profile2
        } else {
            SourceProfile::Profile1
        },
        reachable: descriptors
            .into_iter()
            .map(|(id, encoded_len)| ReachableDescriptor { id, encoded_len })
            .collect(),
        revisions,
        available: AvailableVerdicts {
            requested_roots_canonical: true,
            reachable_digests_valid: true,
            documented_draft_wire_valid: true,
            reference_closure_complete: true,
            reference_kinds_valid: true,
            revision_dag_valid: true,
            requested_roots_antichain: true,
        },
        completeness: ObservationCompleteness::Incomplete {
            pending_checks: vec![
                PendingOracleWork::NativeWireOwnerConfirmation,
                PendingOracleWork::PortableIndexComparison,
                PendingOracleWork::ContextualTriePlacement,
                PendingOracleWork::CrossIndexEquality,
                PendingOracleWork::DerivedPrefixCounts,
            ],
        },
    })
}

fn validate_roots(roots: &[ObjectId]) -> Result<(), ObservationError> {
    if roots.is_empty() {
        return Err(ObservationError::NoRoots);
    }
    if roots.len() > MAX_ROOTS {
        return Err(ObservationError::TooManyRoots {
            actual: roots.len(),
            limit: MAX_ROOTS,
        });
    }
    if roots.windows(2).any(|pair| pair[0] >= pair[1]) {
        return Err(ObservationError::RootsNotStrictlySorted);
    }
    Ok(())
}

fn validate_input_bounds(
    objects: &BTreeMap<ObjectId, Vec<u8>>,
    limits: ObservationLimits,
) -> Result<(), ObservationError> {
    if objects.len() > limits.max_input_objects {
        return Err(ObservationError::InputObjectLimit {
            actual: objects.len(),
            limit: limits.max_input_objects,
        });
    }
    let object_limit = limits.max_encoded_object_bytes.min(WIRE_MAX_OBJECT_BYTES);
    let mut total = 0u64;
    for (id, bytes) in objects {
        if bytes.len() > object_limit {
            return Err(ObservationError::ObjectBytesLimit {
                id: *id,
                actual: bytes.len(),
                limit: object_limit,
            });
        }
        total = total.checked_add(bytes.len() as u64).ok_or(
            ObservationError::TotalInputBytesLimit {
                actual: u64::MAX,
                limit: limits.max_total_input_bytes,
            },
        )?;
        if total > limits.max_total_input_bytes {
            return Err(ObservationError::TotalInputBytesLimit {
                actual: total,
                limit: limits.max_total_input_bytes,
            });
        }
    }
    Ok(())
}

fn charge_work(work: &mut u64, limit: u64) -> Result<(), ObservationError> {
    *work = work
        .checked_add(1)
        .ok_or(ObservationError::TraversalWorkLimit { limit })?;
    if *work > limit {
        Err(ObservationError::TraversalWorkLimit { limit })
    } else {
        Ok(())
    }
}

fn schedule(
    pending: &mut Vec<(ObjectId, ExpectedReference)>,
    edge: (ObjectId, ExpectedReference),
    work: &mut u64,
    limit: u64,
) -> Result<(), ObservationError> {
    charge_work(work, limit)?;
    pending
        .try_reserve(1)
        .map_err(|_| ObservationError::ScratchAllocationFailed)?;
    pending.push(edge);
    Ok(())
}

fn require_kind(
    id: ObjectId,
    expected: ExpectedReference,
    actual: ObjectKind,
) -> Result<(), ObservationError> {
    let matches = match expected {
        ExpectedReference::Revision => {
            matches!(actual, ObjectKind::Revision1 | ObjectKind::Revision2)
        }
        ExpectedReference::Tree => actual == ObjectKind::Tree,
        ExpectedReference::Trie(ns) => {
            actual == ObjectKind::Branch(ns) || actual == ObjectKind::Leaf(ns)
        }
        ExpectedReference::Blob => actual == ObjectKind::Blob,
        ExpectedReference::ExactLeaf => actual == ObjectKind::Leaf(Namespace::Exact),
    };
    if matches {
        Ok(())
    } else {
        Err(ObservationError::WrongReferenceKind {
            id,
            expected,
            actual,
        })
    }
}

fn validate_revision_dag_and_antichain(
    revisions: &BTreeMap<ObjectId, RevisionNode>,
    roots: &[ObjectId],
    work: &mut u64,
    limit: u64,
) -> Result<(), ObservationError> {
    let root_set: BTreeSet<ObjectId> = roots.iter().copied().collect();
    for descendant in roots {
        let mut stack = Vec::new();
        schedule_dag(&mut stack, (*descendant, false), work, limit)?;
        let mut colors = BTreeMap::<ObjectId, u8>::new();
        while let Some((id, exiting)) = stack.pop() {
            charge_work(work, limit)?;
            if exiting {
                colors.insert(id, 2);
                continue;
            }
            match colors.get(&id).copied().unwrap_or(0) {
                1 => return Err(ObservationError::RevisionCycle { id }),
                2 => continue,
                _ => {}
            }
            colors.insert(id, 1);
            schedule_dag(&mut stack, (id, true), work, limit)?;
            if let Some(revision) = revisions.get(&id) {
                for parent in revision.parents.iter().rev() {
                    if parent != descendant && root_set.contains(parent) {
                        return Err(ObservationError::RootIsAncestor {
                            ancestor: *parent,
                            descendant: *descendant,
                        });
                    }
                    schedule_dag(&mut stack, (*parent, false), work, limit)?;
                }
            }
        }
    }
    Ok(())
}

fn schedule_dag(
    stack: &mut Vec<(ObjectId, bool)>,
    frame: (ObjectId, bool),
    work: &mut u64,
    limit: u64,
) -> Result<(), ObservationError> {
    charge_work(work, limit)?;
    stack
        .try_reserve(1)
        .map_err(|_| ObservationError::ScratchAllocationFailed)?;
    stack.push(frame);
    Ok(())
}

enum Decoded<'a> {
    Blob {
        content: &'a [u8],
    },
    Tree {
        exact: ObjectId,
        portable: ObjectId,
        prefix: ObjectId,
    },
    Revision {
        version: u32,
        parents: Vec<ObjectId>,
        tree: ObjectId,
    },
    Branch {
        namespace: Namespace,
        children: Vec<(u8, ObjectId)>,
    },
    Leaf {
        namespace: Namespace,
        value: ObjectId,
        has_entry_kind: bool,
    },
}

impl Decoded<'_> {
    fn kind(&self) -> ObjectKind {
        match self {
            Self::Blob { .. } => ObjectKind::Blob,
            Self::Tree { .. } => ObjectKind::Tree,
            Self::Revision { version: 1, .. } => ObjectKind::Revision1,
            Self::Revision { .. } => ObjectKind::Revision2,
            Self::Branch { namespace, .. } => ObjectKind::Branch(*namespace),
            Self::Leaf { namespace, .. } => ObjectKind::Leaf(*namespace),
        }
    }
}

fn decode(bytes: &[u8]) -> Result<Decoded<'_>, DecodeError> {
    let mut cursor = Cursor::new(bytes);
    let magic = cursor.take(9)?;
    let decoded = if magic == MAGIC_BLOB {
        let length = cursor.u64()?;
        if length > MAX_BLOB_BYTES as u64 || length != cursor.remaining() as u64 {
            return Err(DecodeError::InvalidBlobLength);
        }
        Decoded::Blob {
            content: cursor.take(length as usize)?,
        }
    } else if magic == MAGIC_TREE {
        Decoded::Tree {
            exact: cursor.id()?,
            portable: cursor.id()?,
            prefix: cursor.id()?,
        }
    } else if magic == MAGIC_REVISION_1 {
        decode_revision(&mut cursor, 1)?
    } else if magic == MAGIC_REVISION_2 {
        decode_revision(&mut cursor, 2)?
    } else if magic == MAGIC_BRANCH {
        decode_branch(&mut cursor)?
    } else if magic == MAGIC_LEAF {
        decode_leaf(&mut cursor, false)?
    } else if magic == MAGIC_LEAF_2 {
        decode_leaf(&mut cursor, true)?
    } else {
        return Err(DecodeError::UnknownMagic);
    };
    if cursor.remaining() != 0 {
        return Err(DecodeError::TrailingBytes);
    }
    Ok(decoded)
}

fn decode_revision(
    cursor: &mut Cursor<'_>,
    expected_version: u32,
) -> Result<Decoded<'static>, DecodeError> {
    let version = cursor.u32()?;
    if version != expected_version {
        return Err(DecodeError::InvalidVersion);
    }
    let count = cursor.u8()? as usize;
    if (version == 1 && count > 1) || (version == 2 && !(2..=8).contains(&count)) {
        return Err(DecodeError::InvalidParentCount);
    }
    let mut parents = Vec::with_capacity(count);
    for _ in 0..count {
        let parent = cursor.id()?;
        if version == 2 && parents.contains(&parent) {
            return Err(DecodeError::DuplicateParent);
        }
        parents.push(parent);
    }
    Ok(Decoded::Revision {
        version,
        parents,
        tree: cursor.id()?,
    })
}

fn decode_branch(cursor: &mut Cursor<'_>) -> Result<Decoded<'static>, DecodeError> {
    let namespace = namespace(cursor.u8()?)?;
    let depth = cursor.u8()?;
    if depth > 63 {
        return Err(DecodeError::InvalidDepth);
    }
    let count = cursor.u8()? as usize;
    if count > 16 {
        return Err(DecodeError::InvalidChildCount);
    }
    let mut children = Vec::with_capacity(count);
    for _ in 0..count {
        let slot = cursor.u8()?;
        if slot > 15
            || children
                .last()
                .map(|(prior, _)| *prior >= slot)
                .unwrap_or(false)
        {
            return Err(DecodeError::ChildSlotsNotIncreasing);
        }
        children.push((slot, cursor.id()?));
    }
    Ok(Decoded::Branch {
        namespace,
        children,
    })
}

fn decode_leaf<'a>(cursor: &mut Cursor<'a>, entry_kind: bool) -> Result<Decoded<'a>, DecodeError> {
    let namespace = namespace(cursor.u8()?)?;
    let key = cursor.id()?;
    let path_len = cursor.u16()? as usize;
    if !(1..=1024).contains(&path_len) {
        return Err(DecodeError::InvalidPathLength);
    }
    let path = cursor.take(path_len)?;
    validate_path(path)?;
    let value = cursor.id()?;
    let folded = ascii_lower(path);
    let expected_key = if namespace == Namespace::Exact {
        sha256(path)
    } else {
        sha256(&folded)
    };
    if key != expected_key {
        return Err(DecodeError::InvalidLeafKey);
    }
    if namespace == Namespace::Prefix
        && (value[..24] != [0u8; 24] || u64::from_be_bytes(value[24..].try_into().unwrap()) == 0)
    {
        return Err(DecodeError::InvalidPrefixValue);
    }
    if entry_kind {
        // Kind 0 would duplicate the L1 spelling; kinds apply to exact files only.
        if namespace != Namespace::Exact || !matches!(cursor.u8()?, 1 | 2) {
            return Err(DecodeError::InvalidEntryKind);
        }
    }
    Ok(Decoded::Leaf {
        namespace,
        value,
        has_entry_kind: entry_kind,
    })
}

fn namespace(value: u8) -> Result<Namespace, DecodeError> {
    match value {
        1 => Ok(Namespace::Exact),
        2 => Ok(Namespace::Portable),
        3 => Ok(Namespace::Prefix),
        _ => Err(DecodeError::InvalidNamespace),
    }
}

fn validate_path(path: &[u8]) -> Result<(), DecodeError> {
    if !path.is_ascii() || path.first() == Some(&b'/') {
        return Err(DecodeError::InvalidPath);
    }
    for component in path.split(|byte| *byte == b'/') {
        if component.is_empty() || component.len() > 255 || component == b"." || component == b".."
        {
            return Err(DecodeError::InvalidPath);
        }
        if component.ends_with(b".") || component.ends_with(b" ") {
            return Err(DecodeError::InvalidPath);
        }
        if component.iter().any(|byte| {
            *byte < 0x20
                || matches!(
                    *byte,
                    b'\\' | b'<' | b'>' | b':' | b'"' | b'|' | b'?' | b'*'
                )
        }) {
            return Err(DecodeError::InvalidPath);
        }
        let stem_end = component
            .iter()
            .position(|byte| *byte == b'.')
            .unwrap_or(component.len());
        let stem = ascii_lower(&component[..stem_end]);
        if is_reserved_stem(&stem) {
            return Err(DecodeError::InvalidPath);
        }
    }
    Ok(())
}

fn is_reserved_stem(stem: &[u8]) -> bool {
    stem == b"con"
        || stem == b"prn"
        || stem == b"aux"
        || stem == b"nul"
        || (stem.len() == 4
            && (stem.starts_with(b"com") || stem.starts_with(b"lpt"))
            && matches!(stem[3], b'1'..=b'9'))
}

fn ascii_lower(bytes: &[u8]) -> Vec<u8> {
    bytes.iter().map(u8::to_ascii_lowercase).collect()
}

fn sha256(bytes: &[u8]) -> ObjectId {
    Sha256::digest(bytes).into()
}

struct Cursor<'a> {
    bytes: &'a [u8],
    offset: usize,
}

impl<'a> Cursor<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, offset: 0 }
    }
    fn remaining(&self) -> usize {
        self.bytes.len() - self.offset
    }
    fn take(&mut self, count: usize) -> Result<&'a [u8], DecodeError> {
        let end = self
            .offset
            .checked_add(count)
            .ok_or(DecodeError::Truncated)?;
        let value = self
            .bytes
            .get(self.offset..end)
            .ok_or(DecodeError::Truncated)?;
        self.offset = end;
        Ok(value)
    }
    fn u8(&mut self) -> Result<u8, DecodeError> {
        Ok(self.take(1)?[0])
    }
    fn u16(&mut self) -> Result<u16, DecodeError> {
        Ok(u16::from_be_bytes(self.take(2)?.try_into().unwrap()))
    }
    fn u32(&mut self) -> Result<u32, DecodeError> {
        Ok(u32::from_be_bytes(self.take(4)?.try_into().unwrap()))
    }
    fn u64(&mut self) -> Result<u64, DecodeError> {
        Ok(u64::from_be_bytes(self.take(8)?.try_into().unwrap()))
    }
    fn id(&mut self) -> Result<ObjectId, DecodeError> {
        Ok(self.take(32)?.try_into().unwrap())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn insert(map: &mut BTreeMap<ObjectId, Vec<u8>>, bytes: Vec<u8>) -> ObjectId {
        let id = sha256(&bytes);
        map.insert(id, bytes);
        id
    }

    fn blob(body: &[u8]) -> Vec<u8> {
        let mut out = MAGIC_BLOB.to_vec();
        out.extend_from_slice(&(body.len() as u64).to_be_bytes());
        out.extend_from_slice(body);
        out
    }

    fn leaf(ns: u8, path: &[u8], value: ObjectId) -> Vec<u8> {
        let key_bytes = if ns == 1 {
            path.to_vec()
        } else {
            ascii_lower(path)
        };
        let mut out = MAGIC_LEAF.to_vec();
        out.push(ns);
        out.extend_from_slice(&sha256(&key_bytes));
        out.extend_from_slice(&(path.len() as u16).to_be_bytes());
        out.extend_from_slice(path);
        out.extend_from_slice(&value);
        out
    }

    fn branch(ns: u8, child: Option<ObjectId>) -> Vec<u8> {
        let mut out = MAGIC_BRANCH.to_vec();
        out.extend_from_slice(&[ns, 0, u8::from(child.is_some())]);
        if let Some(id) = child {
            out.push(0);
            out.extend_from_slice(&id);
        }
        out
    }

    fn tree(exact: ObjectId, portable: ObjectId, prefix: ObjectId) -> Vec<u8> {
        let mut out = MAGIC_TREE.to_vec();
        out.extend_from_slice(&exact);
        out.extend_from_slice(&portable);
        out.extend_from_slice(&prefix);
        out
    }

    fn revision(magic: &[u8; 9], version: u32, parents: &[ObjectId], tree: ObjectId) -> Vec<u8> {
        let mut out = magic.to_vec();
        out.extend_from_slice(&version.to_be_bytes());
        out.push(parents.len() as u8);
        for parent in parents {
            out.extend_from_slice(parent);
        }
        out.extend_from_slice(&tree);
        out
    }

    fn from_hex(text: &str) -> Vec<u8> {
        assert_eq!(text.len() % 2, 0);
        text.as_bytes()
            .chunks_exact(2)
            .map(|pair| {
                let digit = |byte| match byte {
                    b'0'..=b'9' => byte - b'0',
                    b'a'..=b'f' => byte - b'a' + 10,
                    _ => panic!("non-lowercase-hex test literal"),
                };
                (digit(pair[0]) << 4) | digit(pair[1])
            })
            .collect()
    }

    fn id_hex(text: &str) -> ObjectId {
        from_hex(text).try_into().unwrap()
    }

    fn fixture() -> (BTreeMap<ObjectId, Vec<u8>>, ObjectId, ObjectId) {
        let mut map = BTreeMap::new();
        let blob_id = insert(&mut map, blob(b"body"));
        let exact_leaf = insert(&mut map, leaf(1, b"Dir/File.txt", blob_id));
        let portable_leaf = insert(&mut map, leaf(2, b"Dir/File.txt", exact_leaf));
        let mut count = [0u8; 32];
        count[24..].copy_from_slice(&1u64.to_be_bytes());
        let prefix_leaf = insert(&mut map, leaf(3, b"Dir", count));
        let exact_root = insert(&mut map, branch(1, Some(exact_leaf)));
        let portable_root = insert(&mut map, branch(2, Some(portable_leaf)));
        let prefix_root = insert(&mut map, branch(3, Some(prefix_leaf)));
        let tree_id = insert(&mut map, tree(exact_root, portable_root, prefix_root));
        let r1a = insert(&mut map, revision(MAGIC_REVISION_1, 1, &[], tree_id));
        let r1b = insert(&mut map, revision(MAGIC_REVISION_1, 1, &[r1a], tree_id));
        let r2 = insert(
            &mut map,
            revision(MAGIC_REVISION_2, 2, &[r1a, r1b], tree_id),
        );
        (map, r2, r1a)
    }

    fn kinded_leaf(ns: u8, path: &[u8], value: ObjectId, kind: &[u8]) -> Vec<u8> {
        let mut out = leaf(ns, path, value);
        out[..9].copy_from_slice(MAGIC_LEAF_2);
        out.extend_from_slice(kind);
        out
    }

    /// One exact file whose L2 leaf carries `kind`; returns the root revision.
    fn kinded_closure(kind: &[u8], namespace: u8) -> (BTreeMap<ObjectId, Vec<u8>>, ObjectId) {
        let mut map = BTreeMap::new();
        let blob_id = insert(&mut map, blob(b"body"));
        let exact_leaf = insert(&mut map, kinded_leaf(namespace, b"run.sh", blob_id, kind));
        let portable_leaf = insert(&mut map, leaf(2, b"run.sh", exact_leaf));
        let exact_root = insert(&mut map, branch(1, Some(exact_leaf)));
        let portable_root = insert(&mut map, branch(2, Some(portable_leaf)));
        let prefix_root = insert(&mut map, branch(3, None));
        let tree_id = insert(&mut map, tree(exact_root, portable_root, prefix_root));
        let root = insert(&mut map, revision(MAGIC_REVISION_1, 1, &[], tree_id));
        (map, root)
    }

    #[test]
    fn entry_kind_leaves_select_profile3_and_decode_strictly() {
        for kind in [[1u8], [2u8]] {
            let (map, root) = kinded_closure(&kind, 1);
            let observed =
                observe_native_closure(&map, &[root], ObservationLimits::default()).unwrap();
            assert_eq!(observed.source_profile, SourceProfile::Profile3);
        }
        // Kind 0 is the L1 spelling, 3 is unknown, and an empty or long tail is malformed.
        for kind in [&[0u8][..], &[3], &[], &[1, 0]] {
            let (map, root) = kinded_closure(kind, 1);
            assert!(
                observe_native_closure(&map, &[root], ObservationLimits::default()).is_err(),
                "accepted kind bytes {kind:?}"
            );
        }
        // Kinds are only valid in the exact namespace.
        let (map, root) = kinded_closure(&[1], 2);
        assert!(observe_native_closure(&map, &[root], ObservationLimits::default()).is_err());
    }

    #[test]
    fn observes_all_six_magics_as_incomplete_profile2() {
        let (map, root, _) = fixture();
        let observed = observe_native_closure(&map, &[root], ObservationLimits::default()).unwrap();
        assert_eq!(observed.source_profile, SourceProfile::Profile2);
        let first_parent: ObjectId = map[&root][14..46].try_into().unwrap();
        let second_parent: ObjectId = map[&root][46..78].try_into().unwrap();
        assert_eq!(
            observed.revisions[&root].parents,
            vec![first_parent, second_parent]
        );
        assert!(matches!(
            &observed.completeness,
            ObservationCompleteness::Incomplete { pending_checks } if pending_checks.len() == 5
        ));
        assert!(observed.available.reference_closure_complete);
    }

    #[test]
    fn matches_fixed_seven_object_375_byte_known_answer() {
        let literals = [
            (
                "1f02b1d13a45fadfeaca4448196e81ebb8698e84eeaf2812eb3c1f728f144f80",
                "414753434d2d4e3100010000",
            ),
            (
                "8c48a97f852d1bb0d24e173d6b3337d929598550cf0a413322a4fefc089250a4",
                "414753434d2d4e3100020000",
            ),
            (
                "cdda7c7b6c2aab9bae2973a72af77406dae7cfab65211cf0142914efc6426d37",
                "414753434d2d4e3100030000",
            ),
            (
                "d52cc8d0c7370d275e4f48ab4d7de6918c7dde9e2968ac8ba9932d37459ead4c",
                "414753434d2d5432001f02b1d13a45fadfeaca4448196e81ebb8698e84eeaf2812eb3c1f728f144f808c48a97f852d1bb0d24e173d6b3337d929598550cf0a413322a4fefc089250a4cdda7c7b6c2aab9bae2973a72af77406dae7cfab65211cf0142914efc6426d37",
            ),
            (
                "c876db6132759aa71fa5de57df4eea0511dc0ef35e2a459c8779d9b1caabe1ba",
                "414753434d2d5231000000000100d52cc8d0c7370d275e4f48ab4d7de6918c7dde9e2968ac8ba9932d37459ead4c",
            ),
            (
                "610067a866fe9a9d9e6bae74d0fdf303e97b93b3874a5a1830910f91121f1a62",
                "414753434d2d5231000000000101c876db6132759aa71fa5de57df4eea0511dc0ef35e2a459c8779d9b1caabe1bad52cc8d0c7370d275e4f48ab4d7de6918c7dde9e2968ac8ba9932d37459ead4c",
            ),
            (
                "8d667d78c15df10affb3ff235706cfd5dd15842400cfdcb606ce149ca77104f2",
                "414753434d2d5232000000000202c876db6132759aa71fa5de57df4eea0511dc0ef35e2a459c8779d9b1caabe1ba610067a866fe9a9d9e6bae74d0fdf303e97b93b3874a5a1830910f91121f1a62d52cc8d0c7370d275e4f48ab4d7de6918c7dde9e2968ac8ba9932d37459ead4c",
            ),
            (
                "0ed74a41756e37cf0abde5b49c4e1a2f529fa7c42ca789a7d5f33a1363e3b323",
                "414753434d2d4231000000000000000000",
            ),
        ];
        let mut map = BTreeMap::new();
        for (id, bytes) in literals {
            map.insert(id_hex(id), from_hex(bytes));
        }
        let root = id_hex("8d667d78c15df10affb3ff235706cfd5dd15842400cfdcb606ce149ca77104f2");
        let observed = observe_native_closure(&map, &[root], ObservationLimits::default()).unwrap();
        let expected = [
            (
                "1f02b1d13a45fadfeaca4448196e81ebb8698e84eeaf2812eb3c1f728f144f80",
                12u64,
            ),
            (
                "610067a866fe9a9d9e6bae74d0fdf303e97b93b3874a5a1830910f91121f1a62",
                78,
            ),
            (
                "8c48a97f852d1bb0d24e173d6b3337d929598550cf0a413322a4fefc089250a4",
                12,
            ),
            (
                "8d667d78c15df10affb3ff235706cfd5dd15842400cfdcb606ce149ca77104f2",
                110,
            ),
            (
                "c876db6132759aa71fa5de57df4eea0511dc0ef35e2a459c8779d9b1caabe1ba",
                46,
            ),
            (
                "cdda7c7b6c2aab9bae2973a72af77406dae7cfab65211cf0142914efc6426d37",
                12,
            ),
            (
                "d52cc8d0c7370d275e4f48ab4d7de6918c7dde9e2968ac8ba9932d37459ead4c",
                105,
            ),
        ];
        assert_eq!(observed.reachable.len(), 7);
        assert_eq!(
            observed
                .reachable
                .iter()
                .map(|entry| entry.encoded_len)
                .sum::<u64>(),
            375
        );
        assert!(
            observed
                .reachable
                .iter()
                .zip(expected)
                .all(|(actual, (id, len))| {
                    actual.id == id_hex(id) && actual.encoded_len == len
                })
        );
    }

    #[test]
    fn rejects_manually_malformed_wire() {
        let mut bad_blob = blob(b"x");
        bad_blob.push(0);
        assert!(matches!(
            decode(&bad_blob),
            Err(DecodeError::InvalidBlobLength)
        ));
        let mut bad_branch = MAGIC_BRANCH.to_vec();
        bad_branch.extend_from_slice(&[1, 64, 0]);
        assert!(matches!(
            decode(&bad_branch),
            Err(DecodeError::InvalidDepth)
        ));
        let mut bad_leaf = leaf(1, b"ok", [7; 32]);
        bad_leaf.push(0);
        assert!(matches!(decode(&bad_leaf), Err(DecodeError::TrailingBytes)));
    }

    #[test]
    fn enforces_revision_leaf_and_prefix_scalar_rules() {
        let tree_id = [9; 32];
        assert!(matches!(
            decode(&revision(MAGIC_REVISION_1, 2, &[], tree_id)),
            Err(DecodeError::InvalidVersion)
        ));
        assert!(matches!(
            decode(&revision(MAGIC_REVISION_2, 2, &[[1; 32]], tree_id)),
            Err(DecodeError::InvalidParentCount)
        ));
        assert!(matches!(
            decode(&revision(MAGIC_REVISION_2, 2, &[[1; 32], [1; 32]], tree_id)),
            Err(DecodeError::DuplicateParent)
        ));
        let mut wrong_key = leaf(1, b"file", [2; 32]);
        wrong_key[10] ^= 1;
        assert!(matches!(
            decode(&wrong_key),
            Err(DecodeError::InvalidLeafKey)
        ));
        assert!(matches!(
            decode(&leaf(3, b"dir", [0; 32])),
            Err(DecodeError::InvalidPrefixValue)
        ));
    }

    #[test]
    fn applies_path_wire_rules_without_workspace_policy() {
        for path in [
            &b"/abs"[..],
            &b"a//b"[..],
            &b"CON.txt"[..],
            &b"a."[..],
            &b"a\\b"[..],
        ] {
            assert!(matches!(
                decode(&leaf(1, path, [3; 32])),
                Err(DecodeError::InvalidPath)
            ));
        }
        assert!(decode(&leaf(1, b".agentscm-workspace/file\x7f", [3; 32])).is_ok());
        for path in [vec![], vec![b'a'; 1025]] {
            assert_eq!(
                decode(&leaf(1, &path, [3; 32])).err(),
                Some(DecodeError::InvalidPathLength)
            );
        }
        let mut maximum_path = Vec::new();
        for component_len in [255, 255, 255, 254, 1] {
            if !maximum_path.is_empty() {
                maximum_path.push(b'/');
            }
            maximum_path.extend(vec![b'a'; component_len]);
        }
        assert_eq!(maximum_path.len(), 1024);
        assert!(decode(&leaf(1, &maximum_path, [3; 32])).is_ok());
    }

    #[test]
    fn accepts_empty_and_unary_branches() {
        assert!(decode(&branch(1, None)).is_ok());
        assert!(decode(&branch(3, Some([4; 32]))).is_ok());
    }

    #[test]
    fn enforces_root_and_resource_bounds_including_overflow() {
        assert_eq!(validate_roots(&[]), Err(ObservationError::NoRoots));
        assert_eq!(
            validate_roots(&[[1; 32], [1; 32]]),
            Err(ObservationError::RootsNotStrictlySorted)
        );
        assert!(matches!(
            validate_roots(&vec![[0; 32]; MAX_ROOTS + 1]),
            Err(ObservationError::TooManyRoots { .. })
        ));

        let (map, root, _) = fixture();
        let mut limits = ObservationLimits {
            max_input_objects: map.len() - 1,
            ..ObservationLimits::default()
        };
        assert!(matches!(
            observe_native_closure(&map, &[root], limits),
            Err(ObservationError::InputObjectLimit { .. })
        ));
        limits = ObservationLimits {
            max_total_input_bytes: 0,
            ..ObservationLimits::default()
        };
        assert!(matches!(
            observe_native_closure(&map, &[root], limits),
            Err(ObservationError::TotalInputBytesLimit { .. })
        ));
        limits = ObservationLimits {
            max_encoded_object_bytes: 0,
            ..ObservationLimits::default()
        };
        assert!(matches!(
            observe_native_closure(&map, &[root], limits),
            Err(ObservationError::ObjectBytesLimit { .. })
        ));
        limits = ObservationLimits {
            max_traversal_work: 0,
            ..ObservationLimits::default()
        };
        assert_eq!(
            observe_native_closure(&map, &[root], limits),
            Err(ObservationError::TraversalWorkLimit { limit: 0 })
        );
        let mut work = u64::MAX;
        assert_eq!(
            charge_work(&mut work, u64::MAX),
            Err(ObservationError::TraversalWorkLimit { limit: u64::MAX })
        );
    }

    #[test]
    fn rechecks_namespace_for_an_already_seen_object() {
        let mut map = BTreeMap::new();
        let blob_id = insert(&mut map, blob(b"x"));
        let exact_leaf = insert(&mut map, leaf(1, b"x", blob_id));
        let portable_root = insert(&mut map, branch(2, Some(exact_leaf)));
        let prefix_root = insert(&mut map, branch(3, None));
        let tree_id = insert(&mut map, tree(exact_leaf, portable_root, prefix_root));
        let root = insert(&mut map, revision(MAGIC_REVISION_1, 1, &[], tree_id));
        assert!(matches!(
            observe_native_closure(&map, &[root], ObservationLimits::default()),
            Err(ObservationError::WrongReferenceKind {
                expected: ExpectedReference::Trie(Namespace::Portable),
                actual: ObjectKind::Leaf(Namespace::Exact),
                ..
            })
        ));
    }

    #[test]
    fn rejects_wrong_digest_for_requested_object() {
        let (mut map, root, _) = fixture();
        map.get_mut(&root).unwrap().push(0);
        assert!(matches!(
            observe_native_closure(&map, &[root], ObservationLimits::default()),
            Err(ObservationError::DigestMismatch { requested, .. }) if requested == root
        ));
    }

    #[test]
    fn rejects_missing_and_wrong_kind_dependencies() {
        let (mut missing, root, _) = fixture();
        let absent = missing[&root][46..78].try_into().unwrap();
        missing.remove(&absent);
        assert_eq!(
            observe_native_closure(&missing, &[root], ObservationLimits::default()),
            Err(ObservationError::MissingObject { id: absent })
        );

        let mut wrong = BTreeMap::new();
        let blob_id = insert(&mut wrong, blob(b"not a tree"));
        let revision_id = insert(&mut wrong, revision(MAGIC_REVISION_1, 1, &[], blob_id));
        assert!(matches!(
            observe_native_closure(&wrong, &[revision_id], ObservationLimits::default()),
            Err(ObservationError::WrongReferenceKind {
                expected: ExpectedReference::Tree,
                actual: ObjectKind::Blob,
                ..
            })
        ));
    }

    #[test]
    fn descriptors_include_reachable_and_exclude_unreachable_objects() {
        let (mut map, _, root) = fixture();
        let unreachable = insert(&mut map, blob(b"unreachable"));
        let observed = observe_native_closure(&map, &[root], ObservationLimits::default()).unwrap();
        assert_eq!(observed.source_profile, SourceProfile::Profile1);
        assert!(
            observed
                .reachable
                .windows(2)
                .all(|pair| pair[0].id < pair[1].id)
        );
        assert!(
            !observed
                .reachable
                .iter()
                .any(|entry| entry.id == unreachable)
        );
    }

    #[test]
    fn rejects_requested_root_that_is_an_ancestor_of_another_root() {
        let (map, descendant, ancestor) = fixture();
        let mut roots = vec![ancestor, descendant];
        roots.sort();
        assert!(matches!(
            observe_native_closure(&map, &roots, ObservationLimits::default()),
            Err(ObservationError::RootIsAncestor { ancestor: found, descendant: found_descendant })
                if found == ancestor && found_descendant == descendant
        ));
    }

    #[test]
    fn revision_dag_rejects_cycle_and_accepts_shared_ancestor_diamond() {
        // Synthetic graph tests isolate traversal; cyclic SHA-addressed bytes are not fabricated.
        let node = |parents| RevisionNode {
            version: 1,
            parents,
            tree: [9; 32],
        };
        let (a, b, c, d) = ([1; 32], [2; 32], [3; 32], [4; 32]);
        let cyclic = BTreeMap::from([(a, node(vec![b])), (b, node(vec![a]))]);
        assert_eq!(
            validate_revision_dag_and_antichain(&cyclic, &[a], &mut 0, 1000),
            Err(ObservationError::RevisionCycle { id: a })
        );
        let self_cycle = BTreeMap::from([(a, node(vec![a]))]);
        assert_eq!(
            validate_revision_dag_and_antichain(&self_cycle, &[a], &mut 0, 1000),
            Err(ObservationError::RevisionCycle { id: a })
        );
        let independent = BTreeMap::from([(a, node(vec![])), (b, node(vec![]))]);
        assert_eq!(
            validate_revision_dag_and_antichain(&independent, &[a, b], &mut 0, 1000),
            Ok(())
        );
        let diamond = BTreeMap::from([
            (a, node(vec![b, c])),
            (b, node(vec![d])),
            (c, node(vec![d])),
            (d, node(vec![])),
        ]);
        assert_eq!(
            validate_revision_dag_and_antichain(&diamond, &[a], &mut 0, 1000),
            Ok(())
        );
    }

    #[test]
    fn malformed_wire_rejection_table_covers_headers_counts_and_slots() {
        let mut cases = vec![
            (vec![], DecodeError::Truncated),
            (b"BADMAGIC!".to_vec(), DecodeError::UnknownMagic),
        ];
        for (fields, error) in [
            ([4, 0, 0], DecodeError::InvalidNamespace),
            ([1, 0, 17], DecodeError::InvalidChildCount),
        ] {
            let mut bytes = MAGIC_BRANCH.to_vec();
            bytes.extend_from_slice(&fields);
            cases.push((bytes, error));
        }
        for slots in [vec![16], vec![1, 1], vec![2, 1]] {
            let mut bytes = MAGIC_BRANCH.to_vec();
            bytes.extend_from_slice(&[1, 0, slots.len() as u8]);
            for slot in slots {
                bytes.push(slot);
                bytes.extend_from_slice(&[0; 32]);
            }
            cases.push((bytes, DecodeError::ChildSlotsNotIncreasing));
        }
        cases.push((
            revision(MAGIC_REVISION_2, 2, &[[0; 32]; 9], [0; 32]),
            DecodeError::InvalidParentCount,
        ));
        let valid_revision = revision(MAGIC_REVISION_1, 1, &[], [0; 32]);
        for length in [9, 10, 13, 14, 45] {
            cases.push((valid_revision[..length].to_vec(), DecodeError::Truncated));
        }
        let leaf_with_path = leaf(1, b"file", [0; 32]);
        cases.push((leaf_with_path[..45].to_vec(), DecodeError::Truncated));
        let mut truncated_leaf = leaf_with_path;
        truncated_leaf.pop();
        cases.push((truncated_leaf, DecodeError::Truncated));
        for (bytes, error) in cases {
            assert_eq!(decode(&bytes).err(), Some(error));
        }
    }
}
