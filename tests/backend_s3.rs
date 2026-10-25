//! ImmutableBackend conformance for S3Backend, run against the S3-compatible
//! server named by AGENTSCM_TEST_S3_* (CI runs versitygw). Each test uses its
//! own random key prefix in a shared bucket.
use std::io::Read;
use std::sync::{Arc, Barrier};
use std::thread;
use std::time::{SystemTime, UNIX_EPOCH};

use agentscm::Error;
use agentscm::backend::{
    ImmutableBackend, MAX_RANGE_BYTES, ObjectKey, PutOutcome, SOURCE_PROFILE_V1, SOURCE_PROFILE_V2,
};
use agentscm::backend_s3::{S3Backend, S3Config, sign};
use agentscm::model::FileChange;
use agentscm::store::{EncodedObjectReader, Engine, MAX_OBJECT_BYTES, SourceReader, digest};
use tempfile::TempDir;

fn key(trust_domain: &str, repository: &str, bytes: &[u8]) -> ObjectKey {
    ObjectKey::new(trust_domain, repository, SOURCE_PROFILE_V1, digest(bytes)).unwrap()
}

struct Fixture {
    config: S3Config,
    backend: S3Backend,
}

/// None (after saying why) when no server is configured; panics under CI.
fn fixture() -> Option<Fixture> {
    let var = |name: &str| std::env::var(name).ok().filter(|value| !value.is_empty());
    let Some(endpoint) = var("AGENTSCM_TEST_S3_ENDPOINT") else {
        assert!(
            !matches!(std::env::var("CI"), Ok(v) if !v.is_empty() && v != "false"),
            "AGENTSCM_TEST_S3_ENDPOINT must be set in CI"
        );
        eprintln!("skipping S3 conformance: AGENTSCM_TEST_S3_ENDPOINT unset");
        return None;
    };
    let required = |name: &str| var(name).unwrap_or_else(|| panic!("{name} must be set"));
    let mut random = [0_u8; 8];
    getrandom::fill(&mut random).unwrap();
    let config = S3Config {
        endpoint,
        region: "us-east-1".into(),
        bucket: required("AGENTSCM_TEST_S3_BUCKET"),
        prefix: format!(
            "t-{}",
            random
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect::<String>()
        ),
        access_key_id: required("AGENTSCM_TEST_S3_ACCESS_KEY_ID"),
        secret_access_key: required("AGENTSCM_TEST_S3_SECRET_ACCESS_KEY"),
    };
    let backend = S3Backend::open(config.clone()).unwrap();
    // versitygw answers 500 to racing CreateBucket calls.
    static CREATE: std::sync::Mutex<()> = std::sync::Mutex::new(());
    let _guard = CREATE.lock().unwrap_or_else(|e| e.into_inner());
    backend.create_bucket().unwrap();
    Some(Fixture { config, backend })
}

impl Fixture {
    fn put(
        &self,
        key: &ObjectKey,
        incarnation: &str,
        bytes: &[u8],
    ) -> agentscm::Result<PutOutcome> {
        self.backend.prepare_publication(key, incarnation)?;
        self.backend.put_if_absent(key, incarnation, bytes)
    }

    /// Writes (or overwrites) arbitrary bytes at the key, bypassing the backend.
    fn tamper(&self, key: &ObjectKey, incarnation: &str, bytes: &[u8]) {
        let config = &self.config;
        let path = format!(
            "/{}/{}/{}/{}/{}/{incarnation}/{}",
            config.bucket,
            config.prefix,
            key.trust_domain(),
            key.repository(),
            key.profile(),
            key.id()
        );
        let endpoint = config.endpoint.trim_end_matches('/');
        let host = endpoint.split_once("://").unwrap().1;
        let payload = digest(bytes);
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let signed = sign(config, "PUT", host, &path, &[], &payload, now);
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let status = runtime
            .block_on(
                reqwest::Client::new()
                    .put(format!("{endpoint}{path}"))
                    .header("x-amz-date", signed.amz_date)
                    .header("x-amz-content-sha256", payload)
                    .header("authorization", signed.authorization)
                    .body(bytes.to_vec())
                    .send(),
            )
            .unwrap()
            .status();
        assert!(status.is_success(), "tamper PUT returned {status}");
    }
}

struct BackendReader<'a> {
    backend: &'a S3Backend,
    incarnation: &'a str,
}

impl EncodedObjectReader for BackendReader<'_> {
    fn open_encoded(&self, id: &str) -> agentscm::Result<Box<dyn Read + '_>> {
        let key = ObjectKey::new("team", "repository", SOURCE_PROFILE_V2, id)?;
        let stream: Box<dyn Read + '_> = self.backend.open_encoded(&key, self.incarnation)?;
        Ok(stream)
    }
}

