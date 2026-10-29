use crate::team_pg;

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::process::Command;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc;
use std::thread::{self, JoinHandle};

use agentscm::backend::{ImmutableBackend, LocalBackend, ObjectKey, ObjectMetadata, PutOutcome};
use agentscm::model::FileChange;
use agentscm::store::Engine;
use agentscm::sync::SourceOffer;
use agentscm::team::TeamError;
use agentscm::team::auth::{Credentials, Role, generate_secret};
use agentscm::team::http;
use agentscm::team::publication::{CompleteRequest, RepositoryScope, ReserveRequest};
use agentscm::team::service::{
    BootstrapRequest, DatabaseConfig, ProvisionRequest, RetireAccessRequest, RetirementOutcome,
    TeamConfig, TeamService, operator_bootstrap, operator_retire_access,
};
use axum::Router;

use team_pg::Cluster;

const ORIGIN: &str = "https://team.example";

fn scope(repository: &str) -> RepositoryScope {
    RepositoryScope {
        trust_domain: "team".into(),
        repository: repository.into(),
    }
}

fn bootstrap_request(repository: &str) -> BootstrapRequest {
    BootstrapRequest {
        scope: scope(repository),
        authority_generation: 1,
        operation: format!("bootstrap-{repository}"),
        max_bytes: 8_000_000,
        max_objects: 8_000,
        principal: "admin".into(),
        roles: vec![Role::Reader, Role::Contributor, Role::Administrator],
    }
}

fn bootstrap(config: &DatabaseConfig, repository: &str, credential: &str) -> BootstrapRequest {
    let request = bootstrap_request(repository);
    operator_bootstrap(config, &request, credential).unwrap();
    request
}

fn service(config: &DatabaseConfig, repository: &str, credential: &str) -> TeamService {
    TeamService::connect(
        config,
        scope(repository),
        Credentials::bearer(credential).unwrap(),
    )
    .unwrap()
}

fn reservation(publication: &str, root: &str) -> ReserveRequest {
    ReserveRequest {
        generation: 1,
        publication: publication.into(),
        root: root.into(),
        byte_limit: 2_000_000,
        object_limit: 2_000,
        lease_seconds: 120,
    }
}

struct TestServer {
    address: SocketAddr,
    shutdown: Option<tokio::sync::oneshot::Sender<()>>,
    thread: Option<JoinHandle<()>>,
}

impl TestServer {
    fn start(app: Router) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        listener.set_nonblocking(true).unwrap();
        let (shutdown, shutdown_rx) = tokio::sync::oneshot::channel();
        let (ready_tx, ready_rx) = mpsc::channel();
        let thread = thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .enable_all()
                .build()
                .unwrap();
            runtime.block_on(async move {
                let listener = tokio::net::TcpListener::from_std(listener).unwrap();
                ready_tx.send(()).unwrap();
                axum::serve(listener, app)
                    .with_graceful_shutdown(async move {
                        let _ = shutdown_rx.await;
                    })
                    .await
                    .unwrap();
            });
        });
        ready_rx.recv().unwrap();
        Self {
            address,
            shutdown: Some(shutdown),
            thread: Some(thread),
        }
    }

    fn session_status(&self, repository: &str, session_token: &str) -> u16 {
        let mut stream = TcpStream::connect(self.address).unwrap();
        stream
            .set_read_timeout(Some(std::time::Duration::from_secs(10)))
            .unwrap();
        write!(
            stream,
            "GET /v1/team/session?trust_domain=team&repository={repository} HTTP/1.1\r\nHost: {}\r\nConnection: close\r\nCookie: __Host-agentscm-session={session_token}\r\nContent-Length: 0\r\n\r\n",
            self.address
        )
        .unwrap();
        stream.flush().unwrap();
        let mut raw = Vec::new();
        stream.read_to_end(&mut raw).unwrap();
        let head_end = raw
            .windows(4)
            .position(|window| window == b"\r\n\r\n")
            .unwrap();
        std::str::from_utf8(&raw[..head_end])
            .unwrap()
            .lines()
            .next()
            .unwrap()
            .split_whitespace()
            .nth(1)
            .unwrap()
            .parse()
            .unwrap()
    }
}

