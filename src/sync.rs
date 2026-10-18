use std::collections::{BTreeMap, BTreeSet, btree_map::Entry};
use std::fs::{self, File, OpenOptions};
use std::io::{ErrorKind, Read, Write};
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::error::{Error, Result};
use crate::model::EntryKind;
use crate::store::{
    EncodedObject, Engine, MAX_OBJECT_BYTES, MAX_SYMLINK_TARGET_BYTES, Namespace, SourceReader,
    create_directory_durable, decode_blob, digest, inspect_encoded_object, validate_id,
    validate_symlink_target,
};

pub const SOURCE_PROFILE_V1: &str = "agentscm-source-v1";
pub const SOURCE_PROFILE_V2: &str = "agentscm-source-v2";
pub const SOURCE_PROFILE_V3: &str = "agentscm-source-v3";
pub const BUNDLE_FORMAT_V1: &str = "agentscm-offline-bundle-v1";
pub const MAX_SYNC_ROOTS: usize = 256;
pub const MAX_SYNC_OBJECTS: usize = 100_000;
pub const MAX_OFFER_BYTES: u64 = 16 * 1024 * 1024;
pub const MAX_BUNDLE_BYTES: u64 = 1024 * 1024 * 1024;

const MAX_REQUEST_BYTES: u64 = 16 * 1024 * 1024;
// Includes graph/node visits and every derived directory-prefix candidate.
// Blob magic (9) plus length (8) plus the longest valid target.
const MAX_SYMLINK_BLOB_BYTES: u64 = 17 + MAX_SYMLINK_TARGET_BYTES as u64;
const MAX_QUALIFICATION_VISITS: usize = 1_000_000;
const BUNDLE_MAGIC: &[u8; 9] = b"AGSCM-S1\0";
const BUNDLE_END: &[u8; 9] = b"AGSCM-E1\0";
const REQUIRED_CAPABILITIES_V1: [&str; 3] = [
    "complete-revision-history",
    "exact-object-bytes",
    "root-last-import",
];
const REQUIRED_CAPABILITIES_V2: [&str; 4] = [
    "complete-revision-history",
    "exact-object-bytes",
    "root-last-import",
    "multi-parent-revisions",
];
const REQUIRED_CAPABILITIES_V3: [&str; 5] = [
    "complete-revision-history",
    "exact-object-bytes",
    "root-last-import",
    "multi-parent-revisions",
    "entry-kinds",
];

static TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);