#[test]
fn concurrent_puts_publish_one_exact_object() {
    let Some(fixture) = fixture() else { return };
    let fixture = Arc::new(fixture);
    let bytes = Arc::new(b"concurrent immutable object".to_vec());
    let key = Arc::new(key("team-a", "repository-a", &bytes));
    let barrier = Arc::new(Barrier::new(8));

    let handles: Vec<_> = (0..8)
        .map(|_| {
            let (fixture, bytes, key, barrier) = (
                Arc::clone(&fixture),
                Arc::clone(&bytes),
                Arc::clone(&key),
                Arc::clone(&barrier),
            );
            thread::spawn(move || {
                barrier.wait();
                fixture.put(&key, "generation-1", &bytes)
            })
        })
        .collect();
    let outcomes: Vec<_> = handles
        .into_iter()
        .map(|handle| handle.join().unwrap().unwrap())
        .collect();
    let created = |wanted| outcomes.iter().filter(|o| **o == wanted).count();
    assert_eq!(created(PutOutcome::Created), 1);
    assert_eq!(created(PutOutcome::AlreadyPresent), 7);
    assert_eq!(
        fixture.backend.get_verified(&key, "generation-1").unwrap(),
        *bytes
    );
}

#[test]
fn namespaces_and_physical_incarnations_are_isolated() {
    let Some(fixture) = fixture() else { return };
    let bytes = b"same canonical bytes";
    let key_a = key("team-a", "repository", bytes);
    let key_b = key("team-b", "repository", bytes);

    for (key, incarnation) in [
        (&key_a, "generation-1"),
        (&key_b, "generation-1"),
        (&key_a, "generation-2"),
    ] {
        assert_eq!(
            fixture.put(key, incarnation, bytes).unwrap(),
            PutOutcome::Created
        );
    }
    fixture.tamper(&key_a, "generation-1", b"corrupt");
    assert!(matches!(
        fixture.backend.get_verified(&key_a, "generation-1"),
        Err(Error::Invalid(_))
    ));
    assert_eq!(
        fixture
            .backend
            .get_verified(&key_b, "generation-1")
            .unwrap(),
        bytes
    );
    assert_eq!(
        fixture
            .backend
            .get_verified(&key_a, "generation-2")
            .unwrap(),
        bytes
    );
}

#[test]
fn verified_reads_reject_corruption_and_truncation() {
    let Some(fixture) = fixture() else { return };
    let bytes = b"verified object bytes";
    let key = key("team", "repository", bytes);
    fixture.put(&key, "corrupt", bytes).unwrap();
    fixture.put(&key, "truncated", bytes).unwrap();
    fixture.tamper(&key, "corrupt", b"verified object bytez");
    fixture.tamper(&key, "truncated", &bytes[..5]);

    assert!(matches!(
        fixture.backend.get_verified(&key, "corrupt"),
        Err(Error::Invalid(_))
    ));
    assert!(matches!(
        fixture.backend.inspect_verified(&key, "truncated"),
        Err(Error::Invalid(_))
    ));
}

#[test]
fn put_rejects_an_existing_collision() {
    let Some(fixture) = fixture() else { return };
    let bytes = b"expected bytes";
    let key = key("team", "repository", bytes);
    fixture.tamper(&key, "generation-1", b"different byte");

    assert!(matches!(
        fixture.put(&key, "generation-1", bytes),
        Err(Error::Conflict(_))
    ));
    let mut stored = Vec::new();
    fixture
        .backend
        .open_encoded(&key, "generation-1")
        .unwrap()
        .read_to_end(&mut stored)
        .unwrap();
    assert_eq!(stored, b"different byte");
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
    for profile in [
        "agentscm-source-v3",
        "agentscm-source-v1-extra",
        "AGENTSCM-SOURCE-V2",
    ] {
        assert!(ObjectKey::new("team", "repository", profile, &id).is_err());
    }
    assert!(ObjectKey::new("team", "repository", SOURCE_PROFILE_V1, "A".repeat(64)).is_err());

    let Some(fixture) = fixture() else { return };
    let key = ObjectKey::new("team", "repository", SOURCE_PROFILE_V1, id).unwrap();
    assert!(
        fixture
            .backend
            .get_verified(&key, "../replacement")
            .is_err()
    );
    assert!(
        fixture
            .backend
            .get_verified(&key, &"x".repeat(129))
            .is_err()
    );
    assert!(fixture.backend.prepare_publication(&key, "a/b").is_err());
}

