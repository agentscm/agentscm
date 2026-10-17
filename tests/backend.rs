use std::fs;
use std::io::Read;
use std::sync::{Arc, Barrier};
use std::thread;

use agentscm::Error;
use agentscm::backend::{
    ImmutableBackend, LocalBackend, MAX_RANGE_BYTES, ObjectKey, ObjectMetadata, PutOutcome,
    SOURCE_PROFILE_V1, SOURCE_PROFILE_V2,
};
use agentscm::model::FileChange;
use agentscm::store::{EncodedObjectReader, Engine, MAX_OBJECT_BYTES, SourceReader, digest};
use tempfile::TempDir;

fn key(trust_domain: &str, repository: &str, bytes: &[u8]) -> ObjectKey {
    ObjectKey::new(trust_domain, repository, SOURCE_PROFILE_V1, digest(bytes)).unwrap()
}

fn object_path(directory: &TempDir, key: &ObjectKey, incarnation: &str) -> std::path::PathBuf {
    fn append(path: &mut std::path::PathBuf, value: &str) {
        let encoded: String = value.bytes().map(|b| format!("{b:02x}")).collect();
        let boundary = encoded.len().min(128);
        path.push(&encoded[..boundary]);
        path.push(if boundary == encoded.len() {
            "_"
        } else {
            &encoded[boundary..]
        });
    }
    let mut path = directory.path().to_path_buf();
    append(&mut path, key.trust_domain());
    append(&mut path, key.repository());
    path.push(key.profile());
    append(&mut path, incarnation);
    path.push(key.id());
    path
}

fn put(
    backend: &LocalBackend,
    key: &ObjectKey,
    incarnation: &str,
    bytes: &[u8],
) -> agentscm::Result<PutOutcome> {
    backend.prepare_publication(key, incarnation)?;
    backend.put_if_absent(key, incarnation, bytes)
}

struct BackendReader<'a> {
    backend: &'a LocalBackend,
    trust_domain: &'a str,
    repository: &'a str,
    profile: &'a str,
    incarnation: &'a str,
}

impl EncodedObjectReader for BackendReader<'_> {
    fn open_encoded(&self, id: &str) -> agentscm::Result<Box<dyn Read + '_>> {
        let key = ObjectKey::new(self.trust_domain, self.repository, self.profile, id)?;
        let stream: Box<dyn Read + '_> = self.backend.open_encoded(&key, self.incarnation)?;
        Ok(stream)
    }
}

struct NoRawBackend;

impl ImmutableBackend for NoRawBackend {
    fn prepare_publication(&self, _: &ObjectKey, _: &str) -> agentscm::Result<()> {
        unreachable!("raw-open default must not prepare a publication")
    }

    fn put_if_absent(&self, _: &ObjectKey, _: &str, _: &[u8]) -> agentscm::Result<PutOutcome> {
        unreachable!("raw-open default must not write")
    }

    fn get_verified(&self, _: &ObjectKey, _: &str) -> agentscm::Result<Vec<u8>> {
        unreachable!("raw-open default must not materialize a verified object")
    }

    fn inspect_verified(&self, _: &ObjectKey, _: &str) -> agentscm::Result<ObjectMetadata> {
        unreachable!("raw-open default must not inspect a verified object")
    }

    fn get_range_verified(
        &self,
        _: &ObjectKey,
        _: &str,
        _: u64,
        _: u64,
    ) -> agentscm::Result<Vec<u8>> {
        unreachable!("raw-open default must not use verified ranges")
    }
}

#[test]
fn concurrent_puts_publish_one_exact_object() {
    let directory = TempDir::new().unwrap();
    let backend = Arc::new(LocalBackend::open(directory.path()).unwrap());
    let bytes = Arc::new(b"concurrent immutable object".to_vec());
    let key = Arc::new(key("team-a", "repository-a", &bytes));
    let barrier = Arc::new(Barrier::new(8));

    let handles: Vec<_> = (0..8)
        .map(|_| {
            let backend = Arc::clone(&backend);
            let bytes = Arc::clone(&bytes);
            let key = Arc::clone(&key);
            let barrier = Arc::clone(&barrier);
            thread::spawn(move || {
                barrier.wait();
                put(&backend, &key, "generation-1", &bytes)
            })
        })
        .collect();

    let outcomes: Vec<_> = handles
        .into_iter()
        .map(|handle| handle.join().unwrap().unwrap())
        .collect();
    assert_eq!(
        outcomes
            .iter()
            .filter(|outcome| **outcome == PutOutcome::Created)
            .count(),
        1
    );
    assert_eq!(
        outcomes
            .iter()
            .filter(|outcome| **outcome == PutOutcome::AlreadyPresent)
            .count(),
        7
    );
    assert_eq!(backend.get_verified(&key, "generation-1").unwrap(), *bytes);
}

