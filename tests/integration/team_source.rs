use crate::team_pg;

use std::io::{Cursor, Read};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Barrier, Mutex};

use agentscm::backend::{ImmutableBackend, LocalBackend, ObjectKey, ObjectMetadata, PutOutcome};
use agentscm::history::{HeadRelation, history_page};
use agentscm::model::FileChange;
use agentscm::store::Engine;
use agentscm::sync::qualify_source;
use agentscm::team::TeamError;
use agentscm::team::auth::{Credentials, Role, generate_secret};
use agentscm::team::publication::{
    CompleteRequest, RepositoryScope, ReserveRequest, SourceNamespace,
};
use agentscm::team::service::{
    BootstrapRequest, DatabaseConfig, ProvisionRequest, RetireAccessRequest, RolesRequest,
    TeamService, operator_bootstrap, operator_retire_access,
};
use agentscm::team::source::{SourceCancelRequest, SourcePageRequest};
use agentscm::{Error, Result};
use team_pg::Cluster;

fn scope(repository: &str) -> RepositoryScope {
    RepositoryScope {
        trust_domain: "team".into(),
        repository: repository.into(),
    }
}

fn change(path: &str, bytes: &[u8]) -> FileChange {
    FileChange {
        path: path.into(),
        content: Some(bytes.to_vec()),
    }
}

fn page(operation: &str, cursor: Option<String>, limit: usize) -> SourcePageRequest {
    SourcePageRequest {
        operation: operation.into(),
        cursor,
        limit,
    }
}

fn bootstrap(config: &DatabaseConfig, repository: &str, credential: &str) {
    operator_bootstrap(
        config,
        &BootstrapRequest {
            scope: scope(repository),
            authority_generation: 1,
            operation: "bootstrap".into(),
            max_bytes: 64 * 1024 * 1024,
            max_objects: 100_000,
            principal: "admin".into(),
            roles: vec![Role::Reader, Role::Contributor, Role::Administrator],
        },
        credential,
    )
    .unwrap();
}

fn service(config: &DatabaseConfig, repository: &str, credential: &str) -> TeamService {
    TeamService::connect(
        config,
        scope(repository),
        Credentials::bearer(credential).unwrap(),
    )
    .unwrap()
}

#[allow(clippy::too_many_arguments)]
fn publish(
    config: &DatabaseConfig,
    repository: &str,
    publication: &str,
    namespace: SourceNamespace,
    source: &Engine,
    root: &str,
    backend: &LocalBackend,
    offer: &std::path::Path,
) {
    let mut controller = config.open(scope(repository)).unwrap();
    controller
        .reserve_profile(
            &ReserveRequest {
                generation: 1,
                publication: publication.into(),
                root: root.into(),
                byte_limit: 32 * 1024 * 1024,
                object_limit: 100_000,
                lease_seconds: 300,
            },
            namespace,
        )
        .unwrap();
    let manifest = controller
        .upload_and_qualify(1, publication, source, backend, offer)
        .unwrap();
    controller
        .complete(&CompleteRequest {
            generation: 1,
            publication: publication.into(),
            operation: format!("complete-{publication}"),
            manifest_digest: manifest,
        })
        .unwrap();
}

type ReadHook = Arc<Mutex<Option<Box<dyn FnOnce() + Send>>>>;

#[derive(Clone, Copy)]
enum ReadMode {
    Normal,
    Missing,
    WrongMissing,
    Short,
    Extra,
    Corrupt,
    Endless,
}

struct CountingBackend {
    inner: LocalBackend,
    opens: AtomicUsize,
    ranges: AtomicUsize,
    mode: Mutex<ReadMode>,
    hook: ReadHook,
}

struct RaceBackend {
    inner: LocalBackend,
    barrier: Barrier,
    target: Mutex<Option<String>>,
    target_opens: AtomicUsize,
}

impl RaceBackend {
    fn new(inner: LocalBackend) -> Self {
        Self {
            inner,
            barrier: Barrier::new(2),
            target: Mutex::new(None),
            target_opens: AtomicUsize::new(0),
        }
    }

    fn arm(&self, target: &str) {
        self.target_opens.store(0, Ordering::SeqCst);
        *self.target.lock().unwrap() = Some(target.into());
    }
}

impl CountingBackend {
    fn new(inner: LocalBackend) -> Self {
        Self {
            inner,
            opens: AtomicUsize::new(0),
            ranges: AtomicUsize::new(0),
            mode: Mutex::new(ReadMode::Normal),
            hook: Arc::new(Mutex::new(None)),
        }
    }

    fn mode(&self, mode: ReadMode) {
        *self.mode.lock().unwrap() = mode;
    }

    fn hook(&self, hook: impl FnOnce() + Send + 'static) {
        *self.hook.lock().unwrap() = Some(Box::new(hook));
    }
}

struct HookedRead<'a> {
    inner: Box<dyn Read + Send + 'a>,
    hook: ReadHook,
}

impl Read for HookedRead<'_> {
    fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
        let read = self.inner.read(buffer)?;
        if read > 0
            && let Some(hook) = self.hook.lock().unwrap().take()
        {
            hook();
        }
        Ok(read)
    }
}

struct FlipFirst<'a> {
    inner: Box<dyn Read + Send + 'a>,
    first: bool,
}

impl Read for FlipFirst<'_> {
    fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
        let read = self.inner.read(buffer)?;
        if self.first && read > 0 {
            buffer[0] ^= 1;
            self.first = false;
        }
        Ok(read)
    }
}

impl ImmutableBackend for CountingBackend {
    fn prepare_publication(&self, key: &ObjectKey, incarnation: &str) -> Result<()> {
        self.inner.prepare_publication(key, incarnation)
    }

    fn put_if_absent(
        &self,
        key: &ObjectKey,
        incarnation: &str,
        bytes: &[u8],
    ) -> Result<PutOutcome> {
        self.inner.put_if_absent(key, incarnation, bytes)
    }

    fn get_verified(&self, key: &ObjectKey, incarnation: &str) -> Result<Vec<u8>> {
        self.inner.get_verified(key, incarnation)
    }

    fn inspect_verified(&self, key: &ObjectKey, incarnation: &str) -> Result<ObjectMetadata> {
        self.inner.inspect_verified(key, incarnation)
    }

    fn open_encoded(
        &self,
        key: &ObjectKey,
        incarnation: &str,
    ) -> Result<Box<dyn Read + Send + '_>> {
        self.opens.fetch_add(1, Ordering::SeqCst);
        let mode = *self.mode.lock().unwrap();
        match mode {
            ReadMode::Missing => return Err(Error::MissingObject(key.id().to_owned())),
            ReadMode::WrongMissing => return Err(Error::MissingObject("0".repeat(64))),
            ReadMode::Endless => return Ok(Box::new(std::io::repeat(0))),
            _ => {}
        }
        let raw = self.inner.open_encoded(key, incarnation)?;
        let raw: Box<dyn Read + Send + '_> = match mode {
            ReadMode::Normal => raw,
            ReadMode::Short => Box::new(raw.take(1)),
            ReadMode::Extra => Box::new(raw.chain(Cursor::new(vec![0]))),
            ReadMode::Corrupt => Box::new(FlipFirst {
                inner: raw,
                first: true,
            }),
            ReadMode::Missing | ReadMode::WrongMissing | ReadMode::Endless => unreachable!(),
        };
        Ok(Box::new(HookedRead {
            inner: raw,
            hook: self.hook.clone(),
        }))
    }

    fn get_range_verified(
        &self,
        key: &ObjectKey,
        incarnation: &str,
        offset: u64,
        length: u64,
    ) -> Result<Vec<u8>> {
        self.ranges.fetch_add(1, Ordering::SeqCst);
        self.inner
            .get_range_verified(key, incarnation, offset, length)
    }
}