#[cfg(test)]
std::thread_local! {
    static INSTALL_FAIL_AFTER: std::cell::Cell<Option<usize>> = const { std::cell::Cell::new(None) };
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct SourceOffer {
    pub version: u32,
    pub profile: String,
    pub required_capabilities: Vec<String>,
    pub origin_repository: String,
    pub roots: Vec<String>,
    pub objects: Vec<ObjectDescriptor>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ObjectDescriptor {
    pub id: String,
    pub encoded_bytes: u64,
}

/// A point-in-time local proof that a complete source closure passed sync qualification.
///
/// This value does not retain its objects or confer authority to publish them. A caller that
/// crosses a retention or authority boundary must revalidate the relevant state atomically.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct QualifiedSource {
    version: u32,
    profile: &'static str,
    required_capabilities: Vec<String>,
    roots: Vec<String>,
    objects: Vec<ObjectDescriptor>,
}

impl QualifiedSource {
    pub fn version(&self) -> u32 {
        self.version
    }

    pub fn profile(&self) -> &str {
        self.profile
    }

    pub fn required_capabilities(&self) -> &[String] {
        &self.required_capabilities
    }

    pub fn roots(&self) -> &[String] {
        &self.roots
    }

    pub fn objects(&self) -> &[ObjectDescriptor] {
        &self.objects
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct SourceRequest {
    pub version: u32,
    pub offer_sha256: String,
    pub missing: Vec<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ImportOutcome {
    pub roots: Vec<String>,
    pub installed: u64,
    pub reused: u64,
}

#[derive(Serialize)]
struct CompletionMarker<'a> {
    version: u32,
    offer_sha256: &'a str,
    request_sha256: &'a str,
    outcome: &'a ImportOutcome,
}

pub fn write_offer(
    store: &Engine,
    origin_repository: &str,
    roots: &[String],
    output: &Path,
) -> Result<SourceOffer> {
    let offer = source_offer(store, origin_repository, roots)?;
    let bytes = serde_json::to_vec(&offer)?;
    write_new(output, |file| file.write_all(&bytes))?;
    Ok(offer)
}

/// Builds the same bounded immutable-source offer as [`write_offer`] without
/// choosing an output path. Transport and authorization remain caller-owned.
pub fn source_offer(
    store: &Engine,
    origin_repository: &str,
    roots: &[String],
) -> Result<SourceOffer> {
    validate_origin(origin_repository)?;
    let qualified = qualify_source(store, roots)?;
    let offer = SourceOffer {
        version: qualified.version,
        profile: qualified.profile.to_owned(),
        required_capabilities: qualified.required_capabilities,
        origin_repository: origin_repository.to_owned(),
        roots: qualified.roots,
        objects: qualified.objects,
    };
    let bytes = serde_json::to_vec(&offer)?;
    if bytes.len() as u64 > MAX_OFFER_BYTES {
        return Err(Error::Limit("sync offer exceeds size limit".into()));
    }
    Ok(offer)
}

pub fn qualify_source(store: &Engine, roots: &[String]) -> Result<QualifiedSource> {
    qualify_complete(roots, |id| store.read_encoded_object(id))
}

/// Validates actual reader bytes. Authorization and retention remain controller-owned.
pub fn qualify_reader(reader: &SourceReader<'_>, roots: &[String]) -> Result<QualifiedSource> {
    qualify_complete(roots, |id| reader.read_encoded(id))
}

/// Validate an offline recovery retention union, not an exchange offer. Retained
/// references may include ancestors and more than 256 independent heads. Object
/// discovery and full qualification still share their original aggregate bounds.
pub(crate) fn validate_retained_reader(reader: &SourceReader<'_>, roots: &[String]) -> Result<()> {
    if roots.is_empty() || roots.len() > MAX_SYNC_OBJECTS {
        return Err(Error::Limit(format!(
            "recovery requires between 1 and {MAX_SYNC_OBJECTS} retained roots"
        )));
    }
    let mut roots = roots.to_vec();
    for root in &roots {
        validate_id(root)?;
    }
    roots.sort();
    roots.dedup();
    let objects = discover(&roots, |id| reader.read_encoded(id))?;
    qualify(&roots, &objects, &mut |id| reader.read_encoded(id))?;
    Ok(())
}

fn qualify_complete<F>(roots: &[String], mut read: F) -> Result<QualifiedSource>
where
    F: FnMut(&str) -> Result<Vec<u8>>,
{
    let roots = validate_roots(roots)?;
    let objects = discover(&roots, &mut read)?;
    let profile = qualify(&roots, &objects, &mut read)?;
    ensure_root_antichain(&roots, &objects)?;
    Ok(QualifiedSource {
        version: profile.version(),
        profile: profile.name(),
        required_capabilities: profile.required_capabilities(),
        roots,
        objects: objects
            .iter()
            .map(|(id, object)| ObjectDescriptor {
                id: id.clone(),
                encoded_bytes: object.encoded_bytes,
            })
            .collect(),
    })
}

pub fn write_request(store: &Engine, offer: &Path, output: &Path) -> Result<SourceRequest> {
    let (offer_bytes, offer) = read_offer(offer)?;
    let request = request_for_offer_bytes(store, &offer, &offer_bytes)?;
    let bytes = serde_json::to_vec(&request)?;
    write_new(output, |file| file.write_all(&bytes))?;
    Ok(request)
}

/// Computes the receiver's bounded missing-object request for a canonical
/// in-memory offer. The resulting digest binds `serde_json::to_vec(offer)`.
pub fn request_for_offer(store: &Engine, offer: &SourceOffer) -> Result<SourceRequest> {
    validate_offer(offer)?;
    let offer_bytes = serde_json::to_vec(offer)?;
    if offer_bytes.len() as u64 > MAX_OFFER_BYTES {
        return Err(Error::Limit("sync offer exceeds size limit".into()));
    }
    request_for_offer_bytes(store, offer, &offer_bytes)
}

fn request_for_offer_bytes(
    store: &Engine,
    offer: &SourceOffer,
    offer_bytes: &[u8],
) -> Result<SourceRequest> {
    let mut missing = Vec::new();
    for object in &offer.objects {
        if !store.has_verified_object(&object.id)? {
            missing.push(object.id.clone());
        }
    }
    let request = SourceRequest {
        version: 1,
        offer_sha256: digest(offer_bytes),
        missing,
    };
    let bytes = serde_json::to_vec(&request)?;
    if bytes.len() as u64 > MAX_REQUEST_BYTES {
        return Err(Error::Limit("sync request exceeds size limit".into()));
    }
    Ok(request)
}

pub fn write_bundle(store: &Engine, offer: &Path, request: &Path, output: &Path) -> Result<()> {
    write_bundle_checked(
        store,
        offer,
        request,
        output,
        MAX_BUNDLE_BYTES,
        MAX_SYNC_OBJECTS,
    )
}

/// Writes a negotiated bundle only when it fits tighter controller bounds.
pub fn write_bundle_bound(
    store: &Engine,
    offer: &Path,
    request: &Path,
    output: &Path,
    max_bundle_bytes: u64,
    max_objects: usize,
) -> Result<()> {
    if max_bundle_bytes > MAX_BUNDLE_BYTES || max_objects > MAX_SYNC_OBJECTS {
        return Err(Error::Invalid(
            "bound bundle limits exceed sync protocol limits".into(),
        ));
    }
    write_bundle_checked(store, offer, request, output, max_bundle_bytes, max_objects)
}

fn write_bundle_checked(
    store: &Engine,
    offer: &Path,
    request: &Path,
    output: &Path,
    max_bundle_bytes: u64,
    max_objects: usize,
) -> Result<()> {
    let (offer_bytes, offer) = read_offer(offer)?;
    let (request_bytes, request) = read_request(request)?;
    validate_request(&request, &offer, &offer_bytes)?;
    if offer.objects.len() > max_objects || request.missing.len() > max_objects {
        return Err(Error::Limit(
            "sync bundle object count exceeds controller limit".into(),
        ));
    }
    let descriptors: BTreeMap<&str, u64> = offer
        .objects
        .iter()
        .map(|object| (object.id.as_str(), object.encoded_bytes))
        .collect();
    let mut total = (BUNDLE_MAGIC.len() + BUNDLE_END.len() + 8 + 8 + 4) as u64
        + offer_bytes.len() as u64
        + request_bytes.len() as u64;
    for id in &request.missing {
        total = total
            .checked_add(32 + 8)
            .and_then(|value| value.checked_add(descriptors[id.as_str()]))
            .ok_or_else(|| Error::Limit("sync bundle byte count overflow".into()))?;
    }
    if total > max_bundle_bytes {
        return Err(Error::Limit("sync bundle exceeds controller limit".into()));
    }

    write_new(output, |file| {
        file.write_all(BUNDLE_MAGIC)?;
        write_bytes(file, &offer_bytes)?;
        write_bytes(file, &request_bytes)?;
        file.write_all(&(request.missing.len() as u32).to_be_bytes())?;
        for id in &request.missing {
            let bytes = store.read_encoded_object(id).map_err(as_io)?;
            if bytes.len() as u64 != descriptors[id.as_str()] {
                return Err(as_io(Error::Conflict(format!(
                    "object length changed after offer: {id}"
                ))));
            }
            file.write_all(&decode_id(id).map_err(as_io)?)?;
            file.write_all(&(bytes.len() as u64).to_be_bytes())?;
            file.write_all(&bytes)?;
        }
        file.write_all(BUNDLE_END)
    })
}

pub fn import_bundle(
    store: &Engine,
    bundle: &Path,
    state_directory: &Path,
) -> Result<ImportOutcome> {
    import_bundle_checked(store, bundle, state_directory, None)
}

/// Imports a bundle only when its embedded offer and request exactly match the
/// controller-authorized negotiation and the tighter transport bounds.
pub fn import_bundle_bound(
    store: &Engine,
    bundle: &Path,
    state_directory: &Path,
    expected_offer: &SourceOffer,
    expected_request: &SourceRequest,
    max_bundle_bytes: u64,
    max_objects: usize,
) -> Result<ImportOutcome> {
    if max_bundle_bytes > MAX_BUNDLE_BYTES || max_objects > MAX_SYNC_OBJECTS {
        return Err(Error::Invalid(
            "bound import limits exceed sync protocol limits".into(),
        ));
    }
    import_bundle_checked(
        store,
        bundle,
        state_directory,
        Some(BoundImport {
            offer: expected_offer,
            request: expected_request,
            max_bundle_bytes,
            max_objects,
        }),
    )
}

struct BoundImport<'a> {
    offer: &'a SourceOffer,
    request: &'a SourceRequest,
    max_bundle_bytes: u64,
    max_objects: usize,
}

fn import_bundle_checked(
    store: &Engine,
    bundle: &Path,
    state_directory: &Path,
    bound: Option<BoundImport<'_>>,
) -> Result<ImportOutcome> {
    let mut file = File::open(bundle)?;
    let length = file.metadata()?.len();
    if length > MAX_BUNDLE_BYTES {
        return Err(Error::Limit("sync bundle exceeds size limit".into()));
    }
    if bound
        .as_ref()
        .is_some_and(|bound| length > bound.max_bundle_bytes)
    {
        return Err(Error::Limit("sync bundle exceeds controller limit".into()));
    }
    expect(&mut file, BUNDLE_MAGIC, "sync bundle magic")?;
    let offer_bytes = read_bytes(&mut file, MAX_OFFER_BYTES, "offer")?;
    let offer: SourceOffer = serde_json::from_slice(&offer_bytes)?;
    validate_offer(&offer)?;
    if let Some(bound) = &bound {
        if offer != *bound.offer {
            return Err(Error::Conflict(
                "sync bundle offer differs from authorized offer".into(),
            ));
        }
        if offer.objects.len() > bound.max_objects {
            return Err(Error::Limit(
                "sync bundle object count exceeds controller limit".into(),
            ));
        }
    }
    let request_bytes = read_bytes(&mut file, MAX_REQUEST_BYTES, "request")?;
    let request: SourceRequest = serde_json::from_slice(&request_bytes)?;
    validate_request(&request, &offer, &offer_bytes)?;
    if bound
        .as_ref()
        .is_some_and(|bound| request != *bound.request)
    {
        return Err(Error::Conflict(
            "sync bundle request differs from authorized request".into(),
        ));
    }
    let record_count = read_u32(&mut file)? as usize;
    if record_count != request.missing.len() {
        return Err(Error::Invalid(
            "sync bundle record count does not match request".into(),
        ));
    }

    let state_directory = create_directory_durable(state_directory)?;
    let request_digest = digest(&request_bytes);
    let transfer_directory = create_directory_durable(state_directory.join(&request_digest))?;
    let descriptor_lengths: BTreeMap<&str, u64> = offer
        .objects
        .iter()
        .map(|object| (object.id.as_str(), object.encoded_bytes))
        .collect();
    for expected_id in &request.missing {
        let mut raw_id = [0u8; 32];
        file.read_exact(&mut raw_id)?;
        let id = encode_id(&raw_id);
        if &id != expected_id {
            return Err(Error::Invalid(
                "sync bundle object order does not match request".into(),
            ));
        }
        let object_length = read_u64(&mut file)?;
        if object_length > MAX_OBJECT_BYTES
            || object_length != descriptor_lengths[expected_id.as_str()]
        {
            return Err(Error::Invalid(format!(
                "sync bundle object length mismatch: {id}"
            )));
        }
        let mut bytes = vec![0; object_length as usize];
        file.read_exact(&mut bytes)?;
        store.inspect_encoded_object(&id, &bytes)?;
        stage_object(&transfer_directory.join(&id), &id, &bytes)?;
    }
    expect(&mut file, BUNDLE_END, "sync bundle footer")?;
    let mut trailing = [0u8; 1];
    if file.read(&mut trailing)? != 0 {
        return Err(Error::Invalid("sync bundle has trailing bytes".into()));
    }

    let objects = discover(&offer.roots, |id| {
        read_stage_or_store(store, &transfer_directory, id)
    })?;
    compare_offer(&offer, &objects)?;
    let profile = qualify(&offer.roots, &objects, &mut |id| {
        read_stage_or_store(store, &transfer_directory, id)
    })?;
    validate_qualified_profile(&offer, profile)?;
    ensure_root_antichain(&offer.roots, &objects)?;
    install_closure(store, &transfer_directory, &offer, &request, &objects)?;

    let installed_objects = discover(&offer.roots, |id| store.read_encoded_object(id))?;
    compare_offer(&offer, &installed_objects)?;
    let installed_profile = qualify(&offer.roots, &installed_objects, &mut |id| {
        store.read_encoded_object(id)
    })?;
    validate_qualified_profile(&offer, installed_profile)?;
    let outcome = ImportOutcome {
        roots: offer.roots.clone(),
        installed: request.missing.len() as u64,
        reused: (offer.objects.len() - request.missing.len()) as u64,
    };
    let offer_digest = digest(&offer_bytes);
    let marker = serde_json::to_vec(&CompletionMarker {
        version: 1,
        offer_sha256: &offer_digest,
        request_sha256: &request_digest,
        outcome: &outcome,
    })?;
    write_or_verify(&transfer_directory.join("complete"), &marker)?;
    Ok(outcome)
}

#[derive(Clone, Debug)]
struct ObjectState {
    encoded_bytes: u64,
    object: EncodedObject,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SourceProfile {
    V1,
    V2,
    V3,
}

impl SourceProfile {
    fn version(self) -> u32 {
        match self {
            Self::V1 => 1,
            Self::V2 => 2,
            Self::V3 => 3,
        }
    }

    fn name(self) -> &'static str {
        match self {
            Self::V1 => SOURCE_PROFILE_V1,
            Self::V2 => SOURCE_PROFILE_V2,
            Self::V3 => SOURCE_PROFILE_V3,
        }
    }

    fn required_capabilities(self) -> Vec<String> {
        match self {
            Self::V1 => REQUIRED_CAPABILITIES_V1.map(str::to_owned).to_vec(),
            Self::V2 => REQUIRED_CAPABILITIES_V2.map(str::to_owned).to_vec(),
            Self::V3 => REQUIRED_CAPABILITIES_V3.map(str::to_owned).to_vec(),
        }
    }
}

fn discover<F>(roots: &[String], mut read: F) -> Result<BTreeMap<String, ObjectState>>
where
    F: FnMut(&str) -> Result<Vec<u8>>,
{
    let mut pending = roots.to_vec();
    let mut objects = BTreeMap::new();
    while let Some(id) = pending.pop() {
        if objects.contains_key(&id) {
            continue;
        }
        if objects.len() == MAX_SYNC_OBJECTS {
            return Err(Error::Limit(format!(
                "sync closure exceeds {MAX_SYNC_OBJECTS} objects"
            )));
        }
        let bytes = read(&id)?;
        let object = inspect_encoded_object(&id, &bytes)?;
        pending.extend(references(&object).into_iter().map(str::to_owned));
        objects.insert(
            id,
            ObjectState {
                encoded_bytes: bytes.len() as u64,
                object,
            },
        );
    }
    Ok(objects)
}

fn references(object: &EncodedObject) -> Vec<&str> {
    match object {
        EncodedObject::Blob => Vec::new(),
        EncodedObject::Tree {
            exact,
            portable,
            prefix,
        } => vec![exact, portable, prefix],
        EncodedObject::Branch { children, .. } => {
            children.iter().map(|(_, id)| id.as_str()).collect()
        }
        EncodedObject::Leaf {
            namespace, value, ..
        } => match namespace {
            Namespace::Exact | Namespace::Portable => vec![value],
            Namespace::Prefix => Vec::new(),
        },
        EncodedObject::Revision(revision) => revision
            .parents
            .iter()
            .map(String::as_str)
            .chain(std::iter::once(revision.tree.as_str()))
            .collect(),
    }
}

fn qualify(
    roots: &[String],
    objects: &BTreeMap<String, ObjectState>,
    read: &mut dyn FnMut(&str) -> Result<Vec<u8>>,
) -> Result<SourceProfile> {
    let mut visits = 0usize;
    for root in roots {
        if !matches!(
            objects.get(root).map(|value| &value.object),
            Some(EncodedObject::Revision(_))
        ) {
            return Err(Error::Invalid(format!(
                "sync root is not a revision: {root}"
            )));
        }
    }
    validate_parent_graph(roots, objects, &mut visits)?;
    let mut profile = SourceProfile::V1;
    let mut has_entry_kinds = false;
    let mut qualified_trees = BTreeSet::new();
    for state in objects.values() {
        if let EncodedObject::Revision(revision) = &state.object {
            if revision.version == 2 {
                profile = SourceProfile::V2;
            }
            if qualified_trees.insert(revision.tree.clone()) {
                qualify_tree(&revision.tree, objects, &mut visits, read)?;
            }
        }
        if matches!(
            state.object,
            EncodedObject::Leaf {
                kind: EntryKind::Executable | EntryKind::Symlink,
                ..
            }
        ) {
            has_entry_kinds = true;
        }
    }
    if has_entry_kinds {
        profile = SourceProfile::V3;
    }
    Ok(profile)
}

fn validate_parent_graph(
    roots: &[String],
    objects: &BTreeMap<String, ObjectState>,
    visits: &mut usize,
) -> Result<()> {
    #[derive(Clone, Copy, PartialEq, Eq)]
    enum VisitState {
        Active,
        Complete,
    }

    let mut states = BTreeMap::new();
    for root in roots {
        if states.get(root) == Some(&VisitState::Complete) {
            continue;
        }
        revision_object(objects, root)?;
        count_visit(visits)?;
        states.insert(root.clone(), VisitState::Active);
        let mut stack = vec![(root.clone(), 0usize)];
        while let Some((id, parent_index)) = stack.last_mut() {
            let revision = revision_object(objects, id)?;
            if *parent_index < revision.parents.len() {
                let parent = revision.parents[*parent_index].clone();
                *parent_index += 1;
                match states.get(&parent) {
                    Some(VisitState::Active) => {
                        return Err(Error::Invalid(
                            "revision parent graph contains a cycle".into(),
                        ));
                    }
                    Some(VisitState::Complete) => continue,
                    None => {
                        revision_object(objects, &parent)?;
                        count_visit(visits)?;
                        states.insert(parent.clone(), VisitState::Active);
                        stack.push((parent, 0));
                    }
                }
            } else {
                let (complete, _) = stack.pop().expect("revision stack is nonempty");
                states.insert(complete, VisitState::Complete);
            }
        }
    }
    Ok(())
}

fn revision_object<'a>(
    objects: &'a BTreeMap<String, ObjectState>,
    id: &str,
) -> Result<&'a crate::model::Revision> {
    let revision = match &object(objects, id)?.object {
        EncodedObject::Revision(revision) => revision,
        _ => return Err(Error::Invalid("revision parent is not a revision".into())),
    };
    validate_revision_shape(revision)?;
    Ok(revision)
}

