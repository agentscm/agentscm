use std::collections::{BTreeMap, BTreeSet};
use std::fs;

use agentscm::Error;
use agentscm::model::{EntryChange, EntryKind, FileChange};
use agentscm::store::{Engine, digest};
use agentscm::sync::{
    ObjectDescriptor, SOURCE_PROFILE_V1, SOURCE_PROFILE_V2, SOURCE_PROFILE_V3, SourceOffer,
    SourceRequest, import_bundle, qualify_source, write_bundle, write_offer, write_request,
};
use sha2::{Digest, Sha256};
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

#[test]
fn file_pages_resume_without_duplicates_and_bind_revision() {
    let directory = TempDir::new().unwrap();
    let engine = Engine::open(directory.path()).unwrap();
    let changes: Vec<_> = (0..200)
        .map(|index| put(&format!("src/file-{index:03}.txt"), b"content"))
        .collect();
    let revision = engine.checkpoint(None, &changes).unwrap();

    let mut cursor = None;
    let mut paths = BTreeSet::new();
    loop {
        let page = engine.list_files(&revision, cursor.as_deref(), 7).unwrap();
        for entry in page.entries {
            assert_eq!(
                engine.read_file(&revision, &entry.path).unwrap(),
                b"content"
            );
            assert_eq!(entry.blob.len(), 64);
            assert!(paths.insert(entry.path));
        }
        cursor = page.next;
        if cursor.is_none() {
            break;
        }
    }
    assert_eq!(paths.len(), 200);

    let page = engine.list_files(&revision, None, 1).unwrap();
    let cursor = page.next.unwrap();
    let other = engine
        .checkpoint(None, &[put("foreign", b"value")])
        .unwrap();
    assert!(matches!(
        engine.list_files(&other, Some(&cursor), 1),
        Err(Error::Conflict(_))
    ));
    let other_cursor = engine.list_files(&other, None, 1).unwrap().next.unwrap();
    let forged = forge_frontier(&cursor, &other_cursor);
    assert!(matches!(
        engine.list_files(&revision, Some(&forged), 1),
        Err(Error::Invalid(_))
    ));
    let mut damaged = cursor.into_bytes();
    damaged[0] = if damaged[0] == b'0' { b'1' } else { b'0' };
    assert!(matches!(
        engine.list_files(&revision, Some(std::str::from_utf8(&damaged).unwrap()), 1),
        Err(Error::Invalid(_))
    ));
}

fn forge_frontier(original: &str, donor: &str) -> String {
    let mut original = decode_hex(original);
    let donor = decode_hex(donor);
    let count = u16::from_be_bytes([original[73], original[74]]) as usize;
    let donor_count = u16::from_be_bytes([donor[73], donor[74]]) as usize;
    let target = 75 + (count - 1) * 33;
    let source = 75 + (donor_count - 1) * 33;
    original[target..target + 32].copy_from_slice(&donor[source..source + 32]);
    let payload = original.len() - 32;
    let checksum = Sha256::digest(&original[..payload]);
    original[payload..].copy_from_slice(&checksum);
    encode_hex(&original)
}

fn decode_hex(value: &str) -> Vec<u8> {
    value
        .as_bytes()
        .chunks_exact(2)
        .map(|pair| u8::from_str_radix(std::str::from_utf8(pair).unwrap(), 16).unwrap())
        .collect()
}

fn encode_hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

#[test]
fn offline_exchange_preserves_ids_is_delta_and_retries_idempotently() {
    let source_directory = TempDir::new().unwrap();
    let receiver_directory = TempDir::new().unwrap();
    let files = TempDir::new().unwrap();
    let state = TempDir::new().unwrap();
    let source = Engine::open(source_directory.path()).unwrap();
    let receiver = Engine::open(receiver_directory.path()).unwrap();
    let base = source
        .checkpoint(None, &[put("a.txt", b"a"), put("b.txt", b"b")])
        .unwrap();
    let head = source
        .checkpoint(Some(&base), &[put("b.txt", b"updated")])
        .unwrap();
    let offer_path = files.path().join("offer.json");
    let request_path = files.path().join("request.json");
    let bundle_path = files.path().join("source.bundle");
    let offer = write_offer(
        &source,
        "source-repository",
        std::slice::from_ref(&head),
        &offer_path,
    )
    .unwrap();
    assert_eq!(offer.version, 1);
    assert_eq!(offer.profile, SOURCE_PROFILE_V1);
    assert_eq!(
        offer.required_capabilities,
        [
            "complete-revision-history",
            "exact-object-bytes",
            "root-last-import"
        ]
    );
    let request = write_request(&receiver, &offer_path, &request_path).unwrap();
    assert_eq!(request.missing.len(), offer.objects.len());
    write_bundle(&source, &offer_path, &request_path, &bundle_path).unwrap();

    let first = import_bundle(&receiver, &bundle_path, state.path()).unwrap();
    let retry = import_bundle(&receiver, &bundle_path, state.path()).unwrap();
    assert_eq!(first, retry);
    assert_eq!(first.roots, vec![head.clone()]);
    assert_eq!(receiver.read_file(&head, "b.txt").unwrap(), b"updated");
    assert_eq!(receiver.load_revision(&head).unwrap().parents, vec![base]);

    let second_request = files.path().join("request-2.json");
    let second_bundle = files.path().join("source-2.bundle");
    let request = write_request(&receiver, &offer_path, &second_request).unwrap();
    assert!(request.missing.is_empty());
    write_bundle(&source, &offer_path, &second_request, &second_bundle).unwrap();
    let outcome = import_bundle(&receiver, &second_bundle, state.path()).unwrap();
    assert_eq!(outcome.installed, 0);
    assert_eq!(outcome.reused, offer.objects.len() as u64);
}

