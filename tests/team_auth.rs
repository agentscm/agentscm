#[path = "common/team_pg.rs"]
mod team_pg;

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Barrier};

use agentscm::backend::{ImmutableBackend, LocalBackend, ObjectKey, ObjectMetadata, PutOutcome};
use agentscm::model::FileChange;
use agentscm::store::Engine;
use agentscm::team::TeamError;
use agentscm::team::auth::{Credentials, Role, generate_secret};
use agentscm::team::publication::{CompleteRequest, RepositoryScope, ReserveRequest};
use agentscm::team::service::{
    BootstrapRequest, DatabaseConfig, ProvisionRequest, RevokeRequest, RolesRequest, TeamService,
    operator_bootstrap,
};
use team_pg::Cluster;

fn scope(repository: &str) -> RepositoryScope {
    RepositoryScope {
        trust_domain: "team".into(),
        repository: repository.into(),
    }
}
fn service(config: &DatabaseConfig, repository: &str, token: &str) -> TeamService {
    TeamService::connect(
        config,
        scope(repository),
        Credentials::bearer(token).unwrap(),
    )
    .unwrap()
}
fn bootstrap(config: &DatabaseConfig, repository: &str, credential: &str) {
    let request = BootstrapRequest {
        scope: scope(repository),
        authority_generation: 1,
        operation: "bootstrap".into(),
        max_bytes: 8_000_000,
        max_objects: 8_000,
        principal: "admin".into(),
        roles: vec![Role::Reader, Role::Contributor, Role::Administrator],
    };
    let original = operator_bootstrap(config, &request, credential).unwrap();
    assert_eq!(
        operator_bootstrap(config, &request, credential).unwrap(),
        original
    );
    assert_eq!(original.scope, scope(repository));
    assert_eq!(original.sequence, 1);
    assert_eq!(original.actor, None);
}

#[test]
fn credentials_are_opaque_and_generated_secrets_are_bounded() {
    let secret = generate_secret().unwrap();
    assert_eq!(secret.expose().len(), 64);
    assert!(!format!("{secret:?}").contains(secret.expose()));
    let material = Credentials::bearer(secret.expose()).unwrap();
    assert!(!format!("{material:?}").contains(secret.expose()));
    for invalid in ["", "abc", &"G".repeat(64), &"a".repeat(65)] {
        assert!(Credentials::bearer(invalid).is_err());
    }
}

struct CountedBackend<'a> {
    backend: &'a LocalBackend,
    reads: AtomicUsize,
}
impl ImmutableBackend for CountedBackend<'_> {
    fn prepare_publication(&self, key: &ObjectKey, incarnation: &str) -> agentscm::Result<()> {
        self.backend.prepare_publication(key, incarnation)
    }
    fn put_if_absent(
        &self,
        key: &ObjectKey,
        incarnation: &str,
        bytes: &[u8],
    ) -> agentscm::Result<PutOutcome> {
        self.backend.put_if_absent(key, incarnation, bytes)
    }
    fn get_verified(&self, key: &ObjectKey, incarnation: &str) -> agentscm::Result<Vec<u8>> {
        self.reads.fetch_add(1, Ordering::SeqCst);
        self.backend.get_verified(key, incarnation)
    }
    fn inspect_verified(
        &self,
        key: &ObjectKey,
        incarnation: &str,
    ) -> agentscm::Result<ObjectMetadata> {
        self.backend.inspect_verified(key, incarnation)
    }
    fn get_range_verified(
        &self,
        key: &ObjectKey,
        incarnation: &str,
        offset: u64,
        length: u64,
    ) -> agentscm::Result<Vec<u8>> {
        self.reads.fetch_add(1, Ordering::SeqCst);
        self.backend
            .get_range_verified(key, incarnation, offset, length)
    }
}
struct RevokeDuringRead<'a> {
    backend: &'a LocalBackend,
    config: &'a DatabaseConfig,
    admin: &'a str,
}
impl ImmutableBackend for RevokeDuringRead<'_> {
    fn prepare_publication(&self, key: &ObjectKey, incarnation: &str) -> agentscm::Result<()> {
        self.backend.prepare_publication(key, incarnation)
    }
    fn put_if_absent(
        &self,
        key: &ObjectKey,
        incarnation: &str,
        bytes: &[u8],
    ) -> agentscm::Result<PutOutcome> {
        self.backend.put_if_absent(key, incarnation, bytes)
    }
    fn get_verified(&self, key: &ObjectKey, incarnation: &str) -> agentscm::Result<Vec<u8>> {
        self.backend.get_verified(key, incarnation)
    }
    fn inspect_verified(
        &self,
        key: &ObjectKey,
        incarnation: &str,
    ) -> agentscm::Result<ObjectMetadata> {
        self.backend.inspect_verified(key, incarnation)
    }
    fn get_range_verified(
        &self,
        key: &ObjectKey,
        incarnation: &str,
        offset: u64,
        length: u64,
    ) -> agentscm::Result<Vec<u8>> {
        // The first read authorization transaction has ended. Another valid
        // administrator can revoke while backend I/O is in flight.
        let mut admin = service(self.config, "repo-a", self.admin);
        admin
            .revoke(
                "reader",
                &RevokeRequest {
                    authority_generation: 1,
                    operation: "revoke-during-read".into(),
                    expected_version: 1,
                },
            )
            .unwrap();
        self.backend
            .get_range_verified(key, incarnation, offset, length)
    }
}