fn validate_revision_shape(revision: &crate::model::Revision) -> Result<()> {
    match (revision.version, revision.parents.len()) {
        (1, 0..=1) | (2, 2..=8) => {}
        _ => {
            return Err(Error::Invalid(
                "unsupported revision parent count or version".into(),
            ));
        }
    }
    let mut parents = BTreeSet::new();
    for parent in &revision.parents {
        validate_id(parent)?;
        if !parents.insert(parent) {
            return Err(Error::Invalid("revision parents must be unique".into()));
        }
    }
    validate_id(&revision.tree)?;
    Ok(())
}

fn qualify_tree(
    tree_id: &str,
    objects: &BTreeMap<String, ObjectState>,
    visits: &mut usize,
    read: &mut dyn FnMut(&str) -> Result<Vec<u8>>,
) -> Result<()> {
    let (exact_root, portable_root, prefix_root) = match &object(objects, tree_id)?.object {
        EncodedObject::Tree {
            exact,
            portable,
            prefix,
        } => (exact, portable, prefix),
        _ => {
            return Err(Error::Invalid(
                "revision tree reference is not a tree".into(),
            ));
        }
    };
    let exact = collect_leaves(exact_root, Namespace::Exact, objects, visits)?;
    let portable = collect_leaves(portable_root, Namespace::Portable, objects, visits)?;
    let prefix = collect_leaves(prefix_root, Namespace::Prefix, objects, visits)?;

    let mut expected_portable = BTreeMap::new();
    let mut expected_prefix: BTreeMap<String, (String, u64)> = BTreeMap::new();
    for (path, (leaf_id, blob)) in &exact {
        let blob_state = object(objects, blob)?;
        if !matches!(blob_state.object, EncodedObject::Blob) {
            return Err(Error::Invalid(format!(
                "exact manifest leaf does not reference a blob: {path}"
            )));
        }
        if matches!(
            object(objects, leaf_id)?.object,
            EncodedObject::Leaf {
                kind: EntryKind::Symlink,
                ..
            }
        ) {
            // Size first: an oversize blob is refused before it is read.
            if blob_state.encoded_bytes > MAX_SYMLINK_BLOB_BYTES {
                return Err(Error::Invalid(format!(
                    "symlink target exceeds size limit: {path}"
                )));
            }
            count_visit(visits)?;
            let target = decode_blob(&read(blob)?)?;
            validate_symlink_target(path, &target)?;
        }
        let portable_path = path.to_ascii_lowercase();
        if expected_portable
            .insert(portable_path, (path.clone(), leaf_id.clone()))
            .is_some()
        {
            return Err(Error::Invalid(
                "portable path collision in exact manifest".into(),
            ));
        }
        for prefix_path in directory_prefixes(path) {
            count_visit(visits)?;
            let portable_prefix = prefix_path.to_ascii_lowercase();
            let expected_count = expected_prefix.len();
            let entry = match expected_prefix.entry(portable_prefix) {
                Entry::Occupied(entry) => entry.into_mut(),
                Entry::Vacant(entry) => {
                    if expected_count >= prefix.len() {
                        return Err(Error::Invalid(
                            "exact manifest directories exceed the prefix index".into(),
                        ));
                    }
                    entry.insert((prefix_path.to_owned(), 0))
                }
            };
            if entry.0 != prefix_path {
                return Err(Error::Invalid(
                    "portable directory spelling collision".into(),
                ));
            }
            entry.1 = entry
                .1
                .checked_add(1)
                .ok_or_else(|| Error::Limit("directory descendant count overflow".into()))?;
        }
    }
    for path in exact.keys() {
        if expected_prefix.contains_key(&path.to_ascii_lowercase()) {
            return Err(Error::Invalid(format!(
                "manifest path is both a file and directory: {path}"
            )));
        }
    }

    let mut actual_portable = BTreeMap::new();
    for (path, (_, exact_leaf)) in portable {
        let target = object(objects, &exact_leaf)?;
        match &target.object {
            EncodedObject::Leaf {
                namespace: Namespace::Exact,
                object_id,
                path: exact_path,
                ..
            } if object_id == &exact_leaf && exact_path == &path => {}
            _ => {
                return Err(Error::Invalid(format!(
                    "portable manifest entry does not reference its exact leaf: {path}"
                )));
            }
        }
        if actual_portable
            .insert(path.to_ascii_lowercase(), (path, exact_leaf))
            .is_some()
        {
            return Err(Error::Invalid("duplicate portable manifest path".into()));
        }
    }
    if actual_portable != expected_portable {
        return Err(Error::Invalid(
            "exact and portable manifests do not describe the same files".into(),
        ));
    }

    let mut actual_prefix = BTreeMap::new();
    for (path, (_, encoded_count)) in prefix {
        let count = parse_prefix_count(&encoded_count)?;
        if actual_prefix
            .insert(path.to_ascii_lowercase(), (path, count))
            .is_some()
        {
            return Err(Error::Invalid("duplicate prefix manifest path".into()));
        }
    }
    if actual_prefix != expected_prefix {
        return Err(Error::Invalid(
            "prefix manifest does not match exact manifest directories".into(),
        ));
    }
    Ok(())
}

