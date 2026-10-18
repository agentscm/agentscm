#[path = "common/team_pg.rs"]
mod team_pg;

use std::path::Path;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

use agentscm::backend::{ImmutableBackend, LocalBackend, ObjectKey, ObjectMetadata, PutOutcome};
use agentscm::model::FileChange;
use agentscm::store::{Engine, digest};
use agentscm::team::TeamError;
use agentscm::team::publication::{
    CompleteRequest, PublicationAuthority, RepositoryScope, ReserveRequest, SourceNamespace,
};

use team_pg::Cluster;

fn scope(repository: &str) -> RepositoryScope {
    RepositoryScope {
        trust_domain: "team".into(),
        repository: repository.into(),
    }
}

fn authority(cluster: &Cluster, repository: &str) -> PublicationAuthority {
    PublicationAuthority::connect_local(
        &cluster.config.socket,
        cluster.config.port,
        &cluster.config.database,
        scope(repository),
    )
    .unwrap()
}

fn reservation(publication: &str, root: &str) -> ReserveRequest {
    ReserveRequest {
        generation: 1,
        publication: publication.into(),
        root: root.into(),
        byte_limit: 1_000_000,
        object_limit: 1_000,
        lease_seconds: 60,
    }
}

fn complete(publication: &str, manifest_digest: &str) -> CompleteRequest {
    CompleteRequest {
        generation: 1,
        publication: publication.into(),
        operation: format!("complete-{publication}"),
        manifest_digest: manifest_digest.into(),
    }
}

fn put(path: &str, bytes: &[u8]) -> FileChange {
    FileChange {
        path: path.into(),
        content: Some(bytes.to_vec()),
    }
}

fn multi_parent_source(directory: &Path) -> (Engine, String, String) {
    let source = Engine::open(directory).unwrap();
    let first = source
        .checkpoint(None, &[put("README.md", b"first")])
        .unwrap();
    let second = source
        .checkpoint(Some(&first), &[put("src/lib.rs", b"second")])
        .unwrap();
    let root = source
        .checkpoint_with_parents(&[first, second], &[put("src/merged.rs", b"multi-parent")])
        .unwrap();
    let r1 = source
        .checkpoint(None, &[put("single-parent.txt", b"v1")])
        .unwrap();
    (source, root, r1)
}

struct RecordingBackend {
    inner: LocalBackend,
    prepares: AtomicUsize,
    puts: AtomicUsize,
    profiles: Mutex<Vec<String>>,
}

impl RecordingBackend {
    fn new(path: &Path) -> Self {
        Self {
            inner: LocalBackend::open(path).unwrap(),
            prepares: AtomicUsize::new(0),
            puts: AtomicUsize::new(0),
            profiles: Mutex::new(Vec::new()),
        }
    }

    fn record(&self, key: &ObjectKey) {
        self.profiles.lock().unwrap().push(key.profile.clone());
    }
}

impl ImmutableBackend for RecordingBackend {
    fn prepare_publication(&self, key: &ObjectKey, incarnation: &str) -> agentscm::Result<()> {
        self.prepares.fetch_add(1, Ordering::SeqCst);
        self.record(key);
        self.inner.prepare_publication(key, incarnation)
    }

    fn put_if_absent(
        &self,
        key: &ObjectKey,
        incarnation: &str,
        bytes: &[u8],
    ) -> agentscm::Result<PutOutcome> {
        self.puts.fetch_add(1, Ordering::SeqCst);
        self.record(key);
        self.inner.put_if_absent(key, incarnation, bytes)
    }

    fn get_verified(&self, key: &ObjectKey, incarnation: &str) -> agentscm::Result<Vec<u8>> {
        self.record(key);
        self.inner.get_verified(key, incarnation)
    }

    fn inspect_verified(
        &self,
        key: &ObjectKey,
        incarnation: &str,
    ) -> agentscm::Result<ObjectMetadata> {
        self.record(key);
        self.inner.inspect_verified(key, incarnation)
    }

    fn get_range_verified(
        &self,
        key: &ObjectKey,
        incarnation: &str,
        offset: u64,
        length: u64,
    ) -> agentscm::Result<Vec<u8>> {
        self.record(key);
        self.inner
            .get_range_verified(key, incarnation, offset, length)
    }
}