#[test]
fn namespaces_and_physical_incarnations_are_isolated() {
    let directory = TempDir::new().unwrap();
    let backend = LocalBackend::open(directory.path()).unwrap();
    let bytes = b"same canonical bytes";
    let key_a = key("team-a", "repository", bytes);
    let key_b = key("team-b", "repository", bytes);

    assert_eq!(
        put(&backend, &key_a, "generation-1", bytes).unwrap(),
        PutOutcome::Created
    );
    assert_eq!(
        put(&backend, &key_b, "generation-1", bytes).unwrap(),
        PutOutcome::Created
    );
    assert_eq!(
        put(&backend, &key_a, "generation-2", bytes).unwrap(),
        PutOutcome::Created
    );

    fs::write(object_path(&directory, &key_a, "generation-1"), b"corrupt").unwrap();
    assert!(matches!(
        backend.get_verified(&key_a, "generation-1"),
        Err(Error::Invalid(_))
    ));
    assert_eq!(backend.get_verified(&key_b, "generation-1").unwrap(), bytes);
    assert_eq!(backend.get_verified(&key_a, "generation-2").unwrap(), bytes);
}

#[test]
fn verified_reads_reject_corruption_and_truncation() {
    let directory = TempDir::new().unwrap();
    let backend = LocalBackend::open(directory.path()).unwrap();
    let bytes = b"verified object bytes";
    let key = key("team", "repository", bytes);
    put(&backend, &key, "corrupt", bytes).unwrap();
    put(&backend, &key, "truncated", bytes).unwrap();

    fs::write(
        object_path(&directory, &key, "corrupt"),
        b"verified object bytez",
    )
    .unwrap();
    fs::write(object_path(&directory, &key, "truncated"), &bytes[..5]).unwrap();

    assert!(matches!(
        backend.get_verified(&key, "corrupt"),
        Err(Error::Invalid(_))
    ));
    assert!(matches!(
        backend.inspect_verified(&key, "truncated"),
        Err(Error::Invalid(_))
    ));
}

#[test]
fn put_rejects_an_existing_collision() {
    let directory = TempDir::new().unwrap();
    let backend = LocalBackend::open(directory.path()).unwrap();
    let bytes = b"expected bytes";
    let key = key("team", "repository", bytes);
    let path = object_path(&directory, &key, "generation-1");
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(&path, b"different byte").unwrap();

    assert!(matches!(
        put(&backend, &key, "generation-1", bytes),
        Err(Error::Conflict(_))
    ));
    assert_eq!(fs::read(path).unwrap(), b"different byte");
}

#[test]
fn identifiers_reject_traversal_and_profiles_outside_the_exact_allowlist() {
    let id = digest(b"object");
    for trust_domain in ["", ".", "..", "team/name", "team\\name", "téam"] {
        assert!(ObjectKey::new(trust_domain, "repository", SOURCE_PROFILE_V1, &id).is_err());
    }
    for repository in ["", ".", "..", "../repository", "repository/name"] {
        assert!(ObjectKey::new("team", repository, SOURCE_PROFILE_V1, &id).is_err());
    }
    assert!(ObjectKey::new("team", "repository", SOURCE_PROFILE_V2, &id).is_ok());
    for profile in [
        "agentscm-source-v3",
        "agentscm-source-v1-extra",
        "AGENTSCM-SOURCE-V2",
    ] {
        assert!(ObjectKey::new("team", "repository", profile, &id).is_err());
    }
    assert!(ObjectKey::new("team", "repository", SOURCE_PROFILE_V1, "A".repeat(64)).is_err());

    let directory = TempDir::new().unwrap();
    let backend = LocalBackend::open(directory.path()).unwrap();
    let key = ObjectKey::new("team", "repository", SOURCE_PROFILE_V1, id).unwrap();
    assert!(backend.get_verified(&key, "../replacement").is_err());
    assert!(backend.get_verified(&key, &"x".repeat(129)).is_err());
}

#[test]
fn raw_stream_reads_exact_bytes_and_reports_typed_missing_objects() {
    let directory = TempDir::new().unwrap();
    let backend = LocalBackend::open(directory.path()).unwrap();
    let bytes = b"raw encoded object bytes";
    let object_key = key("team", "repository", bytes);
    put(&backend, &object_key, "generation-1", bytes).unwrap();

    let mut stream = backend.open_encoded(&object_key, "generation-1").unwrap();
    let mut actual = Vec::new();
    stream.read_to_end(&mut actual).unwrap();
    assert_eq!(actual, bytes);

    let missing = key("team", "repository", b"missing object");
    assert!(matches!(
        backend.open_encoded(&missing, "generation-1"),
        Err(Error::MissingObject(id)) if id == missing.id()
    ));
}