fn collect_leaves(
    root: &str,
    namespace: Namespace,
    objects: &BTreeMap<String, ObjectState>,
    visits: &mut usize,
) -> Result<BTreeMap<String, (String, String)>> {
    let mut seen = BTreeSet::new();
    let mut leaves = BTreeMap::new();
    visit_manifest(
        root,
        namespace,
        None,
        &mut Vec::new(),
        objects,
        visits,
        &mut seen,
        &mut leaves,
    )?;
    Ok(leaves)
}

#[allow(clippy::too_many_arguments)]
fn visit_manifest(
    id: &str,
    namespace: Namespace,
    parent_depth: Option<u8>,
    slots: &mut Vec<(u8, u8)>,
    objects: &BTreeMap<String, ObjectState>,
    visits: &mut usize,
    seen: &mut BTreeSet<String>,
    leaves: &mut BTreeMap<String, (String, String)>,
) -> Result<Option<[u8; 32]>> {
    count_visit(visits)?;
    if !seen.insert(id.to_owned()) {
        return Err(Error::Invalid(
            "manifest node is referenced more than once".into(),
        ));
    }
    let representative = match &object(objects, id)?.object {
        EncodedObject::Branch {
            namespace: actual,
            depth,
            children,
        } if *actual == namespace => {
            if parent_depth.is_some_and(|parent| *depth <= parent) {
                return Err(Error::Invalid("manifest depth does not increase".into()));
            }
            if parent_depth.is_some() && children.is_empty() {
                return Err(Error::Invalid("empty non-root manifest branch".into()));
            }
            let mut representative = None;
            for (slot, child) in children {
                slots.push((*depth, *slot));
                let child_representative = visit_manifest(
                    child,
                    namespace,
                    Some(*depth),
                    slots,
                    objects,
                    visits,
                    seen,
                    leaves,
                )?
                .ok_or_else(|| Error::Invalid("empty non-root manifest branch".into()))?;
                slots.pop();
                if representative.is_some_and(|existing| {
                    (0..usize::from(*depth)).any(|index| {
                        nibble_at(&existing, index) != nibble_at(&child_representative, index)
                    })
                }) {
                    return Err(Error::Invalid(
                        "manifest branch has inconsistent compressed prefix".into(),
                    ));
                }
                representative.get_or_insert(child_representative);
            }
            representative
        }
        EncodedObject::Leaf {
            namespace: actual,
            object_id,
            key,
            path,
            value,
            kind: _,
        } if *actual == namespace => {
            if slots
                .iter()
                .any(|(depth, slot)| nibble_at(key, usize::from(*depth)) != *slot)
            {
                return Err(Error::Invalid(
                    "manifest leaf is stored in the wrong branch slot".into(),
                ));
            }
            if leaves
                .insert(path.clone(), (object_id.clone(), value.clone()))
                .is_some()
            {
                return Err(Error::Invalid("duplicate path in manifest".into()));
            }
            Some(*key)
        }
        EncodedObject::Branch { .. } | EncodedObject::Leaf { .. } => {
            return Err(Error::Invalid("manifest namespace mismatch".into()));
        }
        _ => return Err(Error::Invalid("manifest reference is not a node".into())),
    };
    Ok(representative)
}

fn install_closure(
    store: &Engine,
    transfer_directory: &Path,
    offer: &SourceOffer,
    request: &SourceRequest,
    objects: &BTreeMap<String, ObjectState>,
) -> Result<()> {
    for id in &request.missing {
        if !matches!(object(objects, id)?.object, EncodedObject::Revision(_)) {
            install_staged(store, transfer_directory, id)?;
        }
    }
    let roots: BTreeSet<&str> = offer.roots.iter().map(String::as_str).collect();
    let revisions: BTreeSet<String> = request
        .missing
        .iter()
        .filter(|id| {
            matches!(
                object(objects, id).map(|state| &state.object),
                Ok(EncodedObject::Revision(_))
            )
        })
        .cloned()
        .collect();
    let (revision_order, _) = revision_install_order(&revisions, objects)?;
    for id in revision_order
        .iter()
        .filter(|id| !roots.contains(id.as_str()))
    {
        install_staged(store, transfer_directory, id)?;
    }
    for id in revision_order
        .iter()
        .filter(|id| roots.contains(id.as_str()))
    {
        install_staged(store, transfer_directory, id)?;
    }
    Ok(())
}