#[test]
fn raw_stream_reads_exact_bytes_and_reports_typed_missing_objects() {
    let Some(fixture) = fixture() else { return };
    let bytes = b"raw encoded object bytes";
    let object_key = key("team", "repository", bytes);
    fixture.put(&object_key, "generation-1", bytes).unwrap();

    let mut actual = Vec::new();
    fixture
        .backend
        .open_encoded(&object_key, "generation-1")
        .unwrap()
        .read_to_end(&mut actual)
        .unwrap();
    assert_eq!(actual, bytes);

    let missing = key("team", "repository", b"missing object");
    assert!(matches!(
        fixture.backend.open_encoded(&missing, "generation-1"),
        Err(Error::MissingObject(id)) if id == missing.id()
    ));
    assert!(matches!(
        fixture.backend.get_verified(&missing, "generation-1"),
        Err(Error::NotFound(_))
    ));
}

#[test]
fn raw_stream_leaves_corruption_for_source_reader_validation() {
    let Some(fixture) = fixture() else { return };
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

    let key = ObjectKey::new("team", "repository", SOURCE_PROFILE_V2, revision.clone()).unwrap();
    fixture.put(&key, "generation-2", &encoded).unwrap();
    let mut corrupted = encoded;
    corrupted[0] ^= 1;
    fixture.tamper(&key, "generation-2", &corrupted);

    let mut raw = Vec::new();
    fixture
        .backend
        .open_encoded(&key, "generation-2")
        .unwrap()
        .read_to_end(&mut raw)
        .unwrap();
    assert_eq!(raw, corrupted);

    let scoped = BackendReader {
        backend: &fixture.backend,
        incarnation: "generation-2",
    };
    assert!(matches!(
        SourceReader::new(&scoped).read_encoded(&revision),
        Err(Error::Invalid(_))
    ));
}

#[test]
fn source_profile_namespaces_are_physically_isolated() {
    let Some(fixture) = fixture() else { return };
    let bytes = b"same encoded bytes";
    let id = digest(bytes);
    let v1 = ObjectKey::new("team", "repository", SOURCE_PROFILE_V1, &id).unwrap();
    let v2 = ObjectKey::new("team", "repository", SOURCE_PROFILE_V2, &id).unwrap();

    assert_eq!(
        fixture.put(&v1, "generation", bytes).unwrap(),
        PutOutcome::Created
    );
    assert!(matches!(
        fixture.backend.open_encoded(&v2, "generation"),
        Err(Error::MissingObject(missing)) if missing == id
    ));
    assert_eq!(
        fixture.put(&v2, "generation", bytes).unwrap(),
        PutOutcome::Created
    );
}

#[test]
fn verified_ranges_are_bounded_and_check_the_whole_object() {
    let Some(fixture) = fixture() else { return };
    let backend = &fixture.backend;
    let bytes = b"0123456789";
    let key = key("team", "repository", bytes);
    fixture.put(&key, "generation-1", bytes).unwrap();

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

    fixture.tamper(&key, "generation-1", b"x123456789");
    assert!(matches!(
        backend.get_range_verified(&key, "generation-1", 3, 4),
        Err(Error::Invalid(_))
    ));
}

#[test]
fn metadata_and_put_validate_digest() {
    let Some(fixture) = fixture() else { return };
    let bytes = b"metadata object";
    let key = key("team", "repository", bytes);
    fixture.put(&key, "generation-1", bytes).unwrap();

    let metadata = fixture
        .backend
        .inspect_verified(&key, "generation-1")
        .unwrap();
    assert_eq!(metadata.size, bytes.len() as u64);
    assert_eq!(metadata.digest, key.id());
    assert!(matches!(
        fixture.put(&key, "different", b"wrong bytes"),
        Err(Error::Invalid(_))
    ));
}

#[test]
fn oversized_stored_objects_are_rejected() {
    let Some(fixture) = fixture() else { return };
    let key = key("team", "repository", b"");
    fixture.tamper(
        &key,
        "generation-1",
        &vec![0; MAX_OBJECT_BYTES as usize + 1],
    );

    assert!(matches!(
        fixture.backend.get_verified(&key, "generation-1"),
        Err(Error::Limit(_))
    ));
    assert!(matches!(
        fixture.backend.open_encoded(&key, "generation-1"),
        Err(Error::Limit(_))
    ));
    assert!(matches!(
        fixture.put(&key, "other", &vec![0; MAX_OBJECT_BYTES as usize + 1]),
        Err(Error::Limit(_))
    ));
}