impl Drop for TestServer {
    fn drop(&mut self) {
        if let Some(shutdown) = self.shutdown.take() {
            let _ = shutdown.send(());
        }
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn restart_owned_cluster(cluster: &Cluster) {
    let binaries = std::path::PathBuf::from(
        std::env::var_os("AGENTSCM_PG_BIN_DIR")
            .expect("set AGENTSCM_PG_BIN_DIR to the owned PostgreSQL18 binaries"),
    );
    assert!(binaries.is_absolute());
    let data = cluster.directory.path().join("data");
    let stop = Command::new(binaries.join("pg_ctl"))
        .arg("-D")
        .arg(&data)
        .args(["-w", "-t", "15", "-m", "fast", "stop"])
        .output()
        .unwrap();
    assert!(
        stop.status.success(),
        "owned cluster stop failed: {}",
        String::from_utf8_lossy(&stop.stderr)
    );
    let options = format!(
        "-k {} -p {} -c listen_addresses='' -c max_connections=16",
        cluster.config.socket.display(),
        cluster.config.port
    );
    let start = Command::new(binaries.join("pg_ctl"))
        .arg("-D")
        .arg(&data)
        .arg("-l")
        .arg(cluster.directory.path().join("postgres.log"))
        .args(["-w", "-t", "15", "-o", &options, "start"])
        .output()
        .unwrap();
    assert!(
        start.status.success(),
        "owned cluster restart failed: {}",
        String::from_utf8_lossy(&start.stderr)
    );
}

#[test]
#[ignore = "requires explicit AGENTSCM_PG_BIN_DIR; fresh isolated PostgreSQL18 only"]
fn pg18_terminal_retirement_survives_limits_restart_and_generation_change() {
    let cluster = Cluster::new();
    let repository = "retired";
    let admin_secret = generate_secret().unwrap();
    let bootstrap = bootstrap(&cluster.config, repository, admin_secret.expose());
    let mut admin = service(&cluster.config, repository, admin_secret.expose());
    let session = TeamService::login(
        &cluster.config,
        scope(repository),
        admin_secret.expose(),
        1,
        120,
    )
    .unwrap();

    // Establish a real retained source/history closure and completed root. Its
    // authority rows and immutable bytes must survive access retirement.
    let fixture = tempfile::tempdir().unwrap();
    let source = Engine::open(fixture.path().join("source")).unwrap();
    let base = source
        .checkpoint(
            None,
            &[FileChange {
                path: "src/lib.rs".into(),
                content: Some(b"pub fn value() -> u8 { 1 }".to_vec()),
            }],
        )
        .unwrap();
    let root = source
        .checkpoint(
            Some(&base),
            &[FileChange {
                path: "src/lib.rs".into(),
                content: Some(b"pub fn value() -> u8 { 2 }".to_vec()),
            }],
        )
        .unwrap();
    let backend_path = fixture.path().join("objects");
    let backend = LocalBackend::open(&backend_path).unwrap();
    let offer_path = fixture.path().join("retained-offer");
    let mut controller = cluster.config.open(scope(repository)).unwrap();
    let reserve = reservation("retained", &root);
    controller.reserve(&reserve).unwrap();
    let manifest = controller
        .upload_and_qualify(1, "retained", &source, &backend, &offer_path)
        .unwrap();
    let completion = CompleteRequest {
        generation: 1,
        publication: "retained".into(),
        operation: "complete-retained".into(),
        manifest_digest: manifest,
    };
    let completed = controller.complete(&completion).unwrap();
    let offer: SourceOffer = serde_json::from_slice(&std::fs::read(&offer_path).unwrap()).unwrap();
    assert_eq!(offer.roots, vec![root.clone()]);

    let mut sql = cluster.admin();
    // Synthetic count-only padding exercises both finite boundaries. These
    // rows are not audit evidence, execution proof, or publication fixtures.
    sql.execute(
        "INSERT INTO team_outcomes( \
           trust_domain,repository,generation,operation,request_digest,outcome,kind) \
         SELECT 'team',$1,1,'retirement-padding-'||n,'padding','{}','identity' \
         FROM generate_series(1,9998) n",
        &[&repository],
    )
    .unwrap();
    sql.execute(
        "INSERT INTO team_principals( \
           trust_domain,repository,principal,credential_digest,version,enabled) \
         SELECT 'team',$1,'padding-'||n,lpad(to_hex(n),64,'0'),1,FALSE \
         FROM generate_series(1,127) n",
        &[&repository],
    )
    .unwrap();
    let before = sql
        .query_one(
            "SELECT \
               (SELECT count(*) FROM team_outcomes \
                 WHERE trust_domain='team' AND repository=$1 \
                   AND kind IN ('identity','retirement')), \
               (SELECT count(*) FROM team_principals \
                 WHERE trust_domain='team' AND repository=$1), \
               (SELECT count(*) FROM team_principals \
                 WHERE trust_domain='team' AND repository=$1 AND enabled)",
            &[&repository],
        )
        .unwrap();
    assert_eq!(before.get::<_, i64>(0), 9_999);
    assert_eq!(before.get::<_, i64>(1), 128);
    assert_eq!(before.get::<_, i64>(2), 1);

    let retirement = RetireAccessRequest {
        scope: scope(repository),
        authority_generation: 1,
        operation: "retire-access".into(),
    };
    let retired = operator_retire_access(&cluster.config, &retirement).unwrap();
    assert_eq!(retired.outcome.version, 1);
    assert_eq!(retired.outcome.action, "retire_access");
    assert_eq!(retired.outcome.actor, None);
    assert!(retired.outcome.retired);
    assert_eq!(retired.outcome.sequence, completed.sequence + 1);
    assert!(!retired.historical);
    assert_eq!(retired.current_authority_generation, 1);
    assert!(retired.currently_retired);

    let state = sql
        .query_one(
            "SELECT r.retired,r.sequence, \
               (SELECT count(*) FROM team_principals p \
                 WHERE p.trust_domain=r.trust_domain AND p.repository=r.repository), \
               (SELECT count(*) FROM team_principals p \
                 WHERE p.trust_domain=r.trust_domain AND p.repository=r.repository AND p.enabled), \
               (SELECT count(*) FROM team_roles x \
                 WHERE x.trust_domain=r.trust_domain AND x.repository=r.repository), \
               (SELECT count(*) FROM team_sessions s \
                 WHERE s.trust_domain=r.trust_domain AND s.repository=r.repository AND NOT s.revoked), \
               (SELECT count(*) FROM team_outcomes o \
                 WHERE o.trust_domain=r.trust_domain AND o.repository=r.repository \
                   AND o.kind IN ('identity','retirement')), \
               (SELECT count(*) FROM team_outcomes o \
                 WHERE o.trust_domain=r.trust_domain AND o.repository=r.repository \
                   AND o.kind='retirement') \
             FROM team_repositories r \
             WHERE r.trust_domain='team' AND r.repository=$1",
            &[&repository],
        )
        .unwrap();
    assert!(state.get::<_, bool>(0));
    assert_eq!(state.get::<_, i64>(1), retired.outcome.sequence);
    assert_eq!(state.get::<_, i64>(2), 128);
    assert_eq!(state.get::<_, i64>(3), 0);
    assert_eq!(state.get::<_, i64>(4), 0);
    assert_eq!(state.get::<_, i64>(5), 0);
    assert_eq!(state.get::<_, i64>(6), 10_000);
    assert_eq!(state.get::<_, i64>(7), 1);
    let retained_record = sql
        .query_one(
            "SELECT o.outcome,e.outcome,b.payload,o.kind,e.kind,b.kind \
             FROM team_outcomes o \
             JOIN team_events e ON e.trust_domain=o.trust_domain \
               AND e.repository=o.repository AND e.sequence=$2 \
             JOIN team_outbox b ON b.trust_domain=o.trust_domain \
               AND b.repository=o.repository AND b.sequence=e.sequence \
             WHERE o.trust_domain='team' AND o.repository=$1 \
               AND o.generation=1 AND o.operation='retire-access'",
            &[&repository, &retired.outcome.sequence],
        )
        .unwrap();
    let encoded = retained_record.get::<_, String>(0);
    assert_eq!(retained_record.get::<_, String>(1), encoded);
    assert_eq!(retained_record.get::<_, String>(2), encoded);
    for index in 3..6 {
        assert_eq!(retained_record.get::<_, String>(index), "retirement");
    }
    assert_eq!(
        serde_json::from_str::<RetirementOutcome>(&encoded).unwrap(),
        retired.outcome
    );

    // Credentials, sessions and already-open handles all lose access. Every
    // ordinary controller fence rejects retirement before retained retries or
    // publication/source disclosure can occur.
    assert!(matches!(admin.session(), Err(TeamError::Unauthorized)));
    assert!(matches!(
        TeamService::connect(
            &cluster.config,
            scope(repository),
            Credentials::bearer(admin_secret.expose()).unwrap()
        ),
        Err(TeamError::Unauthorized)
    ));
    let app = http::router(TeamConfig {
        database: cluster.config.clone(),
        objects: backend_path.clone(),
        origin: ORIGIN.into(),
        bind: "127.0.0.1:0".parse().unwrap(),
        max_concurrency: 1,
    })
    .unwrap();
    let server = TestServer::start(app);
    assert_eq!(
        server.session_status(repository, &session.session_token),
        401
    );
    assert!(matches!(
        controller.reserve(&reservation("new", &root)),
        Err(TeamError::Unauthorized)
    ));
    assert!(matches!(
        controller.upload_and_qualify(
            1,
            "retained",
            &source,
            &backend,
            &fixture.path().join("blocked-offer")
        ),
        Err(TeamError::Unauthorized)
    ));
    assert!(matches!(
        controller.complete(&completion),
        Err(TeamError::Unauthorized)
    ));
    assert!(matches!(
        controller.abandon(1, "retained"),
        Err(TeamError::Unauthorized)
    ));
    assert!(matches!(
        operator_bootstrap(&cluster.config, &bootstrap, admin_secret.expose()),
        Err(TeamError::Unauthorized)
    ));

    let root_row = sql
        .query_one(
            "SELECT p.state,p.manifest IS NOT NULL,r.root \
             FROM team_publications p JOIN team_roots r \
             USING(trust_domain,repository,generation,publication) \
             WHERE p.trust_domain='team' AND p.repository=$1 \
               AND p.generation=1 AND p.publication='retained'",
            &[&repository],
        )
        .unwrap();
    assert_eq!(root_row.get::<_, String>(0), "complete");
    assert!(root_row.get::<_, bool>(1));
    assert_eq!(root_row.get::<_, String>(2), root);
    for object in &offer.objects {
        let bytes = backend
            .get_verified(
                &ObjectKey::new(
                    "team",
                    repository,
                    offer.profile.as_str(),
                    object.id.as_str(),
                )
                .unwrap(),
                "1-retained",
            )
            .unwrap();
        assert_eq!(bytes.len() as u64, object.encoded_bytes);
    }

    assert_eq!(
        operator_retire_access(&cluster.config, &retirement).unwrap(),
        retired
    );
    let collision = RetireAccessRequest {
        scope: scope(repository),
        authority_generation: 1,
        operation: "another-retirement".into(),
    };
    assert!(matches!(
        operator_retire_access(&cluster.config, &collision),
        Err(TeamError::Conflict(_))
    ));
    // Raw trusted-state diagnostic: a same-era receipt cannot stand in for a
    // current marker. This temporary edit is not a supported reopen operation.
    sql.execute(
        "UPDATE team_repositories SET retired=FALSE \
         WHERE trust_domain='team' AND repository=$1",
        &[&repository],
    )
    .unwrap();
    assert!(matches!(
        operator_retire_access(&cluster.config, &retirement),
        Err(TeamError::Indeterminate)
    ));
    sql.execute(
        "UPDATE team_repositories SET retired=TRUE \
         WHERE trust_domain='team' AND repository=$1",
        &[&repository],
    )
    .unwrap();

    drop(server);
    drop(admin);
    drop(controller);
    drop(sql);
    restart_owned_cluster(&cluster);

    assert_eq!(
        operator_retire_access(&cluster.config, &retirement).unwrap(),
        retired
    );
    let mut sql = cluster.admin();
    sql.execute(
        "UPDATE team_repositories SET generation=2 \
         WHERE trust_domain='team' AND repository=$1",
        &[&repository],
    )
    .unwrap();
    let historical = operator_retire_access(&cluster.config, &retirement).unwrap();
    assert_eq!(historical.outcome, retired.outcome);
    assert!(historical.historical);
    assert_eq!(historical.current_authority_generation, 2);
    assert!(historical.currently_retired);
    // `historical` describes a retained old result. It is not proof that the
    // old operation executed in the current generation or a new current action.
    assert_eq!(historical.outcome.authority_generation, 1);
    // A later-era raw marker=false state may return only the historical
    // receipt, with current marker state reported separately. It is not proof
    // of a current retirement or supported restoration.
    sql.execute(
        "UPDATE team_repositories SET retired=FALSE \
         WHERE trust_domain='team' AND repository=$1",
        &[&repository],
    )
    .unwrap();
    let historical_unmarked = operator_retire_access(&cluster.config, &retirement).unwrap();
    assert_eq!(historical_unmarked.outcome, retired.outcome);
    assert!(historical_unmarked.historical);
    assert_eq!(historical_unmarked.current_authority_generation, 2);
    assert!(!historical_unmarked.currently_retired);
    sql.execute(
        "UPDATE team_repositories SET retired=TRUE \
         WHERE trust_domain='team' AND repository=$1",
        &[&repository],
    )
    .unwrap();
    let missing_old = RetireAccessRequest {
        scope: scope(repository),
        authority_generation: 1,
        operation: "missing-old-result".into(),
    };
    assert!(matches!(
        operator_retire_access(&cluster.config, &missing_old),
        Err(TeamError::Indeterminate)
    ));
    let current_new = RetireAccessRequest {
        scope: scope(repository),
        authority_generation: 2,
        operation: "current-new-retirement".into(),
    };
    assert!(matches!(
        operator_retire_access(&cluster.config, &current_new),
        Err(TeamError::Conflict(_))
    ));
    let future = RetireAccessRequest {
        scope: scope(repository),
        authority_generation: 3,
        operation: "future-retirement".into(),
    };
    assert!(matches!(
        operator_retire_access(&cluster.config, &future),
        Err(TeamError::Conflict(_))
    ));
    let regressed = RetireAccessRequest {
        scope: scope("retirement-regressed-wrapper"),
        authority_generation: 2,
        operation: "completed-era2-receipt".into(),
    };
    // Retain an actual completed era2 result, then simulate authority
    // regression. Only the raw generation edit is a diagnostic fixture;
    // no request digest, outcome or audit evidence is fabricated here.
    let regression_secret = generate_secret().unwrap();
    operator_bootstrap(
        &cluster.config,
        &bootstrap_request("retirement-regressed-wrapper"),
        regression_secret.expose(),
    )
    .unwrap();
    sql.execute("UPDATE team_repositories SET generation=2 WHERE trust_domain='team' AND repository='retirement-regressed-wrapper'", &[]).unwrap();
    let completed_era2 = operator_retire_access(&cluster.config, &regressed).unwrap();
    assert_eq!(completed_era2.outcome.authority_generation, 2);
    sql.execute("UPDATE team_repositories SET generation=1 WHERE trust_domain='team' AND repository='retirement-regressed-wrapper'", &[]).unwrap();
    assert!(matches!(
        operator_retire_access(&cluster.config, &regressed),
        Err(TeamError::Indeterminate)
    ));
}

#[test]
#[ignore = "requires explicit AGENTSCM_PG_BIN_DIR; fresh isolated PostgreSQL18 only"]
fn pg18_retirement_outbox_failure_rolls_back_every_access_change() {
    let cluster = Cluster::new();
    let repository = "retirement-rollback";
    let admin_secret = generate_secret().unwrap();
    bootstrap(&cluster.config, repository, admin_secret.expose());
    let mut handle = service(&cluster.config, repository, admin_secret.expose());
    let reader_secret = generate_secret().unwrap();
    let contributor_secret = generate_secret().unwrap();
    handle
        .provision(&ProvisionRequest {
            authority_generation: 1,
            operation: "provision-reader".into(),
            principal: "reader".into(),
            credential: reader_secret.expose().into(),
            roles: vec![Role::Reader],
        })
        .unwrap();
    handle
        .provision(&ProvisionRequest {
            authority_generation: 1,
            operation: "provision-contributor".into(),
            principal: "contributor".into(),
            credential: contributor_secret.expose().into(),
            roles: vec![Role::Contributor],
        })
        .unwrap();
    for credential in [
        admin_secret.expose(),
        reader_secret.expose(),
        contributor_secret.expose(),
    ] {
        TeamService::login(&cluster.config, scope(repository), credential, 1, 120).unwrap();
    }

    // Complete a real retained publication before the injected retirement.
    // The successful retry below is triggered inside its backend range read.
    let fixture = tempfile::tempdir().unwrap();
    let source = Engine::open(fixture.path().join("source")).unwrap();
    let root = source
        .checkpoint(
            None,
            &[FileChange {
                path: "read.txt".into(),
                content: Some(b"bytes read before post-I/O retirement denial".to_vec()),
            }],
        )
        .unwrap();
    let backend = LocalBackend::open(fixture.path().join("objects")).unwrap();
    let mut controller = cluster.config.open(scope(repository)).unwrap();
    controller
        .reserve(&reservation("retained-read", &root))
        .unwrap();
    let manifest = controller
        .upload_and_qualify(
            1,
            "retained-read",
            &source,
            &backend,
            &fixture.path().join("read-offer"),
        )
        .unwrap();
    controller
        .complete(&CompleteRequest {
            generation: 1,
            publication: "retained-read".into(),
            operation: "complete-retained-read".into(),
            manifest_digest: manifest,
        })
        .unwrap();

    let mut sql = cluster.admin();
    // An operation ID already retained under another kind cannot be reused for
    // retirement, even though the retirement request digest is otherwise valid.
    let cross_kind = RetireAccessRequest {
        scope: scope(repository),
        authority_generation: 1,
        operation: "provision-reader".into(),
    };
    assert!(matches!(
        operator_retire_access(&cluster.config, &cross_kind),
        Err(TeamError::Conflict(_))
    ));
    let still_current: bool = sql
        .query_one(
            "SELECT NOT retired FROM team_repositories \
             WHERE trust_domain='team' AND repository=$1",
            &[&repository],
        )
        .unwrap()
        .get(0);
    assert!(still_current);

    sql.batch_execute(
        "CREATE FUNCTION reject_retirement_outbox() RETURNS trigger LANGUAGE plpgsql AS $$ \
         BEGIN \
           IF NEW.kind='retirement' THEN RAISE EXCEPTION 'injected retirement outbox failure'; END IF; \
           RETURN NEW; \
         END $$; \
         CREATE TRIGGER reject_retirement_outbox \
         BEFORE INSERT ON team_outbox FOR EACH ROW \
         EXECUTE FUNCTION reject_retirement_outbox();",
    )
    .unwrap();
    let request = RetireAccessRequest {
        scope: scope(repository),
        authority_generation: 1,
        operation: "retirement-must-rollback".into(),
    };
    assert!(operator_retire_access(&cluster.config, &request).is_err());

    let state = sql
        .query_one(
            "SELECT r.retired,r.sequence, \
               (SELECT count(*) FROM team_principals p \
                 WHERE p.trust_domain=r.trust_domain AND p.repository=r.repository AND p.enabled), \
               (SELECT min(version) FROM team_principals p \
                 WHERE p.trust_domain=r.trust_domain AND p.repository=r.repository), \
               (SELECT max(version) FROM team_principals p \
                 WHERE p.trust_domain=r.trust_domain AND p.repository=r.repository), \
               (SELECT count(*) FROM team_roles x \
                 WHERE x.trust_domain=r.trust_domain AND x.repository=r.repository), \
               (SELECT count(*) FROM team_sessions s \
                 WHERE s.trust_domain=r.trust_domain AND s.repository=r.repository AND NOT s.revoked), \
               (SELECT count(*) FROM team_outcomes o \
                 WHERE o.trust_domain=r.trust_domain AND o.repository=r.repository AND o.kind='retirement'), \
               (SELECT count(*) FROM team_events e \
                 WHERE e.trust_domain=r.trust_domain AND e.repository=r.repository AND e.kind='retirement'), \
               (SELECT count(*) FROM team_outbox b \
                 WHERE b.trust_domain=r.trust_domain AND b.repository=r.repository AND b.kind='retirement') \
             FROM team_repositories r \
             WHERE r.trust_domain='team' AND r.repository=$1",
            &[&repository],
        )
        .unwrap();
    assert!(!state.get::<_, bool>(0));
    assert_eq!(state.get::<_, i64>(1), 4);
    assert_eq!(state.get::<_, i64>(2), 3);
    assert_eq!(state.get::<_, i64>(3), 1);
    assert_eq!(state.get::<_, i64>(4), 1);
    assert_eq!(state.get::<_, i64>(5), 5);
    assert_eq!(state.get::<_, i64>(6), 3);
    assert_eq!(state.get::<_, i64>(7), 0);
    assert_eq!(state.get::<_, i64>(8), 0);
    assert_eq!(state.get::<_, i64>(9), 0);
    assert_eq!(handle.session().unwrap().principal.principal, "admin");
    for credential in [
        admin_secret.expose(),
        reader_secret.expose(),
        contributor_secret.expose(),
    ] {
        assert!(
            TeamService::connect(
                &cluster.config,
                scope(repository),
                Credentials::bearer(credential).unwrap()
            )
            .is_ok()
        );
    }
    sql.batch_execute(
        "DROP TRIGGER reject_retirement_outbox ON team_outbox; \
         DROP FUNCTION reject_retirement_outbox();",
    )
    .unwrap();

    let retire_during_read = RetireDuringRead {
        inner: &backend,
        config: &cluster.config,
        request: &request,
        bytes_read: AtomicUsize::new(0),
    };
    assert!(matches!(
        handle.read_object(&retire_during_read, 1, "retained-read", &root, 0, None),
        Err(TeamError::Unauthorized)
    ));
    assert!(retire_during_read.bytes_read.load(Ordering::SeqCst) > 0);
    let successful_retry = operator_retire_access(&cluster.config, &request).unwrap();
    assert_eq!(successful_retry.outcome.operation, request.operation);
    assert_eq!(successful_retry.outcome.sequence, 5);
    assert!(successful_retry.currently_retired);

    let retired = sql
        .query_one(
            "SELECT r.retired,r.sequence, \
               (SELECT count(*) FROM team_principals p \
                 WHERE p.trust_domain=r.trust_domain AND p.repository=r.repository AND p.enabled), \
               (SELECT min(version) FROM team_principals p \
                 WHERE p.trust_domain=r.trust_domain AND p.repository=r.repository), \
               (SELECT max(version) FROM team_principals p \
                 WHERE p.trust_domain=r.trust_domain AND p.repository=r.repository), \
               (SELECT count(*) FROM team_roles x \
                 WHERE x.trust_domain=r.trust_domain AND x.repository=r.repository), \
               (SELECT count(*) FROM team_sessions s \
                 WHERE s.trust_domain=r.trust_domain AND s.repository=r.repository AND NOT s.revoked), \
               (SELECT count(*) FROM team_outcomes o \
                 WHERE o.trust_domain=r.trust_domain AND o.repository=r.repository AND o.kind='retirement'), \
               (SELECT count(*) FROM team_events e \
                 WHERE e.trust_domain=r.trust_domain AND e.repository=r.repository AND e.kind='retirement'), \
               (SELECT count(*) FROM team_outbox b \
                 WHERE b.trust_domain=r.trust_domain AND b.repository=r.repository AND b.kind='retirement') \
             FROM team_repositories r \
             WHERE r.trust_domain='team' AND r.repository=$1",
            &[&repository],
        )
        .unwrap();
    assert!(retired.get::<_, bool>(0));
    assert_eq!(retired.get::<_, i64>(1), 5);
    assert_eq!(retired.get::<_, i64>(2), 0);
    assert_eq!(retired.get::<_, i64>(3), 2);
    assert_eq!(retired.get::<_, i64>(4), 2);
    assert_eq!(retired.get::<_, i64>(5), 0);
    assert_eq!(retired.get::<_, i64>(6), 0);
    assert_eq!(retired.get::<_, i64>(7), 1);
    assert_eq!(retired.get::<_, i64>(8), 1);
    assert_eq!(retired.get::<_, i64>(9), 1);
    for credential in [
        admin_secret.expose(),
        reader_secret.expose(),
        contributor_secret.expose(),
    ] {
        assert!(matches!(
            TeamService::connect(
                &cluster.config,
                scope(repository),
                Credentials::bearer(credential).unwrap()
            ),
            Err(TeamError::Unauthorized)
        ));
    }
}

struct RetireDuringRead<'a> {
    inner: &'a LocalBackend,
    config: &'a DatabaseConfig,
    request: &'a RetireAccessRequest,
    bytes_read: AtomicUsize,
}

impl ImmutableBackend for RetireDuringRead<'_> {
    fn prepare_publication(&self, key: &ObjectKey, incarnation: &str) -> agentscm::Result<()> {
        self.inner.prepare_publication(key, incarnation)
    }