#[test]
fn raw_stream_default_is_explicitly_unsupported_without_verified_fallback() {
    let key = key("team", "repository", b"object");
    assert!(matches!(
        NoRawBackend.open_encoded(&key, "generation-1"),
        Err(Error::Invalid(message))
            if message == "backend does not support raw encoded-object streams"
    ));
}

#[test]
fn raw_stream_leaves_corruption_for_source_reader_validation() {
    let source_directory = TempDir::new().unwrap();
    let engine = Engine::open(source_directory.path()).unwrap();
    let revision = engine
        .checkpoint(
            None,
            &[FileChange {
                path: "file".into(),
                content: Some(b"content".to_vec()),
            }],
        )
        .unwrap();
    let encoded = engine.read_encoded(&revision).unwrap();

    let backend_directory = TempDir::new().unwrap();
    let backend = LocalBackend::open(backend_directory.path()).unwrap();
    let key = ObjectKey::new("team", "repository", SOURCE_PROFILE_V2, revision.clone()).unwrap();
    put(&backend, &key, "generation-2", &encoded).unwrap();
    let mut corrupted = encoded;
    corrupted[0] ^= 1;
    fs::write(
        object_path(&backend_directory, &key, "generation-2"),
        &corrupted,
    )
    .unwrap();

    let mut stream = backend.open_encoded(&key, "generation-2").unwrap();
    let mut raw = Vec::new();
    stream.read_to_end(&mut raw).unwrap();
    assert_eq!(raw, corrupted);

    let scoped = BackendReader {
        backend: &backend,
        trust_domain: "team",
        repository: "repository",
        profile: SOURCE_PROFILE_V2,
        incarnation: "generation-2",
    };
    let reader = SourceReader::new(&scoped);
    assert!(matches!(
        reader.read_encoded(&revision),
        Err(Error::Invalid(_))
    ));
}

#[test]
fn source_profile_namespaces_are_physically_isolated() {
    let directory = TempDir::new().unwrap();
    let backend = LocalBackend::open(directory.path()).unwrap();
    let bytes = b"same encoded bytes";
    let id = digest(bytes);
    let v1 = ObjectKey::new("team", "repository", SOURCE_PROFILE_V1, &id).unwrap();
    let v2 = ObjectKey::new("team", "repository", SOURCE_PROFILE_V2, &id).unwrap();

    assert_eq!(
        put(&backend, &v1, "generation", bytes).unwrap(),
        PutOutcome::Created
    );
    assert!(matches!(
        backend.open_encoded(&v2, "generation"),
        Err(Error::MissingObject(missing)) if missing == id
    ));
    assert_eq!(
        put(&backend, &v2, "generation", bytes).unwrap(),
        PutOutcome::Created
    );
    assert_ne!(
        object_path(&directory, &v1, "generation"),
        object_path(&directory, &v2, "generation")
    );
    let mut v1_bytes = Vec::new();
    backend
        .open_encoded(&v1, "generation")
        .unwrap()
        .read_to_end(&mut v1_bytes)
        .unwrap();
    let mut v2_bytes = Vec::new();
    backend
        .open_encoded(&v2, "generation")
        .unwrap()
        .read_to_end(&mut v2_bytes)
        .unwrap();
    assert_eq!(v1_bytes, bytes);
    assert_eq!(v2_bytes, bytes);
}

#[test]
fn verified_ranges_are_bounded_and_check_the_whole_object() {
    let directory = TempDir::new().unwrap();
    let backend = LocalBackend::open(directory.path()).unwrap();
    let bytes = b"0123456789";
    let key = key("team", "repository", bytes);
    put(&backend, &key, "generation-1", bytes).unwrap();

    assert_eq!(
        backend
            .get_range_verified(&key, "generation-1", 3, 4)
            .unwrap(),
        b"3456"
    );
    assert_eq!(
        backend
            .get_range_verified(&key, "generation-1", bytes.len() as u64, 0)
            .unwrap(),
        b""
    );
    assert!(matches!(
        backend.get_range_verified(&key, "generation-1", 9, 2),
        Err(Error::Invalid(_))
    ));
    assert!(matches!(
        backend.get_range_verified(&key, "generation-1", 0, MAX_RANGE_BYTES + 1),
        Err(Error::Limit(_))
    ));
    assert!(matches!(
        backend.get_range_verified(&key, "generation-1", u64::MAX, 1),
        Err(Error::Limit(_))
    ));

    let path = object_path(&directory, &key, "generation-1");
    let mut corrupted = bytes.to_vec();
    corrupted[0] = b'x';
    fs::write(path, corrupted).unwrap();
    assert!(matches!(
        backend.get_range_verified(&key, "generation-1", 3, 4),
        Err(Error::Invalid(_))
    ));
}

