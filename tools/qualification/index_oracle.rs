//! Independent offline oracle for the frozen A3v4 set-index component protocol.
//!
//! This module proves set construction and A3PW packet semantics only. It does
//! not qualify a native closure, provider, authority decision, or durable meter.

use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::convert::TryInto;
use std::mem::size_of;

pub type Hash32 = [u8; 32];

const MAX_DESCRIPTOR_SIZE: u64 = 67_112_960;
const MAX_STORED_NODE_BYTES: usize = 4096;
const LOOKUP_PACKET_CAP: usize = 262_144;
const RANGE_PACKET_CAP: usize = 4_194_304;
const LOOKUP_NODE_CAP: u64 = 65;
const RANGE_NODE_CAP: u64 = 4096;
const LOOKUP_CHILD_CAP: u64 = 1024;
const RANGE_CHILD_CAP: u64 = 8192;
const MAX_BRANCH_LEVELS: usize = 64;
const MAX_RANGE_LEAVES: usize = 1024;

const DESC_LEAF_DOMAIN: &str = "agentscm-a3-index-leaf-v1";
const DESC_BRANCH_DOMAIN: &str = "agentscm-a3-index-branch-v1";
const REV_LEAF_DOMAIN: &str = "agentscm-a3-revision-set-leaf-v1";
const REV_BRANCH_DOMAIN: &str = "agentscm-a3-revision-set-branch-v1";
const PORTABLE_DOMAIN: &str = "agentscm-a3-portable-closure-v1";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum IndexKind {
    Descriptor,
    RevisionSet,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NodeKind {
    Leaf,
    Branch,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NodeSummary {
    pub kind: NodeKind,
    pub hash: Hash32,
    pub min: Hash32,
    pub max: Hash32,
    pub count: u64,
    pub size_total: Option<u64>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ExpectedRoot {
    pub hash: Hash32,
    pub count: u64,
    pub size_total: Option<u64>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct BuildLimits {
    pub max_entries: u64,
    pub max_source_bytes: u64,
    pub max_entry_buffer_bytes: u64,
    pub max_metadata_bytes: u64,
    pub max_nodes: u64,
    pub max_work: u64,
    pub max_branch_scratch_bytes: u64,
}

impl Default for BuildLimits {
    fn default() -> Self {
        Self {
            max_entries: 100_000,
            max_source_bytes: MAX_DESCRIPTOR_SIZE * 100_000,
            max_entry_buffer_bytes: (size_of::<Entry>() as u64) * 100_000,
            max_metadata_bytes: 64 * 1024 * 1024,
            max_nodes: 200_000,
            max_work: 16_000_000,
            max_branch_scratch_bytes: (size_of::<(u8, NodeSummary)>() as u64) * 16 * 64,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BuildCounters {
    pub entry_buffer_reservation_bytes: u64,
    pub metadata_bytes: u64,
    pub nodes_written: u64,
    pub work: u64,
    pub branch_scratch_peak_bytes: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BuiltIndex {
    pub kind: IndexKind,
    pub root: NodeSummary,
    pub nodes: BTreeMap<Hash32, Vec<u8>>,
    pub counters: BuildCounters,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PortableIdentity {
    pub profile: u8,
    pub native_roots: Vec<Hash32>,
    pub descriptor_root_hash: Hash32,
    pub object_count: u64,
    pub encoded_source_bytes: u64,
    pub digest: Hash32,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LookupResult {
    Member { size: Option<u64> },
    Nonmember,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RangeRequest {
    pub ordinal: u32,
    pub frontier: Hash32,
    pub leaf_limit: u16,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RangeEntry {
    pub key: Hash32,
    pub size: Option<u64>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RangeResult {
    pub entries: Vec<RangeEntry>,
    /// `None` means the covered interval ends at the authenticated root maximum.
    pub next_request: Option<RangeRequest>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum OracleError {
    EmptySet,
    UnsortedOrDuplicate,
    DuplicateKey,
    InvalidDescriptorSize,
    InvalidKind,
    InvalidShape,
    InvalidSummary,
    Truncated,
    TrailingBytes,
    SizeLimit,
    NodeLimit,
    ChildLimit,
    DepthLimit,
    LeafLimit,
    WorkLimit,
    MetadataLimit,
    ArithmeticOverflow,
    RootMismatch,
    RequestMismatch,
    UnsupportedOperation,
    NonCanonicalExpansion,
    OmittedInterior,
    NonterminalShortPage,
    FrontierOverflow,
}

#[derive(Clone, Copy)]
struct Entry {
    key: Hash32,
    size: Option<u64>,
}

struct Builder {
    kind: IndexKind,
    limits: BuildLimits,
    nodes: BTreeMap<Hash32, Vec<u8>>,
    counters: BuildCounters,
    branch_scratch_bytes: u64,
}

pub fn build_descriptor_index(
    entries: &[(Hash32, u64)],
    limits: BuildLimits,
) -> Result<BuiltIndex, OracleError> {
    let (mut builder, mut raw) = Builder::start(IndexKind::Descriptor, entries.len(), limits)?;
    let mut source_bytes = 0u64;
    let mut previous = None;
    for (key, size) in entries {
        builder.charge_work(1)?; // one validation work unit per caller entry
        if previous.map(|prior| prior >= *key).unwrap_or(false) {
            return Err(OracleError::UnsortedOrDuplicate);
        }
        if !(1..=MAX_DESCRIPTOR_SIZE).contains(size) {
            return Err(OracleError::InvalidDescriptorSize);
        }
        source_bytes = source_bytes
            .checked_add(*size)
            .ok_or(OracleError::ArithmeticOverflow)?;
        if source_bytes > limits.max_source_bytes {
            return Err(OracleError::SizeLimit);
        }
        builder.charge_work(1)?; // one conversion-copy work unit per accepted entry
        raw.push(Entry {
            key: *key,
            size: Some(*size),
        });
        previous = Some(*key);
    }
    builder.finish(&raw)
}

pub fn build_revision_index(
    keys: &[Hash32],
    limits: BuildLimits,
) -> Result<BuiltIndex, OracleError> {
    let (mut builder, mut raw) = Builder::start(IndexKind::RevisionSet, keys.len(), limits)?;
    let mut previous = None;
    for key in keys {
        builder.charge_work(1)?;
        if previous.map(|prior| prior >= *key).unwrap_or(false) {
            return Err(OracleError::UnsortedOrDuplicate);
        }
        builder.charge_work(1)?;
        raw.push(Entry {
            key: *key,
            size: None,
        });
        previous = Some(*key);
    }
    builder.finish(&raw)
}

impl Builder {
    /// Private E work units are charged before: one entry-buffer reservation,
    /// each caller-entry validation, each accepted-entry copy, each span/group
    /// scan, each branch-scratch reservation, and each node hash plus child
    /// record encoded. These units intentionally do not model A's counters.
    fn start(
        kind: IndexKind,
        len: usize,
        limits: BuildLimits,
    ) -> Result<(Self, Vec<Entry>), OracleError> {
        if len == 0 {
            return Err(OracleError::EmptySet);
        }
        if len as u64 > limits.max_entries {
            return Err(OracleError::SizeLimit);
        }
        let reservation = (len as u64)
            .checked_mul(size_of::<Entry>() as u64)
            .ok_or(OracleError::ArithmeticOverflow)?;
        if reservation > limits.max_entry_buffer_bytes || reservation > limits.max_metadata_bytes {
            return Err(OracleError::MetadataLimit);
        }
        let mut builder = Self {
            kind,
            limits,
            nodes: BTreeMap::new(),
            counters: BuildCounters {
                entry_buffer_reservation_bytes: reservation,
                metadata_bytes: reservation,
                nodes_written: 0,
                work: 0,
                branch_scratch_peak_bytes: 0,
            },
            branch_scratch_bytes: 0,
        };
        builder.charge_work(1)?;
        let mut entries = Vec::new();
        entries
            .try_reserve_exact(len)
            .map_err(|_| OracleError::MetadataLimit)?;
        Ok((builder, entries))
    }

    fn finish(mut self, entries: &[Entry]) -> Result<BuiltIndex, OracleError> {
        let root = self.build_span(entries)?;
        Ok(BuiltIndex {
            kind: self.kind,
            root,
            nodes: self.nodes,
            counters: self.counters,
        })
    }

    fn charge_work(&mut self, amount: u64) -> Result<(), OracleError> {
        self.counters.work = self
            .counters
            .work
            .checked_add(amount)
            .ok_or(OracleError::ArithmeticOverflow)?;
        if self.counters.work > self.limits.max_work {
            Err(OracleError::WorkLimit)
        } else {
            Ok(())
        }
    }

    fn precharge_node(&mut self, encoded_len: usize, work: u64) -> Result<(), OracleError> {
        self.charge_work(work)?;
        let nodes = self
            .counters
            .nodes_written
            .checked_add(1)
            .ok_or(OracleError::ArithmeticOverflow)?;
        if nodes > self.limits.max_nodes {
            return Err(OracleError::NodeLimit);
        }
        let metadata = self
            .counters
            .metadata_bytes
            .checked_add(encoded_len as u64)
            .ok_or(OracleError::ArithmeticOverflow)?;
        if metadata > self.limits.max_metadata_bytes {
            return Err(OracleError::MetadataLimit);
        }
        self.counters.nodes_written = nodes;
        self.counters.metadata_bytes = metadata;
        Ok(())
    }

    fn retain_precharged_node(
        &mut self,
        summary: &NodeSummary,
        bytes: Vec<u8>,
    ) -> Result<(), OracleError> {
        if self.nodes.insert(summary.hash, bytes).is_some() {
            return Err(OracleError::DuplicateKey);
        }
        Ok(())
    }

    fn begin_branch_scratch(&mut self) -> Result<u64, OracleError> {
        self.charge_work(1)?;
        let bytes = (size_of::<(u8, NodeSummary)>() as u64)
            .checked_mul(16)
            .ok_or(OracleError::ArithmeticOverflow)?;
        let current = self
            .branch_scratch_bytes
            .checked_add(bytes)
            .ok_or(OracleError::ArithmeticOverflow)?;
        if current > self.limits.max_branch_scratch_bytes {
            return Err(OracleError::MetadataLimit);
        }
        self.branch_scratch_bytes = current;
        self.counters.branch_scratch_peak_bytes =
            self.counters.branch_scratch_peak_bytes.max(current);
        Ok(bytes)
    }

    fn end_branch_scratch(&mut self, bytes: u64) {
        self.branch_scratch_bytes -= bytes;
    }

    fn build_span(&mut self, entries: &[Entry]) -> Result<NodeSummary, OracleError> {
        self.charge_work(1)?;
        if entries.len() == 1 {
            let encoded_len = leaf_encoded_len(self.kind);
            self.precharge_node(encoded_len, 1)?;
            let bytes = encode_leaf(self.kind, entries[0])?;
            let summary = leaf_summary_from_bytes(self.kind, entries[0], &bytes)?;
            self.retain_precharged_node(&summary, bytes)?;
            return Ok(summary);
        }
        let depth = first_differing_nibble(&entries[0].key, &entries[entries.len() - 1].key)
            .ok_or(OracleError::DuplicateKey)?;
        let scratch = self.begin_branch_scratch()?;
        let built = (|| {
            let mut groups = Vec::<(u8, NodeSummary)>::new();
            groups
                .try_reserve_exact(16)
                .map_err(|_| OracleError::MetadataLimit)?;
            let mut start = 0usize;
            while start < entries.len() {
                self.charge_work(1)?;
                let slot = nibble(&entries[start].key, depth);
                let mut end = start + 1;
                while end < entries.len() {
                    self.charge_work(1)?;
                    if nibble(&entries[end].key, depth) != slot {
                        break;
                    }
                    end += 1;
                }
                groups.push((slot, self.build_span(&entries[start..end])?));
                start = end;
            }
            let encoded_len = branch_encoded_len(self.kind, groups.len())?;
            self.precharge_node(encoded_len, 1 + groups.len() as u64)?;
            let (bytes, summary) = encode_validated_branch(self.kind, depth, &groups)?;
            self.retain_precharged_node(&summary, bytes)?;
            Ok(summary)
        })();
        self.end_branch_scratch(scratch);
        built
    }
}

pub fn decode_stored_node(kind: IndexKind, bytes: &[u8]) -> Result<NodeSummary, OracleError> {
    if bytes.is_empty() || bytes.len() > MAX_STORED_NODE_BYTES {
        return Err(OracleError::SizeLimit);
    }
    let mut cursor = Cursor::new(bytes);
    let tag = cursor.u8()?;
    let summary = match tag {
        0 => {
            let key = cursor.id()?;
            let size = if kind == IndexKind::Descriptor {
                Some(cursor.u64()?)
            } else {
                None
            };
            leaf_summary(kind, Entry { key, size })?
        }
        1 => {
            let depth = cursor.u8()?;
            let count = cursor.u8()? as usize;
            if !(2..=16).contains(&count) {
                return Err(OracleError::InvalidShape);
            }
            let mut children = Vec::new();
            children
                .try_reserve_exact(count)
                .map_err(|_| OracleError::MetadataLimit)?;
            for _ in 0..count {
                children.push((cursor.u8()?, decode_summary(kind, &mut cursor)?));
            }
            let (canonical, summary) = encode_validated_branch(kind, depth, &children)?;
            if canonical.as_slice() != bytes {
                return Err(OracleError::InvalidShape);
            }
            summary
        }
        _ => return Err(OracleError::InvalidKind),
    };
    cursor.finish()?;
    Ok(summary)
}

pub fn decode_portable_identity(bytes: &[u8]) -> Result<PortableIdentity, OracleError> {
    let mut cursor = Cursor::new(bytes);
    if cursor.take(10)? != b"AGSCM-PC1\0" {
        return Err(OracleError::InvalidKind);
    }
    let profile = cursor.u8()?;
    if !(1..=3).contains(&profile) {
        return Err(OracleError::InvalidKind);
    }
    let count = cursor.u16()? as usize;
    if !(1..=256).contains(&count) {
        return Err(OracleError::InvalidShape);
    }
    let mut roots = Vec::new();
    roots
        .try_reserve_exact(count)
        .map_err(|_| OracleError::MetadataLimit)?;
    for _ in 0..count {
        roots.push(cursor.id()?);
    }
    if roots.windows(2).any(|pair| pair[0] >= pair[1]) {
        return Err(OracleError::UnsortedOrDuplicate);
    }
    let descriptor_root_hash = cursor.id()?;
    let object_count = cursor.u64()?;
    let encoded_source_bytes = cursor.u64()?;
    cursor.finish()?;
    Ok(PortableIdentity {
        profile,
        native_roots: roots,
        descriptor_root_hash,
        object_count,
        encoded_source_bytes,
        digest: domain_hash(PORTABLE_DOMAIN, bytes),
    })
}

fn encode_leaf(kind: IndexKind, entry: Entry) -> Result<Vec<u8>, OracleError> {
    let mut out = Vec::new();
    out.try_reserve_exact(leaf_encoded_len(kind))
        .map_err(|_| OracleError::MetadataLimit)?;
    out.push(0);
    out.extend_from_slice(&entry.key);
    match (kind, entry.size) {
        (IndexKind::Descriptor, Some(size)) if (1..=MAX_DESCRIPTOR_SIZE).contains(&size) => {
            out.extend_from_slice(&size.to_be_bytes());
        }
        (IndexKind::RevisionSet, None) => {}
        _ => return Err(OracleError::InvalidDescriptorSize),
    }
    Ok(out)
}

fn leaf_summary(kind: IndexKind, entry: Entry) -> Result<NodeSummary, OracleError> {
    validate_entry(kind, entry)?;
    let mut payload = [0u8; 41];
    payload[1..33].copy_from_slice(&entry.key);
    if let Some(size) = entry.size {
        payload[33..41].copy_from_slice(&size.to_be_bytes());
    }
    leaf_summary_from_bytes(kind, entry, &payload[..leaf_encoded_len(kind)])
}

fn leaf_summary_from_bytes(
    kind: IndexKind,
    entry: Entry,
    bytes: &[u8],
) -> Result<NodeSummary, OracleError> {
    validate_entry(kind, entry)?;
    if bytes.len() != leaf_encoded_len(kind) {
        return Err(OracleError::InvalidShape);
    }
    Ok(NodeSummary {
        kind: NodeKind::Leaf,
        hash: domain_hash(leaf_domain(kind), bytes),
        min: entry.key,
        max: entry.key,
        count: 1,
        size_total: entry.size,
    })
}

fn validate_entry(kind: IndexKind, entry: Entry) -> Result<(), OracleError> {
    match (kind, entry.size) {
        (IndexKind::Descriptor, Some(size)) if (1..=MAX_DESCRIPTOR_SIZE).contains(&size) => Ok(()),
        (IndexKind::RevisionSet, None) => Ok(()),
        _ => Err(OracleError::InvalidDescriptorSize),
    }
}

fn leaf_encoded_len(kind: IndexKind) -> usize {
    match kind {
        IndexKind::Descriptor => 41,
        IndexKind::RevisionSet => 33,
    }
}

fn branch_encoded_len(kind: IndexKind, children: usize) -> Result<usize, OracleError> {
    let summary_len = match kind {
        IndexKind::Descriptor => 113usize,
        IndexKind::RevisionSet => 105usize,
    };
    3usize
        .checked_add(
            children
                .checked_mul(1 + summary_len)
                .ok_or(OracleError::ArithmeticOverflow)?,
        )
        .ok_or(OracleError::ArithmeticOverflow)
}

fn encode_validated_branch(
    kind: IndexKind,
    depth: u8,
    children: &[(u8, NodeSummary)],
) -> Result<(Vec<u8>, NodeSummary), OracleError> {
    if depth >= 64 || !(2..=16).contains(&children.len()) {
        return Err(OracleError::InvalidShape);
    }
    let min = children.first().unwrap().1.min;
    let max = children.last().unwrap().1.max;
    if first_differing_nibble(&min, &max) != Some(depth) {
        return Err(OracleError::InvalidShape);
    }
    let mut count = 0u64;
    let mut size_total = if kind == IndexKind::Descriptor {
        Some(0u64)
    } else {
        None
    };
    let mut previous_slot = None;
    let mut previous_max = None;
    let encoded_len = branch_encoded_len(kind, children.len())?;
    let mut out = Vec::new();
    out.try_reserve_exact(encoded_len)
        .map_err(|_| OracleError::MetadataLimit)?;
    out.extend_from_slice(&[1, depth, children.len() as u8]);
    for (slot, child) in children {
        if *slot > 15 || previous_slot.map(|prior| prior >= *slot).unwrap_or(false) {
            return Err(OracleError::InvalidShape);
        }
        validate_summary(kind, child)?;
        if !same_prefix(&min, &child.min, depth)
            || nibble(&child.min, depth) != *slot
            || nibble(&child.max, depth) != *slot
            || previous_max
                .map(|prior| prior >= child.min)
                .unwrap_or(false)
        {
            return Err(OracleError::InvalidShape);
        }
        if child.kind == NodeKind::Branch {
            let child_depth = first_differing_nibble(&child.min, &child.max)
                .ok_or(OracleError::InvalidSummary)?;
            if child_depth <= depth {
                return Err(OracleError::InvalidShape);
            }
        }
        count = count
            .checked_add(child.count)
            .ok_or(OracleError::ArithmeticOverflow)?;
        if let Some(total) = &mut size_total {
            *total = total
                .checked_add(child.size_total.ok_or(OracleError::InvalidSummary)?)
                .ok_or(OracleError::ArithmeticOverflow)?;
        }
        out.push(*slot);
        encode_summary(kind, child, &mut out)?;
        previous_slot = Some(*slot);
        previous_max = Some(child.max);
    }
    let summary = NodeSummary {
        kind: NodeKind::Branch,
        hash: domain_hash(branch_domain(kind), &out),
        min,
        max,
        count,
        size_total,
    };
    validate_summary(kind, &summary)?;
    if out.len() != encoded_len {
        return Err(OracleError::InvalidShape);
    }
    Ok((out, summary))
}

fn validate_summary(kind: IndexKind, summary: &NodeSummary) -> Result<(), OracleError> {
    match summary.kind {
        NodeKind::Leaf => {
            if summary.min != summary.max || summary.count != 1 {
                return Err(OracleError::InvalidSummary);
            }
            let expected = leaf_summary(
                kind,
                Entry {
                    key: summary.min,
                    size: summary.size_total,
                },
            )?;
            if expected.hash != summary.hash {
                return Err(OracleError::InvalidSummary);
            }
        }
        NodeKind::Branch => {
            if summary.min >= summary.max || summary.count < 2 {
                return Err(OracleError::InvalidSummary);
            }
            if first_differing_nibble(&summary.min, &summary.max).is_none() {
                return Err(OracleError::InvalidSummary);
            }
            match (kind, summary.size_total) {
                (IndexKind::Descriptor, Some(total)) => {
                    let max = summary
                        .count
                        .checked_mul(MAX_DESCRIPTOR_SIZE)
                        .ok_or(OracleError::ArithmeticOverflow)?;
                    if total < summary.count || total > max {
                        return Err(OracleError::InvalidSummary);
                    }
                }
                (IndexKind::RevisionSet, None) => {}
                _ => return Err(OracleError::InvalidSummary),
            }
        }
    }
    Ok(())
}

fn encode_summary(
    kind: IndexKind,
    summary: &NodeSummary,
    out: &mut Vec<u8>,
) -> Result<(), OracleError> {
    out.push(match summary.kind {
        NodeKind::Leaf => 0,
        NodeKind::Branch => 1,
    });
    out.extend_from_slice(&summary.hash);
    out.extend_from_slice(&summary.min);
    out.extend_from_slice(&summary.max);
    out.extend_from_slice(&summary.count.to_be_bytes());
    if kind == IndexKind::Descriptor {
        out.extend_from_slice(
            &summary
                .size_total
                .ok_or(OracleError::InvalidSummary)?
                .to_be_bytes(),
        );
    }
    Ok(())
}

fn decode_summary(kind: IndexKind, cursor: &mut Cursor<'_>) -> Result<NodeSummary, OracleError> {
    let node_kind = match cursor.u8()? {
        0 => NodeKind::Leaf,
        1 => NodeKind::Branch,
        _ => return Err(OracleError::InvalidKind),
    };
    let summary = NodeSummary {
        kind: node_kind,
        hash: cursor.id()?,
        min: cursor.id()?,
        max: cursor.id()?,
        count: cursor.u64()?,
        size_total: if kind == IndexKind::Descriptor {
            Some(cursor.u64()?)
        } else {
            None
        },
    };
    validate_summary(kind, &summary)?;
    Ok(summary)
}

fn domain_hash(domain: &str, payload: &[u8]) -> Hash32 {
    let mut digest = Sha256::new();
    digest.update((domain.len() as u16).to_be_bytes());
    digest.update(domain.as_bytes());
    digest.update(payload);
    digest.finalize().into()
}

fn leaf_domain(kind: IndexKind) -> &'static str {
    match kind {
        IndexKind::Descriptor => DESC_LEAF_DOMAIN,
        IndexKind::RevisionSet => REV_LEAF_DOMAIN,
    }
}

fn branch_domain(kind: IndexKind) -> &'static str {
    match kind {
        IndexKind::Descriptor => DESC_BRANCH_DOMAIN,
        IndexKind::RevisionSet => REV_BRANCH_DOMAIN,
    }
}

fn nibble(key: &Hash32, depth: u8) -> u8 {
    let byte = key[(depth / 2) as usize];
    if depth.is_multiple_of(2) {
        byte >> 4
    } else {
        byte & 0x0f
    }
}

fn first_differing_nibble(left: &Hash32, right: &Hash32) -> Option<u8> {
    (0..64).find(|depth| nibble(left, *depth) != nibble(right, *depth))
}

fn same_prefix(left: &Hash32, right: &Hash32, depth: u8) -> bool {
    (0..depth).all(|position| nibble(left, position) == nibble(right, position))
}

struct Cursor<'a> {
    bytes: &'a [u8],
    offset: usize,
}

impl<'a> Cursor<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, offset: 0 }
    }
    fn take(&mut self, count: usize) -> Result<&'a [u8], OracleError> {
        let end = self
            .offset
            .checked_add(count)
            .ok_or(OracleError::ArithmeticOverflow)?;
        let value = self
            .bytes
            .get(self.offset..end)
            .ok_or(OracleError::Truncated)?;
        self.offset = end;
        Ok(value)
    }
    fn u8(&mut self) -> Result<u8, OracleError> {
        Ok(self.take(1)?[0])
    }
    fn u16(&mut self) -> Result<u16, OracleError> {
        Ok(u16::from_be_bytes(self.take(2)?.try_into().unwrap()))
    }
    fn u32(&mut self) -> Result<u32, OracleError> {
        Ok(u32::from_be_bytes(self.take(4)?.try_into().unwrap()))
    }
    fn u64(&mut self) -> Result<u64, OracleError> {
        Ok(u64::from_be_bytes(self.take(8)?.try_into().unwrap()))
    }
    fn id(&mut self) -> Result<Hash32, OracleError> {
        Ok(self.take(32)?.try_into().unwrap())
    }
    fn finish(&self) -> Result<(), OracleError> {
        if self.offset == self.bytes.len() {
            Ok(())
        } else {
            Err(OracleError::TrailingBytes)
        }
    }
}

#[derive(Clone, Copy)]
enum ProofOperation {
    Lookup,
    Range,
}

struct ProofState {
    kind: IndexKind,
    operation: ProofOperation,
    query: Option<Hash32>,
    nodes: u64,
    child_entries: u64,
    leaves: usize,
    max_levels: usize,
    expanded: Vec<NodeSummary>,
    opaque: Vec<NodeSummary>,
    range_entries: Vec<RangeEntry>,
}

struct Evaluated {
    summary: NodeSummary,
    lookup: Option<LookupResult>,
}

struct EvaluatedChild {
    slot: u8,
    summary: NodeSummary,
    expanded: bool,
    lookup: Option<LookupResult>,
}

struct ProofFrame {
    depth: u8,
    slots: Vec<u8>,
    next: usize,
    children: Vec<EvaluatedChild>,
}

pub fn verify_lookup(
    packet: &[u8],
    kind: IndexKind,
    expected_root: ExpectedRoot,
    expected_query: Hash32,
) -> Result<LookupResult, OracleError> {
    let body = proof_body(packet, kind, 1, LOOKUP_PACKET_CAP)?;
    let mut cursor = Cursor::new(body);
    let packet_query = cursor.id()?;
    if packet_query != expected_query {
        return Err(OracleError::RequestMismatch);
    }
    let mut state = ProofState::new(kind, ProofOperation::Lookup, Some(expected_query))?;
    let root = parse_expanded_root(&mut cursor, &mut state)?;
    cursor.finish()?;
    compare_root(kind, &root.summary, expected_root)?;
    root.lookup.ok_or(OracleError::NonCanonicalExpansion)
}

pub fn verify_range(
    packet: &[u8],
    kind: IndexKind,
    expected_root: ExpectedRoot,
    expected_request: RangeRequest,
) -> Result<RangeResult, OracleError> {
    let body = proof_body(packet, kind, 2, RANGE_PACKET_CAP)?;
    let mut cursor = Cursor::new(body);
    let packet_request = RangeRequest {
        ordinal: cursor.u32()?,
        frontier: cursor.id()?,
        leaf_limit: cursor.u16()?,
    };
    if packet_request != expected_request {
        return Err(OracleError::RequestMismatch);
    }
    if packet_request.ordinal == 0 && packet_request.frontier != [0; 32] {
        return Err(OracleError::RequestMismatch);
    }
    if !(1..=1024).contains(&packet_request.leaf_limit) {
        return Err(OracleError::LeafLimit);
    }
    let mut state = ProofState::new(kind, ProofOperation::Range, None)?;
    let root = parse_expanded_root(&mut cursor, &mut state)?;
    cursor.finish()?;
    compare_root(kind, &root.summary, expected_root)?;
    validate_range_trace(state, &root.summary, packet_request)
}

fn proof_body(
    packet: &[u8],
    expected_kind: IndexKind,
    expected_operation: u8,
    cap: usize,
) -> Result<&[u8], OracleError> {
    enforce_packet_size(packet.len(), cap)?;
    let mut cursor = Cursor::new(packet);
    if cursor.take(4)? != b"A3PW" || cursor.u8()? != 1 {
        return Err(OracleError::InvalidKind);
    }
    let kind = match cursor.u8()? {
        1 => IndexKind::Descriptor,
        2 => IndexKind::RevisionSet,
        _ => return Err(OracleError::InvalidKind),
    };
    if kind != expected_kind {
        return Err(OracleError::InvalidKind);
    }
    let operation = cursor.u8()?;
    if operation == 3 || (operation != 1 && operation != 2) {
        return Err(OracleError::UnsupportedOperation);
    }
    if operation != expected_operation {
        return Err(OracleError::RequestMismatch);
    }
    if cursor.u8()? != 0 {
        return Err(OracleError::InvalidShape);
    }
    let body_len = cursor.u32()? as usize;
    if body_len != packet.len().saturating_sub(12) {
        return Err(OracleError::InvalidShape);
    }
    cursor.take(body_len)
}

fn enforce_packet_size(actual: usize, cap: usize) -> Result<(), OracleError> {
    if actual > cap {
        Err(OracleError::SizeLimit)
    } else {
        Ok(())
    }
}

impl ProofState {
    fn new(
        kind: IndexKind,
        operation: ProofOperation,
        query: Option<Hash32>,
    ) -> Result<Self, OracleError> {
        let (node_cap, child_cap, leaf_cap) = match operation {
            ProofOperation::Lookup => (LOOKUP_NODE_CAP as usize, LOOKUP_CHILD_CAP as usize, 0),
            ProofOperation::Range => (
                RANGE_NODE_CAP as usize,
                RANGE_CHILD_CAP as usize,
                MAX_RANGE_LEAVES,
            ),
        };
        let mut expanded = Vec::new();
        expanded
            .try_reserve_exact(node_cap)
            .map_err(|_| OracleError::MetadataLimit)?;
        let mut opaque = Vec::new();
        opaque
            .try_reserve_exact(child_cap)
            .map_err(|_| OracleError::MetadataLimit)?;
        let mut range_entries = Vec::new();
        range_entries
            .try_reserve_exact(leaf_cap)
            .map_err(|_| OracleError::MetadataLimit)?;
        Ok(Self {
            kind,
            operation,
            query,
            nodes: 0,
            child_entries: 0,
            leaves: 0,
            max_levels: 0,
            expanded,
            opaque,
            range_entries,
        })
    }

    fn add_node(&mut self, levels: usize) -> Result<(), OracleError> {
        self.nodes = self
            .nodes
            .checked_add(1)
            .ok_or(OracleError::ArithmeticOverflow)?;
        let cap = match self.operation {
            ProofOperation::Lookup => LOOKUP_NODE_CAP,
            ProofOperation::Range => RANGE_NODE_CAP,
        };
        if self.nodes > cap {
            return Err(OracleError::NodeLimit);
        }
        self.max_levels = self.max_levels.max(levels);
        if self.max_levels > MAX_BRANCH_LEVELS {
            return Err(OracleError::DepthLimit);
        }
        Ok(())
    }

    fn add_children(&mut self, count: usize) -> Result<(), OracleError> {
        self.child_entries = self
            .child_entries
            .checked_add(count as u64)
            .ok_or(OracleError::ArithmeticOverflow)?;
        let cap = match self.operation {
            ProofOperation::Lookup => LOOKUP_CHILD_CAP,
            ProofOperation::Range => RANGE_CHILD_CAP,
        };
        if self.child_entries > cap {
            Err(OracleError::ChildLimit)
        } else {
            Ok(())
        }
    }

    fn add_leaf(&mut self, entry: RangeEntry) -> Result<(), OracleError> {
        self.leaves = self
            .leaves
            .checked_add(1)
            .ok_or(OracleError::ArithmeticOverflow)?;
        let cap = match self.operation {
            ProofOperation::Lookup => 1,
            ProofOperation::Range => MAX_RANGE_LEAVES,
        };
        if self.leaves > cap {
            return Err(OracleError::LeafLimit);
        }
        if matches!(self.operation, ProofOperation::Range) {
            self.range_entries.push(entry);
        }
        Ok(())
    }
}

fn parse_expanded_root(
    cursor: &mut Cursor<'_>,
    state: &mut ProofState,
) -> Result<Evaluated, OracleError> {
    let mut frames = Vec::<ProofFrame>::new();
    frames
        .try_reserve_exact(MAX_BRANCH_LEVELS)
        .map_err(|_| OracleError::MetadataLimit)?;
    let mut completed = start_expanded(cursor, state, &mut frames)?;
    loop {
        if let Some(node) = completed.take() {
            if let Some(frame) = frames.last_mut() {
                let slot = *frame
                    .slots
                    .get(frame.next)
                    .ok_or(OracleError::InvalidShape)?;
                frame.children.push(EvaluatedChild {
                    slot,
                    summary: node.summary,
                    expanded: true,
                    lookup: node.lookup,
                });
                frame.next += 1;
            } else {
                return Ok(node);
            }
        }

        let finished = frames
            .last()
            .map(|frame| frame.next == frame.slots.len())
            .ok_or(OracleError::InvalidShape)?;
        if finished {
            let frame = frames.pop().unwrap();
            completed = Some(finish_frame(state, frame)?);
            continue;
        }

        let (slot, marker) = {
            let frame = frames.last().unwrap();
            (
                *frame
                    .slots
                    .get(frame.next)
                    .ok_or(OracleError::InvalidShape)?,
                cursor.u8()?,
            )
        };
        match marker {
            0 => {
                completed = start_expanded(cursor, state, &mut frames)?;
            }
            1 => {
                let summary = decode_summary(state.kind, cursor)?;
                state.opaque.push(summary.clone());
                let frame = frames.last_mut().unwrap();
                frame.children.push(EvaluatedChild {
                    slot,
                    summary,
                    expanded: false,
                    lookup: None,
                });
                frame.next += 1;
            }
            _ => return Err(OracleError::InvalidKind),
        }
    }
}

fn start_expanded(
    cursor: &mut Cursor<'_>,
    state: &mut ProofState,
    frames: &mut Vec<ProofFrame>,
) -> Result<Option<Evaluated>, OracleError> {
    state.add_node(frames.len())?;
    match cursor.u8()? {
        0 => {
            let key = cursor.id()?;
            let size = if state.kind == IndexKind::Descriptor {
                Some(cursor.u64()?)
            } else {
                None
            };
            let summary = leaf_summary(state.kind, Entry { key, size })?;
            state.add_leaf(RangeEntry { key, size })?;
            state.expanded.push(summary.clone());
            let lookup = state.query.map(|query| {
                if query == key {
                    LookupResult::Member { size }
                } else {
                    LookupResult::Nonmember
                }
            });
            Ok(Some(Evaluated { summary, lookup }))
        }
        1 => {
            let depth = cursor.u8()?;
            let mask = cursor.u16()?;
            let mut slots = Vec::new();
            slots
                .try_reserve_exact(16)
                .map_err(|_| OracleError::MetadataLimit)?;
            for slot in 0u8..16 {
                if mask & (1u16 << (slot as u32)) != 0 {
                    slots.push(slot);
                }
            }
            if depth >= 64 || !(2..=16).contains(&slots.len()) {
                return Err(OracleError::InvalidShape);
            }
            state.add_children(slots.len())?;
            if frames.len() + 1 > MAX_BRANCH_LEVELS {
                return Err(OracleError::DepthLimit);
            }
            let mut children = Vec::new();
            children
                .try_reserve_exact(slots.len())
                .map_err(|_| OracleError::MetadataLimit)?;
            frames.push(ProofFrame {
                depth,
                slots,
                next: 0,
                children,
            });
            Ok(None)
        }
        _ => Err(OracleError::InvalidKind),
    }
}

fn finish_frame(state: &mut ProofState, frame: ProofFrame) -> Result<Evaluated, OracleError> {
    if frame.children.len() != frame.slots.len() {
        return Err(OracleError::InvalidShape);
    }
    let mut summaries = Vec::new();
    summaries
        .try_reserve_exact(frame.children.len())
        .map_err(|_| OracleError::MetadataLimit)?;
    summaries.extend(
        frame
            .children
            .iter()
            .map(|child| (child.slot, child.summary.clone())),
    );
    let (_, summary) = encode_validated_branch(state.kind, frame.depth, &summaries)?;
    let lookup = if let Some(query) = state.query {
        let on_prefix = same_prefix(&query, &summary.min, frame.depth);
        let query_slot = nibble(&query, frame.depth);
        let selected = frame
            .children
            .iter()
            .position(|child| child.slot == query_slot);
        if let (true, Some(selected)) = (on_prefix, selected) {
            if frame
                .children
                .iter()
                .enumerate()
                .any(|(index, child)| child.expanded != (index == selected))
            {
                return Err(OracleError::NonCanonicalExpansion);
            }
            Some(
                frame.children[selected]
                    .lookup
                    .ok_or(OracleError::NonCanonicalExpansion)?,
            )
        } else {
            if frame.children.iter().any(|child| child.expanded) {
                return Err(OracleError::NonCanonicalExpansion);
            }
            Some(LookupResult::Nonmember)
        }
    } else {
        None
    };
    state.expanded.push(summary.clone());
    Ok(Evaluated { summary, lookup })
}

fn compare_root(
    kind: IndexKind,
    actual: &NodeSummary,
    expected: ExpectedRoot,
) -> Result<(), OracleError> {
    let expected_size = match kind {
        IndexKind::Descriptor => expected.size_total.ok_or(OracleError::RootMismatch)?,
        IndexKind::RevisionSet => {
            if expected.size_total.is_some() {
                return Err(OracleError::RootMismatch);
            }
            0
        }
    };
    if actual.hash != expected.hash
        || actual.count != expected.count
        || (kind == IndexKind::Descriptor && actual.size_total != Some(expected_size))
    {
        Err(OracleError::RootMismatch)
    } else {
        Ok(())
    }
}

fn validate_range_trace(
    state: ProofState,
    root: &NodeSummary,
    request: RangeRequest,
) -> Result<RangeResult, OracleError> {
    let entries = state.range_entries;
    if entries.is_empty() || entries.len() > request.leaf_limit as usize {
        return Err(OracleError::LeafLimit);
    }
    if entries.windows(2).any(|pair| pair[0].key >= pair[1].key)
        || entries.iter().any(|entry| entry.key < request.frontier)
    {
        return Err(OracleError::NonCanonicalExpansion);
    }
    let last = entries.last().unwrap().key;
    let terminal = last == root.max;
    let end = if terminal {
        None
    } else {
        if entries.len() != request.leaf_limit as usize {
            return Err(OracleError::NonterminalShortPage);
        }
        Some(successor(last).ok_or(OracleError::FrontierOverflow)?)
    };
    for summary in &state.expanded {
        if !intersects(summary, request.frontier, end) {
            return Err(OracleError::NonCanonicalExpansion);
        }
    }
    for summary in &state.opaque {
        if intersects(summary, request.frontier, end) {
            return Err(OracleError::OmittedInterior);
        }
    }
    let next_request = match end {
        None => None,
        Some(frontier) => Some(RangeRequest {
            ordinal: request
                .ordinal
                .checked_add(1)
                .ok_or(OracleError::FrontierOverflow)?,
            frontier,
            leaf_limit: request.leaf_limit,
        }),
    };
    Ok(RangeResult {
        entries,
        next_request,
    })
}

fn intersects(summary: &NodeSummary, start: Hash32, end: Option<Hash32>) -> bool {
    summary.max >= start && end.map(|exclusive| summary.min < exclusive).unwrap_or(true)
}

fn successor(mut key: Hash32) -> Option<Hash32> {
    for byte in key.iter_mut().rev() {
        if *byte != 0xff {
            *byte += 1;
            return Some(key);
        }
        *byte = 0;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(text: &str) -> Vec<u8> {
        assert_eq!(text.len() % 2, 0);
        text.as_bytes()
            .chunks_exact(2)
            .map(|pair| {
                let digit = |byte| match byte {
                    b'0'..=b'9' => byte - b'0',
                    b'a'..=b'f' => byte - b'a' + 10,
                    _ => panic!("invalid test hex"),
                };
                (digit(pair[0]) << 4) | digit(pair[1])
            })
            .collect()
    }

    fn id(text: &str) -> Hash32 {
        hex(text).try_into().unwrap()
    }

    fn key(first: u8, rest: u8) -> Hash32 {
        let mut key = [rest; 32];
        key[0] = first;
        key
    }

    fn abc_entries() -> [(Hash32, u64); 3] {
        [([0; 32], 9), (key(0x0f, 0xff), 10), (key(0x10, 0), 11)]
    }

    fn abc_summaries() -> (
        NodeSummary,
        NodeSummary,
        NodeSummary,
        NodeSummary,
        NodeSummary,
    ) {
        let entries = abc_entries();
        let a = leaf_summary(
            IndexKind::Descriptor,
            Entry {
                key: entries[0].0,
                size: Some(9),
            },
        )
        .unwrap();
        let b = leaf_summary(
            IndexKind::Descriptor,
            Entry {
                key: entries[1].0,
                size: Some(10),
            },
        )
        .unwrap();
        let c = leaf_summary(
            IndexKind::Descriptor,
            Entry {
                key: entries[2].0,
                size: Some(11),
            },
        )
        .unwrap();
        let (_, ab) =
            encode_validated_branch(IndexKind::Descriptor, 1, &[(0, a.clone()), (15, b.clone())])
                .unwrap();
        let (_, abc) =
            encode_validated_branch(IndexKind::Descriptor, 0, &[(0, ab.clone()), (1, c.clone())])
                .unwrap();
        (a, b, c, ab, abc)
    }

    fn expanded_leaf(key: Hash32, size: Option<u64>) -> Vec<u8> {
        let mut out = vec![0];
        out.extend_from_slice(&key);
        if let Some(size) = size {
            out.extend_from_slice(&size.to_be_bytes());
        }
        out
    }

    fn opaque(kind: IndexKind, summary: &NodeSummary) -> Vec<u8> {
        let mut out = vec![1];
        encode_summary(kind, summary, &mut out).unwrap();
        out
    }

    fn expanded(node: Vec<u8>) -> Vec<u8> {
        let mut out = vec![0];
        out.extend_from_slice(&node);
        out
    }

    fn expanded_branch(depth: u8, children: &[(u8, Vec<u8>)]) -> Vec<u8> {
        let mut mask = 0u16;
        for (slot, _) in children {
            mask |= 1u16 << (*slot as u32);
        }
        let mut out = vec![1, depth];
        out.extend_from_slice(&mask.to_be_bytes());
        for (_, child) in children {
            out.extend_from_slice(child);
        }
        out
    }

    fn packet(kind: IndexKind, operation: u8, body: Vec<u8>) -> Vec<u8> {
        let mut out = b"A3PW".to_vec();
        out.extend_from_slice(&[
            1,
            if kind == IndexKind::Descriptor { 1 } else { 2 },
            operation,
            0,
        ]);
        out.extend_from_slice(&(body.len() as u32).to_be_bytes());
        out.extend_from_slice(&body);
        out
    }

    fn lookup_packet(kind: IndexKind, query: Hash32, root: Vec<u8>) -> Vec<u8> {
        let mut body = query.to_vec();
        body.extend_from_slice(&root);
        packet(kind, 1, body)
    }

    fn range_packet(kind: IndexKind, request: RangeRequest, root: Vec<u8>) -> Vec<u8> {
        let mut body = request.ordinal.to_be_bytes().to_vec();
        body.extend_from_slice(&request.frontier);
        body.extend_from_slice(&request.leaf_limit.to_be_bytes());
        body.extend_from_slice(&root);
        packet(kind, 2, body)
    }

    fn expected(summary: &NodeSummary) -> ExpectedRoot {
        ExpectedRoot {
            hash: summary.hash,
            count: summary.count,
            size_total: summary.size_total,
        }
    }

    fn identity_bytes(profile: u8, roots: &[Hash32], descriptor_root: Hash32) -> Vec<u8> {
        let mut bytes = b"AGSCM-PC1\0".to_vec();
        bytes.push(profile);
        bytes.extend_from_slice(&(roots.len() as u16).to_be_bytes());
        for root in roots {
            bytes.extend_from_slice(root);
        }
        bytes.extend_from_slice(&descriptor_root);
        bytes.extend_from_slice(&(roots.len() as u64).to_be_bytes());
        bytes.extend_from_slice(&99u64.to_be_bytes());
        bytes
    }

    fn abc_a_lookup_proof() -> Vec<u8> {
        let (a, b, c, _, _) = abc_summaries();
        let ab = expanded_branch(
            1,
            &[
                (0, expanded(expanded_leaf(a.min, a.size_total))),
                (15, opaque(IndexKind::Descriptor, &b)),
            ],
        );
        expanded_branch(
            0,
            &[(0, expanded(ab)), (1, opaque(IndexKind::Descriptor, &c))],
        )
    }

    fn abc_first_range_proof() -> Vec<u8> {
        abc_a_lookup_proof()
    }

    #[test]
    fn canonical_builder_matches_known_abc_root_and_decodes_both_kinds() {
        let built = build_descriptor_index(&abc_entries(), BuildLimits::default()).unwrap();
        assert_eq!(
            built.root.hash,
            id("3ceaa62121c0eeac2b8bbee6f4fbb1c134fc35d614c8d3b55b0f83f131d191c0")
        );
        assert_eq!(built.root.count, 3);
        assert_eq!(built.root.size_total, Some(30));
        assert_eq!(
            decode_stored_node(IndexKind::Descriptor, &built.nodes[&built.root.hash]).unwrap(),
            built.root
        );

        let revisions = build_revision_index(&[[0; 32], [1; 32]], BuildLimits::default()).unwrap();
        assert_eq!(revisions.root.size_total, None);
        assert_eq!(
            decode_stored_node(
                IndexKind::RevisionSet,
                &revisions.nodes[&revisions.root.hash]
            )
            .unwrap(),
            revisions.root
        );
        assert_eq!(
            leaf_summary(
                IndexKind::RevisionSet,
                Entry {
                    key: [0; 32],
                    size: None
                }
            )
            .unwrap()
            .hash,
            id("85dcd0fd7a7023eb9cfc19ebe80ca47e37297b250815565588fce29f9f2591ee")
        );
    }

    #[test]
    fn builder_rejects_empty_duplicates_sizes_limits_and_checked_overflow() {
        assert!(matches!(
            build_descriptor_index(&[], BuildLimits::default()),
            Err(OracleError::EmptySet)
        ));
        assert!(matches!(
            build_descriptor_index(&[([0; 32], 1), ([0; 32], 1)], BuildLimits::default()),
            Err(OracleError::UnsortedOrDuplicate)
        ));
        assert!(matches!(
            build_descriptor_index(&[([0; 32], 0)], BuildLimits::default()),
            Err(OracleError::InvalidDescriptorSize)
        ));
        assert!(matches!(
            build_descriptor_index(
                &[([0; 32], MAX_DESCRIPTOR_SIZE + 1)],
                BuildLimits::default()
            ),
            Err(OracleError::InvalidDescriptorSize)
        ));
        let limits = BuildLimits {
            max_entries: 1,
            ..BuildLimits::default()
        };
        assert!(matches!(
            build_revision_index(&[[0; 32], [1; 32]], limits),
            Err(OracleError::SizeLimit)
        ));
        let overflowing = NodeSummary {
            kind: NodeKind::Branch,
            hash: [1; 32],
            min: [0; 32],
            max: [0xff; 32],
            count: u64::MAX,
            size_total: Some(u64::MAX),
        };
        assert_eq!(
            validate_summary(IndexKind::Descriptor, &overflowing),
            Err(OracleError::ArithmeticOverflow)
        );
        assert_eq!(successor([0xff; 32]), None);
    }

    #[test]
    fn private_work_and_retention_are_precharged_before_covered_operations() {
        let limits = BuildLimits {
            max_work: 0,
            ..BuildLimits::default()
        };
        let deliberately_invalid_later = [([0; 32], 1), ([0; 32], 0)];
        assert_eq!(
            build_descriptor_index(&deliberately_invalid_later, limits),
            Err(OracleError::WorkLimit)
        );

        let (mut node_limited, _) = Builder::start(
            IndexKind::Descriptor,
            1,
            BuildLimits {
                max_nodes: 0,
                ..BuildLimits::default()
            },
        )
        .unwrap();
        assert_eq!(
            node_limited.precharge_node(leaf_encoded_len(IndexKind::Descriptor), 1),
            Err(OracleError::NodeLimit)
        );
        assert!(node_limited.nodes.is_empty());
        assert_eq!(node_limited.counters.nodes_written, 0);

        let reservation = size_of::<Entry>() as u64;
        let metadata_limits = BuildLimits {
            max_metadata_bytes: reservation,
            ..BuildLimits::default()
        };
        let (mut metadata_limited, _) =
            Builder::start(IndexKind::Descriptor, 1, metadata_limits).unwrap();
        assert_eq!(
            metadata_limited.precharge_node(leaf_encoded_len(IndexKind::Descriptor), 1),
            Err(OracleError::MetadataLimit)
        );
        assert!(metadata_limited.nodes.is_empty());
        assert_eq!(metadata_limited.counters.metadata_bytes, reservation);

        let scratch_limited = BuildLimits {
            max_branch_scratch_bytes: 0,
            ..BuildLimits::default()
        };
        assert_eq!(
            build_descriptor_index(&[([0; 32], 1), ([1; 32], 1)], scratch_limited),
            Err(OracleError::MetadataLimit)
        );
    }

    #[test]
    fn rejects_bad_stored_shapes_and_accepts_empty_set_never() {
        assert!(matches!(
            decode_stored_node(IndexKind::Descriptor, &[1, 0, 1]),
            Err(OracleError::InvalidShape)
        ));
        assert!(matches!(
            decode_stored_node(IndexKind::Descriptor, &[9]),
            Err(OracleError::InvalidKind)
        ));
        let oversized = vec![0; MAX_STORED_NODE_BYTES + 1];
        assert_eq!(
            decode_stored_node(IndexKind::RevisionSet, &oversized),
            Err(OracleError::SizeLimit)
        );
    }

    #[test]
    fn rejects_malformed_summaries_depth_prefix_slot_and_ranges() {
        let (a, b, c, ab, _) = abc_summaries();
        let mut bad_leaf = a.clone();
        bad_leaf.hash[0] ^= 1;
        let mut encoded = Vec::new();
        encode_summary(IndexKind::Descriptor, &bad_leaf, &mut encoded).unwrap();
        assert_eq!(
            decode_summary(IndexKind::Descriptor, &mut Cursor::new(&encoded)),
            Err(OracleError::InvalidSummary)
        );

        let bad_count = NodeSummary {
            count: 1,
            ..ab.clone()
        };
        let bad_size = NodeSummary {
            size_total: Some(1),
            ..ab.clone()
        };
        let bad_range = NodeSummary {
            max: ab.min,
            ..ab.clone()
        };
        assert_eq!(
            validate_summary(IndexKind::Descriptor, &bad_count),
            Err(OracleError::InvalidSummary)
        );
        assert_eq!(
            validate_summary(IndexKind::Descriptor, &bad_size),
            Err(OracleError::InvalidSummary)
        );
        assert_eq!(
            validate_summary(IndexKind::Descriptor, &bad_range),
            Err(OracleError::InvalidSummary)
        );

        assert!(matches!(
            encode_validated_branch(IndexKind::Descriptor, 0, &[(0, a.clone()), (15, b.clone())]),
            Err(OracleError::InvalidShape)
        ));
        assert!(matches!(
            encode_validated_branch(IndexKind::Descriptor, 1, &[(1, a.clone()), (15, b.clone())]),
            Err(OracleError::InvalidShape)
        ));
        let foreign_prefix = leaf_summary(
            IndexKind::Descriptor,
            Entry {
                key: key(0x18, 0),
                size: Some(1),
            },
        )
        .unwrap();
        assert!(matches!(
            encode_validated_branch(
                IndexKind::Descriptor,
                1,
                &[(0, a.clone()), (8, foreign_prefix), (15, b.clone())]
            ),
            Err(OracleError::InvalidShape)
        ));
        assert!(matches!(
            encode_validated_branch(IndexKind::Descriptor, 0, &[(0, c), (1, ab)]),
            Err(OracleError::InvalidShape)
        ));
    }

    #[test]
    fn portable_identity_rejects_profile_count_order_and_trailing_bytes() {
        let root0 = [0; 32];
        let root1 = [1; 32];
        let valid = identity_bytes(1, &[root0, root1], [9; 32]);
        assert!(decode_portable_identity(&valid).is_ok());

        let mut profile = valid.clone();
        profile[10] = 4;
        assert_eq!(
            decode_portable_identity(&profile),
            Err(OracleError::InvalidKind)
        );
        let mut zero_count = valid.clone();
        zero_count[11..13].copy_from_slice(&0u16.to_be_bytes());
        assert_eq!(
            decode_portable_identity(&zero_count),
            Err(OracleError::InvalidShape)
        );
        let mut excessive_count = valid.clone();
        excessive_count[11..13].copy_from_slice(&257u16.to_be_bytes());
        assert_eq!(
            decode_portable_identity(&excessive_count),
            Err(OracleError::InvalidShape)
        );
        let unordered = identity_bytes(1, &[root1, root0], [9; 32]);
        assert_eq!(
            decode_portable_identity(&unordered),
            Err(OracleError::UnsortedOrDuplicate)
        );
        let mut trailing = valid;
        trailing.push(0);
        assert_eq!(
            decode_portable_identity(&trailing),
            Err(OracleError::TrailingBytes)
        );
    }

    #[test]
    fn verifies_lookup_membership_for_both_kinds_and_request_binding() {
        let descriptor_leaf = leaf_summary(
            IndexKind::Descriptor,
            Entry {
                key: [0; 32],
                size: Some(9),
            },
        )
        .unwrap();
        let descriptor_packet = lookup_packet(
            IndexKind::Descriptor,
            [0; 32],
            expanded_leaf([0; 32], Some(9)),
        );
        assert_eq!(
            verify_lookup(
                &descriptor_packet,
                IndexKind::Descriptor,
                expected(&descriptor_leaf),
                [0; 32]
            )
            .unwrap(),
            LookupResult::Member { size: Some(9) }
        );
        assert_eq!(
            verify_lookup(
                &descriptor_packet,
                IndexKind::Descriptor,
                expected(&descriptor_leaf),
                [1; 32]
            ),
            Err(OracleError::RequestMismatch)
        );

        let revision_leaf = leaf_summary(
            IndexKind::RevisionSet,
            Entry {
                key: [0; 32],
                size: None,
            },
        )
        .unwrap();
        let revision_packet = lookup_packet(
            IndexKind::RevisionSet,
            [0; 32],
            expanded_leaf([0; 32], None),
        );
        assert_eq!(
            verify_lookup(
                &revision_packet,
                IndexKind::RevisionSet,
                expected(&revision_leaf),
                [0; 32]
            )
            .unwrap(),
            LookupResult::Member { size: None }
        );
    }

    #[test]
    fn rejects_noncanonical_packet_headers_lengths_and_descriptor_sizes() {
        let leaf = leaf_summary(
            IndexKind::Descriptor,
            Entry {
                key: [0; 32],
                size: Some(9),
            },
        )
        .unwrap();
        let valid = lookup_packet(
            IndexKind::Descriptor,
            [0; 32],
            expanded_leaf([0; 32], Some(9)),
        );
        let verify =
            |packet: &[u8]| verify_lookup(packet, IndexKind::Descriptor, expected(&leaf), [0; 32]);

        let mut bad_magic = valid.clone();
        bad_magic[0] ^= 1;
        assert_eq!(verify(&bad_magic), Err(OracleError::InvalidKind));
        let mut bad_version = valid.clone();
        bad_version[4] = 2;
        assert_eq!(verify(&bad_version), Err(OracleError::InvalidKind));
        let mut cross_kind = valid.clone();
        cross_kind[5] = 2;
        assert_eq!(verify(&cross_kind), Err(OracleError::InvalidKind));
        let mut flags = valid.clone();
        flags[7] = 1;
        assert_eq!(verify(&flags), Err(OracleError::InvalidShape));
        let mut body_len = valid.clone();
        let declared = u32::from_be_bytes(body_len[8..12].try_into().unwrap());
        body_len[8..12].copy_from_slice(&(declared + 1).to_be_bytes());
        assert_eq!(verify(&body_len), Err(OracleError::InvalidShape));
        let mut truncated = valid.clone();
        truncated.pop();
        let truncated_len = (truncated.len() - 12) as u32;
        truncated[8..12].copy_from_slice(&truncated_len.to_be_bytes());
        assert_eq!(verify(&truncated), Err(OracleError::Truncated));
        let mut trailing = valid;
        trailing.push(0);
        let trailing_len = (trailing.len() - 12) as u32;
        trailing[8..12].copy_from_slice(&trailing_len.to_be_bytes());
        assert_eq!(verify(&trailing), Err(OracleError::TrailingBytes));

        let zero = lookup_packet(
            IndexKind::Descriptor,
            [0; 32],
            expanded_leaf([0; 32], Some(0)),
        );
        assert_eq!(verify(&zero), Err(OracleError::InvalidDescriptorSize));
        let oversized = lookup_packet(
            IndexKind::Descriptor,
            [0; 32],
            expanded_leaf([0; 32], Some(MAX_DESCRIPTOR_SIZE + 1)),
        );
        assert_eq!(verify(&oversized), Err(OracleError::InvalidDescriptorSize));
    }

    #[test]
    fn enforces_each_operation_codec_cap_at_its_boundary() {
        assert_eq!(
            enforce_packet_size(LOOKUP_PACKET_CAP, LOOKUP_PACKET_CAP),
            Ok(())
        );
        assert_eq!(
            enforce_packet_size(LOOKUP_PACKET_CAP + 1, LOOKUP_PACKET_CAP),
            Err(OracleError::SizeLimit)
        );
        assert_eq!(
            enforce_packet_size(RANGE_PACKET_CAP, RANGE_PACKET_CAP),
            Ok(())
        );
        assert_eq!(
            enforce_packet_size(RANGE_PACKET_CAP + 1, RANGE_PACKET_CAP),
            Err(OracleError::SizeLimit)
        );

        let mut lookup =
            ProofState::new(IndexKind::Descriptor, ProofOperation::Lookup, Some([0; 32])).unwrap();
        lookup.nodes = LOOKUP_NODE_CAP - 1;
        assert_eq!(lookup.add_node(MAX_BRANCH_LEVELS), Ok(()));
        assert_eq!(lookup.add_node(0), Err(OracleError::NodeLimit));
        lookup.child_entries = LOOKUP_CHILD_CAP - 1;
        assert_eq!(lookup.add_children(1), Ok(()));
        assert_eq!(lookup.add_children(1), Err(OracleError::ChildLimit));
        lookup.leaves = 0;
        assert_eq!(
            lookup.add_leaf(RangeEntry {
                key: [0; 32],
                size: Some(1)
            }),
            Ok(())
        );
        assert_eq!(
            lookup.add_leaf(RangeEntry {
                key: [1; 32],
                size: Some(1)
            }),
            Err(OracleError::LeafLimit)
        );
        let mut lookup_depth = ProofState::new(
            IndexKind::RevisionSet,
            ProofOperation::Lookup,
            Some([0; 32]),
        )
        .unwrap();
        assert_eq!(lookup_depth.add_node(MAX_BRANCH_LEVELS), Ok(()));
        assert_eq!(
            lookup_depth.add_node(MAX_BRANCH_LEVELS + 1),
            Err(OracleError::DepthLimit)
        );

        let mut range =
            ProofState::new(IndexKind::Descriptor, ProofOperation::Range, None).unwrap();
        range.nodes = RANGE_NODE_CAP - 1;
        assert_eq!(range.add_node(MAX_BRANCH_LEVELS), Ok(()));
        assert_eq!(
            range.add_node(MAX_BRANCH_LEVELS + 1),
            Err(OracleError::NodeLimit)
        );
        range.child_entries = RANGE_CHILD_CAP - 1;
        assert_eq!(range.add_children(1), Ok(()));
        assert_eq!(range.add_children(1), Err(OracleError::ChildLimit));
        range.leaves = MAX_RANGE_LEAVES - 1;
        assert_eq!(
            range.add_leaf(RangeEntry {
                key: [0; 32],
                size: Some(1)
            }),
            Ok(())
        );
        assert_eq!(
            range.add_leaf(RangeEntry {
                key: [1; 32],
                size: Some(1)
            }),
            Err(OracleError::LeafLimit)
        );

        let mut depth =
            ProofState::new(IndexKind::RevisionSet, ProofOperation::Range, None).unwrap();
        assert_eq!(depth.add_node(MAX_BRANCH_LEVELS), Ok(()));
        assert_eq!(
            depth.add_node(MAX_BRANCH_LEVELS + 1),
            Err(OracleError::DepthLimit)
        );
    }

    #[test]
    fn rejects_lookup_off_path_expansion_and_opaque_on_path() {
        let (a, b, c, ab, abc) = abc_summaries();
        let member_packet = lookup_packet(IndexKind::Descriptor, a.min, abc_a_lookup_proof());
        assert_eq!(
            verify_lookup(&member_packet, IndexKind::Descriptor, expected(&abc), a.min).unwrap(),
            LookupResult::Member { size: Some(9) }
        );

        let all_opaque_root = expanded_branch(
            0,
            &[
                (0, opaque(IndexKind::Descriptor, &ab)),
                (1, opaque(IndexKind::Descriptor, &c)),
            ],
        );
        let missing_slot_query = key(0x20, 0);
        let missing_slot_packet =
            lookup_packet(IndexKind::Descriptor, missing_slot_query, all_opaque_root);
        assert_eq!(
            verify_lookup(
                &missing_slot_packet,
                IndexKind::Descriptor,
                expected(&abc),
                missing_slot_query
            )
            .unwrap(),
            LookupResult::Nonmember
        );

        let prefix_mismatch_root = expanded_branch(
            1,
            &[
                (0, opaque(IndexKind::Descriptor, &a)),
                (15, opaque(IndexKind::Descriptor, &b)),
            ],
        );
        let prefix_mismatch_query = key(0x10, 0);
        let prefix_mismatch_packet = lookup_packet(
            IndexKind::Descriptor,
            prefix_mismatch_query,
            prefix_mismatch_root,
        );
        assert_eq!(
            verify_lookup(
                &prefix_mismatch_packet,
                IndexKind::Descriptor,
                expected(&ab),
                prefix_mismatch_query
            )
            .unwrap(),
            LookupResult::Nonmember
        );

        let ab_all_opaque = expanded_branch(
            1,
            &[
                (0, opaque(IndexKind::Descriptor, &a)),
                (15, opaque(IndexKind::Descriptor, &b)),
            ],
        );
        let off_path_root = expanded_branch(
            0,
            &[
                (0, expanded(ab_all_opaque)),
                (1, opaque(IndexKind::Descriptor, &c)),
            ],
        );
        let off_path_query = key(0x20, 0);
        let off_path_packet = lookup_packet(IndexKind::Descriptor, off_path_query, off_path_root);
        assert_eq!(
            verify_lookup(
                &off_path_packet,
                IndexKind::Descriptor,
                expected(&abc),
                off_path_query
            ),
            Err(OracleError::NonCanonicalExpansion)
        );

        let opaque_path_root = expanded_branch(
            0,
            &[
                (0, opaque(IndexKind::Descriptor, &ab)),
                (1, opaque(IndexKind::Descriptor, &c)),
            ],
        );
        let opaque_packet = lookup_packet(IndexKind::Descriptor, [0; 32], opaque_path_root);
        assert_eq!(
            verify_lookup(
                &opaque_packet,
                IndexKind::Descriptor,
                expected(&abc),
                [0; 32]
            ),
            Err(OracleError::NonCanonicalExpansion)
        );
    }

    #[test]
    fn range_derives_frontier_and_rejects_replay_short_pages_and_omissions() {
        let (a, b, c, _, abc) = abc_summaries();
        let request = RangeRequest {
            ordinal: 0,
            frontier: [0; 32],
            leaf_limit: 1,
        };
        let first_packet = range_packet(IndexKind::Descriptor, request, abc_first_range_proof());
        let first = verify_range(
            &first_packet,
            IndexKind::Descriptor,
            expected(&abc),
            request,
        )
        .unwrap();
        assert_eq!(
            first.entries,
            vec![RangeEntry {
                key: a.min,
                size: Some(9)
            }]
        );
        assert_eq!(
            first.next_request,
            Some(RangeRequest {
                ordinal: 1,
                frontier: successor(a.min).unwrap(),
                leaf_limit: 1
            })
        );
        assert_eq!(
            verify_range(
                &first_packet,
                IndexKind::Descriptor,
                expected(&abc),
                RangeRequest {
                    ordinal: 1,
                    ..request
                }
            ),
            Err(OracleError::RequestMismatch)
        );

        let second_ab = expanded_branch(
            1,
            &[
                (0, opaque(IndexKind::Descriptor, &a)),
                (15, expanded(expanded_leaf(b.min, Some(10)))),
            ],
        );
        let second_root = expanded_branch(
            0,
            &[
                (0, expanded(second_ab)),
                (1, expanded(expanded_leaf(c.min, Some(11)))),
            ],
        );
        // Independent limit-2 request; continuation of the first page would retain limit 1.
        let second_request = RangeRequest {
            ordinal: 1,
            frontier: successor(a.min).unwrap(),
            leaf_limit: 2,
        };
        let second_packet = range_packet(IndexKind::Descriptor, second_request, second_root);
        let second = verify_range(
            &second_packet,
            IndexKind::Descriptor,
            expected(&abc),
            second_request,
        )
        .unwrap();
        assert_eq!(
            second.entries,
            vec![
                RangeEntry {
                    key: b.min,
                    size: Some(10)
                },
                RangeEntry {
                    key: c.min,
                    size: Some(11)
                }
            ]
        );
        assert_eq!(second.next_request, None);

        let short_request = RangeRequest {
            leaf_limit: 2,
            ..request
        };
        let short_packet = range_packet(
            IndexKind::Descriptor,
            short_request,
            abc_first_range_proof(),
        );
        assert_eq!(
            verify_range(
                &short_packet,
                IndexKind::Descriptor,
                expected(&abc),
                short_request
            ),
            Err(OracleError::NonterminalShortPage)
        );

        let ab_with_gap = expanded_branch(
            1,
            &[
                (0, expanded(expanded_leaf(a.min, Some(9)))),
                (15, opaque(IndexKind::Descriptor, &b)),
            ],
        );
        let omitted_root = expanded_branch(
            0,
            &[
                (0, expanded(ab_with_gap)),
                (1, expanded(expanded_leaf(c.min, Some(11)))),
            ],
        );
        let omitted_request = RangeRequest {
            ordinal: 0,
            frontier: [0; 32],
            leaf_limit: 2,
        };
        let omitted_packet = range_packet(IndexKind::Descriptor, omitted_request, omitted_root);
        assert_eq!(
            verify_range(
                &omitted_packet,
                IndexKind::Descriptor,
                expected(&abc),
                omitted_request
            ),
            Err(OracleError::OmittedInterior)
        );
    }

    #[test]
    fn range_handles_nonzero_frontiers_ordinal_overflow_and_terminal_max_key() {
        let (_, _, _, _, abc) = abc_summaries();
        let request = RangeRequest {
            ordinal: u32::MAX,
            frontier: [0; 32],
            leaf_limit: 1,
        };
        let packet = range_packet(IndexKind::Descriptor, request, abc_first_range_proof());
        assert_eq!(
            verify_range(&packet, IndexKind::Descriptor, expected(&abc), request),
            Err(OracleError::FrontierOverflow)
        );

        let max_leaf = leaf_summary(
            IndexKind::Descriptor,
            Entry {
                key: [0xff; 32],
                size: Some(1),
            },
        )
        .unwrap();
        let terminal_request = RangeRequest {
            ordinal: 4,
            frontier: [0xfe; 32],
            leaf_limit: 3,
        };
        let terminal_packet = range_packet(
            IndexKind::Descriptor,
            terminal_request,
            expanded_leaf([0xff; 32], Some(1)),
        );
        let terminal = verify_range(
            &terminal_packet,
            IndexKind::Descriptor,
            expected(&max_leaf),
            terminal_request,
        )
        .unwrap();
        assert_eq!(terminal.next_request, None);
        let bad_initial = RangeRequest {
            ordinal: 0,
            frontier: [1; 32],
            leaf_limit: 1,
        };
        let bad_packet = range_packet(
            IndexKind::Descriptor,
            bad_initial,
            expanded_leaf([0xff; 32], Some(1)),
        );
        assert_eq!(
            verify_range(
                &bad_packet,
                IndexKind::Descriptor,
                expected(&max_leaf),
                bad_initial
            ),
            Err(OracleError::RequestMismatch)
        );
    }

    #[test]
    fn rejects_op03_union_before_body_interpretation() {
        let packet = packet(IndexKind::Descriptor, 3, Vec::new());
        assert_eq!(
            verify_lookup(
                &packet,
                IndexKind::Descriptor,
                ExpectedRoot {
                    hash: [0; 32],
                    count: 0,
                    size_total: Some(0)
                },
                [0; 32]
            ),
            Err(OracleError::UnsupportedOperation)
        );
    }

    #[test]
    fn bridges_fixed_native_descriptors_to_set_root_and_advertised_identity() {
        let descriptors = [
            (
                id("1f02b1d13a45fadfeaca4448196e81ebb8698e84eeaf2812eb3c1f728f144f80"),
                12,
            ),
            (
                id("610067a866fe9a9d9e6bae74d0fdf303e97b93b3874a5a1830910f91121f1a62"),
                78,
            ),
            (
                id("8c48a97f852d1bb0d24e173d6b3337d929598550cf0a413322a4fefc089250a4"),
                12,
            ),
            (
                id("8d667d78c15df10affb3ff235706cfd5dd15842400cfdcb606ce149ca77104f2"),
                110,
            ),
            (
                id("c876db6132759aa71fa5de57df4eea0511dc0ef35e2a459c8779d9b1caabe1ba"),
                46,
            ),
            (
                id("cdda7c7b6c2aab9bae2973a72af77406dae7cfab65211cf0142914efc6426d37"),
                12,
            ),
            (
                id("d52cc8d0c7370d275e4f48ab4d7de6918c7dde9e2968ac8ba9932d37459ead4c"),
                105,
            ),
        ];
        assert_eq!(descriptors.iter().map(|entry| entry.1).sum::<u64>(), 375);
        let built = build_descriptor_index(&descriptors, BuildLimits::default()).unwrap();
        let native_root = id("8d667d78c15df10affb3ff235706cfd5dd15842400cfdcb606ce149ca77104f2");
        let mut bytes = b"AGSCM-PC1\0".to_vec();
        bytes.push(2);
        bytes.extend_from_slice(&1u16.to_be_bytes());
        bytes.extend_from_slice(&native_root);
        bytes.extend_from_slice(&built.root.hash);
        bytes.extend_from_slice(&7u64.to_be_bytes());
        bytes.extend_from_slice(&375u64.to_be_bytes());
        let identity = decode_portable_identity(&bytes).unwrap();
        assert_eq!(identity.native_roots, vec![native_root]);
        assert_eq!(identity.descriptor_root_hash, built.root.hash);
        assert_eq!(identity.object_count, 7);
        assert_eq!(identity.encoded_source_bytes, 375);
    }

    #[test]
    fn public_entrypoints_enforce_packet_depth_node_and_metadata_limits() {
        let kind = IndexKind::Descriptor;
        let root = ExpectedRoot {
            hash: [0; 32],
            count: 1,
            size_total: Some(1),
        };
        assert_eq!(
            verify_lookup(&vec![0; LOOKUP_PACKET_CAP + 1], kind, root, [0; 32]),
            Err(OracleError::SizeLimit)
        );
        let request = RangeRequest {
            ordinal: 0,
            frontier: [0; 32],
            leaf_limit: 1,
        };
        assert_eq!(
            verify_range(&vec![0; RANGE_PACKET_CAP + 1], kind, root, request),
            Err(OracleError::SizeLimit)
        );

        // An unfinished malformed chain reaches the depth guard before its absent children.
        let mut nested = Vec::new();
        for level in 0..=MAX_BRANCH_LEVELS {
            if level != 0 {
                nested.push(0);
            }
            nested.extend_from_slice(&[1, 0, 0, 3]);
        }
        assert_eq!(
            verify_lookup(
                &lookup_packet(kind, [0; 32], nested.clone()),
                kind,
                root,
                [0; 32]
            ),
            Err(OracleError::DepthLimit)
        );
        assert_eq!(
            verify_range(&range_packet(kind, request, nested), kind, root, request),
            Err(OracleError::DepthLimit)
        );

        let entry = [([0; 32], 1)];
        let mut limits = BuildLimits {
            max_nodes: 0,
            ..BuildLimits::default()
        };
        assert_eq!(
            build_descriptor_index(&entry, limits).err(),
            Some(OracleError::NodeLimit)
        );
        limits = BuildLimits {
            max_metadata_bytes: size_of::<Entry>() as u64,
            ..BuildLimits::default()
        };
        assert_eq!(
            build_descriptor_index(&entry, limits).err(),
            Some(OracleError::MetadataLimit)
        );
        limits = BuildLimits {
            max_work: 0,
            ..BuildLimits::default()
        };
        assert_eq!(
            build_descriptor_index(&entry, limits).err(),
            Some(OracleError::WorkLimit)
        );
    }
}