#[test]
fn source_namespace_has_exact_public_wire_contract() {
    assert_eq!(SourceNamespace::V1.as_str(), "agentscm-source-v1");
    assert_eq!(SourceNamespace::V2.as_str(), "agentscm-source-v2");
    assert_eq!(SourceNamespace::V1.version(), 1);
    assert_eq!(SourceNamespace::V2.version(), 2);
    assert_eq!(
        SourceNamespace::V2.required_capabilities(),
        [
            "complete-revision-history",
            "exact-object-bytes",
            "root-last-import",
            "multi-parent-revisions",
        ]
    );
    assert_eq!(
        serde_json::to_string(&SourceNamespace::V1).unwrap(),
        "\"agentscm-source-v1\""
    );
    assert_eq!(
        serde_json::from_str::<SourceNamespace>("\"agentscm-source-v2\"").unwrap(),
        SourceNamespace::V2
    );
}

#[test]
#[ignore = "requires explicit AGENTSCM_PG_BIN_DIR; fresh isolated PostgreSQL18 only"]
fn pg18_default_v1_rejects_r2_before_writes_and_refunds() {
    let cluster = Cluster::new();
    let fixture = tempfile::tempdir().unwrap();
    let (source, r2, _) = multi_parent_source(&fixture.path().join("source"));
    let backend = RecordingBackend::new(&fixture.path().join("backend"));
    let mut publication = authority(&cluster, "default-v1");
    publication.register(1, 1_000_000, 1_000).unwrap();
    let request = reservation("r2-default", &r2);
    publication.reserve(&request).unwrap();

    let result = publication.upload_and_qualify(
        1,
        "r2-default",
        &source,
        &backend,
        &fixture.path().join("offer-default"),
    );
    assert!(
        matches!(result, Err(TeamError::Storage(agentscm::Error::Invalid(message))) if message.contains("source profile v1"))
    );
    assert_eq!(backend.prepares.load(Ordering::SeqCst), 0);
    assert_eq!(backend.puts.load(Ordering::SeqCst), 0);

    let mut sql = cluster.admin();
    let stored = sql
        .query_one(
            "SELECT request_digest,source_profile,state FROM team_publications WHERE repository='default-v1' AND publication='r2-default'",
            &[],
        )
        .unwrap();
    let expected_digest = digest(&serde_json::to_vec(&(scope("default-v1"), &request)).unwrap());
    assert_eq!(stored.get::<_, String>(0), expected_digest);
    assert_eq!(stored.get::<_, String>(1), "agentscm-source-v1");
    assert_eq!(stored.get::<_, String>(2), "reserved");

    publication.abandon(1, "r2-default").unwrap();
    let quota = sql
        .query_one(
            "SELECT allocated_bytes,allocated_objects FROM team_repositories WHERE repository='default-v1'",
            &[],
        )
        .unwrap();
    assert_eq!(quota.get::<_, i64>(0), 0);
    assert_eq!(quota.get::<_, i64>(1), 0);
}

#[test]
#[ignore = "requires explicit AGENTSCM_PG_BIN_DIR; fresh isolated PostgreSQL18 only"]
fn pg18_explicit_v2_completes_closure_and_rejects_r1_before_writes() {
    let cluster = Cluster::new();
    let fixture = tempfile::tempdir().unwrap();
    let (source, r2, r1) = multi_parent_source(&fixture.path().join("source"));
    let backend = RecordingBackend::new(&fixture.path().join("backend"));
    let mut publication = authority(&cluster, "explicit-v2");
    publication.register(1, 2_000_000, 2_000).unwrap();

    let r2_request = reservation("r2", &r2);
    publication
        .reserve_profile(&r2_request, SourceNamespace::V2)
        .unwrap();
    publication
        .reserve_profile(&r2_request, SourceNamespace::V2)
        .unwrap();
    assert!(matches!(
        publication.reserve_profile(&r2_request, SourceNamespace::V1),
        Err(TeamError::Conflict(_))
    ));
    assert!(matches!(
        publication.reserve(&r2_request),
        Err(TeamError::Conflict(_))
    ));

    let manifest_digest = publication
        .upload_and_qualify(1, "r2", &source, &backend, &fixture.path().join("offer-v2"))
        .unwrap();
    let outcome = publication
        .complete(&complete("r2", &manifest_digest))
        .unwrap();
    assert_eq!(outcome.root, r2);
    assert_eq!(backend.prepares.load(Ordering::SeqCst), 1);
    assert!(backend.puts.load(Ordering::SeqCst) > 0);
    assert!(
        backend
            .profiles
            .lock()
            .unwrap()
            .iter()
            .all(|profile| profile == "agentscm-source-v2")
    );

    let mut sql = cluster.admin();
    let row = sql
        .query_one(
            "SELECT source_profile,manifest FROM team_publications WHERE repository='explicit-v2' AND publication='r2'",
            &[],
        )
        .unwrap();
    assert_eq!(row.get::<_, String>(0), "agentscm-source-v2");
    let manifest: serde_json::Value = serde_json::from_str(&row.get::<_, String>(1)).unwrap();
    assert_eq!(manifest["profile"], "agentscm-source-v2");

    let writes_before = backend.puts.load(Ordering::SeqCst);
    let prepares_before = backend.prepares.load(Ordering::SeqCst);
    let r1_request = reservation("r1-as-v2", &r1);
    publication
        .reserve_profile(&r1_request, SourceNamespace::V2)
        .unwrap();
    let result = publication.upload_and_qualify(
        1,
        "r1-as-v2",
        &source,
        &backend,
        &fixture.path().join("offer-r1-as-v2"),
    );
    assert!(
        matches!(result, Err(TeamError::Storage(agentscm::Error::Invalid(message))) if message.contains("reserved profile agentscm-source-v2"))
    );
    assert_eq!(backend.prepares.load(Ordering::SeqCst), prepares_before);
    assert_eq!(backend.puts.load(Ordering::SeqCst), writes_before);
    publication.abandon(1, "r1-as-v2").unwrap();
}