impl ImmutableBackend for RaceBackend {
    fn prepare_publication(&self, key: &ObjectKey, incarnation: &str) -> Result<()> {
        self.inner.prepare_publication(key, incarnation)
    }

    fn put_if_absent(
        &self,
        key: &ObjectKey,
        incarnation: &str,
        bytes: &[u8],
    ) -> Result<PutOutcome> {
        self.inner.put_if_absent(key, incarnation, bytes)
    }

    fn get_verified(&self, key: &ObjectKey, incarnation: &str) -> Result<Vec<u8>> {
        self.inner.get_verified(key, incarnation)
    }

    fn inspect_verified(&self, key: &ObjectKey, incarnation: &str) -> Result<ObjectMetadata> {
        self.inner.inspect_verified(key, incarnation)
    }

    fn open_encoded(
        &self,
        key: &ObjectKey,
        incarnation: &str,
    ) -> Result<Box<dyn Read + Send + '_>> {
        let wait = self.target.lock().unwrap().as_deref() == Some(key.id())
            && self.target_opens.fetch_add(1, Ordering::SeqCst) < 2;
        if wait {
            self.barrier.wait();
        }
        self.inner.open_encoded(key, incarnation)
    }

    fn get_range_verified(
        &self,
        key: &ObjectKey,
        incarnation: &str,
        offset: u64,
        length: u64,
    ) -> Result<Vec<u8>> {
        self.inner
            .get_range_verified(key, incarnation, offset, length)
    }
}

#[test]
#[ignore = "requires installed PG18; creates only an isolated local cluster"]
fn pg18_native_r1_reads_match_engine_and_use_opaque_bound_cursors() {
    let cluster = Cluster::new();
    let directory = tempfile::tempdir().unwrap();
    let source = Engine::open(directory.path().join("source")).unwrap();
    let base = source
        .checkpoint(None, &[change("a", b"base"), change("dir/b", b"b")])
        .unwrap();
    let root = source
        .checkpoint(Some(&base), &[change("a", b"tip"), change("c", b"c")])
        .unwrap();
    let local_backend = LocalBackend::open(directory.path().join("backend")).unwrap();
    let credential = "1".repeat(64);
    bootstrap(&cluster.config, "native-r1", &credential);
    publish(
        &cluster.config,
        "native-r1",
        "publication",
        SourceNamespace::V1,
        &source,
        &root,
        &local_backend,
        &directory.path().join("offer"),
    );
    let backend = CountingBackend::new(local_backend);
    let mut reader = service(&cluster.config, "native-r1", &credential);

    let revision = reader.source_revision(&backend, 1, "publication").unwrap();
    assert_eq!(revision.id, root);
    assert_eq!(revision.revision, source.load_revision(&root).unwrap());
    assert_eq!(revision.source.profile, "agentscm-source-v1");
    assert_eq!(revision.source.authority_generation, 1);
    assert_eq!(backend.ranges.load(Ordering::SeqCst), 0);
    assert!(
        backend.opens.load(Ordering::SeqCst)
            < qualify_source(&source, std::slice::from_ref(&root))
                .unwrap()
                .objects()
                .len()
    );

    let file = reader.source_file(&backend, 1, "publication", "a").unwrap();
    assert_eq!(file.path, "a");
    assert_eq!(file.bytes, source.read_file(&root, "a").unwrap());

    let mut files = Vec::new();
    let mut cursor = None;
    loop {
        let page = reader
            .source_files(
                &backend,
                1,
                "publication",
                &page("files-walk", cursor.clone(), 1),
            )
            .unwrap();
        files.extend(
            page.entries
                .into_iter()
                .map(|entry| (entry.path, entry.blob)),
        );
        cursor = page.next;
        if let Some(token) = &cursor {
            assert_eq!(token.len(), 64);
            assert!(token.bytes().all(|byte| byte.is_ascii_hexdigit()));
            assert!(!token.contains(':'));
        } else {
            break;
        }
    }
    let expected = source
        .list_files(&root, None, 256)
        .unwrap()
        .entries
        .into_iter()
        .map(|entry| (entry.path, entry.blob))
        .collect::<Vec<_>>();
    assert_eq!(files, expected);

    let first = reader
        .source_history(&backend, 1, "publication", &page("history-walk", None, 1))
        .unwrap();
    assert_eq!(first.entries[0].id, root);
    let opaque = first.next.unwrap();
    assert!(matches!(
        reader.source_history(
            &backend,
            1,
            "publication",
            &page("history-walk", Some(opaque.clone()), 2),
        ),
        Err(TeamError::Unauthorized)
    ));
    assert!(matches!(
        reader.source_files(
            &backend,
            1,
            "publication",
            &page("history-walk", Some(opaque.clone()), 1),
        ),
        Err(TeamError::Unauthorized)
    ));
    let second = reader
        .source_history(
            &backend,
            1,
            "publication",
            &page("history-walk", Some(opaque.clone()), 1),
        )
        .unwrap();
    assert_eq!(second.entries[0].id, base);
    assert!(second.next.is_none());

    let diff = reader
        .source_diff(
            &backend,
            1,
            "publication",
            &base,
            &root,
            &page("diff-main", None, 32),
        )
        .unwrap();
    assert_eq!(
        diff.entries,
        source.diff_page(&base, &root, None, 32).unwrap().entries
    );
    let relation = reader
        .source_relation(
            &backend,
            1,
            "publication",
            &base,
            &root,
            &page("relation-main", None, 32),
        )
        .unwrap();
    assert_eq!(relation.relation, HeadRelation::LeftAncestor);
    assert!(matches!(
        reader.source_history(
            &backend,
            1,
            "publication",
            &page("history-walk", Some("f".repeat(64)), 1),
        ),
        Err(TeamError::Indeterminate)
    ));
    // A valid reply digest does not make malformed cached DTO data trusted.
    let mut database = cluster.admin();
    let original: String = database.query_one(
        "SELECT initial_reply FROM team_source_cursors WHERE repository='native-r1' AND operation='history-walk'", &[],
    ).unwrap().get(0);
    let mut malformed: serde_json::Value = serde_json::from_str(&original).unwrap();
    malformed["unexpected"] = serde_json::json!(true);
    let malformed = serde_json::to_string(&malformed).unwrap();
    database.execute(
        "UPDATE team_source_cursors SET initial_reply=$1,initial_reply_digest=$2 WHERE repository='native-r1' AND operation='history-walk'",
        &[&malformed, &agentscm::store::digest(malformed.as_bytes())],
    ).unwrap();
    let opens_before_corrupt_cache = backend.opens.load(Ordering::SeqCst);
    assert!(matches!(
        reader.source_history(&backend, 1, "publication", &page("history-walk", None, 1)),
        Err(TeamError::Indeterminate)
    ));
    assert_eq!(
        backend.opens.load(Ordering::SeqCst),
        opens_before_corrupt_cache
    );
    database.execute(
        "UPDATE team_source_cursors SET initial_reply=$1,initial_reply_digest=$2 WHERE repository='native-r1' AND operation='history-walk'",
        &[&original, &agentscm::store::digest(original.as_bytes())],
    ).unwrap();
    let count: i64 = cluster
        .admin()
        .query_one(
            "SELECT count(*) FROM team_source_cursors WHERE repository='native-r1' AND position($1 in binding)=0 AND position($1 in COALESCE(current_continuation,''))=0",
            &[&opaque],
        )
        .unwrap()
        .get(0);
    assert!(
        count >= 1,
        "opaque token must not be retained in bindings or native continuations"
    );
}