#[test]
fn multi_parent_closure_uses_v2_and_transfers_in_both_directions() {
    let left_directory = TempDir::new().unwrap();
    let right_directory = TempDir::new().unwrap();
    let files = TempDir::new().unwrap();
    let left_state = TempDir::new().unwrap();
    let right_state = TempDir::new().unwrap();
    let left = Engine::open(left_directory.path()).unwrap();
    let right = Engine::open(right_directory.path()).unwrap();

    let base = left.checkpoint(None, &[put("base", b"base")]).unwrap();
    transfer(
        &left,
        &right,
        &base,
        "base",
        files.path(),
        right_state.path(),
    );
    let left_head = left
        .checkpoint(Some(&base), &[put("left", b"left")])
        .unwrap();
    let right_head = right
        .checkpoint(Some(&base), &[put("right", b"right")])
        .unwrap();
    transfer(
        &right,
        &left,
        &right_head,
        "right-head",
        files.path(),
        left_state.path(),
    );

    let merge = left
        .checkpoint_with_parents(
            &[left_head.clone(), right_head.clone()],
            &[put("resolution", b"left-tree-with-resolution")],
        )
        .unwrap();
    let qualified = qualify_source(&left, std::slice::from_ref(&merge)).unwrap();
    assert_eq!(qualified.version(), 2);
    assert_eq!(qualified.profile(), SOURCE_PROFILE_V2);
    assert_eq!(qualified.roots(), std::slice::from_ref(&merge));
    assert!(!qualified.objects().is_empty());
    assert_eq!(
        qualified.required_capabilities().last().map(String::as_str),
        Some("multi-parent-revisions")
    );
    let merge_offer = transfer(
        &left,
        &right,
        &merge,
        "merge",
        files.path(),
        right_state.path(),
    );
    assert_eq!(merge_offer.version, 2);
    assert_eq!(merge_offer.profile, SOURCE_PROFILE_V2);
    assert_eq!(
        merge_offer.required_capabilities,
        [
            "complete-revision-history",
            "exact-object-bytes",
            "root-last-import",
            "multi-parent-revisions"
        ]
    );
    assert_eq!(
        right.load_revision(&merge).unwrap().parents,
        [left_head.clone(), right_head.clone()]
    );

    let right_descendant = right
        .checkpoint(Some(&merge), &[put("after", b"right")])
        .unwrap();
    assert_eq!(right.load_revision(&right_descendant).unwrap().version, 1);
    let return_offer = transfer(
        &right,
        &left,
        &right_descendant,
        "return",
        files.path(),
        left_state.path(),
    );
    assert_eq!(return_offer.version, 2);
    assert_eq!(return_offer.profile, SOURCE_PROFILE_V2);
    assert_eq!(
        left.load_revision(&right_descendant).unwrap().parents,
        [merge]
    );
    assert_eq!(
        left.read_file(&right_descendant, "after").unwrap(),
        b"right"
    );
}

#[test]
fn declared_profile_must_match_revision_versions() {
    let source_directory = TempDir::new().unwrap();
    let receiver_directory = TempDir::new().unwrap();
    let files = TempDir::new().unwrap();
    let state = TempDir::new().unwrap();
    let source = Engine::open(source_directory.path()).unwrap();
    let receiver = Engine::open(receiver_directory.path()).unwrap();
    let base = source.checkpoint(None, &[put("base", b"base")]).unwrap();
    let left = source
        .checkpoint(Some(&base), &[put("left", b"left")])
        .unwrap();
    let right = source
        .checkpoint(Some(&base), &[put("right", b"right")])
        .unwrap();
    let merge = source
        .checkpoint_with_parents(&[left, right], &[put("merged", b"merged")])
        .unwrap();

    let r2_offer_path = files.path().join("r2-offer.json");
    let mut r2_offer = write_offer(
        &source,
        "source",
        std::slice::from_ref(&merge),
        &r2_offer_path,
    )
    .unwrap();
    r2_offer.version = 1;
    r2_offer.profile = SOURCE_PROFILE_V1.into();
    r2_offer.required_capabilities = v1_capabilities();
    assert_profile_mismatch_rejected(
        &source,
        &receiver,
        &r2_offer,
        "r2-as-v1",
        files.path(),
        state.path(),
    );

    let r1_offer_path = files.path().join("r1-offer.json");
    let mut r1_offer = write_offer(
        &source,
        "source",
        std::slice::from_ref(&base),
        &r1_offer_path,
    )
    .unwrap();
    r1_offer.version = 2;
    r1_offer.profile = SOURCE_PROFILE_V2.into();
    r1_offer.required_capabilities = v1_capabilities();
    r1_offer
        .required_capabilities
        .push("multi-parent-revisions".into());
    assert_profile_mismatch_rejected(
        &source,
        &receiver,
        &r1_offer,
        "r1-as-v2",
        files.path(),
        state.path(),
    );

    r1_offer
        .required_capabilities
        .push("unknown-required".into());
    let malformed = files.path().join("malformed-v2.json");
    fs::write(&malformed, serde_json::to_vec(&r1_offer).unwrap()).unwrap();
    assert!(matches!(
        write_request(
            &receiver,
            &malformed,
            &files.path().join("malformed-v2-request.json")
        ),
        Err(Error::Invalid(_))
    ));
}

