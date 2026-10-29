use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Barrier};

use agentscm::backend::{ImmutableBackend, LocalBackend, ObjectKey, ObjectMetadata, PutOutcome};
use agentscm::model::FileChange;
use agentscm::store::Engine;
use agentscm::team::TeamError;
use agentscm::team::publication::{
    CompleteRequest, PublicationAuthority, RepositoryScope, ReserveRequest,
};
use postgres::{Client, NoTls};
use tempfile::TempDir;

// Explicit live qualification starts only a fresh project-owned cluster with
// TCP disabled. The default suite cannot silently skip and claim PG passed.
const PG: &str = "/opt/homebrew/opt/postgresql@18/bin";
struct Cluster {
    directory: TempDir,
    socket: PathBuf,
}
impl Cluster {
    fn new() -> Self {
        let directory = tempfile::Builder::new()
            .prefix("agentscm-pg-")
            .tempdir_in("/tmp")
            .unwrap();
        let socket = directory.path().join("socket");
        std::fs::create_dir(&socket).unwrap();
        let init = Command::new(format!("{PG}/initdb"))
            .args(["-D"])
            .arg(directory.path().join("data"))
            .args([
                "--no-locale",
                "--encoding=UTF8",
                "--auth-local=trust",
                "--auth-host=reject",
            ])
            .output()
            .expect("PG18 initdb is required");
        assert!(
            init.status.success(),
            "{}",
            String::from_utf8_lossy(&init.stderr)
        );
        let cluster = Self { directory, socket };
        cluster.start();
        cluster
    }
    fn control(&self, action: &str) {
        let result = Command::new(format!("{PG}/pg_ctl"))
            .arg("-D")
            .arg(self.directory.path().join("data"))
            .args(["-w", "-t", "15", "-m", "fast", action])
            .output()
            .unwrap();
        assert!(
            result.status.success(),
            "{}",
            String::from_utf8_lossy(&result.stderr)
        );
    }
    fn start(&self) {
        let options = format!(
            "-k {} -p 5432 -c listen_addresses='' -c max_connections=12",
            self.socket.display()
        );
        let result = Command::new(format!("{PG}/pg_ctl"))
            .arg("-D")
            .arg(self.directory.path().join("data"))
            .arg("-l")
            .arg(self.directory.path().join("postgres.log"))
            .args(["-w", "-t", "15", "-o", &options, "start"])
            .output()
            .unwrap();
        assert!(
            result.status.success(),
            "{}",
            String::from_utf8_lossy(&result.stderr)
        );
    }
    fn admin(&self) -> Client {
        postgres::Config::new()
            .host_path(&self.socket)
            .port(5432)
            .dbname("postgres")
            .connect(NoTls)
            .unwrap()
    }
    fn authority(&self, repository: &str) -> PublicationAuthority {
        authority(&self.socket, repository)
    }
}
impl Drop for Cluster {
    fn drop(&mut self) {
        let _ = Command::new(format!("{PG}/pg_ctl"))
            .arg("-D")
            .arg(self.directory.path().join("data"))
            .args(["-w", "-t", "15", "-m", "fast", "stop"])
            .output();
    }
}
fn authority(socket: &Path, repository: &str) -> PublicationAuthority {
    PublicationAuthority::connect_local(
        socket,
        5432,
        "postgres",
        RepositoryScope {
            trust_domain: "team".into(),
            repository: repository.into(),
        },
    )
    .unwrap()
}
fn reserve(id: &str, root: &str) -> ReserveRequest {
    ReserveRequest {
        generation: 1,
        publication: id.into(),
        root: root.into(),
        byte_limit: 1_000_000,
        object_limit: 1_000,
        lease_seconds: 60,
    }
}
fn complete(id: &str, manifest: &str) -> CompleteRequest {
    CompleteRequest {
        generation: 1,
        publication: id.into(),
        operation: "same-operation".into(),
        manifest_digest: manifest.into(),
    }
}