#[test]
fn metadata_and_put_validate_digest() {
    let directory = TempDir::new().unwrap();
    let backend = LocalBackend::open(directory.path()).unwrap();
    let bytes = b"metadata object";
    let key = key("team", "repository", bytes);
    put(&backend, &key, "generation-1", bytes).unwrap();

    let metadata = backend.inspect_verified(&key, "generation-1").unwrap();
    assert_eq!(metadata.size, bytes.len() as u64);
    assert_eq!(metadata.digest, key.id());

    assert!(matches!(
        put(&backend, &key, "different", b"wrong bytes"),
        Err(Error::Invalid(_))
    ));
}

#[test]
fn oversized_stored_objects_are_rejected_before_reading() {
    let directory = TempDir::new().unwrap();
    let backend = LocalBackend::open(directory.path()).unwrap();
    let key = key("team", "repository", b"");
    let path = object_path(&directory, &key, "generation-1");
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    let file = fs::File::create(path).unwrap();
    file.set_len(MAX_OBJECT_BYTES + 1).unwrap();

    assert!(matches!(
        backend.get_verified(&key, "generation-1"),
        Err(Error::Limit(_))
    ));
    assert!(matches!(
        backend.open_encoded(&key, "generation-1"),
        Err(Error::Limit(_))
    ));
}

#[test]
fn case_sensitive_identifiers_never_alias_on_local_filesystems() {
    let directory = TempDir::new().unwrap();
    let backend = LocalBackend::open(directory.path()).unwrap();
    let bytes = b"same object";
    let original = key("Team", "Repository", bytes);
    put(&backend, &original, "Publication", bytes).unwrap();
    for distinct in [
        key("team", "Repository", bytes),
        key("Team", "repository", bytes),
    ] {
        assert!(matches!(
            backend.get_verified(&distinct, "Publication"),
            Err(Error::NotFound(_))
        ));
        assert_eq!(
            put(&backend, &distinct, "Publication", bytes).unwrap(),
            PutOutcome::Created
        );
    }
    assert!(matches!(
        backend.get_verified(&original, "publication"),
        Err(Error::NotFound(_))
    ));
    assert_eq!(
        put(&backend, &original, "publication", bytes).unwrap(),
        PutOutcome::Created
    );
}

#[test]
fn maximum_identifiers_fit_bounded_physical_components() {
    let directory = TempDir::new().unwrap();
    let backend = LocalBackend::open(directory.path()).unwrap();
    let bytes = b"long identity";
    let key = key(&"T".repeat(128), &"R".repeat(128), bytes);
    let incarnation = "I".repeat(128);
    put(&backend, &key, &incarnation, bytes).unwrap();
    assert_eq!(backend.get_verified(&key, &incarnation).unwrap(), bytes);
}

#[test]
#[ignore = "explicit bounded local backend measurement; no service-capacity claim"]
fn local_10000_object_publication_measurement() {
    let directory = TempDir::new().unwrap();
    let backend = LocalBackend::open(directory.path()).unwrap();
    let key = key("team", "repository", b"prepare");
    let started = std::time::Instant::now();
    backend.prepare_publication(&key, "measurement").unwrap();
    let mut bytes_written = 0usize;
    for index in 0u64..10_000 {
        let mut bytes = vec![0u8; 1024];
        bytes[..8].copy_from_slice(&index.to_le_bytes());
        let key = ObjectKey::new("team", "repository", SOURCE_PROFILE_V1, digest(&bytes)).unwrap();
        backend.put_if_absent(&key, "measurement", &bytes).unwrap();
        assert_eq!(backend.get_verified(&key, "measurement").unwrap(), bytes);
        bytes_written += bytes.len();
    }
    println!(
        "local backend: objects=10000 bytes={} elapsed_ms={} prepare_calls=1; includes put+file/dir sync+verified read; excludes source qualification/PG/admission; no usable-capacity claim",
        bytes_written,
        started.elapsed().as_millis()
    );
}

#[test]
fn preparation_is_required_once_for_multiple_objects() {
    let directory = TempDir::new().unwrap();
    let backend = LocalBackend::open(directory.path()).unwrap();
    let first = key("team", "repository", b"first");
    let second = key("team", "repository", b"second");
    assert!(
        backend
            .put_if_absent(&first, "publication", b"first")
            .is_err()
    );
    assert!(matches!(
        backend.get_verified(&first, "publication"),
        Err(Error::NotFound(_))
    ));
    backend.prepare_publication(&first, "publication").unwrap();
    assert_eq!(
        backend
            .put_if_absent(&first, "publication", b"first")
            .unwrap(),
        PutOutcome::Created
    );
    assert_eq!(
        backend
            .put_if_absent(&second, "publication", b"second")
            .unwrap(),
        PutOutcome::Created
    );
    assert_eq!(
        backend.get_verified(&first, "publication").unwrap(),
        b"first"
    );
    assert_eq!(
        backend.get_verified(&second, "publication").unwrap(),
        b"second"
    );
}