#[test]
#[ignore = "requires installed PG18; creates only an isolated local cluster"]
fn pg18_native_r2_membership_and_midstream_fences_prevent_disclosure() {
    let cluster = Cluster::new();
    let directory = tempfile::tempdir().unwrap();
    let source = Engine::open(directory.path().join("source")).unwrap();
    let base = source.checkpoint(None, &[change("a", b"base")]).unwrap();
    let left = source
        .checkpoint(Some(&base), &[change("a", b"left")])
        .unwrap();
    let right = source
        .checkpoint(Some(&base), &[change("right", b"right")])
        .unwrap();
    let root = source
        .checkpoint_with_parents(&[left.clone(), right.clone()], &[change("merge", b"merge")])
        .unwrap();
    let outsider = source.checkpoint(None, &[change("outside", b"x")]).unwrap();
    let local_backend = LocalBackend::open(directory.path().join("backend")).unwrap();
    let admin_credential = "2".repeat(64);
    bootstrap(&cluster.config, "native-r2", &admin_credential);
    publish(
        &cluster.config,
        "native-r2",
        "publication",
        SourceNamespace::V2,
        &source,
        &root,
        &local_backend,
        &directory.path().join("offer"),
    );
    let backend = CountingBackend::new(local_backend);
    let mut admin = service(&cluster.config, "native-r2", &admin_credential);
    let reader_credential = generate_secret().unwrap();
    admin
        .provision(&ProvisionRequest {
            authority_generation: 1,
            operation: "provision-reader".into(),
            principal: "reader".into(),
            credential: reader_credential.expose().into(),
            roles: vec![Role::Reader],
        })
        .unwrap();
    let mut reader = service(&cluster.config, "native-r2", reader_credential.expose());
    assert_eq!(
        reader
            .source_revision(&backend, 1, "publication")
            .unwrap()
            .revision
            .version,
        2
    );

    let mut native_history = Vec::new();
    let mut history_cursor = None;
    loop {
        let page_result = reader
            .source_history(
                &backend,
                1,
                "publication",
                &page("r2-history", history_cursor.clone(), 1),
            )
            .unwrap();
        native_history.extend(
            page_result
                .entries
                .into_iter()
                .map(|entry| (entry.id, entry.revision)),
        );
        history_cursor = page_result.next;
        if history_cursor.is_none() {
            break;
        }
    }
    let mut local_history = Vec::new();
    let mut local_cursor = None;
    loop {
        let page_result = history_page(&source, &root, local_cursor.as_deref(), 1).unwrap();
        local_history.extend(
            page_result
                .entries
                .into_iter()
                .map(|entry| (entry.id, entry.revision)),
        );
        local_cursor = page_result.next;
        if local_cursor.is_none() {
            break;
        }
    }
    assert_eq!(native_history, local_history);

    let mut native_diff = Vec::new();
    let mut diff_cursor = None;
    loop {
        let page_result = reader
            .source_diff(
                &backend,
                1,
                "publication",
                &base,
                &root,
                &page("r2-diff", diff_cursor.clone(), 1),
            )
            .unwrap();
        native_diff.extend(page_result.entries);
        diff_cursor = page_result.next;
        if diff_cursor.is_none() {
            break;
        }
    }
    let mut local_diff = Vec::new();
    let mut local_cursor = None;
    loop {
        let page_result = source
            .diff_page(&base, &root, local_cursor.as_deref(), 1)
            .unwrap();
        local_diff.extend(page_result.entries);
        local_cursor = page_result.next;
        if local_cursor.is_none() {
            break;
        }
    }
    assert_eq!(native_diff, local_diff);

    let selection_page = reader
        .source_diff(
            &backend,
            1,
            "publication",
            &right,
            &root,
            &page("selection-bound", None, 1),
        )
        .unwrap();
    let selection_cursor = selection_page.next.unwrap();
    assert!(matches!(
        reader.source_diff(
            &backend,
            1,
            "publication",
            &base,
            &root,
            &page("selection-bound", Some(selection_cursor), 1),
        ),
        Err(TeamError::Unauthorized)
    ));

    let mut relation_cursor = None;
    let native_relation = loop {
        let page_result = reader
            .source_relation(
                &backend,
                1,
                "publication",
                &base,
                &root,
                &page("r2-relation", relation_cursor.clone(), 1),
            )
            .unwrap();
        relation_cursor = page_result.next.clone();
        if relation_cursor.is_none() {
            break page_result.relation;
        }
    };
    assert_eq!(native_relation, HeadRelation::LeftAncestor);

    let before = backend.opens.load(Ordering::SeqCst);
    assert!(matches!(
        reader.source_diff(
            &backend,
            1,
            "publication",
            &outsider,
            &root,
            &page("outsider-diff", None, 1),
        ),
        Err(TeamError::Storage(Error::SourceDenied))
    ));
    assert_eq!(backend.opens.load(Ordering::SeqCst), before);

    let config = cluster.config.clone();
    let hook_admin_credential = admin_credential.clone();
    backend.hook(move || {
        let mut hook_admin = service(&config, "native-r2", &hook_admin_credential);
        hook_admin
            .roles(
                "reader",
                &RolesRequest {
                    authority_generation: 1,
                    operation: "strip-reader-during-native-read".into(),
                    expected_version: 1,
                    roles: vec![Role::Contributor],
                },
            )
            .unwrap();
    });
    assert!(matches!(
        reader.source_revision(&backend, 1, "publication"),
        Err(TeamError::Storage(Error::SourceDenied))
    ));

    let replacement = generate_secret().unwrap();
    admin
        .provision(&ProvisionRequest {
            authority_generation: 1,
            operation: "provision-reader-two".into(),
            principal: "reader-two".into(),
            credential: replacement.expose().into(),
            roles: vec![Role::Reader],
        })
        .unwrap();
    let mut replacement_reader = service(&cluster.config, "native-r2", replacement.expose());
    let config = cluster.config.clone();
    backend.hook(move || {
        let mut database = postgres::Config::new()
            .host_path(&config.socket)
            .port(config.port)
            .dbname(&config.database)
            .connect(postgres::NoTls)
            .unwrap();
        database
            .execute(
                "UPDATE team_publications SET manifest_digest=$1 WHERE repository='native-r2' AND publication='publication'",
                &[&"0".repeat(64)],
            )
            .unwrap();
    });
    assert!(matches!(
        replacement_reader.source_revision(&backend, 1, "publication"),
        Err(TeamError::Storage(Error::SourceIndeterminate))
    ));
}