fn revision_install_order(
    revisions: &BTreeSet<String>,
    objects: &BTreeMap<String, ObjectState>,
) -> Result<(Vec<String>, usize)> {
    #[derive(Clone, Copy, PartialEq, Eq)]
    enum VisitState {
        Active,
        Complete,
    }

    let mut states = BTreeMap::new();
    let mut order = Vec::with_capacity(revisions.len());
    let mut examined = 0usize;
    for start in revisions {
        if states.get(start) == Some(&VisitState::Complete) {
            continue;
        }
        revision_object(objects, start)?;
        count_bounded_revision(&mut examined, "revision ordering")?;
        states.insert(start.clone(), VisitState::Active);
        let mut stack = vec![(start.clone(), 0usize)];
        while let Some((id, parent_index)) = stack.last_mut() {
            let revision = revision_object(objects, id)?;
            if *parent_index < revision.parents.len() {
                let parent = revision.parents[*parent_index].clone();
                *parent_index += 1;
                if !revisions.contains(&parent) {
                    continue;
                }
                match states.get(&parent) {
                    Some(VisitState::Active) => {
                        return Err(Error::Invalid(
                            "revision parent graph contains a cycle".into(),
                        ));
                    }
                    Some(VisitState::Complete) => continue,
                    None => {
                        revision_object(objects, &parent)?;
                        count_bounded_revision(&mut examined, "revision ordering")?;
                        states.insert(parent.clone(), VisitState::Active);
                        stack.push((parent, 0));
                    }
                }
            } else {
                let (complete, _) = stack.pop().expect("revision stack is nonempty");
                states.insert(complete.clone(), VisitState::Complete);
                order.push(complete);
            }
        }
    }
    if order.len() != revisions.len() {
        return Err(Error::Invalid(
            "revision import order could not be resolved".into(),
        ));
    }
    Ok((order, examined))
}

fn install_staged(store: &Engine, transfer_directory: &Path, id: &str) -> Result<()> {
    #[cfg(test)]
    INSTALL_FAIL_AFTER.with(|remaining| match remaining.get() {
        Some(0) => {
            remaining.set(None);
            Err(Error::Io(std::io::Error::other(
                "injected installation interruption",
            )))
        }
        Some(count) => {
            remaining.set(Some(count - 1));
            Ok(())
        }
        None => Ok(()),
    })?;
    let bytes = read_bounded(
        &transfer_directory.join(id),
        MAX_OBJECT_BYTES,
        "staged object",
    )?;
    store.install_encoded_object(id, bytes)
}

fn ensure_root_antichain(
    roots: &[String],
    objects: &BTreeMap<String, ObjectState>,
) -> Result<usize> {
    let roots_set: BTreeSet<&str> = roots.iter().map(String::as_str).collect();
    let mut seen = BTreeSet::new();
    let mut pending = roots.to_vec();
    let mut examined = 0usize;
    while let Some(id) = pending.pop() {
        if !seen.insert(id.clone()) {
            continue;
        }
        count_bounded_revision(&mut examined, "root ancestry")?;
        let revision = revision_object(objects, &id)?;
        for parent in &revision.parents {
            if roots_set.contains(parent.as_str()) {
                return Err(Error::Invalid(
                    "sync roots must not contain one another's ancestors".into(),
                ));
            }
            if !seen.contains(parent) {
                revision_object(objects, parent)?;
                pending.push(parent.clone());
            }
        }
    }
    Ok(examined)
}

fn count_bounded_revision(count: &mut usize, operation: &str) -> Result<()> {
    *count = count
        .checked_add(1)
        .ok_or_else(|| Error::Limit(format!("{operation} work overflow")))?;
    if *count > MAX_SYNC_OBJECTS {
        return Err(Error::Limit(format!(
            "{operation} exceeds {MAX_SYNC_OBJECTS} revisions"
        )));
    }
    Ok(())
}

fn compare_offer(offer: &SourceOffer, objects: &BTreeMap<String, ObjectState>) -> Result<()> {
    if offer.objects.len() != objects.len() {
        return Err(Error::Invalid(
            "sync offer does not equal the rooted source closure".into(),
        ));
    }
    for descriptor in &offer.objects {
        let actual = object(objects, &descriptor.id)?;
        if actual.encoded_bytes != descriptor.encoded_bytes {
            return Err(Error::Invalid(format!(
                "sync offer length does not match object: {}",
                descriptor.id
            )));
        }
    }
    Ok(())
}

fn object<'a>(objects: &'a BTreeMap<String, ObjectState>, id: &str) -> Result<&'a ObjectState> {
    objects
        .get(id)
        .ok_or_else(|| Error::NotFound(format!("sync object: {id}")))
}

fn count_visit(visits: &mut usize) -> Result<()> {
    *visits = visits
        .checked_add(1)
        .ok_or_else(|| Error::Limit("sync qualification work overflow".into()))?;
    if *visits > MAX_QUALIFICATION_VISITS {
        return Err(Error::Limit(format!(
            "sync qualification exceeds {MAX_QUALIFICATION_VISITS} work steps"
        )));
    }
    Ok(())
}

fn validate_roots(roots: &[String]) -> Result<Vec<String>> {
    if roots.is_empty() || roots.len() > MAX_SYNC_ROOTS {
        return Err(Error::Limit(format!(
            "sync requires between 1 and {MAX_SYNC_ROOTS} roots"
        )));
    }
    let mut ordered = roots.to_vec();
    ordered.sort();
    for root in &ordered {
        validate_id(root)?;
    }
    if ordered.windows(2).any(|pair| pair[0] == pair[1]) {
        return Err(Error::Invalid("duplicate sync root".into()));
    }
    Ok(ordered)
}

fn validate_origin(origin: &str) -> Result<()> {
    if origin.is_empty()
        || origin.len() > 128
        || !origin
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
    {
        return Err(Error::Invalid(
            "origin repository must be 1 to 128 portable identifier bytes".into(),
        ));
    }
    Ok(())
}

fn validate_offer(offer: &SourceOffer) -> Result<()> {
    let profile = match (offer.version, offer.profile.as_str()) {
        (1, SOURCE_PROFILE_V1) => SourceProfile::V1,
        (2, SOURCE_PROFILE_V2) => SourceProfile::V2,
        (3, SOURCE_PROFILE_V3) => SourceProfile::V3,
        _ => return Err(Error::Invalid("unsupported sync source profile".into())),
    };
    if offer.required_capabilities != profile.required_capabilities() {
        return Err(Error::Invalid(
            "unsupported or noncanonical required sync capabilities".into(),
        ));
    }
    validate_origin(&offer.origin_repository)?;
    if validate_roots(&offer.roots)? != offer.roots {
        return Err(Error::Invalid("sync roots are not canonical".into()));
    }
    if offer.objects.is_empty() || offer.objects.len() > MAX_SYNC_OBJECTS {
        return Err(Error::Limit(format!(
            "sync offer must contain between 1 and {MAX_SYNC_OBJECTS} objects"
        )));
    }
    let mut previous = None;
    for descriptor in &offer.objects {
        validate_id(&descriptor.id)?;
        if descriptor.encoded_bytes > MAX_OBJECT_BYTES {
            return Err(Error::Limit(format!(
                "offered object exceeds size limit: {}",
                descriptor.id
            )));
        }
        if previous.is_some_and(|value: &str| value >= descriptor.id.as_str()) {
            return Err(Error::Invalid(
                "sync offer objects are not sorted and unique".into(),
            ));
        }
        previous = Some(descriptor.id.as_str());
    }
    Ok(())
}

fn validate_qualified_profile(offer: &SourceOffer, actual: SourceProfile) -> Result<()> {
    if offer.version != actual.version()
        || offer.profile != actual.name()
        || offer.required_capabilities != actual.required_capabilities()
    {
        return Err(Error::Invalid(
            "sync source profile does not match revision encodings".into(),
        ));
    }
    Ok(())
}

fn validate_request(
    request: &SourceRequest,
    offer: &SourceOffer,
    offer_bytes: &[u8],
) -> Result<()> {
    if request.version != 1 || request.offer_sha256 != digest(offer_bytes) {
        return Err(Error::Conflict(
            "sync request does not bind the exact offer".into(),
        ));
    }
    let offered: BTreeSet<&str> = offer.objects.iter().map(|item| item.id.as_str()).collect();
    let mut previous = None;
    for id in &request.missing {
        validate_id(id)?;
        if !offered.contains(id.as_str()) {
            return Err(Error::Invalid(format!(
                "sync request names an unoffered object: {id}"
            )));
        }
        if previous.is_some_and(|value: &str| value >= id.as_str()) {
            return Err(Error::Invalid(
                "sync request objects are not sorted and unique".into(),
            ));
        }
        previous = Some(id.as_str());
    }
    Ok(())
}