// A write can become durable while its reply is lost. The retry must verify
// existing identity; the failed call cannot produce a qualification.
struct LostReplyBackend<'a> {
    inner: &'a LocalBackend,
    fail: AtomicBool,
}
impl ImmutableBackend for LostReplyBackend<'_> {
    fn prepare_publication(&self, key: &ObjectKey, incarnation: &str) -> agentscm::Result<()> {
        self.inner.prepare_publication(key, incarnation)
    }
    fn put_if_absent(
        &self,
        key: &ObjectKey,
        incarnation: &str,
        bytes: &[u8],
    ) -> agentscm::Result<PutOutcome> {
        let result = self.inner.put_if_absent(key, incarnation, bytes)?;
        if self.fail.swap(false, Ordering::SeqCst) {
            return Err(agentscm::Error::Invalid("injected lost write reply".into()));
        }
        Ok(result)
    }
    fn get_verified(&self, key: &ObjectKey, incarnation: &str) -> agentscm::Result<Vec<u8>> {
        self.inner.get_verified(key, incarnation)
    }
    fn inspect_verified(
        &self,
        key: &ObjectKey,
        incarnation: &str,
    ) -> agentscm::Result<ObjectMetadata> {
        self.inner.inspect_verified(key, incarnation)
    }
    fn get_range_verified(
        &self,
        key: &ObjectKey,
        incarnation: &str,
        offset: u64,
        length: u64,
    ) -> agentscm::Result<Vec<u8>> {
        self.inner
            .get_range_verified(key, incarnation, offset, length)
    }
}