#[test]
#[ignore = "requires installed PG18; creates only an isolated local cluster"]
fn pg18_native_stream_failures_are_typed_and_never_partial() {
    let cluster = Cluster::new();
    let directory = tempfile::tempdir().unwrap();
    let source = Engine::open(directory.path().join("source")).unwrap();
    let root = source.checkpoint(None, &[change("a", b"bytes")]).unwrap();
    let local_backend = LocalBackend::open(directory.path().join("backend")).unwrap();
    let credential = "3".repeat(64);
    bootstrap(&cluster.config, "native-errors", &credential);
    publish(
        &cluster.config,
        "native-errors",
        "publication",
        SourceNamespace::V1,
        &source,
        &root,
        &local_backend,
        &directory.path().join("offer"),
    );
    let backend = CountingBackend::new(local_backend);
    let mut reader = service(&cluster.config, "native-errors", &credential);

    backend.mode(ReadMode::Missing);
    assert!(matches!(
        reader.source_history(
            &backend,
            1,
            "publication",
            &page("stream-failure", None, 1),
        ),
        Err(TeamError::Storage(Error::MissingObject(id))) if id == root
    ));
    backend.mode(ReadMode::Normal);
    assert!(
        reader
            .source_history(&backend, 1, "publication", &page("stream-failure", None, 1),)
            .is_ok(),
        "a failed pending first computation must be retryable"
    );
    for mode in [
        ReadMode::WrongMissing,
        ReadMode::Short,
        ReadMode::Extra,
        ReadMode::Endless,
    ] {
        backend.mode(mode);
        assert!(matches!(
            reader.source_revision(&backend, 1, "publication"),
            Err(TeamError::Storage(Error::SourceIndeterminate))
        ));
    }
    backend.mode(ReadMode::Corrupt);
    assert!(matches!(
        reader.source_revision(&backend, 1, "publication"),
        Err(TeamError::Storage(Error::Invalid(_)))
    ));
}

#[test]
#[ignore = "requires installed PG18; creates only an isolated local cluster"]
fn pg18_native_cursor_expiry_and_actor_binding_are_enforced() {
    let cluster = Cluster::new();
    let directory = tempfile::tempdir().unwrap();
    let source = Engine::open(directory.path().join("source")).unwrap();
    let base = source.checkpoint(None, &[]).unwrap();
    let root = source.checkpoint(Some(&base), &[]).unwrap();
    let local_backend = LocalBackend::open(directory.path().join("backend")).unwrap();
    let credential = "4".repeat(64);
    bootstrap(&cluster.config, "native-cursors", &credential);
    publish(
        &cluster.config,
        "native-cursors",
        "publication",
        SourceNamespace::V1,
        &source,
        &root,
        &local_backend,
        &directory.path().join("offer"),
    );
    let backend = CountingBackend::new(local_backend);
    let mut admin = service(&cluster.config, "native-cursors", &credential);
    let first = admin
        .source_history(&backend, 1, "publication", &page("actor-bound", None, 1))
        .unwrap();
    let cursor = first.next.unwrap();
    let other = generate_secret().unwrap();
    admin
        .provision(&ProvisionRequest {
            authority_generation: 1,
            operation: "other-reader".into(),
            principal: "other".into(),
            credential: other.expose().into(),
            roles: vec![Role::Reader],
        })
        .unwrap();
    let mut other_reader = service(&cluster.config, "native-cursors", other.expose());
    assert!(matches!(
        other_reader.source_history(
            &backend,
            1,
            "publication",
            &page("actor-bound", Some(cursor.clone()), 1),
        ),
        Err(TeamError::Unauthorized)
    ));
    cluster
        .admin()
        .execute(
            "UPDATE team_source_cursors SET expires_at=clock_timestamp()-interval '1 second' WHERE repository='native-cursors'",
            &[],
        )
        .unwrap();
    assert!(matches!(
        admin.source_history(
            &backend,
            1,
            "publication",
            &page("actor-bound", Some(cursor.clone()), 1),
        ),
        Err(TeamError::Expired)
    ));
    let opens_after_expiry = backend.opens.load(Ordering::SeqCst);
    assert!(matches!(
        admin.source_history(
            &backend,
            1,
            "publication",
            &page("actor-bound", Some(cursor.clone()), 1)
        ),
        Err(TeamError::Unauthorized)
    ));
    assert_eq!(backend.opens.load(Ordering::SeqCst), opens_after_expiry);
    // After bounded tombstone cleanup there is no indefinite operation registry.
    // An explicit initial request can create a fresh series, but old handles
    // never resume it. Clients use a new operation for uncertain restarts.
    let restarted = admin
        .source_history(&backend, 1, "publication", &page("actor-bound", None, 1))
        .unwrap();
    assert_eq!(restarted.entries[0].id, root);
    assert_ne!(restarted.next.as_deref(), Some(cursor.as_str()));
    assert!(matches!(
        admin.source_history(
            &backend,
            1,
            "publication",
            &page("actor-bound", Some(cursor), 1)
        ),
        Err(TeamError::Indeterminate)
    ));
    assert_eq!(
        history_page(&source, &root, None, 1).unwrap().entries[0].id,
        root
    );
}