fn read_offer(path: &Path) -> Result<(Vec<u8>, SourceOffer)> {
    let bytes = read_bounded(path, MAX_OFFER_BYTES, "sync offer")?;
    let offer: SourceOffer = serde_json::from_slice(&bytes)?;
    validate_offer(&offer)?;
    Ok((bytes, offer))
}

fn read_request(path: &Path) -> Result<(Vec<u8>, SourceRequest)> {
    let bytes = read_bounded(path, MAX_REQUEST_BYTES, "sync request")?;
    let request = serde_json::from_slice(&bytes)?;
    Ok((bytes, request))
}

fn read_stage_or_store(store: &Engine, directory: &Path, id: &str) -> Result<Vec<u8>> {
    let path = directory.join(id);
    match fs::metadata(&path) {
        Ok(metadata) if metadata.is_file() => {
            read_bounded(&path, MAX_OBJECT_BYTES, "staged object")
        }
        Ok(_) => Err(Error::Invalid(format!("staged object is not a file: {id}"))),
        Err(error) if error.kind() == ErrorKind::NotFound => store.read_encoded_object(id),
        Err(error) => Err(Error::Io(error)),
    }
}

fn stage_object(path: &Path, id: &str, bytes: &[u8]) -> Result<()> {
    if path.exists() {
        return verify_file(path, id, bytes);
    }
    let parent = path
        .parent()
        .ok_or_else(|| Error::Invalid("staged object has no parent directory".into()))?;
    let (temporary, mut file) = loop {
        let sequence = TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let temporary = parent.join(format!(
            ".stage-{}-{sequence}-{}",
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
        match fs::hard_link(&temporary, path) {
            Ok(()) => sync_directory(parent),
            Err(error) if error.kind() == ErrorKind::AlreadyExists => verify_file(path, id, bytes),
            Err(error) => Err(Error::Io(error)),
        }
    })();
    drop(file);
    let _ = fs::remove_file(&temporary);
    result
}

fn write_or_verify(path: &Path, bytes: &[u8]) -> Result<()> {
    if path.exists() {
        let existing = read_bounded(path, MAX_OFFER_BYTES, "completion marker")?;
        return if existing == bytes {
            Ok(())
        } else {
            Err(Error::Conflict("sync completion marker differs".into()))
        };
    }
    write_new(path, |file| file.write_all(bytes))
}

fn verify_file(path: &Path, id: &str, expected: &[u8]) -> Result<()> {
    let mut file = File::open(path)?;
    if file.metadata()?.len() != expected.len() as u64 {
        return Err(Error::Conflict(format!(
            "staged object differs at digest path: {id}"
        )));
    }
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
                "staged object differs at digest path: {id}"
            )));
        }
        hasher.update(&buffer[..read]);
        offset += read;
    }
    if offset != expected.len() || format!("{:x}", hasher.finalize()) != id {
        return Err(Error::Conflict(format!(
            "staged object differs at digest path: {id}"
        )));
    }
    Ok(())
}

pub(crate) fn read_bounded(path: &Path, limit: u64, label: &str) -> Result<Vec<u8>> {
    let mut file = File::open(path)?;
    let length = file.metadata()?.len();
    if length > limit {
        return Err(Error::Limit(format!("{label} exceeds size limit")));
    }
    let mut bytes = Vec::with_capacity(length as usize);
    Read::by_ref(&mut file)
        .take(limit + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() as u64 > limit {
        return Err(Error::Limit(format!("{label} exceeds size limit")));
    }
    Ok(bytes)
}

pub(crate) fn write_new<F>(path: &Path, write: F) -> Result<()>
where
    F: FnOnce(&mut File) -> std::io::Result<()>,
{
    if path.exists() {
        return Err(Error::Conflict(format!(
            "output path already exists: {}",
            path.display()
        )));
    }
    let parent = path
        .parent()
        .ok_or_else(|| Error::Invalid("output path has no parent directory".into()))?;
    if !parent.is_dir() {
        return Err(Error::Invalid(format!(
            "output parent is not a directory: {}",
            parent.display()
        )));
    }
    let (temporary, mut file) = loop {
        let sequence = TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let temporary = parent.join(format!(".agentscm-sync-{}-{sequence}", std::process::id()));
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
        write(&mut file)?;
        file.sync_all()?;
        fs::hard_link(&temporary, path).map_err(|error| {
            if error.kind() == ErrorKind::AlreadyExists {
                Error::Conflict(format!("output path already exists: {}", path.display()))
            } else {
                Error::Io(error)
            }
        })?;
        sync_directory(parent)
    })();
    drop(file);
    let _ = fs::remove_file(&temporary);
    result
}

fn write_bytes(file: &mut File, bytes: &[u8]) -> std::io::Result<()> {
    file.write_all(&(bytes.len() as u64).to_be_bytes())?;
    file.write_all(bytes)
}

fn read_bytes(file: &mut File, limit: u64, label: &str) -> Result<Vec<u8>> {
    let length = read_u64(file)?;
    if length > limit {
        return Err(Error::Limit(format!(
            "sync bundle {label} exceeds size limit"
        )));
    }
    let mut bytes = vec![0; length as usize];
    file.read_exact(&mut bytes)?;
    Ok(bytes)
}

fn expect(file: &mut File, expected: &[u8], label: &str) -> Result<()> {
    let mut actual = vec![0; expected.len()];
    file.read_exact(&mut actual)?;
    if actual != expected {
        return Err(Error::Invalid(format!("invalid {label}")));
    }
    Ok(())
}

fn read_u32(file: &mut File) -> Result<u32> {
    let mut bytes = [0u8; 4];
    file.read_exact(&mut bytes)?;
    Ok(u32::from_be_bytes(bytes))
}

fn read_u64(file: &mut File) -> Result<u64> {
    let mut bytes = [0u8; 8];
    file.read_exact(&mut bytes)?;
    Ok(u64::from_be_bytes(bytes))
}

fn encode_id(bytes: &[u8; 32]) -> String {
    format!("{:x}", Sha256Display(bytes))
}

struct Sha256Display<'a>(&'a [u8; 32]);

impl std::fmt::LowerHex for Sha256Display<'_> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        for byte in self.0 {
            write!(formatter, "{byte:02x}")?;
        }
        Ok(())
    }
}