fn transfer(
    source: &Engine,
    receiver: &Engine,
    root: &str,
    label: &str,
    files: &std::path::Path,
    state: &std::path::Path,
) -> SourceOffer {
    let offer_path = files.join(format!("{label}-offer.json"));
    let request_path = files.join(format!("{label}-request.json"));
    let bundle_path = files.join(format!("{label}.bundle"));
    let offer = write_offer(source, "source", &[root.to_owned()], &offer_path).unwrap();
    write_request(receiver, &offer_path, &request_path).unwrap();
    write_bundle(source, &offer_path, &request_path, &bundle_path).unwrap();
    import_bundle(receiver, &bundle_path, state).unwrap();
    offer
}

fn assert_profile_mismatch_rejected(
    source: &Engine,
    receiver: &Engine,
    offer: &SourceOffer,
    label: &str,
    files: &std::path::Path,
    state: &std::path::Path,
) {
    let offer_path = files.join(format!("{label}-offer.json"));
    let request_path = files.join(format!("{label}-request.json"));
    let bundle_path = files.join(format!("{label}.bundle"));
    fs::write(&offer_path, serde_json::to_vec(offer).unwrap()).unwrap();
    write_request(receiver, &offer_path, &request_path).unwrap();
    write_bundle(source, &offer_path, &request_path, &bundle_path).unwrap();
    assert!(matches!(
        import_bundle(receiver, &bundle_path, state),
        Err(Error::Invalid(message))
            if message.contains("profile does not match revision encodings")
    ));
    for root in &offer.roots {
        assert!(matches!(
            receiver.load_revision(root),
            Err(Error::NotFound(_))
        ));
    }
}

fn v1_capabilities() -> Vec<String> {
    [
        "complete-revision-history",
        "exact-object-bytes",
        "root-last-import",
    ]
    .map(str::to_owned)
    .to_vec()
}

#[test]
fn truncated_import_exposes_no_revision_and_full_retry_succeeds() {
    let source_directory = TempDir::new().unwrap();
    let receiver_directory = TempDir::new().unwrap();
    let files = TempDir::new().unwrap();
    let state = TempDir::new().unwrap();
    let source = Engine::open(source_directory.path()).unwrap();
    let receiver = Engine::open(receiver_directory.path()).unwrap();
    let revision = source
        .checkpoint(None, &[put("large.bin", &vec![7; 128 * 1024])])
        .unwrap();
    let offer = files.path().join("offer.json");
    let request = files.path().join("request.json");
    let bundle = files.path().join("source.bundle");
    write_offer(&source, "source", std::slice::from_ref(&revision), &offer).unwrap();
    write_request(&receiver, &offer, &request).unwrap();
    write_bundle(&source, &offer, &request, &bundle).unwrap();
    let mut bytes = fs::read(&bundle).unwrap();
    bytes.truncate(bytes.len() - 5);
    let truncated = files.path().join("truncated.bundle");
    fs::write(&truncated, bytes).unwrap();

    assert!(import_bundle(&receiver, &truncated, state.path()).is_err());
    assert!(matches!(
        receiver.load_revision(&revision),
        Err(Error::NotFound(_))
    ));
    import_bundle(&receiver, &bundle, state.path()).unwrap();
    assert_eq!(
        receiver.read_file(&revision, "large.bin").unwrap().len(),
        128 * 1024
    );
}

#[test]
fn unknown_capability_and_unrelated_offer_object_are_rejected() {
    let source_directory = TempDir::new().unwrap();
    let receiver_directory = TempDir::new().unwrap();
    let files = TempDir::new().unwrap();
    let source = Engine::open(source_directory.path()).unwrap();
    let receiver = Engine::open(receiver_directory.path()).unwrap();
    let first = source.checkpoint(None, &[put("one", b"1")]).unwrap();
    let unrelated = source.checkpoint(None, &[put("two", b"2")]).unwrap();
    let first_offer_path = files.path().join("first.json");
    let unrelated_offer_path = files.path().join("unrelated.json");
    let mut offer = write_offer(
        &source,
        "source",
        std::slice::from_ref(&first),
        &first_offer_path,
    )
    .unwrap();
    let unrelated_offer =
        write_offer(&source, "source", &[unrelated], &unrelated_offer_path).unwrap();

    offer.required_capabilities.push("unknown-required".into());
    let unknown_path = files.path().join("unknown.json");
    fs::write(&unknown_path, serde_json::to_vec(&offer).unwrap()).unwrap();
    assert!(matches!(
        write_request(
            &receiver,
            &unknown_path,
            &files.path().join("unknown-request.json")
        ),
        Err(Error::Invalid(_))
    ));

    let mut offer: SourceOffer =
        serde_json::from_slice(&fs::read(&first_offer_path).unwrap()).unwrap();
    let existing: BTreeSet<_> = offer.objects.iter().map(|item| item.id.clone()).collect();
    offer.objects.extend(
        unrelated_offer
            .objects
            .into_iter()
            .filter(|item| !existing.contains(&item.id)),
    );
    offer.objects.sort_by(|left, right| left.id.cmp(&right.id));
    let extra_offer = files.path().join("extra.json");
    let extra_request = files.path().join("extra-request.json");
    let extra_bundle = files.path().join("extra.bundle");
    fs::write(&extra_offer, serde_json::to_vec(&offer).unwrap()).unwrap();
    write_request(&receiver, &extra_offer, &extra_request).unwrap();
    write_bundle(&source, &extra_offer, &extra_request, &extra_bundle).unwrap();
    assert!(matches!(
        import_bundle(&receiver, &extra_bundle, files.path()),
        Err(Error::Invalid(_))
    ));
    assert!(matches!(
        receiver.load_revision(&first),
        Err(Error::NotFound(_))
    ));
}