#[test]
#[ignore = "requires installed PG18; creates only an isolated local cluster"]
fn pg18_native_cancel_is_actor_scoped_idempotent_and_wins_an_inflight_advance() {
    let cluster = Cluster::new();
    let directory = tempfile::tempdir().unwrap();
    let source = Engine::open(directory.path().join("source")).unwrap();
    let base = source.checkpoint(None, &[]).unwrap();
    let root = source.checkpoint(Some(&base), &[]).unwrap();
    let local_backend = LocalBackend::open(directory.path().join("backend")).unwrap();
    let credential = "4a".repeat(32);
    bootstrap(&cluster.config, "native-cancel", &credential);
    publish(
        &cluster.config,
        "native-cancel",
        "publication",
        SourceNamespace::V1,
        &source,
        &root,
        &local_backend,
        &directory.path().join("offer"),
    );
    let backend = CountingBackend::new(local_backend);
    let mut admin = service(&cluster.config, "native-cancel", &credential);
    let operation = "cancelled-history";
    let first = admin
        .source_history(&backend, 1, "publication", &page(operation, None, 1))
        .unwrap();
    let cursor = first.next.unwrap();

    let other_credential = generate_secret().unwrap();
    admin
        .provision(&ProvisionRequest {
            authority_generation: 1,
            operation: "cancel-other-reader".into(),
            principal: "other-reader".into(),
            credential: other_credential.expose().into(),
            roles: vec![Role::Reader],
        })
        .unwrap();
    let mut other = service(&cluster.config, "native-cancel", other_credential.expose());
    let opens_before_cancel = backend.opens.load(Ordering::SeqCst);
    other
        .source_cancel(&SourceCancelRequest {
            operation: operation.into(),
        })
        .unwrap();
    assert_eq!(backend.opens.load(Ordering::SeqCst), opens_before_cancel);
    let admin_state: String = cluster
        .admin()
        .query_one(
            "SELECT state FROM team_source_cursors WHERE repository='native-cancel' AND principal='admin' AND operation=$1",
            &[&operation],
        )
        .unwrap()
        .get(0);
    assert_eq!(admin_state, "active");

    let cancelled = admin
        .source_cancel(&SourceCancelRequest {
            operation: operation.into(),
        })
        .unwrap();
    assert!(cancelled.cancelled);
    assert_eq!(cancelled.operation, operation);
    assert_eq!(backend.opens.load(Ordering::SeqCst), opens_before_cancel);
    let row = cluster
        .admin()
        .query_one(
            "SELECT state,reserved_bytes,current_token_digest IS NULL,current_continuation IS NULL,floor(extract(epoch FROM expires_at)*1000000)::bigint FROM team_source_cursors WHERE repository='native-cancel' AND principal='admin' AND operation=$1",
            &[&operation],
        )
        .unwrap();
    assert_eq!(row.get::<_, String>(0), "cancelled");
    assert!(row.get::<_, i64>(1) < 19 * 1024 * 1024);
    assert!(row.get::<_, bool>(2));
    assert!(row.get::<_, bool>(3));
    let expiry: i64 = row.get(4);
    let retry = admin
        .source_cancel(&SourceCancelRequest {
            operation: operation.into(),
        })
        .unwrap();
    assert_eq!(retry, cancelled);
    let retry_expiry: i64 = cluster
        .admin()
        .query_one(
            "SELECT floor(extract(epoch FROM expires_at)*1000000)::bigint FROM team_source_cursors WHERE repository='native-cancel' AND principal='admin' AND operation=$1",
            &[&operation],
        )
        .unwrap()
        .get(0);
    assert_eq!(retry_expiry, expiry);
    assert!(matches!(
        admin.source_history(
            &backend,
            1,
            "publication",
            &page(operation, Some(cursor), 1),
        ),
        Err(TeamError::Indeterminate)
    ));
    assert!(matches!(
        admin.source_history(&backend, 1, "publication", &page(operation, None, 1)),
        Err(TeamError::Indeterminate)
    ));
    assert_eq!(backend.opens.load(Ordering::SeqCst), opens_before_cancel);

    let racing_operation = "cancel-during-advance";
    let racing_first = admin
        .source_history(&backend, 1, "publication", &page(racing_operation, None, 1))
        .unwrap();
    let racing_cursor = racing_first.next.unwrap();
    let cancel_config = cluster.config.clone();
    let cancel_credential = credential.clone();
    backend.hook(move || {
        let mut canceller = service(&cancel_config, "native-cancel", &cancel_credential);
        canceller
            .source_cancel(&SourceCancelRequest {
                operation: racing_operation.into(),
            })
            .unwrap();
    });
    assert!(matches!(
        admin.source_history(
            &backend,
            1,
            "publication",
            &page(racing_operation, Some(racing_cursor), 1),
        ),
        Err(TeamError::Indeterminate)
    ));
    let state: String = cluster
        .admin()
        .query_one(
            "SELECT state FROM team_source_cursors WHERE repository='native-cancel' AND principal='admin' AND operation=$1",
            &[&racing_operation],
        )
        .unwrap()
        .get(0);
    assert_eq!(state, "cancelled");
}

#[test]
#[ignore = "requires installed PG18; creates only an isolated local cluster"]
fn pg18_retirement_and_era_change_during_stream_deny_the_whole_native_result() {
    let cluster = Cluster::new();
    let directory = tempfile::tempdir().unwrap();
    let source = Engine::open(directory.path().join("source")).unwrap();
    let root = source
        .checkpoint(None, &[change("a", b"private bytes")])
        .unwrap();
    let local_backend = LocalBackend::open(directory.path().join("backend")).unwrap();

    let retired_credential = "5".repeat(64);
    bootstrap(&cluster.config, "native-retire", &retired_credential);
    publish(
        &cluster.config,
        "native-retire",
        "publication",
        SourceNamespace::V1,
        &source,
        &root,
        &local_backend,
        &directory.path().join("retire-offer"),
    );
    let retired_backend = CountingBackend::new(local_backend.clone());
    let mut retiring_reader = service(&cluster.config, "native-retire", &retired_credential);
    retiring_reader
        .source_history(
            &retired_backend,
            1,
            "publication",
            &page("retire-cache", None, 1),
        )
        .unwrap();
    assert_eq!(
        cluster
            .admin()
            .query_one(
                "SELECT count(*) FROM team_source_cursors WHERE repository='native-retire'",
                &[],
            )
            .unwrap()
            .get::<_, i64>(0),
        1
    );
    let retire_config = cluster.config.clone();
    retired_backend.hook(move || {
        operator_retire_access(
            &retire_config,
            &RetireAccessRequest {
                scope: scope("native-retire"),
                authority_generation: 1,
                operation: "retire-during-read".into(),
            },
        )
        .unwrap();
    });
    assert!(matches!(
        retiring_reader.source_file(&retired_backend, 1, "publication", "a"),
        Err(TeamError::Storage(Error::SourceDenied))
    ));

    assert_eq!(
        cluster
            .admin()
            .query_one(
                "SELECT count(*) FROM team_source_cursors WHERE repository='native-retire'",
                &[],
            )
            .unwrap()
            .get::<_, i64>(0),
        0
    );

    let era_credential = "6".repeat(64);
    bootstrap(&cluster.config, "native-era", &era_credential);
    publish(
        &cluster.config,
        "native-era",
        "publication",
        SourceNamespace::V1,
        &source,
        &root,
        &local_backend,
        &directory.path().join("era-offer"),
    );
    let era_backend = CountingBackend::new(local_backend);
    let mut old_request = service(&cluster.config, "native-era", &era_credential);
    let era_config = cluster.config.clone();
    era_backend.hook(move || {
        let mut database = postgres::Config::new()
            .host_path(&era_config.socket)
            .port(era_config.port)
            .dbname(&era_config.database)
            .connect(postgres::NoTls)
            .unwrap();
        database
            .execute(
                "UPDATE team_repositories SET generation=2 WHERE repository='native-era'",
                &[],
            )
            .unwrap();
    });
    assert!(matches!(
        old_request.source_file(&era_backend, 1, "publication", "a"),
        Err(TeamError::Storage(Error::SourceDenied))
    ));
    // The bearer grant remains current. A fresh request captures era two and
    // can inspect the retained generation-one publication.
    let mut fresh_request = service(&cluster.config, "native-era", &era_credential);
    let file = fresh_request
        .source_file(&era_backend, 1, "publication", "a")
        .unwrap();
    assert_eq!(file.bytes, b"private bytes");
    assert_eq!(file.source.authority_generation, 2);
}

#[test]
#[ignore = "requires installed PG18; creates only an isolated local cluster"]
fn pg18_native_file_output_cap_and_reader_auth_precede_raw_opens() {
    let cluster = Cluster::new();
    let directory = tempfile::tempdir().unwrap();
    let source = Engine::open(directory.path().join("source")).unwrap();
    let large_bytes = vec![7; 1024 * 1024 + 1];
    let root = source
        .checkpoint(None, &[change("large", &large_bytes)])
        .unwrap();
    let local_backend = LocalBackend::open(directory.path().join("backend")).unwrap();
    let credential = "7".repeat(64);
    bootstrap(&cluster.config, "native-output", &credential);
    publish(
        &cluster.config,
        "native-output",
        "publication",
        SourceNamespace::V1,
        &source,
        &root,
        &local_backend,
        &directory.path().join("offer"),
    );
    let backend = CountingBackend::new(local_backend);
    let mut reader = service(&cluster.config, "native-output", &credential);
    assert!(matches!(
        reader.source_file(&backend, 1, "publication", "large"),
        Err(TeamError::Storage(Error::Limit(_)))
    ));

    let before = backend.opens.load(Ordering::SeqCst);
    assert!(matches!(
        TeamService::connect(
            &cluster.config,
            scope("native-output"),
            Credentials::bearer(&"8".repeat(64)).unwrap(),
        ),
        Err(TeamError::Unauthorized)
    ));
    assert_eq!(backend.opens.load(Ordering::SeqCst), before);

    let contributor = generate_secret().unwrap();
    reader
        .provision(&ProvisionRequest {
            authority_generation: 1,
            operation: "contributor-without-reader".into(),
            principal: "contributor".into(),
            credential: contributor.expose().into(),
            roles: vec![Role::Contributor],
        })
        .unwrap();
    let mut non_reader = service(&cluster.config, "native-output", contributor.expose());
    assert!(matches!(
        non_reader.source_revision(&backend, 1, "publication"),
        Err(TeamError::Denied)
    ));
    assert_eq!(backend.opens.load(Ordering::SeqCst), before);
}