    fn put_if_absent(
        &self,
        key: &ObjectKey,
        incarnation: &str,
        bytes: &[u8],
    ) -> agentscm::Result<PutOutcome> {
        self.inner.put_if_absent(key, incarnation, bytes)
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
        let bytes = self
            .inner
            .get_range_verified(key, incarnation, offset, length)?;
        self.bytes_read.store(bytes.len(), Ordering::SeqCst);
        operator_retire_access(self.config, self.request).unwrap();
        Ok(bytes)
    }
}

struct RetireDuringBackendIo<'a> {
    inner: &'a LocalBackend,
    config: &'a DatabaseConfig,
    request: &'a RetireAccessRequest,
    retired: AtomicBool,
    puts: AtomicUsize,
    reads: AtomicUsize,
}

impl ImmutableBackend for RetireDuringBackendIo<'_> {
    fn prepare_publication(&self, key: &ObjectKey, incarnation: &str) -> agentscm::Result<()> {
        if !self.retired.swap(true, Ordering::SeqCst) {
            let result = operator_retire_access(self.config, self.request).unwrap();
            assert!(result.currently_retired);
        }
        self.inner.prepare_publication(key, incarnation)
    }

    fn put_if_absent(
        &self,
        key: &ObjectKey,
        incarnation: &str,
        bytes: &[u8],
    ) -> agentscm::Result<PutOutcome> {
        let outcome = self.inner.put_if_absent(key, incarnation, bytes)?;
        self.puts.fetch_add(1, Ordering::SeqCst);
        Ok(outcome)
    }

    fn get_verified(&self, key: &ObjectKey, incarnation: &str) -> agentscm::Result<Vec<u8>> {
        let bytes = self.inner.get_verified(key, incarnation)?;
        self.reads.fetch_add(1, Ordering::SeqCst);
        Ok(bytes)
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
#[ignore = "requires explicit AGENTSCM_PG_BIN_DIR; fresh isolated PostgreSQL18 only"]
fn pg18_retirement_during_upload_allows_bytes_but_final_fence_rejects_qualification() {
    let cluster = Cluster::new();
    let repository = "retire-during-upload";
    let admin_secret = generate_secret().unwrap();
    bootstrap(&cluster.config, repository, admin_secret.expose());
    let fixture = tempfile::tempdir().unwrap();
    let source = Engine::open(fixture.path().join("source")).unwrap();
    let base = source
        .checkpoint(
            None,
            &[FileChange {
                path: "README.md".into(),
                content: Some(b"retained base".to_vec()),
            }],
        )
        .unwrap();
    let root = source
        .checkpoint(
            Some(&base),
            &[FileChange {
                path: "src/main.rs".into(),
                content: Some(b"fn main() {}".to_vec()),
            }],
        )
        .unwrap();
    let backend = LocalBackend::open(fixture.path().join("objects")).unwrap();
    let mut controller = cluster.config.open(scope(repository)).unwrap();
    controller
        .reserve(&reservation("interrupted", &root))
        .unwrap();
    let retirement = RetireAccessRequest {
        scope: scope(repository),
        authority_generation: 1,
        operation: "retire-during-backend-io".into(),
    };
    let wrapped = RetireDuringBackendIo {
        inner: &backend,
        config: &cluster.config,
        request: &retirement,
        retired: AtomicBool::new(false),
        puts: AtomicUsize::new(0),
        reads: AtomicUsize::new(0),
    };
    let offer_path = fixture.path().join("interrupted-offer");
    assert!(matches!(
        controller.upload_and_qualify(1, "interrupted", &source, &wrapped, &offer_path),
        Err(TeamError::Unauthorized)
    ));
    let offer: SourceOffer = serde_json::from_slice(&std::fs::read(&offer_path).unwrap()).unwrap();
    assert_eq!(offer.roots, vec![root]);
    assert!(!offer.objects.is_empty());
    assert_eq!(wrapped.puts.load(Ordering::SeqCst), offer.objects.len());
    assert_eq!(wrapped.reads.load(Ordering::SeqCst), offer.objects.len());

    // Retirement is an authority fence, not an external write fence or object
    // deletion promise. The in-flight immutable writes finish and remain
    // readable, but the final PostgreSQL fence cannot publish qualification.
    for object in &offer.objects {
        let bytes = backend
            .get_verified(
                &ObjectKey::new(
                    "team",
                    repository,
                    offer.profile.as_str(),
                    object.id.as_str(),
                )
                .unwrap(),
                "1-interrupted",
            )
            .unwrap();
        assert_eq!(bytes.len() as u64, object.encoded_bytes);
    }
    let mut sql = cluster.admin();
    let publication = sql
        .query_one(
            "SELECT state,manifest,manifest_digest FROM team_publications \
             WHERE trust_domain='team' AND repository=$1 \
               AND generation=1 AND publication='interrupted'",
            &[&repository],
        )
        .unwrap();
    assert_eq!(publication.get::<_, String>(0), "uploading");
    assert_eq!(publication.get::<_, Option<String>>(1), None);
    assert_eq!(publication.get::<_, Option<String>>(2), None);
    let unpublished = sql
        .query_one(
            "SELECT \
               (SELECT count(*) FROM team_roots \
                 WHERE trust_domain='team' AND repository=$1), \
               (SELECT count(*) FROM team_outcomes \
                 WHERE trust_domain='team' AND repository=$1 AND kind='publication'), \
               (SELECT count(*) FROM team_events \
                 WHERE trust_domain='team' AND repository=$1 AND kind='publication'), \
               (SELECT count(*) FROM team_outbox \
                 WHERE trust_domain='team' AND repository=$1 AND kind='publication')",
            &[&repository],
        )
        .unwrap();
    for index in 0..4 {
        assert_eq!(unpublished.get::<_, i64>(index), 0);
    }
    let complete = CompleteRequest {
        generation: 1,
        publication: "interrupted".into(),
        operation: "must-not-complete".into(),
        manifest_digest: "0".repeat(64),
    };
    assert!(matches!(
        controller.complete(&complete),
        Err(TeamError::Unauthorized)
    ));
}