#[test]
fn case_sensitive_and_maximum_identifiers_never_alias() {
    let Some(fixture) = fixture() else { return };
    let bytes = b"same object";
    let original = key("Team", "Repository", bytes);
    fixture.put(&original, "Publication", bytes).unwrap();
    for distinct in [
        key("team", "Repository", bytes),
        key("Team", "repository", bytes),
    ] {
        assert!(matches!(
            fixture.backend.get_verified(&distinct, "Publication"),
            Err(Error::NotFound(_))
        ));
    }
    assert!(matches!(
        fixture.backend.get_verified(&original, "publication"),
        Err(Error::NotFound(_))
    ));

    let long = key(&"T".repeat(128), &"R".repeat(128), bytes);
    let incarnation = "I".repeat(128);
    fixture.put(&long, &incarnation, bytes).unwrap();
    assert_eq!(
        fixture.backend.get_verified(&long, &incarnation).unwrap(),
        bytes
    );
}

#[test]
fn a_fresh_backend_sees_previously_written_objects() {
    let Some(fixture) = fixture() else { return };
    let bytes = b"persisted object";
    let key = key("team", "repository", bytes);
    fixture.put(&key, "generation-1", bytes).unwrap();

    let reopened = S3Backend::open(fixture.config.clone()).unwrap();
    drop(fixture.backend);
    assert_eq!(reopened.get_verified(&key, "generation-1").unwrap(), bytes);
    assert_eq!(
        reopened.put_if_absent(&key, "generation-1", bytes).unwrap(),
        PutOutcome::AlreadyPresent
    );
}

#[tokio::test]
async fn dropping_inside_an_async_context_does_not_panic() {
    let Some(fixture) = tokio::task::spawn_blocking(fixture).await.unwrap() else {
        return;
    };
    drop(fixture.backend);
}

#[test]
fn delete_removes_the_object_and_is_idempotent() {
    let Some(fixture) = fixture() else { return };
    let key = key("hub-conversation", "work-1", b"shared body");
    fixture.put(&key, "v1", b"shared body").unwrap();

    fixture.backend.delete_incarnation(&key, "v1").unwrap();
    assert!(matches!(
        fixture.backend.get_verified(&key, "v1"),
        Err(Error::NotFound(_))
    ));
    fixture.backend.delete_incarnation(&key, "v1").unwrap();
}

/// Serves one scripted reply per connection and returns the request lines it saw.
fn scripted_s3(
    replies: &'static [(u16, &'static str)],
) -> (String, thread::JoinHandle<Vec<String>>) {
    use std::io::Write;
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let server = thread::spawn(move || {
        let mut seen = Vec::new();
        for (status, body) in replies {
            let (mut stream, _) = listener.accept().unwrap();
            let mut head = Vec::new();
            let mut byte = [0_u8; 1];
            while !head.ends_with(b"\r\n\r\n") {
                stream.read_exact(&mut byte).unwrap();
                head.push(byte[0]);
            }
            let head = String::from_utf8(head).unwrap();
            seen.push(head.lines().next().unwrap().to_owned());
            let reply = format!(
                "HTTP/1.1 {status} X\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            );
            stream.write_all(reply.as_bytes()).unwrap();
        }
        seen
    });
    (endpoint, server)
}

#[test]
fn delete_treats_204_and_no_such_key_as_removed_and_reports_other_failures() {
    let (endpoint, server) = scripted_s3(&[
        (204, ""),
        (404, "<Error><Code>NoSuchKey</Code></Error>"),
        (404, "<Error><Code>NoSuchBucket</Code></Error>"),
        (403, "<Error><Code>AccessDenied</Code></Error>"),
    ]);
    let backend = S3Backend::open(S3Config {
        endpoint,
        region: "us-east-1".into(),
        bucket: "bucket".into(),
        prefix: "pfx".into(),
        access_key_id: ["synthetic", "access"].join("-"),
        secret_access_key: ["synthetic", "secret"].join("-"),
    })
    .unwrap();
    let key = key("hub-conversation", "work-1", b"shared body");
    backend.delete_incarnation(&key, "v1").unwrap();
    backend.delete_incarnation(&key, "v1").unwrap();
    assert!(backend.delete_incarnation(&key, "v1").is_err());
    assert!(backend.delete_incarnation(&key, "v1").is_err());
    let seen = server.join().unwrap();
    let path = format!(
        "DELETE /bucket/pfx/hub-conversation/work-1/{}/v1/{} HTTP/1.1",
        SOURCE_PROFILE_V1,
        key.id()
    );
    assert!(seen.iter().all(|line| *line == path), "{seen:?}");
}