#[test]
#[ignore = "requires installed PG18; creates only an isolated local cluster"]
fn pg18_native_history_over_128_pages_uses_one_session_and_exact_replay_window() {
    let cluster = Cluster::new();
    let directory = tempfile::tempdir().unwrap();
    let source = Engine::open(directory.path().join("source")).unwrap();
    let mut root = source.checkpoint(None, &[]).unwrap();
    for _ in 0..130 {
        root = source.checkpoint(Some(&root), &[]).unwrap();
    }
    let local_backend = LocalBackend::open(directory.path().join("backend")).unwrap();
    let credential = "c".repeat(64);
    bootstrap(&cluster.config, "native-long-history", &credential);
    publish(
        &cluster.config,
        "native-long-history",
        "publication",
        SourceNamespace::V1,
        &source,
        &root,
        &local_backend,
        &directory.path().join("offer"),
    );
    let backend = CountingBackend::new(local_backend);
    let mut reader = service(&cluster.config, "native-long-history", &credential);
    let operation = "long-history";

    let first = reader
        .source_history(&backend, 1, "publication", &page(operation, None, 1))
        .unwrap();
    let opens_after_first = backend.opens.load(Ordering::SeqCst);
    let expiry_after_first: i64 = cluster
        .admin()
        .query_one(
            "SELECT floor(extract(epoch FROM expires_at)*1000000)::bigint FROM team_source_cursors WHERE repository='native-long-history' AND operation=$1",
            &[&operation],
        )
        .unwrap()
        .get(0);
    let first_retry = reader
        .source_history(&backend, 1, "publication", &page(operation, None, 1))
        .unwrap();
    assert_eq!(first_retry, first);
    assert_eq!(backend.opens.load(Ordering::SeqCst), opens_after_first);
    let expiry_after_retry: i64 = cluster
        .admin()
        .query_one(
            "SELECT floor(extract(epoch FROM expires_at)*1000000)::bigint FROM team_source_cursors WHERE repository='native-long-history' AND operation=$1",
            &[&operation],
        )
        .unwrap()
        .get(0);
    assert_eq!(expiry_after_retry, expiry_after_first);
    let first_token = first.next.clone().unwrap();
    let second = reader
        .source_history(
            &backend,
            1,
            "publication",
            &page(operation, Some(first_token.clone()), 1),
        )
        .unwrap();
    let opens_after_second = backend.opens.load(Ordering::SeqCst);
    let second_retry = reader
        .source_history(
            &backend,
            1,
            "publication",
            &page(operation, Some(first_token.clone()), 1),
        )
        .unwrap();
    assert_eq!(second_retry, second);
    assert_eq!(backend.opens.load(Ordering::SeqCst), opens_after_second);
    let initial_at_version_two = reader
        .source_history(&backend, 1, "publication", &page(operation, None, 1))
        .unwrap();
    assert_eq!(initial_at_version_two, first);

    let mut pages = 2usize;
    let mut cursor = second.next.clone();
    let mut max_state_bytes = 0i32;
    let mut final_page = None;
    let mut final_consumed = None;
    while let Some(token) = cursor.take() {
        let response = reader
            .source_history(
                &backend,
                1,
                "publication",
                &page(operation, Some(token.clone()), 1),
            )
            .unwrap();
        pages += 1;
        let state_bytes: Option<i32> = cluster
            .admin()
            .query_one(
                "SELECT octet_length(current_continuation) FROM team_source_cursors WHERE repository='native-long-history' AND operation=$1",
                &[&operation],
            )
            .unwrap()
            .get(0);
        max_state_bytes = max_state_bytes.max(state_bytes.unwrap_or(0));
        if response.next.is_none() {
            final_consumed = Some(token);
            final_page = Some(response);
            break;
        }
        cursor = response.next.clone();
        if pages == 3 {
            assert!(matches!(
                reader.source_history(&backend, 1, "publication", &page(operation, None, 1),),
                Err(TeamError::Indeterminate)
            ));
            assert!(matches!(
                reader.source_history(
                    &backend,
                    1,
                    "publication",
                    &page(operation, Some(first_token.clone()), 1),
                ),
                Err(TeamError::Indeterminate)
            ));
        }
    }
    assert_eq!(pages, 131);
    assert!(max_state_bytes > 8 * 1024);
    let final_page = final_page.unwrap();
    let opens_before_final_retry = backend.opens.load(Ordering::SeqCst);
    let final_retry = reader
        .source_history(
            &backend,
            1,
            "publication",
            &page(operation, final_consumed, 1),
        )
        .unwrap();
    assert_eq!(final_retry, final_page);
    assert_eq!(
        backend.opens.load(Ordering::SeqCst),
        opens_before_final_retry
    );
    let row = cluster
        .admin()
        .query_one(
            "SELECT count(*),max(version),bool_and(state='terminal'),max(reserved_bytes) FROM team_source_cursors WHERE repository='native-long-history' AND operation=$1",
            &[&operation],
        )
        .unwrap();
    assert_eq!(row.get::<_, i64>(0), 1);
    assert_eq!(row.get::<_, Option<i64>>(1), Some(131));
    assert_eq!(row.get::<_, Option<bool>>(2), Some(true));
    assert!(row.get::<_, Option<i64>>(3).unwrap() < 19 * 1024 * 1024);
}