#[test]
#[ignore = "requires installed PG18; creates only an isolated local cluster"]
fn pg18_publication_lifecycle_concurrency_restart_and_failure() {
    let cluster = Cluster::new();
    let fixtures = tempfile::tempdir().unwrap();
    let engine = Engine::open(fixtures.path().join("source")).unwrap();
    let root = engine
        .checkpoint(
            None,
            &[FileChange {
                path: "src/main.rs".into(),
                content: Some(b"fn main() {}".to_vec()),
            }],
        )
        .unwrap();
    let backend = LocalBackend::open(fixtures.path().join("backend")).unwrap();
    let mut a = cluster.authority("repo-a");
    a.register(1, 3_000_000, 3_000).unwrap();
    assert!(a.register(1, 3_000_000, 3_000).is_err());
    let req = reserve("pub", &root);
    a.reserve(&req).unwrap();
    a.reserve(&req).unwrap(); // no duplicate allocation
    let mut wrong = req.clone();
    wrong.root = "0".repeat(64);
    assert!(matches!(a.reserve(&wrong), Err(TeamError::Conflict(_))));
    assert!(a.complete(&complete("pub", &"0".repeat(64))).is_err());
    let manifest = a
        .upload_and_qualify(1, "pub", &engine, &backend, &fixtures.path().join("offer"))
        .unwrap();
    let request = complete("pub", &manifest);
    // A lost qualification reply followed by a retry with unavailable source
    // and occupied scratch must preserve the stored qualification.
    let empty = Engine::open(fixtures.path().join("empty-source")).unwrap();
    assert_eq!(
        a.upload_and_qualify(1, "pub", &empty, &backend, &fixtures.path().join("offer"))
            .unwrap(),
        manifest
    );

    // Atomic root, outcome, event and outbox. Force the last insert to fail.
    let mut admin = cluster.admin();
    admin.batch_execute("CREATE FUNCTION reject_outbox() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RAISE EXCEPTION 'injected outbox failure'; END $$; CREATE TRIGGER reject_outbox BEFORE INSERT ON team_outbox FOR EACH ROW EXECUTE FUNCTION reject_outbox();").unwrap();
    assert!(a.complete(&request).is_err());
    for table in ["team_roots", "team_outcomes", "team_events", "team_outbox"] {
        let count: i64 = admin
            .query_one(&format!("SELECT count(*) FROM {table}"), &[])
            .unwrap()
            .get(0);
        assert_eq!(count, 0, "{table} leaked a partial completion");
    }
    let state: String = admin
        .query_one(
            "SELECT state FROM team_publications WHERE repository='repo-a' AND publication='pub'",
            &[],
        )
        .unwrap()
        .get(0);
    assert_eq!(state, "qualifying");
    admin
        .batch_execute("DROP TRIGGER reject_outbox ON team_outbox; DROP FUNCTION reject_outbox();")
        .unwrap();

    // Two independent connections race one idempotent completion. One durable
    // transition; both receive the identical original outcome.
    let barrier = Arc::new(Barrier::new(2));
    let handles: Vec<_> = (0..2)
        .map(|_| {
            let socket = cluster.socket.clone();
            let request = request.clone();
            let barrier = barrier.clone();
            std::thread::spawn(move || {
                let mut peer = authority(&socket, "repo-a");
                barrier.wait();
                peer.complete(&request).unwrap()
            })
        })
        .collect();
    let outcomes: Vec<_> = handles.into_iter().map(|h| h.join().unwrap()).collect();
    assert_eq!(outcomes[0], outcomes[1]);
    assert_eq!(outcomes[0].sequence, 1);
    assert_eq!(a.complete(&request).unwrap(), outcomes[0]);
    let mut collision = request.clone();
    collision.manifest_digest = "0".repeat(64);
    assert!(matches!(
        a.complete(&collision),
        Err(TeamError::Conflict(_))
    ));
    assert!(a.abandon(1, "pub").is_err());

    // Identical client IDs in another repository do not return repo-a's result.
    let mut b = cluster.authority("repo-b");
    b.register(1, 3_000_000, 3_000).unwrap();
    assert!(b.complete(&request).is_err());
    b.reserve(&req).unwrap();
    let manifest_b = b
        .upload_and_qualify(
            1,
            "pub",
            &engine,
            &backend,
            &fixtures.path().join("offer-b"),
        )
        .unwrap();
    let outcome_b = b.complete(&complete("pub", &manifest_b)).unwrap();
    let mut second_operation = request.clone();
    second_operation.operation = "different-operation".into();
    assert!(a.complete(&second_operation).is_err());
    assert_eq!(outcome_b.scope.repository, "repo-b");
    assert_ne!(outcome_b, outcomes[0]);

    // A durable write with a lost reply never qualifies, and resumed upload
    // verifies the already-present incarnation before atomic completion.
    let mut c = cluster.authority("repo-c");
    c.register(1, 3_000_000, 3_000).unwrap();
    c.reserve(&req).unwrap();
    let failing = LostReplyBackend {
        inner: &backend,
        fail: AtomicBool::new(true),
    };
    assert!(
        c.upload_and_qualify(
            1,
            "pub",
            &engine,
            &failing,
            &fixtures.path().join("lost-offer")
        )
        .is_err()
    );
    assert!(c.complete(&request).is_err());
    let resumed = c
        .upload_and_qualify(
            1,
            "pub",
            &engine,
            &backend,
            &fixtures.path().join("resumed-offer"),
        )
        .unwrap();
    c.complete(&complete("pub", &resumed)).unwrap();
    // Once uploading begins, failed writes may remain. Abandonment must not
    // refund their budget and permit repeated physical-storage growth.
    let mut orphan_request = reserve("orphan", &root);
    orphan_request.byte_limit = 2_000_000;
    c.reserve(&orphan_request).unwrap();
    let failing = LostReplyBackend {
        inner: &backend,
        fail: AtomicBool::new(true),
    };
    assert!(
        c.upload_and_qualify(
            1,
            "orphan",
            &engine,
            &failing,
            &fixtures.path().join("orphan-offer")
        )
        .is_err()
    );
    c.abandon(1, "orphan").unwrap();
    c.abandon(1, "orphan").unwrap();
    assert!(c.reserve(&reserve("replacement", &root)).is_err());
    let charged: i64 = admin
        .query_one(
            "SELECT allocated_bytes FROM team_repositories WHERE repository='repo-c'",
            &[],
        )
        .unwrap()
        .get(0);
    assert_eq!(charged, 3_000_000);
    drop(c);

    // Expiry comes from database clock; abandonment releases quota once.
    let exp = reserve("expired", &root);
    a.reserve(&exp).unwrap();
    let exp_manifest = a
        .upload_and_qualify(
            1,
            "expired",
            &engine,
            &backend,
            &fixtures.path().join("exp-offer"),
        )
        .unwrap();
    admin.execute("UPDATE team_publications SET expires_at=clock_timestamp()-interval '1 second' WHERE repository='repo-a' AND publication='expired'", &[]).unwrap();
    assert!(matches!(
        a.upload_and_qualify(
            1,
            "expired",
            &engine,
            &backend,
            &fixtures.path().join("expired-offer")
        ),
        Err(TeamError::Expired)
    ));
    let mut exp_complete = complete("expired", &exp_manifest);
    exp_complete.operation = "expired-operation".into();
    assert!(matches!(a.complete(&exp_complete), Err(TeamError::Expired)));
    a.abandon(1, "expired").unwrap();
    a.abandon(1, "expired").unwrap();
    assert!(a.reserve(&exp).is_err());
    let allocated: i64 = admin
        .query_one(
            "SELECT allocated_bytes FROM team_repositories WHERE repository='repo-a'",
            &[],
        )
        .unwrap()
        .get(0);
    assert_eq!(allocated, req.byte_limit + exp.byte_limit);
    let never_uploaded = reserve("never-uploaded", &root);
    a.reserve(&never_uploaded).unwrap();
    a.abandon(1, "never-uploaded").unwrap();
    a.abandon(1, "never-uploaded").unwrap();
    let charged: i64 = admin
        .query_one(
            "SELECT allocated_bytes FROM team_repositories WHERE repository='repo-a'",
            &[],
        )
        .unwrap()
        .get(0);
    assert_eq!(charged, req.byte_limit + exp.byte_limit);

    // Missing source closure and reservation overflow cannot qualify a root.
    let mut failures = cluster.authority("repo-failures");
    failures.register(1, 3_000_000, 3_000).unwrap();
    let mut tiny = reserve("tiny", &root);
    tiny.byte_limit = 1;
    failures.reserve(&tiny).unwrap();
    assert!(
        failures
            .upload_and_qualify(
                1,
                "tiny",
                &engine,
                &backend,
                &fixtures.path().join("tiny-offer")
            )
            .is_err()
    );
    assert!(failures.complete(&complete("tiny", &manifest)).is_err());
    failures.abandon(1, "tiny").unwrap();
    let missing = reserve("missing", &"0".repeat(64));
    failures.reserve(&missing).unwrap();
    assert!(
        failures
            .upload_and_qualify(
                1,
                "missing",
                &engine,
                &backend,
                &fixtures.path().join("missing-offer")
            )
            .is_err()
    );
    assert!(failures.complete(&complete("missing", &manifest)).is_err());
    failures.abandon(1, "missing").unwrap();
    let counters = admin.query_one("SELECT allocated_bytes,allocated_objects FROM team_repositories WHERE repository='repo-failures'", &[]).unwrap();
    assert_eq!(
        counters.get::<_, i64>(0),
        0,
        "zero-write failures refund bytes"
    );
    assert_eq!(
        counters.get::<_, i64>(1),
        0,
        "zero-write failures refund objects"
    );
    failures
        .reserve(&reserve("fresh-after-failure", &root))
        .unwrap();
    let mut large = reserve("large", &root);
    large.byte_limit = 3_000_000;
    assert!(a.reserve(&large).is_err());
    let mut stale = req.clone();
    stale.generation = 2;
    assert!(a.reserve(&stale).is_err());

    // A fence established externally is simulated administratively. This is
    // mutation/retry semantics, not proof of production old-writer fencing.
    admin
        .execute(
            "UPDATE team_repositories SET generation=2 WHERE repository='repo-a'",
            &[],
        )
        .unwrap();
    assert!(a.reserve(&reserve("stale", &root)).is_err());
    assert_eq!(a.complete(&request).unwrap(), outcomes[0]);
    let mut lost = request.clone();
    lost.operation = "lost-before-backup".into();
    assert!(matches!(a.complete(&lost), Err(TeamError::Indeterminate)));
    drop(failures);
    drop(a);
    drop(b);
    drop(admin);
    cluster.control("stop");
    cluster.start();
    let mut reopened = cluster.authority("repo-a");
    assert_eq!(reopened.complete(&request).unwrap(), outcomes[0]);
    assert!(matches!(
        reopened.complete(&lost),
        Err(TeamError::Indeterminate)
    ));
    let mut admin = cluster.admin();
    for table in ["team_roots", "team_outcomes", "team_events", "team_outbox"] {
        let count: i64 = admin
            .query_one(
                &format!("SELECT count(*) FROM {table} WHERE repository='repo-a'"),
                &[],
            )
            .unwrap()
            .get(0);
        assert_eq!(count, 1);
    }
}