#[test]
#[ignore = "requires explicit AGENTSCM_PG_BIN_DIR; fresh isolated PostgreSQL18 only"]
fn pg18_authentication_roles_revocation_atomicity_and_manifest_reads() {
    let cluster = Cluster::new();
    let admin_secret = generate_secret().unwrap();
    let other_secret = generate_secret().unwrap();
    bootstrap(&cluster.config, "repo-a", admin_secret.expose());
    bootstrap(&cluster.config, "repo-b", other_secret.expose());
    assert!(matches!(
        TeamService::connect(
            &cluster.config,
            scope("repo-b"),
            Credentials::bearer(admin_secret.expose()).unwrap()
        ),
        Err(TeamError::Unauthorized)
    ));
    assert!(matches!(
        TeamService::connect(
            &cluster.config,
            scope("missing"),
            Credentials::bearer(admin_secret.expose()).unwrap()
        ),
        Err(TeamError::Unauthorized)
    ));
    let mut admin = service(&cluster.config, "repo-a", admin_secret.expose());
    let reader_secret = generate_secret().unwrap();
    let contributor_secret = generate_secret().unwrap();
    let reviewer_secret = generate_secret().unwrap();
    let requests = [
        ("reader", reader_secret.expose(), vec![Role::Reader]),
        (
            "contributor",
            contributor_secret.expose(),
            vec![Role::Contributor],
        ),
        ("reviewer", reviewer_secret.expose(), vec![Role::Reviewer]),
    ];
    for (id, token, roles) in requests {
        let request = ProvisionRequest {
            authority_generation: 1,
            operation: format!("provision-{id}"),
            principal: id.into(),
            credential: token.into(),
            roles,
        };
        let original = admin.provision(&request).unwrap();
        assert_eq!(original.actor.as_deref(), Some("admin"));
        let row = cluster.admin().query_one("SELECT e.outcome,o.payload FROM team_events e JOIN team_outbox o USING(trust_domain,repository,sequence) WHERE e.trust_domain=$1 AND e.repository=$2 AND e.sequence=$3", &[&"team", &"repo-a", &original.sequence]).unwrap();
        for index in 0..2 {
            let retained: serde_json::Value =
                serde_json::from_str(&row.get::<_, String>(index)).unwrap();
            assert_eq!(retained["actor"], "admin");
            assert_eq!(retained["principal"], id);
        }
        assert_eq!(admin.provision(&request).unwrap(), original);
    }
    assert!(
        admin
            .revoke(
                "admin",
                &RevokeRequest {
                    authority_generation: 1,
                    operation: "revoke-last-admin".into(),
                    expected_version: 1
                }
            )
            .is_err()
    );
    assert!(matches!(
        admin.roles(
            "admin",
            &RolesRequest {
                authority_generation: 1,
                operation: "strip-last-admin".into(),
                expected_version: 1,
                roles: vec![Role::Reader]
            }
        ),
        Err(TeamError::Conflict(_))
    ));
    assert!(
        admin
            .principal("admin")
            .unwrap()
            .state
            .roles
            .contains(&Role::Administrator)
    );
    let mut reviewer = service(&cluster.config, "repo-a", reviewer_secret.expose());
    assert!(matches!(
        reviewer.protected_target_admission(),
        Err(TeamError::Denied)
    ));
    let granted = admin
        .roles(
            "reviewer",
            &RolesRequest {
                authority_generation: 1,
                operation: "grant-integrator".into(),
                expected_version: 1,
                roles: vec![Role::Reviewer, Role::Integrator],
            },
        )
        .unwrap();
    assert_eq!(granted.principal_version, 2);
    assert!(matches!(
        reviewer.protected_target_admission(),
        Err(TeamError::Unavailable)
    ));
    // No role hierarchy: reviewer/integrator still cannot read or write drafts.
    assert!(matches!(
        reviewer.inspect(1, "guess"),
        Err(TeamError::Denied)
    ));

    let fixture = tempfile::tempdir().unwrap();
    let engine = Engine::open(fixture.path().join("source")).unwrap();
    let backend = LocalBackend::open(fixture.path().join("backend")).unwrap();
    let root = engine
        .checkpoint(
            None,
            &[FileChange {
                path: "a.rs".into(),
                content: Some(b"fn main(){}".to_vec()),
            }],
        )
        .unwrap();
    let reservation = ReserveRequest {
        generation: 1,
        publication: "draft".into(),
        root: root.clone(),
        byte_limit: 1_000_000,
        object_limit: 1000,
        lease_seconds: 60,
    };
    assert!(matches!(
        reviewer.reserve(&reservation),
        Err(TeamError::Denied)
    ));
    let mut contributor = service(&cluster.config, "repo-a", contributor_secret.expose());
    contributor.reserve(&reservation).unwrap();
    // Qualification remains controller-only, through the same authority and
    // real native validator; it is not a fake client complete flag.
    let mut controller = cluster.config.open(scope("repo-a")).unwrap();
    let manifest = controller
        .upload_and_qualify(1, "draft", &engine, &backend, &fixture.path().join("offer"))
        .unwrap();
    let complete = CompleteRequest {
        generation: 1,
        publication: "draft".into(),
        operation: "complete-draft".into(),
        manifest_digest: manifest,
    };
    let outcome = contributor.complete(&complete).unwrap();
    assert_eq!(contributor.complete(&complete).unwrap(), outcome);
    let mut reader = service(&cluster.config, "repo-a", reader_secret.expose());
    assert_eq!(reader.inspect(1, "draft").unwrap().state, "complete");
    let counted = CountedBackend {
        backend: &backend,
        reads: AtomicUsize::new(0),
    };
    assert!(matches!(
        reviewer.read_object(&counted, 1, "draft", &root, 0, None),
        Err(TeamError::Denied)
    ));
    assert_eq!(counted.reads.load(Ordering::SeqCst), 0);
    assert!(
        reader
            .read_object(&counted, 1, "draft", &"0".repeat(64), 0, None)
            .is_err()
    );
    assert_eq!(counted.reads.load(Ordering::SeqCst), 0);
    let bytes = reader
        .read_object(&counted, 1, "draft", &root, 0, None)
        .unwrap();
    assert!(bytes.bytes.starts_with(b"AGSCM-R1\0"));
    assert_eq!(counted.reads.load(Ordering::SeqCst), 1);
    assert!(matches!(
        reader.read_object(&counted, 1, "draft", &root, 0, Some(1024 * 1024 + 1)),
        Err(TeamError::Storage(agentscm::Error::Limit(_)))
    ));
    assert_eq!(counted.reads.load(Ordering::SeqCst), 1);
    // Current permission before exact retained retry disclosure, including a
    // handle constructed while the principal was still allowed.
    admin
        .revoke(
            "contributor",
            &RevokeRequest {
                authority_generation: 1,
                operation: "revoke-contributor".into(),
                expected_version: 1,
            },
        )
        .unwrap();
    assert!(matches!(
        contributor.complete(&complete),
        Err(TeamError::Unauthorized)
    ));
    let revoker = RevokeDuringRead {
        backend: &backend,
        config: &cluster.config,
        admin: admin_secret.expose(),
    };
    assert!(matches!(
        reader.read_object(&revoker, 1, "draft", &root, 0, None),
        Err(TeamError::Unauthorized)
    ));
    assert!(matches!(
        reader.read_object(&counted, 1, "draft", &root, 0, None),
        Err(TeamError::Unauthorized)
    ));
    assert_eq!(counted.reads.load(Ordering::SeqCst), 1);

    // Losing the final auth outbox insert cannot leave new identity/roles or
    // durable outcome/event fragments.
    let mut sql = cluster.admin();
    sql.batch_execute("CREATE FUNCTION reject_auth() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN IF NEW.kind='identity' THEN RAISE EXCEPTION 'injected'; END IF; RETURN NEW; END $$; CREATE TRIGGER reject_auth BEFORE INSERT ON team_outbox FOR EACH ROW EXECUTE FUNCTION reject_auth();").unwrap();
    let failed_secret = generate_secret().unwrap();
    let request = ProvisionRequest {
        authority_generation: 1,
        operation: "atomic-failure".into(),
        principal: "failed".into(),
        credential: failed_secret.expose().into(),
        roles: vec![Role::Reader],
    };
    assert!(admin.provision(&request).is_err());
    let count: i64 = sql
        .query_one(
            "SELECT count(*) FROM team_principals WHERE principal='failed'",
            &[],
        )
        .unwrap()
        .get(0);
    assert_eq!(count, 0);
    let count: i64 = sql
        .query_one(
            "SELECT count(*) FROM team_outcomes WHERE operation='atomic-failure'",
            &[],
        )
        .unwrap()
        .get(0);
    assert_eq!(count, 0);
    sql.batch_execute("DROP TRIGGER reject_auth ON team_outbox; DROP FUNCTION reject_auth();")
        .unwrap();

    let race_secret = generate_secret().unwrap();
    admin
        .provision(&ProvisionRequest {
            authority_generation: 1,
            operation: "provision-race".into(),
            principal: "race".into(),
            credential: race_secret.expose().into(),
            roles: vec![Role::Reader],
        })
        .unwrap();
    let barrier = Arc::new(Barrier::new(2));
    let handles: Vec<_> = (0..2)
        .map(|index| {
            let config = cluster.config.clone();
            let token = admin_secret.expose().to_owned();
            let barrier = barrier.clone();
            std::thread::spawn(move || {
                let mut admin = service(&config, "repo-a", &token);
                barrier.wait();
                admin.roles(
                    "race",
                    &RolesRequest {
                        authority_generation: 1,
                        operation: format!("race-{index}"),
                        expected_version: 1,
                        roles: vec![Role::Reader, Role::Contributor],
                    },
                )
            })
        })
        .collect();
    let results: Vec<_> = handles
        .into_iter()
        .map(|handle| handle.join().unwrap())
        .collect();
    assert_eq!(results.iter().filter(|result| result.is_ok()).count(), 1);
    assert_eq!(
        results
            .iter()
            .filter(|result| matches!(result, Err(TeamError::Conflict(_))))
            .count(),
        1
    );
    let counts=sql.query_one("SELECT (SELECT count(*) FROM team_events WHERE trust_domain='team' AND repository='repo-a'),(SELECT count(*) FROM team_outbox WHERE trust_domain='team' AND repository='repo-a'),(SELECT count(*) FROM team_outcomes WHERE trust_domain='team' AND repository='repo-a')",&[]).unwrap();
    assert_eq!(counts.get::<_, i64>(0), counts.get::<_, i64>(1));
    assert_eq!(counts.get::<_, i64>(1), counts.get::<_, i64>(2));

    // Exact old-generation result remains typed and current authorization is
    // still required; absent historical result cannot execute in a new era.
    sql.execute(
        "UPDATE team_repositories SET generation=2 WHERE repository='repo-a'",
        &[],
    )
    .unwrap();
    let old = admin
        .roles(
            "reviewer",
            &RolesRequest {
                authority_generation: 1,
                operation: "grant-integrator".into(),
                expected_version: 1,
                roles: vec![Role::Reviewer, Role::Integrator],
            },
        )
        .unwrap();
    assert_eq!(old, granted);
    assert!(matches!(
        admin.roles(
            "reviewer",
            &RolesRequest {
                authority_generation: 1,
                operation: "lost-old-result".into(),
                expected_version: 2,
                roles: vec![Role::Reader]
            }
        ),
        Err(TeamError::Indeterminate)
    ));
}