#[test]
fn correctly_hashed_but_inconsistent_manifest_is_not_published() {
    let (root, objects) = inconsistent_source_objects();
    assert_hostile_source_rejected(root, objects);
}

#[test]
fn correctly_hashed_leaf_in_wrong_branch_slot_is_not_published() {
    let (root, objects) = wrong_slot_source_objects();
    assert_hostile_source_rejected(root, objects);
}

#[test]
fn correctly_hashed_empty_non_root_branch_is_not_published() {
    let (root, objects) = empty_non_root_branch_source_objects();
    assert_hostile_source_rejected(root, objects);
}

#[test]
fn correctly_hashed_root_with_inconsistent_compressed_prefix_is_not_published() {
    let (root, objects) = malformed_root_compressed_prefix_source_objects();
    assert_eq!(objects.len(), 10);
    assert_hostile_source_rejected(root, objects);
}

#[test]
fn correctly_hashed_nested_branch_with_inconsistent_compressed_prefix_is_not_published() {
    let (root, objects) = malformed_nested_compressed_prefix_source_objects();
    assert_hostile_source_rejected(root, objects);
}

#[test]
fn valid_compressed_manifest_imports_and_remains_editable() {
    let source_directory = TempDir::new().unwrap();
    let receiver_directory = TempDir::new().unwrap();
    let files = TempDir::new().unwrap();
    let state = TempDir::new().unwrap();
    let source = Engine::open(source_directory.path()).unwrap();
    let receiver = Engine::open(receiver_directory.path()).unwrap();
    let root = source
        .checkpoint(None, &[put("n0", b"zero"), put("n3", b"three")])
        .unwrap();
    let offer = files.path().join("compressed.json");
    let request = files.path().join("compressed-request.json");
    let bundle = files.path().join("compressed.bundle");
    write_offer(&source, "source", std::slice::from_ref(&root), &offer).unwrap();
    write_request(&receiver, &offer, &request).unwrap();
    write_bundle(&source, &offer, &request, &bundle).unwrap();
    import_bundle(&receiver, &bundle, state.path()).unwrap();

    let edited = receiver.checkpoint(Some(&root), &[delete("n3")]).unwrap();
    assert_eq!(receiver.read_file(&edited, "n0").unwrap(), b"zero");
    assert!(matches!(
        receiver.read_file(&edited, "n3"),
        Err(Error::NotFound(_))
    ));
}

fn assert_hostile_source_rejected(root: String, objects: BTreeMap<String, Vec<u8>>) {
    let receiver_directory = TempDir::new().unwrap();
    let files = TempDir::new().unwrap();
    let state = TempDir::new().unwrap();
    let receiver = Engine::open(receiver_directory.path()).unwrap();
    let offer = SourceOffer {
        version: 1,
        profile: SOURCE_PROFILE_V1.into(),
        required_capabilities: vec![
            "complete-revision-history".into(),
            "exact-object-bytes".into(),
            "root-last-import".into(),
        ],
        origin_repository: "hostile".into(),
        roots: vec![root.clone()],
        objects: objects
            .iter()
            .map(|(id, bytes)| ObjectDescriptor {
                id: id.clone(),
                encoded_bytes: bytes.len() as u64,
            })
            .collect(),
    };
    let offer_bytes = serde_json::to_vec(&offer).unwrap();
    let request = SourceRequest {
        version: 1,
        offer_sha256: digest(&offer_bytes),
        missing: objects.keys().cloned().collect(),
    };
    let request_bytes = serde_json::to_vec(&request).unwrap();
    let bundle = files.path().join("hostile.bundle");
    write_raw_bundle(&bundle, &offer_bytes, &request_bytes, &objects);

    assert!(matches!(
        import_bundle(&receiver, &bundle, state.path()),
        Err(Error::Invalid(_))
    ));
    assert!(matches!(
        receiver.load_revision(&root),
        Err(Error::NotFound(_))
    ));
}