#[test]
fn reject_invalid_scope_before_any_database_connection() {
    let scope = RepositoryScope {
        trust_domain: "../escape".into(),
        repository: "repo".into(),
    };
    assert!(
        PublicationAuthority::connect_local(Path::new("/does-not-exist"), 5432, "postgres", scope)
            .is_err()
    );
}

#[test]
#[ignore = "requires installed PG18; creates only an isolated local cluster"]
fn pg18_r2_closure_is_rejected_before_backend_writes_and_remains_refundable() {
    let cluster = Cluster::new();
    let fixtures = tempfile::tempdir().unwrap();
    let source = Engine::open(fixtures.path().join("source")).unwrap();
    let base = source.checkpoint(None, &[]).unwrap();
    let child = source.checkpoint(Some(&base), &[]).unwrap();
    let r2 = source.checkpoint_with_parents(&[base, child], &[]).unwrap();
    let r1_with_r2_parent = source.checkpoint(Some(&r2), &[]).unwrap();
    assert_eq!(source.load_revision(&r1_with_r2_parent).unwrap().version, 1);
    let backend_path = fixtures.path().join("backend");
    let backend = LocalBackend::open(&backend_path).unwrap();
    let mut authority = cluster.authority("profile-rejection");
    authority.register(1, 3_000_000, 3_000).unwrap();
    let mut admin = cluster.admin();
    for (publication, root) in [("r2", r2), ("r1-r2-ancestry", r1_with_r2_parent)] {
        authority.reserve(&reserve(publication, &root)).unwrap();
        let result = authority.upload_and_qualify(
            1,
            publication,
            &source,
            &backend,
            &fixtures.path().join(format!("offer-{publication}")),
        );
        assert!(
            matches!(result, Err(TeamError::Storage(agentscm::Error::Invalid(message))) if message.contains("source profile v1"))
        );
        assert!(
            std::fs::read_dir(&backend_path).unwrap().next().is_none(),
            "profile rejection must precede namespace preparation and all object writes"
        );
        let row = admin
            .query_one(
                "SELECT state FROM team_publications WHERE repository=$1 AND publication=$2",
                &[&"profile-rejection", &publication],
            )
            .unwrap();
        assert_eq!(row.get::<_, String>(0), "reserved");
        assert!(
            authority
                .complete(&complete(publication, &"0".repeat(64)))
                .is_err()
        );
        authority.abandon(1, publication).unwrap();
        let counters = admin.query_one("SELECT allocated_bytes, allocated_objects FROM team_repositories WHERE repository=$1", &[&"profile-rejection"]).unwrap();
        assert_eq!(counters.get::<_, i64>(0), 0);
        assert_eq!(counters.get::<_, i64>(1), 0);
        let roots: i64 = admin
            .query_one(
                "SELECT count(*) FROM team_roots WHERE repository=$1",
                &[&"profile-rejection"],
            )
            .unwrap()
            .get(0);
        assert_eq!(roots, 0);
    }
}