#[test]
#[ignore = "requires installed PG18; creates only an isolated local cluster"]
fn pg18_native_same_token_race_returns_one_exact_winner_reply_and_one_advance() {
    let cluster = Cluster::new();
    let directory = tempfile::tempdir().unwrap();
    let source = Engine::open(directory.path().join("source")).unwrap();
    let oldest = source.checkpoint(None, &[]).unwrap();
    let middle = source.checkpoint(Some(&oldest), &[]).unwrap();
    let root = source.checkpoint(Some(&middle), &[]).unwrap();
    let local_backend = LocalBackend::open(directory.path().join("backend")).unwrap();
    let credential = "ca".repeat(32);
    bootstrap(&cluster.config, "native-same-token-race", &credential);
    publish(
        &cluster.config,
        "native-same-token-race",
        "publication",
        SourceNamespace::V1,
        &source,
        &root,
        &local_backend,
        &directory.path().join("offer"),
    );
    let backend = Arc::new(RaceBackend::new(local_backend));
    let operation = "same-token-race";
    let mut first_reader = service(&cluster.config, "native-same-token-race", &credential);
    let first = first_reader
        .source_history(
            backend.as_ref(),
            1,
            "publication",
            &page(operation, None, 1),
        )
        .unwrap();
    let cursor = first.next.unwrap();
    let second_reader = service(&cluster.config, "native-same-token-race", &credential);
    backend.arm(&root);

    let first_backend = backend.clone();
    let first_cursor = cursor.clone();
    let first_thread = std::thread::spawn(move || {
        let mut reader = first_reader;
        reader.source_history(
            first_backend.as_ref(),
            1,
            "publication",
            &page(operation, Some(first_cursor), 1),
        )
    });
    let second_backend = backend.clone();
    let second_thread = std::thread::spawn(move || {
        let mut reader = second_reader;
        reader.source_history(
            second_backend.as_ref(),
            1,
            "publication",
            &page(operation, Some(cursor), 1),
        )
    });
    let first_result = first_thread.join().unwrap().unwrap();
    let second_result = second_thread.join().unwrap().unwrap();
    assert_eq!(first_result, second_result);
    assert_eq!(first_result.entries.len(), 1);
    assert_eq!(first_result.entries[0].id, middle);
    assert!(first_result.next.is_some());
    assert!(backend.target_opens.load(Ordering::SeqCst) >= 2);

    let row = cluster
        .admin()
        .query_one(
            "SELECT version,state,previous_token_digest IS NOT NULL,previous_reply IS NOT NULL,current_token_digest IS NOT NULL FROM team_source_cursors WHERE repository='native-same-token-race' AND principal='admin' AND operation=$1",
            &[&operation],
        )
        .unwrap();
    assert_eq!(row.get::<_, i64>(0), 2);
    assert_eq!(row.get::<_, String>(1), "active");
    assert!(row.get::<_, bool>(2));
    assert!(row.get::<_, bool>(3));
    assert!(row.get::<_, bool>(4));
}

fn insert_synthetic_cursor_rows(
    database: &mut postgres::Client,
    repository: &str,
    principal: &str,
    start: usize,
    count: usize,
) {
    // These rows exercise count and cleanup boundaries only. They are not
    // evidence that an application cursor was ever issued or authenticated.
    for index in start..start + count {
        let operation = format!("synthetic-{index}");
        database
            .execute(
                "INSERT INTO team_source_cursors(trust_domain,repository,principal,principal_version,authority_generation,operation,binding_digest,binding,state,version,initial_reply,initial_reply_digest,reserved_bytes,expires_at) VALUES('team',$1,$2,1,1,$3,$4,'{}','terminal',1,'{}',$5,4096,clock_timestamp()+interval '1 hour')",
                &[
                    &repository,
                    &principal,
                    &operation,
                    &"a".repeat(64),
                    &"b".repeat(64),
                ],
            )
            .unwrap();
    }
}

fn insert_synthetic_principal(
    database: &mut postgres::Client,
    repository: &str,
    principal: &str,
    sequence: usize,
) {
    let credential_digest = hex64(100_000 + sequence);
    database
        .execute(
            "INSERT INTO team_principals(trust_domain,repository,principal,credential_digest,version,enabled) VALUES('team',$1,$2,$3,1,TRUE)",
            &[&repository, &principal, &credential_digest],
        )
        .unwrap();
}

fn insert_synthetic_pending(
    database: &mut postgres::Client,
    repository: &str,
    principal: &str,
    operation: &str,
) {
    database
        .execute(
            "INSERT INTO team_source_cursors(trust_domain,repository,principal,principal_version,authority_generation,operation,binding_digest,binding,state,version,reserved_bytes,expires_at) VALUES('team',$1,$2,1,1,$3,$4,'{}','pending',0,19922944,clock_timestamp()+interval '1 hour')",
            &[&repository, &principal, &operation, &"a".repeat(64)],
        )
        .unwrap();
}

fn hex64(value: usize) -> String {
    format!("{value:064x}")
}

#[test]
#[ignore = "requires installed PG18; synthetic row exercises only schema storage bounds"]
fn pg18_native_cursor_schema_accepts_the_reserved_maximum_state_vector() {
    let cluster = Cluster::new();
    let credential = "f".repeat(64);
    bootstrap(&cluster.config, "cursor-max-state", &credential);
    let continuation = "s".repeat(16 * 1024 * 1024);
    let mut database = cluster.admin();
    database
        .execute(
            "INSERT INTO team_source_cursors(trust_domain,repository,principal,principal_version,authority_generation,operation,binding_digest,binding,state,version,current_token_digest,current_continuation,initial_reply,initial_reply_digest,reserved_bytes,expires_at) VALUES('team','cursor-max-state','admin',1,1,'synthetic-max-state',$1,'{}','active',1,$2,$3,'{}',$4,19922944,clock_timestamp()+interval '1 hour')",
            &[&"a".repeat(64), &"b".repeat(64), &continuation, &"c".repeat(64)],
        )
        .unwrap();
    let row = database
        .query_one(
            "SELECT octet_length(current_continuation),reserved_bytes,octet_length(current_continuation) < reserved_bytes FROM team_source_cursors WHERE repository='cursor-max-state'",
            &[],
        )
        .unwrap();
    assert_eq!(row.get::<_, Option<i32>>(0), Some(16 * 1024 * 1024));
    assert_eq!(row.get::<_, i64>(1), 19 * 1024 * 1024);
    assert!(row.get::<_, bool>(2));
    let over_limit = "x".repeat(16 * 1024 * 1024 + 1);
    assert!(
        database
            .execute(
                "INSERT INTO team_source_cursors(trust_domain,repository,principal,principal_version,authority_generation,operation,binding_digest,binding,state,version,current_token_digest,current_continuation,initial_reply,initial_reply_digest,reserved_bytes,expires_at) VALUES('team','cursor-max-state','admin',1,1,'synthetic-over-state',$1,'{}','active',1,$2,$3,'{}',$4,19922944,clock_timestamp()+interval '1 hour')",
                &[&"d".repeat(64), &"e".repeat(64), &over_limit, &"f".repeat(64)],
            )
            .is_err()
    );
    let count: i64 = database
        .query_one(
            "SELECT count(*) FROM team_source_cursors WHERE repository='cursor-max-state'",
            &[],
        )
        .unwrap()
        .get(0);
    assert_eq!(count, 1);
}