fn inconsistent_source_objects() -> (String, BTreeMap<String, Vec<u8>>) {
    let mut objects = BTreeMap::new();
    let mut blob_body = 1u64.to_be_bytes().to_vec();
    blob_body.push(b'x');
    let blob = object(b"AGSCM-B1\0", &blob_body);
    objects.insert(blob.0.clone(), blob.1);
    let key: [u8; 32] = Sha256::digest(b"file").into();
    let mut leaf_body = Vec::new();
    leaf_body.push(1);
    leaf_body.extend_from_slice(&key);
    leaf_body.extend_from_slice(&(4u16.to_be_bytes()));
    leaf_body.extend_from_slice(b"file");
    leaf_body.extend_from_slice(&id_bytes(&blob.0));
    let exact = object(b"AGSCM-L1\0", &leaf_body);
    objects.insert(exact.0.clone(), exact.1);
    let portable = object(b"AGSCM-N1\0", &[2, 0, 0]);
    objects.insert(portable.0.clone(), portable.1);
    let prefix = object(b"AGSCM-N1\0", &[3, 0, 0]);
    objects.insert(prefix.0.clone(), prefix.1);
    let mut tree_body = Vec::new();
    tree_body.extend_from_slice(&id_bytes(&exact.0));
    tree_body.extend_from_slice(&id_bytes(&portable.0));
    tree_body.extend_from_slice(&id_bytes(&prefix.0));
    let tree = object(b"AGSCM-T2\0", &tree_body);
    objects.insert(tree.0.clone(), tree.1);
    let mut revision_body = Vec::new();
    revision_body.extend_from_slice(&1u32.to_be_bytes());
    revision_body.push(0);
    revision_body.extend_from_slice(&id_bytes(&tree.0));
    let revision = object(b"AGSCM-R1\0", &revision_body);
    objects.insert(revision.0.clone(), revision.1);
    (revision.0, objects)
}

fn wrong_slot_source_objects() -> (String, BTreeMap<String, Vec<u8>>) {
    let mut objects = BTreeMap::new();
    let mut blob_body = 1u64.to_be_bytes().to_vec();
    blob_body.push(b'x');
    let blob = object(b"AGSCM-B1\0", &blob_body);
    objects.insert(blob.0.clone(), blob.1);
    let key: [u8; 32] = Sha256::digest(b"file").into();
    let mut leaf_body = vec![1];
    leaf_body.extend_from_slice(&key);
    leaf_body.extend_from_slice(&4u16.to_be_bytes());
    leaf_body.extend_from_slice(b"file");
    leaf_body.extend_from_slice(&id_bytes(&blob.0));
    let leaf = object(b"AGSCM-L1\0", &leaf_body);
    objects.insert(leaf.0.clone(), leaf.1);
    let mut branch_body = vec![1, 0, 1, ((key[0] >> 4) + 1) % 16];
    branch_body.extend_from_slice(&id_bytes(&leaf.0));
    let exact = object(b"AGSCM-N1\0", &branch_body);
    objects.insert(exact.0.clone(), exact.1);
    let portable = object(b"AGSCM-N1\0", &[2, 0, 0]);
    objects.insert(portable.0.clone(), portable.1);
    let prefix = object(b"AGSCM-N1\0", &[3, 0, 0]);
    objects.insert(prefix.0.clone(), prefix.1);
    let mut tree_body = Vec::new();
    tree_body.extend_from_slice(&id_bytes(&exact.0));
    tree_body.extend_from_slice(&id_bytes(&portable.0));
    tree_body.extend_from_slice(&id_bytes(&prefix.0));
    let tree = object(b"AGSCM-T2\0", &tree_body);
    objects.insert(tree.0.clone(), tree.1);
    let mut revision_body = 1u32.to_be_bytes().to_vec();
    revision_body.push(0);
    revision_body.extend_from_slice(&id_bytes(&tree.0));
    let revision = object(b"AGSCM-R1\0", &revision_body);
    objects.insert(revision.0.clone(), revision.1);
    (revision.0, objects)
}

fn empty_non_root_branch_source_objects() -> (String, BTreeMap<String, Vec<u8>>) {
    let mut objects = BTreeMap::new();
    let child = object(b"AGSCM-N1\0", &[1, 1, 0]);
    objects.insert(child.0.clone(), child.1);
    let mut exact_body = vec![1, 0, 1, 0];
    exact_body.extend_from_slice(&id_bytes(&child.0));
    let exact = object(b"AGSCM-N1\0", &exact_body);
    objects.insert(exact.0.clone(), exact.1);
    let portable = object(b"AGSCM-N1\0", &[2, 0, 0]);
    objects.insert(portable.0.clone(), portable.1);
    let prefix = object(b"AGSCM-N1\0", &[3, 0, 0]);
    objects.insert(prefix.0.clone(), prefix.1);
    let mut tree_body = Vec::new();
    tree_body.extend_from_slice(&id_bytes(&exact.0));
    tree_body.extend_from_slice(&id_bytes(&portable.0));
    tree_body.extend_from_slice(&id_bytes(&prefix.0));
    let tree = object(b"AGSCM-T2\0", &tree_body);
    objects.insert(tree.0.clone(), tree.1);
    let mut revision_body = 1u32.to_be_bytes().to_vec();
    revision_body.push(0);
    revision_body.extend_from_slice(&id_bytes(&tree.0));
    let revision = object(b"AGSCM-R1\0", &revision_body);
    objects.insert(revision.0.clone(), revision.1);
    (revision.0, objects)
}

fn malformed_root_compressed_prefix_source_objects() -> (String, BTreeMap<String, Vec<u8>>) {
    let mut objects = BTreeMap::new();
    let (exact_a, portable_a) = insert_file_nodes(&mut objects, "a", b"x");
    let (exact_b, portable_b) = insert_file_nodes(&mut objects, "b", b"x");
    let key_a: [u8; 32] = Sha256::digest(b"a").into();
    let key_b: [u8; 32] = Sha256::digest(b"b").into();

    let exact = insert_branch(
        &mut objects,
        1,
        1,
        vec![(nibble(&key_a, 1), exact_a), (nibble(&key_b, 1), exact_b)],
    );
    let portable = insert_branch(
        &mut objects,
        2,
        0,
        vec![
            (nibble(&key_a, 0), portable_a),
            (nibble(&key_b, 0), portable_b),
        ],
    );
    finish_source_objects(objects, exact, portable)
}