#[test]
#[ignore = "requires explicit AGENTSCM_PG_BIN_DIR; fresh isolated PostgreSQL18 only"]
fn pg18_schema_upgrade_preserves_existing_publication_records() {
    let cluster = Cluster::new();
    let mut sql = cluster.admin();
    sql.batch_execute(include_str!("../src/team/schema.sql"))
        .unwrap();
    sql.execute("INSERT INTO team_repositories(trust_domain,repository,generation,max_bytes,max_objects) VALUES('team','legacy',1,1000000,1000)",&[]).unwrap();
    sql.execute(
        "INSERT INTO team_outcomes VALUES('team','legacy',1,'old','digest','{}')",
        &[],
    )
    .unwrap();
    drop(sql);
    let mut legacy = cluster.config.open(scope("legacy")).unwrap();
    // Exercise the shared controller lock after upgrading the original v1
    // tables. These are reservation bounds, not source qualification evidence.
    let migrated = ReserveRequest {
        generation: 1,
        publication: "migrated-draft".into(),
        root: "0".repeat(64),
        byte_limit: 4096,
        object_limit: 4,
        lease_seconds: 60,
    };
    legacy.reserve(&migrated).unwrap();
    legacy.abandon(1, "migrated-draft").unwrap();
    let mut sql = cluster.admin();
    let version: i32 = sql
        .query_one("SELECT version FROM team_schema", &[])
        .unwrap()
        .get(0);
    assert_eq!(version, 3);
    let retired: bool = sql
        .query_one(
            "SELECT retired FROM team_repositories WHERE repository='legacy'",
            &[],
        )
        .unwrap()
        .get(0);
    assert!(!retired);
    let row = sql
        .query_one(
            "SELECT request_digest,outcome,kind FROM team_outcomes WHERE operation='old'",
            &[],
        )
        .unwrap();
    assert_eq!(row.get::<_, String>(0), "digest");
    assert_eq!(row.get::<_, String>(1), "{}");
    assert_eq!(row.get::<_, String>(2), "publication");
}