#[test]
#[ignore = "requires explicit AGENTSCM_PG_BIN_DIR; fresh isolated PostgreSQL18 only"]
fn pg18_schema_v1_and_v2_upgrade_preserves_rows_as_default_v1() {
    let cluster = Cluster::new();
    for initial_version in [1, 2] {
        let mut sql = cluster.admin();
        if initial_version == 1 {
            sql.batch_execute(include_str!("../src/team/schema.sql"))
                .unwrap();
        } else {
            sql.batch_execute(include_str!("../src/team/schema.sql"))
                .unwrap();
            sql.batch_execute(include_str!("../src/team/auth-schema.sql"))
                .unwrap();
        }
        sql.execute("INSERT INTO team_repositories(trust_domain,repository,generation,max_bytes,max_objects) VALUES('team','legacy',1,1000000,1000)",&[]).unwrap();
        let old_request = reservation("old-publication", &"0".repeat(64));
        let old_json = serde_json::to_string(&old_request).unwrap();
        let old_digest = digest(&serde_json::to_vec(&(scope("legacy"), &old_request)).unwrap());
        sql.execute("INSERT INTO team_publications(trust_domain,repository,generation,publication,request_digest,request,state,expires_at) VALUES('team','legacy',1,'old-publication',$1,$2,'reserved',clock_timestamp()+interval '1 hour')", &[&old_digest, &old_json]).unwrap();
        sql.execute("INSERT INTO team_outcomes(trust_domain,repository,generation,operation,request_digest,outcome) VALUES('team','legacy',1,'old-operation','old-outcome-digest','{\"retained\":true}')", &[]).unwrap();
        drop(sql);

        let mut migrated = authority(&cluster, "legacy");
        migrated.reserve(&old_request).unwrap();
        assert!(matches!(
            migrated.reserve_profile(&old_request, SourceNamespace::V1),
            Err(TeamError::Conflict(_))
        ));
        drop(migrated);
        let mut sql = cluster.admin();
        let version: i32 = sql
            .query_one("SELECT version FROM team_schema", &[])
            .unwrap()
            .get(0);
        assert_eq!(version, 4);
        let publication = sql
            .query_one("SELECT request_digest,request,state,source_profile FROM team_publications WHERE repository='legacy'", &[])
            .unwrap();
        assert_eq!(publication.get::<_, String>(0), old_digest);
        assert_eq!(publication.get::<_, String>(1), old_json);
        assert_eq!(publication.get::<_, String>(2), "reserved");
        assert_eq!(publication.get::<_, String>(3), "agentscm-source-v1");
        let outcome = sql
            .query_one(
                "SELECT request_digest,outcome,kind FROM team_outcomes WHERE repository='legacy'",
                &[],
            )
            .unwrap();
        assert_eq!(outcome.get::<_, String>(0), "old-outcome-digest");
        assert_eq!(outcome.get::<_, String>(1), "{\"retained\":true}");
        assert_eq!(outcome.get::<_, String>(2), "publication");

        sql.batch_execute("DROP SCHEMA public CASCADE; CREATE SCHEMA public")
            .unwrap();
    }
}