fn malformed_nested_compressed_prefix_source_objects() -> (String, BTreeMap<String, Vec<u8>>) {
    let mut objects = BTreeMap::new();
    let (exact_n0, portable_n0) = insert_file_nodes(&mut objects, "n0", b"zero");
    let (exact_n3, portable_n3) = insert_file_nodes(&mut objects, "n3", b"three");
    let key_n0: [u8; 32] = Sha256::digest(b"n0").into();
    let key_n3: [u8; 32] = Sha256::digest(b"n3").into();
    assert_eq!(nibble(&key_n0, 0), nibble(&key_n3, 0));
    assert_ne!(nibble(&key_n0, 1), nibble(&key_n3, 1));

    let malformed_child = insert_branch(
        &mut objects,
        1,
        2,
        vec![
            (nibble(&key_n0, 2), exact_n0),
            (nibble(&key_n3, 2), exact_n3),
        ],
    );
    let exact = insert_branch(
        &mut objects,
        1,
        0,
        vec![(nibble(&key_n0, 0), malformed_child)],
    );
    let portable_child = insert_branch(
        &mut objects,
        2,
        1,
        vec![
            (nibble(&key_n0, 1), portable_n0),
            (nibble(&key_n3, 1), portable_n3),
        ],
    );
    let portable = insert_branch(
        &mut objects,
        2,
        0,
        vec![(nibble(&key_n0, 0), portable_child)],
    );
    finish_source_objects(objects, exact, portable)
}

fn insert_file_nodes(
    objects: &mut BTreeMap<String, Vec<u8>>,
    path: &str,
    content: &[u8],
) -> (String, String) {
    let mut blob_body = (content.len() as u64).to_be_bytes().to_vec();
    blob_body.extend_from_slice(content);
    let blob = object(b"AGSCM-B1\0", &blob_body);
    objects.insert(blob.0.clone(), blob.1);
    let key: [u8; 32] = Sha256::digest(path.as_bytes()).into();
    let exact = encoded_leaf(1, key, path, &blob.0);
    objects.insert(exact.0.clone(), exact.1);
    let portable = encoded_leaf(2, key, path, &exact.0);
    objects.insert(portable.0.clone(), portable.1);
    (exact.0, portable.0)
}

fn encoded_leaf(namespace: u8, key: [u8; 32], path: &str, value: &str) -> (String, Vec<u8>) {
    let mut body = vec![namespace];
    body.extend_from_slice(&key);
    body.extend_from_slice(&(path.len() as u16).to_be_bytes());
    body.extend_from_slice(path.as_bytes());
    body.extend_from_slice(&id_bytes(value));
    object(b"AGSCM-L1\0", &body)
}

fn insert_branch(
    objects: &mut BTreeMap<String, Vec<u8>>,
    namespace: u8,
    depth: u8,
    mut children: Vec<(u8, String)>,
) -> String {
    children.sort_by_key(|(slot, _)| *slot);
    let mut body = vec![namespace, depth, children.len() as u8];
    for (slot, child) in children {
        body.push(slot);
        body.extend_from_slice(&id_bytes(&child));
    }
    let branch = object(b"AGSCM-N1\0", &body);
    objects.insert(branch.0.clone(), branch.1);
    branch.0
}

fn finish_source_objects(
    mut objects: BTreeMap<String, Vec<u8>>,
    exact: String,
    portable: String,
) -> (String, BTreeMap<String, Vec<u8>>) {
    let prefix = object(b"AGSCM-N1\0", &[3, 0, 0]);
    objects.insert(prefix.0.clone(), prefix.1);
    let mut tree_body = Vec::new();
    tree_body.extend_from_slice(&id_bytes(&exact));
    tree_body.extend_from_slice(&id_bytes(&portable));
    tree_body.extend_from_slice(&id_bytes(&prefix.0));
    let tree = object(b"AGSCM-T2\0", &tree_body);
    objects.insert(tree.0.clone(), tree.1);
    let mut revision_body = 1u32.to_be_bytes().to_vec();
    revision_body.push(0);
    revision_body.extend_from_slice(&id_bytes(&tree.0));
    let revision = object(b"AGSCM-R1\0", &revision_body);
    objects.insert(revision.0.clone(), revision.1);
    (revision.0, objects)
}

fn nibble(key: &[u8; 32], depth: usize) -> u8 {
    if depth.is_multiple_of(2) {
        key[depth / 2] >> 4
    } else {
        key[depth / 2] & 0x0f
    }
}

fn object(magic: &[u8], body: &[u8]) -> (String, Vec<u8>) {
    let mut bytes = magic.to_vec();
    bytes.extend_from_slice(body);
    (digest(&bytes), bytes)
}

fn id_bytes(id: &str) -> Vec<u8> {
    id.as_bytes()
        .chunks_exact(2)
        .map(|pair| u8::from_str_radix(std::str::from_utf8(pair).unwrap(), 16).unwrap())
        .collect()
}