#[test]
#[ignore = "requires explicit AGENTSCM_PG_BIN_DIR; fresh isolated PostgreSQL18 only"]
fn pg18_identity_budget_reserves_terminal_revocation_and_retains_actor() {
    let cluster = Cluster::new();
    let admin_secret = generate_secret().unwrap();
    bootstrap(&cluster.config, "budget", admin_secret.expose());
    let mut admin = service(&cluster.config, "budget", admin_secret.expose());
    let reader_secret = generate_secret().unwrap();
    admin
        .provision(&ProvisionRequest {
            authority_generation: 1,
            operation: "reader".into(),
            principal: "reader".into(),
            credential: reader_secret.expose().into(),
            roles: vec![Role::Reader],
        })
        .unwrap();
    let mut sql = cluster.admin();
    // Synthetic count-only padding tests the finite-log boundary. These rows
    // are not execution/audit evidence or a publication qualification fixture.
    sql.execute("INSERT INTO team_outcomes(trust_domain,repository,generation,operation,request_digest,outcome,kind) SELECT 'team','budget',1,'padding-'||n,'padding','{}','identity' FROM generate_series(1,9996) n", &[]).unwrap();
    let roles = RolesRequest {
        authority_generation: 1,
        operation: "blocked-roles".into(),
        expected_version: 1,
        roles: vec![Role::Reader, Role::Contributor],
    };
    assert!(matches!(
        admin.roles("reader", &roles),
        Err(TeamError::Storage(agentscm::Error::Limit(_)))
    ));
    let extra = generate_secret().unwrap();
    assert!(matches!(
        admin.provision(&ProvisionRequest {
            authority_generation: 1,
            operation: "blocked-provision".into(),
            principal: "extra".into(),
            credential: extra.expose().into(),
            roles: vec![Role::Reader]
        }),
        Err(TeamError::Storage(agentscm::Error::Limit(_)))
    ));
    assert!(admin.principal("extra").is_err());
    assert_eq!(admin.principal("reader").unwrap().state.version, 1);
    let request = RevokeRequest {
        authority_generation: 1,
        operation: "emergency-revoke".into(),
        expected_version: 1,
    };
    let outcome = admin.revoke("reader", &request).unwrap();
    assert_eq!(outcome.actor.as_deref(), Some("admin"));
    assert_eq!(admin.revoke("reader", &request).unwrap(), outcome);
    assert!(matches!(
        TeamService::connect(
            &cluster.config,
            scope("budget"),
            Credentials::bearer(reader_secret.expose()).unwrap()
        ),
        Err(TeamError::Unauthorized)
    ));
    let row = sql.query_one("SELECT (SELECT count(*) FROM team_outcomes WHERE repository='budget' AND kind='identity'),(SELECT count(*) FROM team_principals WHERE repository='budget' AND enabled)", &[]).unwrap();
    assert_eq!(row.get::<_, i64>(0), 9999);
    assert_eq!(row.get::<_, i64>(1), 1);
    let row = sql.query_one("SELECT e.outcome,o.payload FROM team_events e JOIN team_outbox o USING(trust_domain,repository,sequence) WHERE e.repository='budget' AND e.sequence=$1", &[&outcome.sequence]).unwrap();
    for index in 0..2 {
        let retained: serde_json::Value =
            serde_json::from_str(&row.get::<_, String>(index)).unwrap();
        assert_eq!(retained["actor"], "admin");
        assert_eq!(retained["action"], "revoke");
    }
}