fn decode_id(id: &str) -> Result<[u8; 32]> {
    validate_id(id)?;
    let mut bytes = [0u8; 32];
    for (index, pair) in id.as_bytes().chunks_exact(2).enumerate() {
        let pair = std::str::from_utf8(pair)
            .map_err(|_| Error::Invalid("object ID is not ASCII".into()))?;
        bytes[index] = u8::from_str_radix(pair, 16)
            .map_err(|_| Error::Invalid("object ID is not hexadecimal".into()))?;
    }
    Ok(bytes)
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

fn directory_prefixes(path: &str) -> impl Iterator<Item = &str> {
    path.match_indices('/').map(|(index, _)| &path[..index])
}

fn nibble_at(key: &[u8; 32], depth: usize) -> u8 {
    let byte = key[depth / 2];
    if depth.is_multiple_of(2) {
        byte >> 4
    } else {
        byte & 0x0f
    }
}

// Windows cannot open a directory as a file; NTFS journals renames, so there is nothing to sync.
fn sync_directory(path: &Path) -> Result<()> {
    #[cfg(unix)]
    File::open(path)?.sync_all()?;
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
}

fn as_io(error: Error) -> std::io::Error {
    std::io::Error::other(error)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Revision;

    #[test]
    fn retained_roots_allow_ancestors_without_relaxing_offer_antichains() {
        let directory = tempfile::TempDir::new().unwrap();
        let engine = Engine::open(directory.path()).unwrap();
        let base = engine.checkpoint(None, &[]).unwrap();
        let child = engine
            .checkpoint(
                Some(&base),
                &[crate::model::FileChange {
                    path: "file".into(),
                    content: Some(vec![1]),
                }],
            )
            .unwrap();
        let source = SourceReader::new(&engine);
        validate_retained_reader(&source, &[base.clone(), child.clone(), base.clone()]).unwrap();
        assert!(
            matches!(qualify_reader(&source,&[base,child]),Err(Error::Invalid(message)) if message.contains("ancestors"))
        );
    }

    #[test]
    fn retained_roots_above_offer_limit_share_one_discovery_union() {
        struct Counting<'a> {
            engine: &'a Engine,
            seen: std::cell::RefCell<BTreeSet<String>>,
        }
        impl crate::store::EncodedObjectReader for Counting<'_> {
            fn open_encoded(&self, id: &str) -> Result<Box<dyn Read + '_>> {
                assert!(
                    self.seen.borrow_mut().insert(id.into()),
                    "shared object read twice: {id}"
                );
                Ok(Box::new(std::io::Cursor::new(
                    self.engine.read_encoded(id)?,
                )))
            }
        }
        let directory = tempfile::TempDir::new().unwrap();
        let engine = Engine::open(directory.path()).unwrap();
        let roots: Vec<_> = (0..257u64)
            .map(|number| {
                engine
                    .checkpoint(
                        None,
                        &[crate::model::FileChange {
                            path: "file".into(),
                            content: Some(number.to_le_bytes().to_vec()),
                        }],
                    )
                    .unwrap()
            })
            .collect();
        let reader = Counting {
            engine: &engine,
            seen: std::cell::RefCell::new(BTreeSet::new()),
        };
        let source = SourceReader::new(&reader);
        validate_retained_reader(&source, &roots).unwrap();
        let reads = reader.seen.borrow().len();
        assert!(reads > 257);
        assert!(matches!(
            qualify_reader(&source, &roots),
            Err(Error::Limit(_))
        ));
        assert_eq!(reader.seen.borrow().len(), reads);
    }

    #[test]
    fn retained_roots_reject_missing_corrupt_invalid_and_over_budget_unions() {
        struct Broken {
            missing: bool,
        }
        impl crate::store::EncodedObjectReader for Broken {
            fn open_encoded(&self, id: &str) -> Result<Box<dyn Read + '_>> {
                if self.missing {
                    Err(Error::MissingObject(id.into()))
                } else {
                    Ok(Box::new(std::io::Cursor::new(vec![0u8])))
                }
            }
        }
        for missing in [true, false] {
            let reader = Broken { missing };
            let source = SourceReader::new(&reader);
            assert!(validate_retained_reader(&source, &["a".repeat(64)]).is_err());
        }
        let reader = Broken { missing: true };
        let source = SourceReader::new(&reader);
        assert!(matches!(
            validate_retained_reader(&source, &["bad".into()]),
            Err(Error::Invalid(_))
        ));
        assert!(matches!(
            validate_retained_reader(&source, &vec!["a".repeat(64); MAX_SYNC_OBJECTS + 1]),
            Err(Error::Limit(_))
        ));
    }

    #[test]
    fn retained_union_parent_walk_consumes_one_shared_visit_budget() {
        let directory = tempfile::TempDir::new().unwrap();
        let engine = Engine::open(directory.path()).unwrap();
        let roots: Vec<_> = [1u8, 2]
            .into_iter()
            .map(|byte| {
                engine
                    .checkpoint(
                        None,
                        &[crate::model::FileChange {
                            path: "file".into(),
                            content: Some(vec![byte]),
                        }],
                    )
                    .unwrap()
            })
            .collect();
        let objects = discover(&roots, |id| engine.read_encoded(id)).unwrap();
        // A seeded counter exercises the exact aggregate boundary without a
        // million-object fixture. This is the same walker used once by qualify.
        let mut visits = MAX_QUALIFICATION_VISITS - 1;
        assert!(matches!(
            validate_parent_graph(&roots, &objects, &mut visits),
            Err(Error::Limit(_))
        ));
        assert_eq!(visits, MAX_QUALIFICATION_VISITS + 1);
        validate_retained_reader(&SourceReader::new(&engine), &roots).unwrap();
    }

    #[test]
    fn derived_directory_prefixes_consume_the_shared_qualification_budget() {
        let directory = tempfile::TempDir::new().unwrap();
        let engine = Engine::open(directory.path()).unwrap();
        let path = format!("root{}/file", "/a".repeat(500));
        let root = engine
            .checkpoint(
                None,
                &[crate::model::FileChange {
                    path: path.clone(),
                    content: Some(vec![1]),
                }],
            )
            .unwrap();
        let objects = discover(std::slice::from_ref(&root), |id| {
            engine.read_encoded_object(id)
        })
        .unwrap();
        let tree = engine.load_revision(&root).unwrap().tree;
        // This closure has one blob, one tree and one revision; all other objects are index nodes.
        let node_visits = objects.len() - 3;
        let mut visits = 0;
        qualify_tree(&tree, &objects, &mut visits, &mut |_| unreachable!()).unwrap();
        assert_eq!(visits, node_visits + directory_prefixes(&path).count());
        let mut visits = MAX_QUALIFICATION_VISITS - node_visits;
        assert!(matches!(
            qualify_tree(&tree, &objects, &mut visits, &mut |_| unreachable!()),
            Err(Error::Limit(_))
        ));
        assert_eq!(visits, MAX_QUALIFICATION_VISITS + 1);
    }

    #[test]
    fn empty_prefix_index_rejects_deep_paths_before_expansion() {
        let directory = tempfile::TempDir::new().unwrap();
        let engine = Engine::open(directory.path()).unwrap();
        let path = format!("root{}/file", "/a".repeat(500));
        let root = engine
            .checkpoint(
                None,
                &[crate::model::FileChange {
                    path,
                    content: Some(vec![1]),
                }],
            )
            .unwrap();
        let empty = engine.checkpoint(None, &[]).unwrap();
        let mut objects = discover(&[root.clone(), empty.clone()], |id| {
            engine.read_encoded_object(id)
        })
        .unwrap();
        let tree = engine.load_revision(&root).unwrap().tree;
        let empty_tree = engine.load_revision(&empty).unwrap().tree;
        let empty_prefix = match &objects[&empty_tree].object {
            EncodedObject::Tree { prefix, .. } => prefix.clone(),
            _ => unreachable!(),
        };
        if let EncodedObject::Tree { prefix, .. } = &mut objects.get_mut(&tree).unwrap().object {
            *prefix = empty_prefix;
        }
        let mut visits = 0;
        assert!(
            matches!(qualify_tree(&tree, &objects, &mut visits, &mut |_| unreachable!()), Err(Error::Invalid(message)) if message.contains("exceed the prefix index"))
        );
        // Exact/portable roots and their leaves, empty prefix root, then the first candidate.
        assert_eq!(visits, 6);
    }

    fn revision_state(version: u32, parents: Vec<String>) -> ObjectState {
        ObjectState {
            encoded_bytes: 0,
            object: EncodedObject::Revision(Revision {
                version,
                parents,
                tree: "f".repeat(64),
            }),
        }
    }

    #[test]
    fn long_reverse_sorted_history_is_ordered_with_one_examination_per_revision() {
        const COUNT: usize = 20_000;
        let mut revisions = BTreeSet::new();
        let mut objects = BTreeMap::new();
        for index in 0..COUNT {
            let id = format!("{index:064x}");
            let parents = (index + 1 < COUNT)
                .then(|| format!("{:064x}", index + 1))
                .into_iter()
                .collect();
            revisions.insert(id.clone());
            objects.insert(
                id,
                ObjectState {
                    encoded_bytes: 0,
                    object: EncodedObject::Revision(Revision {
                        version: 1,
                        parents,
                        tree: "f".repeat(64),
                    }),
                },
            );
        }

        let (order, examined) = revision_install_order(&revisions, &objects).unwrap();
        assert_eq!(examined, COUNT);
        assert_eq!(order.len(), COUNT);
        assert_eq!(order.first().unwrap(), &format!("{:064x}", COUNT - 1));
        assert_eq!(order.last().unwrap(), &format!("{:064x}", 0));
        for pair in order.windows(2) {
            let child = match &objects[&pair[1]].object {
                EncodedObject::Revision(revision) => revision,
                _ => unreachable!(),
            };
            assert_eq!(child.parents.as_slice(), std::slice::from_ref(&pair[0]));
        }
    }

    #[test]
    fn shared_root_ancestry_is_examined_once() {
        const HISTORY: usize = 20_000;
        const ROOTS: usize = 256;
        let mut objects = BTreeMap::new();
        for index in 0..HISTORY {
            let id = format!("{index:064x}");
            let parents = (index > 0)
                .then(|| format!("{:064x}", index - 1))
                .into_iter()
                .collect();
            objects.insert(
                id,
                ObjectState {
                    encoded_bytes: 0,
                    object: EncodedObject::Revision(Revision {
                        version: 1,
                        parents,
                        tree: "f".repeat(64),
                    }),
                },
            );
        }

        let shared_parent = format!("{:064x}", HISTORY - 1);
        let roots: Vec<_> = (0..ROOTS)
            .map(|index| format!("{:064x}", HISTORY + index))
            .collect();
        for root in &roots {
            objects.insert(
                root.clone(),
                ObjectState {
                    encoded_bytes: 0,
                    object: EncodedObject::Revision(Revision {
                        version: 1,
                        parents: vec![shared_parent.clone()],
                        tree: "f".repeat(64),
                    }),
                },
            );
        }

        let examined = ensure_root_antichain(&roots, &objects).unwrap();
        assert_eq!(examined, HISTORY + ROOTS);
    }

    #[test]
    fn root_ancestor_is_rejected_after_its_chain_was_memoized() {
        let ancestor = "0".repeat(64);
        let child = "1".repeat(64);
        let mut objects = BTreeMap::new();
        objects.insert(
            ancestor.clone(),
            ObjectState {
                encoded_bytes: 0,
                object: EncodedObject::Revision(Revision {
                    version: 1,
                    parents: Vec::new(),
                    tree: "f".repeat(64),
                }),
            },
        );
        objects.insert(
            child.clone(),
            ObjectState {
                encoded_bytes: 0,
                object: EncodedObject::Revision(Revision {
                    version: 1,
                    parents: vec![ancestor.clone()],
                    tree: "f".repeat(64),
                }),
            },
        );

        for roots in [
            vec![ancestor.clone(), child.clone()],
            vec![child.clone(), ancestor.clone()],
        ] {
            let error = ensure_root_antichain(&roots, &objects).unwrap_err();
            assert!(
                error
                    .to_string()
                    .contains("sync roots must not contain one another's ancestors")
            );
        }
    }

    #[test]
    fn diamond_dag_visits_each_revision_once_and_orders_every_parent_first() {
        let base = "0".repeat(64);
        let left = "1".repeat(64);
        let right = "2".repeat(64);
        let merge = "3".repeat(64);
        let objects = BTreeMap::from([
            (base.clone(), revision_state(1, Vec::new())),
            (left.clone(), revision_state(1, vec![base.clone()])),
            (right.clone(), revision_state(1, vec![base.clone()])),
            (
                merge.clone(),
                revision_state(2, vec![left.clone(), right.clone()]),
            ),
        ]);
        let roots = vec![merge.clone()];
        let mut visits = 0;
        validate_parent_graph(&roots, &objects, &mut visits).unwrap();
        assert_eq!(visits, 4);
        assert_eq!(ensure_root_antichain(&roots, &objects).unwrap(), 4);

        let revisions = objects.keys().cloned().collect();
        let (order, examined) = revision_install_order(&revisions, &objects).unwrap();
        assert_eq!(examined, 4);
        let position = |id: &str| order.iter().position(|item| item == id).unwrap();
        assert!(position(&base) < position(&left));
        assert!(position(&base) < position(&right));
        assert!(position(&left) < position(&merge));
        assert!(position(&right) < position(&merge));

        for roots in [
            vec![left.clone(), merge.clone()],
            vec![merge.clone(), left.clone()],
        ] {
            assert!(matches!(
                ensure_root_antichain(&roots, &objects),
                Err(Error::Invalid(message))
                    if message.contains("must not contain one another's ancestors")
            ));
        }
    }

    #[test]
    fn cyclic_missing_and_wrong_type_parent_graphs_are_rejected() {
        let first = "0".repeat(64);
        let second = "1".repeat(64);
        let missing = "2".repeat(64);
        let wrong_type = "3".repeat(64);
        let cycle = BTreeMap::from([
            (first.clone(), revision_state(1, vec![second.clone()])),
            (second.clone(), revision_state(1, vec![first.clone()])),
        ]);
        let revisions = cycle.keys().cloned().collect();
        let mut visits = 0;
        assert!(matches!(
            validate_parent_graph(std::slice::from_ref(&first), &cycle, &mut visits),
            Err(Error::Invalid(message)) if message.contains("cycle")
        ));
        assert!(matches!(
            revision_install_order(&revisions, &cycle),
            Err(Error::Invalid(message)) if message.contains("cycle")
        ));

        let missing_graph = BTreeMap::from([(first.clone(), revision_state(1, vec![missing]))]);
        let mut visits = 0;
        assert!(matches!(
            validate_parent_graph(std::slice::from_ref(&first), &missing_graph, &mut visits),
            Err(Error::NotFound(_))
        ));

        let wrong_type_graph = BTreeMap::from([
            (first.clone(), revision_state(1, vec![wrong_type.clone()])),
            (
                wrong_type,
                ObjectState {
                    encoded_bytes: 0,
                    object: EncodedObject::Blob,
                },
            ),
        ]);
        let mut visits = 0;
        assert!(matches!(
            validate_parent_graph(
                std::slice::from_ref(&first),
                &wrong_type_graph,
                &mut visits
            ),
            Err(Error::Invalid(message)) if message.contains("not a revision")
        ));
    }
    #[test]
    fn interrupted_diamond_install_publishes_only_complete_revisions_and_retries() {
        let sender_dir = tempfile::TempDir::new().unwrap();
        let sender = Engine::open(sender_dir.path().join("source")).unwrap();
        let base = sender.checkpoint(None, &[]).unwrap();
        let left = sender.checkpoint(Some(&base), &[]).unwrap();
        let right = sender
            .checkpoint(
                Some(&base),
                &[crate::model::FileChange {
                    path: "right".into(),
                    content: Some(vec![1]),
                }],
            )
            .unwrap();
        let root = sender
            .checkpoint_with_parents(&[left.clone(), right.clone()], &[])
            .unwrap();
        let offer_path = sender_dir.path().join("offer.json");
        let offer = write_offer(
            &sender,
            "synthetic",
            std::slice::from_ref(&root),
            &offer_path,
        )
        .unwrap();
        let non_revisions = offer
            .objects
            .iter()
            .filter(|descriptor| {
                let bytes = sender.read_encoded_object(&descriptor.id).unwrap();
                !matches!(
                    sender
                        .inspect_encoded_object(&descriptor.id, &bytes)
                        .unwrap(),
                    EncodedObject::Revision(_)
                )
            })
            .count();
        for installed_parents in [1, 3] {
            let receiver_dir = tempfile::TempDir::new().unwrap();
            let source_path = receiver_dir.path().join("source");
            let receiver = Engine::open(&source_path).unwrap();
            let request_path = receiver_dir.path().join("request.json");
            write_request(&receiver, &offer_path, &request_path).unwrap();
            let bundle_path = receiver_dir.path().join("bundle");
            write_bundle(&sender, &offer_path, &request_path, &bundle_path).unwrap();
            let state_path = receiver_dir.path().join("state");
            let completion_path = state_path
                .join(digest(&fs::read(&request_path).unwrap()))
                .join("complete");
            INSTALL_FAIL_AFTER.with(|counter| counter.set(Some(non_revisions + installed_parents)));
            assert!(
                matches!(import_bundle(&receiver, &bundle_path, &state_path), Err(Error::Io(error)) if error.to_string().contains("injected installation"))
            );
            drop(receiver);
            let receiver = Engine::open(&source_path).unwrap();
            assert!(!receiver.has_verified_object(&root).unwrap());
            assert!(!completion_path.exists());
            let mut visible = 0;
            for id in [&base, &left, &right] {
                if receiver.has_verified_object(id).unwrap() {
                    qualify_source(&receiver, std::slice::from_ref(id)).unwrap();
                    visible += 1;
                }
            }
            assert_eq!(visible, installed_parents);
            let complete = import_bundle(&receiver, &bundle_path, &state_path).unwrap();
            assert_eq!(complete.roots, vec![root.clone()]);
            qualify_source(&receiver, std::slice::from_ref(&root)).unwrap();
            assert!(completion_path.exists());
        }
    }
}