fn write_raw_bundle(
    path: &std::path::Path,
    offer: &[u8],
    request: &[u8],
    objects: &BTreeMap<String, Vec<u8>>,
) {
    let mut bytes = b"AGSCM-S1\0".to_vec();
    bytes.extend_from_slice(&(offer.len() as u64).to_be_bytes());
    bytes.extend_from_slice(offer);
    bytes.extend_from_slice(&(request.len() as u64).to_be_bytes());
    bytes.extend_from_slice(request);
    bytes.extend_from_slice(&(objects.len() as u32).to_be_bytes());
    for (id, object) in objects {
        bytes.extend_from_slice(&id_bytes(id));
        bytes.extend_from_slice(&(object.len() as u64).to_be_bytes());
        bytes.extend_from_slice(object);
    }
    bytes.extend_from_slice(b"AGSCM-E1\0");
    fs::write(path, bytes).unwrap();
}

fn v3_capabilities() -> Vec<String> {
    let mut capabilities = v1_capabilities();
    capabilities.push("multi-parent-revisions".into());
    capabilities.push("entry-kinds".into());
    capabilities
}

fn kinded(path: &str, content: &[u8], kind: EntryKind) -> EntryChange {
    EntryChange {
        path: path.to_owned(),
        content: Some(content.to_vec()),
        kind,
    }
}

#[test]
fn entry_kinds_select_v3_and_round_trip_through_a_bundle() {
    let source_directory = TempDir::new().unwrap();
    let receiver_directory = TempDir::new().unwrap();
    let files = TempDir::new().unwrap();
    let state = TempDir::new().unwrap();
    let source = Engine::open(source_directory.path()).unwrap();
    let receiver = Engine::open(receiver_directory.path()).unwrap();
    let plain = source.checkpoint(None, &[put("plain", b"p")]).unwrap();
    let plain_offer = transfer(
        &source,
        &receiver,
        &plain,
        "plain",
        files.path(),
        state.path(),
    );
    assert_eq!(plain_offer.profile, SOURCE_PROFILE_V1);

    let head = source
        .checkpoint_entries(
            Some(&plain),
            &[
                kinded("bin/run.sh", b"#!/bin/sh\n", EntryKind::Executable),
                kinded("bin/link", b"../plain", EntryKind::Symlink),
            ],
        )
        .unwrap();
    let qualified = qualify_source(&source, std::slice::from_ref(&head)).unwrap();
    assert_eq!(qualified.version(), 3);
    assert_eq!(qualified.profile(), SOURCE_PROFILE_V3);
    assert_eq!(qualified.required_capabilities(), v3_capabilities());

    let offer = transfer(
        &source,
        &receiver,
        &head,
        "kinds",
        files.path(),
        state.path(),
    );
    assert_eq!(offer.version, 3);
    assert_eq!(offer.profile, SOURCE_PROFILE_V3);
    assert_eq!(offer.required_capabilities, v3_capabilities());
    assert_eq!(
        receiver.read_entry(&head, "bin/run.sh").unwrap(),
        (EntryKind::Executable, b"#!/bin/sh\n".to_vec())
    );
    assert_eq!(
        receiver.read_entry(&head, "bin/link").unwrap(),
        (EntryKind::Symlink, b"../plain".to_vec())
    );
    // The receiver re-exports the imported closure as v3 again.
    let echo = qualify_source(&receiver, std::slice::from_ref(&head)).unwrap();
    assert_eq!(echo.profile(), SOURCE_PROFILE_V3);
    // A later regular-only edit still carries the kinds, so it stays v3.
    let child = receiver
        .checkpoint(Some(&head), &[put("plain", b"changed")])
        .unwrap();
    assert_eq!(
        qualify_source(&receiver, std::slice::from_ref(&child))
            .unwrap()
            .profile(),
        SOURCE_PROFILE_V3
    );
}