#[test]
#[ignore = "requires installed PG18; synthetic rows exercise quota accounting only"]
fn pg18_native_cursor_reserved_byte_quotas_reject_without_partial_rows() {
    let cluster = Cluster::new();
    let directory = tempfile::tempdir().unwrap();
    let source = Engine::open(directory.path().join("source")).unwrap();
    let base = source.checkpoint(None, &[]).unwrap();
    let root = source.checkpoint(Some(&base), &[]).unwrap();
    let local_backend = LocalBackend::open(directory.path().join("backend")).unwrap();

    let principal_credential = "d".repeat(64);
    bootstrap(
        &cluster.config,
        "cursor-principal-bytes",
        &principal_credential,
    );
    publish(
        &cluster.config,
        "cursor-principal-bytes",
        "publication",
        SourceNamespace::V1,
        &source,
        &root,
        &local_backend,
        &directory.path().join("principal-byte-offer"),
    );
    let backend = CountingBackend::new(local_backend.clone());
    let mut database = cluster.admin();
    for index in 0..3 {
        insert_synthetic_pending(
            &mut database,
            "cursor-principal-bytes",
            "admin",
            &format!("pending-{index}"),
        );
    }
    let mut reader = service(
        &cluster.config,
        "cursor-principal-bytes",
        &principal_credential,
    );
    assert!(matches!(
        reader.source_history(
            &backend,
            1,
            "publication",
            &page("over-principal-bytes", None, 1),
        ),
        Err(TeamError::Storage(Error::Limit(_)))
    ));
    let count: i64 = database
        .query_one(
            "SELECT count(*) FROM team_source_cursors WHERE repository='cursor-principal-bytes'",
            &[],
        )
        .unwrap()
        .get(0);
    assert_eq!(count, 3);
    reader
        .source_cancel(&SourceCancelRequest {
            operation: "pending-0".into(),
        })
        .unwrap();
    assert!(
        reader
            .source_history(
                &backend,
                1,
                "publication",
                &page("after-principal-byte-cancel", None, 1),
            )
            .unwrap()
            .next
            .is_some()
    );

    let repository_credential = "e".repeat(64);
    bootstrap(
        &cluster.config,
        "cursor-repository-bytes",
        &repository_credential,
    );
    publish(
        &cluster.config,
        "cursor-repository-bytes",
        "publication",
        SourceNamespace::V1,
        &source,
        &root,
        &local_backend,
        &directory.path().join("repository-byte-offer"),
    );
    for index in 0..13 {
        let principal = format!("byte-principal-{index}");
        insert_synthetic_principal(
            &mut database,
            "cursor-repository-bytes",
            &principal,
            100 + index,
        );
        insert_synthetic_pending(
            &mut database,
            "cursor-repository-bytes",
            &principal,
            &format!("pending-{index}"),
        );
    }
    let mut repository_reader = service(
        &cluster.config,
        "cursor-repository-bytes",
        &repository_credential,
    );
    assert!(matches!(
        repository_reader.source_history(
            &backend,
            1,
            "publication",
            &page("over-repository-bytes", None, 1),
        ),
        Err(TeamError::Storage(Error::Limit(_)))
    ));
    let count: i64 = database
        .query_one(
            "SELECT count(*) FROM team_source_cursors WHERE repository='cursor-repository-bytes'",
            &[],
        )
        .unwrap()
        .get(0);
    assert_eq!(count, 13);
}

#[test]
#[ignore = "requires installed PG18; creates only an isolated local cluster"]
fn pg18_native_cursor_caps_are_atomic_and_cleanup_restores_capacity() {
    let cluster = Cluster::new();
    let directory = tempfile::tempdir().unwrap();
    let source = Engine::open(directory.path().join("source")).unwrap();
    let base = source.checkpoint(None, &[]).unwrap();
    let root = source.checkpoint(Some(&base), &[]).unwrap();
    let local_backend = LocalBackend::open(directory.path().join("backend")).unwrap();

    let principal_credential = "9".repeat(64);
    bootstrap(
        &cluster.config,
        "cursor-principal-cap",
        &principal_credential,
    );
    publish(
        &cluster.config,
        "cursor-principal-cap",
        "publication",
        SourceNamespace::V1,
        &source,
        &root,
        &local_backend,
        &directory.path().join("principal-offer"),
    );
    let backend = CountingBackend::new(local_backend.clone());
    let mut database = cluster.admin();
    insert_synthetic_cursor_rows(&mut database, "cursor-principal-cap", "admin", 1, 128);
    let mut principal_reader = service(
        &cluster.config,
        "cursor-principal-cap",
        &principal_credential,
    );
    assert!(matches!(
        principal_reader.source_history(&backend, 1, "publication", &page("real-at-cap", None, 1),),
        Err(TeamError::Storage(Error::Limit(_)))
    ));
    let count: i64 = database
        .query_one(
            "SELECT count(*) FROM team_source_cursors WHERE repository='cursor-principal-cap'",
            &[],
        )
        .unwrap()
        .get(0);
    assert_eq!(count, 128);
    database
        .execute(
            "UPDATE team_source_cursors SET expires_at=clock_timestamp()-interval '1 second' WHERE repository='cursor-principal-cap' AND operation='synthetic-1'",
            &[],
        )
        .unwrap();
    assert!(
        principal_reader
            .source_history(
                &backend,
                1,
                "publication",
                &page("real-after-expiry", None, 1),
            )
            .unwrap()
            .next
            .is_some()
    );
    let count: i64 = database
        .query_one(
            "SELECT count(*) FROM team_source_cursors WHERE repository='cursor-principal-cap'",
            &[],
        )
        .unwrap()
        .get(0);
    assert_eq!(count, 128);
    database
        .execute(
            "UPDATE team_source_cursors SET authority_generation=2 WHERE repository='cursor-principal-cap' AND operation=(SELECT min(operation) FROM team_source_cursors WHERE repository='cursor-principal-cap')",
            &[],
        )
        .unwrap();
    assert!(
        principal_reader
            .source_history(
                &backend,
                1,
                "publication",
                &page("real-after-stale", None, 1),
            )
            .unwrap()
            .next
            .is_some()
    );
    let count: i64 = database
        .query_one(
            "SELECT count(*) FROM team_source_cursors WHERE repository='cursor-principal-cap'",
            &[],
        )
        .unwrap()
        .get(0);
    assert_eq!(count, 128);

    let repository_credential = "b".repeat(64);
    bootstrap(
        &cluster.config,
        "cursor-repository-cap",
        &repository_credential,
    );
    publish(
        &cluster.config,
        "cursor-repository-cap",
        "publication",
        SourceNamespace::V1,
        &source,
        &root,
        &local_backend,
        &directory.path().join("repository-offer"),
    );
    for principal_index in 0..8 {
        let principal = format!("synthetic-{principal_index}");
        insert_synthetic_principal(
            &mut database,
            "cursor-repository-cap",
            &principal,
            principal_index,
        );
        insert_synthetic_cursor_rows(
            &mut database,
            "cursor-repository-cap",
            &principal,
            10_000 + principal_index * 128,
            128,
        );
    }
    let mut repository_reader = service(
        &cluster.config,
        "cursor-repository-cap",
        &repository_credential,
    );
    assert!(matches!(
        repository_reader.source_history(
            &backend,
            1,
            "publication",
            &page("real-at-repository-cap", None, 1),
        ),
        Err(TeamError::Storage(Error::Limit(_)))
    ));
    let count: i64 = database
        .query_one(
            "SELECT count(*) FROM team_source_cursors WHERE repository='cursor-repository-cap'",
            &[],
        )
        .unwrap()
        .get(0);
    assert_eq!(count, 1_024);
    database
        .execute(
            "UPDATE team_source_cursors SET expires_at=clock_timestamp()-interval '1 second' WHERE repository='cursor-repository-cap' AND operation=(SELECT min(operation) FROM team_source_cursors WHERE repository='cursor-repository-cap')",
            &[],
        )
        .unwrap();
    assert!(
        repository_reader
            .source_history(
                &backend,
                1,
                "publication",
                &page("real-after-repository-expiry", None, 1),
            )
            .unwrap()
            .next
            .is_some()
    );
    let count: i64 = database
        .query_one(
            "SELECT count(*) FROM team_source_cursors WHERE repository='cursor-repository-cap'",
            &[],
        )
        .unwrap()
        .get(0);
    assert_eq!(count, 1_024);
}