#[test]
fn declared_profile_must_match_entry_kind_presence() {
    let source_directory = TempDir::new().unwrap();
    let receiver_directory = TempDir::new().unwrap();
    let files = TempDir::new().unwrap();
    let state = TempDir::new().unwrap();
    let source = Engine::open(source_directory.path()).unwrap();
    let receiver = Engine::open(receiver_directory.path()).unwrap();
    let base = source.checkpoint(None, &[put("base", b"base")]).unwrap();
    let left = source
        .checkpoint(Some(&base), &[put("left", b"left")])
        .unwrap();
    let right = source
        .checkpoint(Some(&base), &[put("right", b"right")])
        .unwrap();
    let merge = source
        .checkpoint_with_parents(&[left, right], &[put("merged", b"merged")])
        .unwrap();
    let kinds = source
        .checkpoint_entries(
            Some(&base),
            &[kinded("run.sh", b"x", EntryKind::Executable)],
        )
        .unwrap();
    let kinds_merge = source
        .checkpoint_entries_with_parents(
            &[kinds.clone(), merge.clone()],
            &[kinded("link", b"run.sh", EntryKind::Symlink)],
        )
        .unwrap();

    // L2 closures claiming v1 or v2.
    for (label, root, version, profile, capabilities) in [
        (
            "kinds-as-v1",
            &kinds,
            1,
            SOURCE_PROFILE_V1,
            v1_capabilities(),
        ),
        (
            "kinds-as-v2",
            &kinds_merge,
            2,
            SOURCE_PROFILE_V2,
            v3_capabilities()[..4].to_vec(),
        ),
    ] {
        let path = files.path().join(format!("{label}-source.json"));
        let mut offer = write_offer(&source, "source", std::slice::from_ref(root), &path).unwrap();
        assert_eq!(offer.profile, SOURCE_PROFILE_V3);
        offer.version = version;
        offer.profile = profile.into();
        offer.required_capabilities = capabilities;
        assert_profile_mismatch_rejected(
            &source,
            &receiver,
            &offer,
            label,
            files.path(),
            state.path(),
        );
    }

    // V1 and V2 closures claiming v3, and v3 with the v2 capability list.
    for (label, root) in [("r1-as-v3", &base), ("r2-as-v3", &merge)] {
        let path = files.path().join(format!("{label}-source.json"));
        let mut offer = write_offer(&source, "source", std::slice::from_ref(root), &path).unwrap();
        offer.version = 3;
        offer.profile = SOURCE_PROFILE_V3.into();
        offer.required_capabilities = v3_capabilities();
        assert_profile_mismatch_rejected(
            &source,
            &receiver,
            &offer,
            label,
            files.path(),
            state.path(),
        );
    }
    let path = files.path().join("v3-caps.json");
    let mut offer = write_offer(&source, "source", std::slice::from_ref(&kinds), &path).unwrap();
    offer.required_capabilities.pop();
    fs::write(&path, serde_json::to_vec(&offer).unwrap()).unwrap();
    assert!(matches!(
        write_request(&receiver, &path, &files.path().join("v3-caps-request.json")),
        Err(Error::Invalid(_))
    ));
}

fn symlink_source_objects(link_path: &str, target: &[u8]) -> (String, BTreeMap<String, Vec<u8>>) {
    let mut objects = BTreeMap::new();
    let mut blob_body = (target.len() as u64).to_be_bytes().to_vec();
    blob_body.extend_from_slice(target);
    let blob = object(b"AGSCM-B1\0", &blob_body);
    objects.insert(blob.0.clone(), blob.1);
    let key: [u8; 32] = Sha256::digest(link_path.as_bytes()).into();
    let mut exact_body = vec![1u8];
    exact_body.extend_from_slice(&key);
    exact_body.extend_from_slice(&(link_path.len() as u16).to_be_bytes());
    exact_body.extend_from_slice(link_path.as_bytes());
    exact_body.extend_from_slice(&id_bytes(&blob.0));
    exact_body.push(2);
    let exact = object(b"AGSCM-L2\0", &exact_body);
    objects.insert(exact.0.clone(), exact.1);
    let portable = encoded_leaf(2, key, link_path, &exact.0);
    objects.insert(portable.0.clone(), portable.1);
    finish_source_objects(objects, exact.0, portable.0)
}

fn import_raw_v3(
    root: &str,
    objects: &BTreeMap<String, Vec<u8>>,
    receiver: &Engine,
) -> agentscm::Result<agentscm::sync::ImportOutcome> {
    let files = TempDir::new().unwrap();
    let state = TempDir::new().unwrap();
    let offer = SourceOffer {
        version: 3,
        profile: SOURCE_PROFILE_V3.into(),
        required_capabilities: v3_capabilities(),
        origin_repository: "hostile".into(),
        roots: vec![root.to_owned()],
        objects: objects
            .iter()
            .map(|(id, bytes)| ObjectDescriptor {
                id: id.clone(),
                encoded_bytes: bytes.len() as u64,
            })
            .collect(),
    };
    let offer_bytes = serde_json::to_vec(&offer).unwrap();
    let request = SourceRequest {
        version: 1,
        offer_sha256: digest(&offer_bytes),
        missing: objects.keys().cloned().collect(),
    };
    let request_bytes = serde_json::to_vec(&request).unwrap();
    let bundle = files.path().join("raw.bundle");
    write_raw_bundle(&bundle, &offer_bytes, &request_bytes, objects);
    import_bundle(receiver, &bundle, state.path())
}

#[test]
fn hostile_symlink_blobs_in_an_imported_closure_are_rejected() {
    let directory = TempDir::new().unwrap();
    let receiver = Engine::open(directory.path()).unwrap();
    let (root, objects) = symlink_source_objects("l", b"target.txt");
    import_raw_v3(&root, &objects, &receiver).unwrap();
    assert_eq!(
        receiver.read_entry(&root, "l").unwrap(),
        (EntryKind::Symlink, b"target.txt".to_vec())
    );

    let long = "a/".repeat(600);
    for (label, link, target) in [
        ("absolute", "l", b"/etc/passwd".to_vec()),
        ("escape", "l", b"../x".to_vec()),
        ("git", "l", b".git/config".to_vec()),
        ("backslash", "l", b"a\\b".to_vec()),
        ("empty", "l", Vec::new()),
        ("oversize", "l", long.into_bytes()),
    ] {
        let receiver_directory = TempDir::new().unwrap();
        let receiver = Engine::open(receiver_directory.path()).unwrap();
        let (root, objects) = symlink_source_objects(link, &target);
        assert!(
            matches!(
                import_raw_v3(&root, &objects, &receiver),
                Err(Error::Invalid(_))
            ),
            "{label} was imported"
        );
        assert!(matches!(
            receiver.load_revision(&root),
            Err(Error::NotFound(_))
        ));
    }
}
